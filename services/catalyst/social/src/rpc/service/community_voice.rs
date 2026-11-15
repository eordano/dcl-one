//! Community voice: the operations every transport shares (upstream
//! `src/logic/community-voice/{community-voice,moderation,validation}.ts` at
//! `b6b2cef`) and the procedures answering them in their `oneof`s with
//! upstream's messages.

use super::SocialServiceImpl;
use super::domain::{
    host_gate, moderator_gate, not_a_member, target_membership_gate, target_user_gate,
};
use super::helpers::{Refusal, SocialError, normalize};
use crate::CommunityStanding;
use crate::comms::gatekeeper::voice::rooms::{CommunityVoiceAction, VoiceUnavailable};
use crate::notifications::community::{Audience, CommunityEvent, notify, now_ms};
use crate::rpc::context::Context;
use crate::rpc::db::VoiceGate;
use crate::rpc::drpc::service_module_definition::ProcedureContext;
use crate::rpc::proto::errors::{
    ConflictingError, ForbiddenError, InternalServerError, InvalidRequest, NotFoundError,
};
use crate::rpc::proto::v2::*;
use uuid::Uuid;

/// Upstream `InvalidCommunityIdError`.
const COMMUNITY_ID_REQUIRED: &str = "Community ID is required and cannot be empty";
/// Upstream `InvalidUserAddressError`.
const USER_ADDRESS_REQUIRED: &str = "User address is required and cannot be empty";
/// Upstream `InvalidGatekeeperIdentifierError` for a community id that is
/// not a uuid, where the gatekeeper is the first to read it.
const INVALID_GATEKEEPER_COMMUNITY_ID: &str = "Invalid Gatekeeper community ID";

/// Upstream `CommunityVoiceChatResourceNotFoundError`: the gatekeeper found
/// no room, or no such participant in it, for an authorized action.
pub(crate) fn resource_not_found(community_id: &str) -> Refusal {
    Refusal::NotFound(format!(
        "Community voice chat or participant not found for community {community_id}"
    ))
}

/// Upstream `CommunityVoiceChatNotFoundError`: no live room (or no active
/// community) for a start-dependent action.
fn room_not_found(community_id: &str) -> Refusal {
    Refusal::NotFound(format!(
        "Community voice chat not found for community {community_id}"
    ))
}

/// A runtime failure is the action's internal error (upstream's gatekeeper
/// call throwing); a room or participant the runtime does not hold is the
/// action's not-found (the gatekeeper's 404).
fn runtime(
    _action: &str,
    community_id: &str,
    outcome: Result<(), VoiceUnavailable>,
) -> Result<(), Refusal> {
    match outcome {
        Ok(()) => Ok(()),
        Err(VoiceUnavailable::NotFound) => Err(resource_not_found(community_id)),
        Err(e) => Err(Refusal::Internal(e.to_string())),
    }
}

fn internal(_what: &str) -> impl Fn(VoiceUnavailable) -> Refusal + '_ {
    move |e| Refusal::Internal(e.to_string())
}

/// The community id as upstream reads it: empty is an invalid request; one
/// that is not a uuid fails where upstream's first read of it fails, the
/// database (an internal error) or the gatekeeper adapter (an invalid
/// request, `gatekeeper_first`).
fn community_id(raw: &str, gatekeeper_first: bool) -> Result<Uuid, Refusal> {
    if raw.trim().is_empty() {
        return Err(Refusal::Invalid(COMMUNITY_ID_REQUIRED.into()));
    }
    Uuid::parse_str(raw).map_err(|_| {
        if gatekeeper_first {
            Refusal::Invalid(INVALID_GATEKEEPER_COMMUNITY_ID.into())
        } else {
            Refusal::Internal(format!("community id {raw} is not a uuid"))
        }
    })
}

/// The privileged adapter accepts the dashed UUID spelling only. PostgreSQL's
/// UUID parser is deliberately more permissive, so this check belongs at the
/// media boundary after the social authorization reads.
fn gatekeeper_identifiers(
    raw: &str,
    target: Option<&str>,
    opaque_error: bool,
) -> Result<(), Refusal> {
    let canonical = raw.len() == 36
        && raw.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        });
    let invalid = if !canonical {
        Some(INVALID_GATEKEEPER_COMMUNITY_ID)
    } else if target.is_some_and(|address| !crate::auth::is_eth_address(address)) {
        Some("Invalid Gatekeeper user address")
    } else {
        None
    };
    match invalid {
        Some(message) if opaque_error => Err(Refusal::Internal(message.into())),
        Some(message) => Err(Refusal::Invalid(message.into())),
        None => Ok(()),
    }
}

/// Upstream `assertIdentifiers` (promote, demote, reject, kick).
fn identifiers(raw: &str, target: &str) -> Result<Uuid, Refusal> {
    if raw.trim().is_empty() {
        return Err(Refusal::Invalid(COMMUNITY_ID_REQUIRED.into()));
    }
    if target.trim().is_empty() {
        return Err(Refusal::Invalid(USER_ADDRESS_REQUIRED.into()));
    }
    community_id(raw, false)
}

/// Upstream `getUserProfileData`: the name, claimed-name flag and picture
/// the room metadata carries; none without a profile.
async fn voice_profile(ctx: &Context, address: &str) -> Option<serde_json::Value> {
    let info = ctx.profiles().get_profile(address).await?;
    Some(serde_json::json!({
        "name": info.name,
        "has_claimed_name": info.has_claimed_name,
        "profile_picture_url": info.profile_picture_url,
    }))
}

/// Upstream `validateCommunityVoiceChatParticipation` (join, raising a
/// hand, self-unmute): a live room in an active community, no ban, and
/// membership when private. Answers the stored role (`none` for a public
/// guest).
async fn participation(ctx: &Context, cid: Uuid, raw: &str, me: &str) -> Result<String, Refusal> {
    gatekeeper_identifiers(raw, None, false)?;
    let live = ctx
        .voice()
        .community_voice_chat_active(cid)
        .await
        .map_err(internal("community voice chat status"))?;
    let gate = ctx.db().community_voice_gate(cid, me, me).await?;
    let Some(community) = gate.community.as_ref().filter(|_| live) else {
        return Err(room_not_found(raw));
    };
    if gate.a.banned {
        return Err(Refusal::Forbidden(format!(
            "The user {me} is banned from community {raw}"
        )));
    }
    let member = CommunityStanding::from_rows(false, gate.a.role.as_deref())
        != CommunityStanding::NotAMember;
    if community.private && !member {
        return Err(not_a_member(me, raw));
    }
    Ok(gate
        .a
        .role
        .filter(|_| member)
        .unwrap_or_else(|| "none".into()))
}

/// The STARTED update and its audience (upstream `startCommunityVoiceChat`
/// and `communityVoiceChatUpdateHandler`): every subscriber for a public,
/// listed community, its members otherwise, never the starter; the places'
/// positions and worlds and the thumbnail ride along.
async fn announce_started(
    ctx: &Context,
    me: &str,
    cid: Uuid,
    raw: &str,
    gate: &VoiceGate,
) -> Result<(), Refusal> {
    let Some(community) = &gate.community else {
        return Ok(());
    };
    let everyone = !community.private && !community.unlisted;
    let created_at = now_ms();
    // Persist the routing fact before optional enrichment, as upstream does before its
    // thumbnail/place reads. The generation guards notification metadata, not media I/O.
    let generation = ctx
        .db()
        .record_voice_notification(
            cid,
            raw,
            if everyone {
                crate::rpc::fanout::Scope::All
            } else {
                crate::rpc::fanout::Scope::Members
            },
        )
        .await?;
    let id = crate::CommunityId::from_uuid(cid);
    let (positions, worlds, image) = match ctx.communities() {
        Some(state) => crate::communities::voice::started_enrichment(state, id).await,
        None => (Vec::new(), Vec::new(), None),
    };
    let update = CommunityVoiceChatUpdate {
        stream_closed: None,
        community_id: raw.to_string(),
        created_at,
        status: CommunityVoiceChatStatus::CommunityVoiceChatStarted as i32,
        ended_at: None,
        positions,
        is_member: false,
        community_name: community.name.clone(),
        community_image: image.clone(),
        worlds,
    };
    let current_generation = ctx.db().voice_generation_is(raw, Some(generation)).await?;
    // upstream `broadcastVoiceChatStarted` (`logic/community/broadcaster.ts`):
    // the online members but the opener
    let online = ctx.topology().snapshot().wallets;
    let event = CommunityEvent {
        kind: "community_voice_chat_started",
        key: format!("{raw}-{created_at}"),
        community_id: id,
        metadata: serde_json::json!({
            "communityId": raw,
            "communityName": community.name,
            "thumbnailUrl": image.unwrap_or_default(),
        }),
    };
    let audience = Audience::OnlineMembersExcept {
        online: &online,
        except: me,
    };
    // Source Promise.all starts pub/sub and the broadcast before analytics,
    // without waiting for either notification transport to finish first.
    let publish = async {
        if current_generation {
            ctx.pubsub()
                .community_voice(ctx.db().pool(), cid, update, everyone, Some(me))
                .await;
        }
    };
    let broadcast = async {
        notify(ctx.db().pool(), event, audience).await;
    };
    let analytics = async {
        if let Some(state) = ctx.communities() {
            state.fire_analytics(
                "START_COMMUNITY_CALL",
                serde_json::json!({"call_id":raw,"user_id":me}),
            );
        }
    };
    tokio::join!(publish, broadcast, analytics);
    Ok(())
}

/// An owner or moderator opens the room (no room may be live); its
/// audience hears STARTED. Answers the opener's connection URL.
pub(crate) async fn start_community_voice_chat(
    ctx: &Context,
    me: &str,
    raw: &str,
    include_profile: bool,
) -> Result<String, Refusal> {
    let cid = community_id(raw, false)?;
    let gate = ctx.db().community_voice_gate(cid, me, me).await?;
    host_gate(&gate, me, raw, "start voice chats")?;
    gatekeeper_identifiers(raw, None, true)?;
    let live = ctx
        .voice()
        .community_voice_chat_active(cid)
        .await
        .map_err(internal("community voice chat status"))?;
    if live {
        return Err(Refusal::Conflict(format!(
            "Community {raw} already has an active voice chat"
        )));
    }
    let profile = if include_profile {
        voice_profile(ctx, me).await
    } else {
        None
    };
    let role = gate.a.role.clone().unwrap_or_default();
    let conn = ctx
        .voice()
        .community_voice_credentials(cid, me, &role, CommunityVoiceAction::Create, profile)
        .await
        .map_err(|error| match error {
            VoiceUnavailable::NotFound => resource_not_found(raw),
            other => internal("community voice credentials")(other),
        })?;
    announce_started(ctx, me, cid, raw, &gate).await?;
    Ok(conn)
}

/// A participant joins a live room; the ban is read again once the seat
/// exists, and a ban that landed meanwhile evicts it and refuses the join
/// (upstream `joinCommunityVoiceChat`).
pub(crate) async fn join_community_voice_chat(
    ctx: &Context,
    me: &str,
    raw: &str,
    include_profile: bool,
) -> Result<String, Refusal> {
    let cid = community_id(raw, false)?;
    gatekeeper_identifiers(raw, None, true)?;
    let role = participation(ctx, cid, raw, me).await?;
    let profile = if include_profile {
        voice_profile(ctx, me).await
    } else {
        None
    };
    let conn = ctx
        .voice()
        .community_voice_credentials(cid, me, &role, CommunityVoiceAction::Join, profile)
        .await
        .map_err(|error| match error {
            VoiceUnavailable::NotFound => resource_not_found(raw),
            other => internal("community voice credentials")(other),
        })?;
    if ctx.db().is_member_banned(&cid.to_string(), me).await? {
        tracing::warn!(%me, community_id = %cid, "banned while joining; evicting");
        if let Err(e) = ctx.voice().kick_player(cid, me).await {
            tracing::error!(error = %e, community_id = %cid, "failed to evict after a racing ban");
        }
        return Err(Refusal::Forbidden(format!(
            "The user {me} is banned from community {raw}"
        )));
    }
    if let Some(state) = ctx.communities() {
        state.fire_analytics(
            "JOIN_COMMUNITY_CALL",
            serde_json::json!({"call_id":raw,"user_id":me}),
        );
    }
    Ok(conn)
}

/// Raising a hand needs participation; lowering it is idempotent and never
/// discloses whether the room or the participant still exists.
pub(crate) async fn request_to_speak(
    ctx: &Context,
    me: &str,
    raw: &str,
    raising_hand: bool,
) -> Result<(), Refusal> {
    let cid = community_id(raw, true)?;
    if raising_hand {
        participation(ctx, cid, raw, me).await?;
    }
    gatekeeper_identifiers(raw, Some(me), false)?;
    let outcome = ctx.voice().request_to_speak(cid, me, raising_hand).await;
    match runtime("request to speak", raw, outcome) {
        Err(Refusal::NotFound(_)) if !raising_hand => Ok(()),
        other => other,
    }
}

/// Promotes `target` (upstream `promoteSpeakerInCommunityVoiceChat`), or
/// demotes it (`demoteSpeakerInCommunityVoiceChat`: anyone may demote
/// themselves, a moderator anyone else). Answers whether it was a
/// self-demotion.
pub(crate) async fn set_speaker(
    ctx: &Context,
    me: &str,
    raw: &str,
    target: &str,
    promote: bool,
) -> Result<bool, Refusal> {
    let cid = identifiers(raw, target)?;
    let target_address = normalize(target);
    let gate = ctx
        .db()
        .community_voice_gate(cid, me, &target_address)
        .await?;
    let self_demote = !promote && target_address == me;
    if promote {
        moderator_gate(&gate, me, &target_address, "promote speakers")?;
        let community = gate
            .community
            .as_ref()
            .ok_or_else(|| Refusal::Invalid(COMMUNITY_ID_REQUIRED.into()))?;
        target_user_gate(community.private, &gate.b, target, raw)?;
    } else {
        let community = gate
            .community
            .as_ref()
            .ok_or_else(|| Refusal::Invalid(COMMUNITY_ID_REQUIRED.into()))?;
        let actor_member = CommunityStanding::from_rows(false, gate.a.role.as_deref())
            != CommunityStanding::NotAMember;
        if community.private && !actor_member {
            return Err(not_a_member(me, raw));
        }
        if !self_demote {
            moderator_gate(&gate, me, &target_address, "demote other speakers")?;
            target_user_gate(community.private, &gate.b, target, raw)?;
        }
    }
    let action = if promote {
        "promote speaker"
    } else {
        "demote speaker"
    };
    gatekeeper_identifiers(raw, Some(&target_address), false)?;
    let outcome = ctx.voice().set_speaker(cid, &target_address, promote).await;
    runtime(action, raw, outcome).map(|()| self_demote)
}

pub(crate) async fn kick_player(
    ctx: &Context,
    me: &str,
    raw: &str,
    target: &str,
) -> Result<(), Refusal> {
    let cid = identifiers(raw, target)?;
    let target_address = normalize(target);
    let gate = ctx
        .db()
        .community_voice_gate(cid, me, &target_address)
        .await?;
    moderator_gate(&gate, me, &target_address, "kick players")?;
    gatekeeper_identifiers(raw, Some(&target_address), false)?;
    let outcome = ctx.voice().kick_player(cid, &target_address).await;
    runtime("kick player", raw, outcome)
}

pub(crate) async fn reject_speak_request(
    ctx: &Context,
    me: &str,
    raw: &str,
    target: &str,
) -> Result<(), Refusal> {
    let cid = identifiers(raw, target)?;
    let target_address = normalize(target);
    let gate = ctx
        .db()
        .community_voice_gate(cid, me, &target_address)
        .await?;
    moderator_gate(&gate, me, &target_address, "reject speak requests")?;
    let community = gate
        .community
        .as_ref()
        .ok_or_else(|| Refusal::Invalid(COMMUNITY_ID_REQUIRED.into()))?;
    // Public guests can raise a hand; a banned target stays rejectable
    // since denying grants nothing.
    target_membership_gate(community.private, &gate.b, target, raw)?;
    gatekeeper_identifiers(raw, Some(&target_address), false)?;
    let outcome = ctx.voice().reject_speak_request(cid, &target_address).await;
    runtime("reject speak request", raw, outcome)
}

/// An owner or moderator ends the live room; the audience its start
/// recorded hears ENDED.
pub(crate) async fn end_community_voice_chat(
    ctx: &Context,
    me: &str,
    raw: &str,
) -> Result<(), Refusal> {
    let cid = community_id(raw, false)?;
    let gate = ctx.db().community_voice_gate(cid, me, me).await?;
    host_gate(&gate, me, raw, "end voice chats")?;
    gatekeeper_identifiers(raw, None, true)?;
    let live = ctx
        .voice()
        .community_voice_chat_active(cid)
        .await
        .map_err(internal("community voice chat status"))?;
    if !live {
        return Err(room_not_found(raw));
    }
    // Claim only this raw key's generation before media teardown. Other spellings
    // retain their own facts until the poll observes the shared room has ended.
    let audience = ctx.db().voice_notification(raw).await?;
    let claimed = match &audience {
        Some(entry) => ctx.db().clear_voice_notification(entry).await?,
        None => true,
    };
    if let Err(error) = ctx.voice().end_community_voice_chat(cid, me).await {
        if claimed && let Some(entry) = &audience {
            ctx.db().restore_voice_notification(entry).await?;
        }
        return runtime("end community voice chat", raw, Err(error));
    }
    if claimed && ctx.db().voice_generation_is(raw, None).await? {
        let everyone = audience
            .as_ref()
            .is_some_and(|entry| entry.fresh && entry.scope == crate::rpc::fanout::Scope::All);
        ctx.pubsub()
            .community_voice(
                ctx.db().pool(),
                cid,
                crate::rpc::pubsub::community_voice_ended(raw),
                everyone,
                None,
            )
            .await;
    }
    if let Some(state) = ctx.communities() {
        state.fire_analytics(
            "END_COMMUNITY_CALL",
            serde_json::json!({"call_id":raw,"user_id":me}),
        );
    }
    Ok(())
}

/// Self-mute is free; self-unmute needs participation; muting another
/// needs the moderator gate with owner protection.
pub(crate) async fn mute_speaker(
    ctx: &Context,
    me: &str,
    raw: &str,
    target: &str,
    muted: bool,
) -> Result<(), Refusal> {
    if raw.trim().is_empty() {
        return Err(Refusal::Invalid(COMMUNITY_ID_REQUIRED.into()));
    }
    if target.trim().is_empty() {
        return Err(Refusal::Invalid(USER_ADDRESS_REQUIRED.into()));
    }
    let wire_target = target;
    let target = normalize(target);
    if target != me {
        let cid = community_id(raw, false)?;
        let gate = ctx.db().community_voice_gate(cid, me, &target).await?;
        moderator_gate(&gate, me, &target, "mute/unmute speakers")?;
    } else if !muted {
        let cid = community_id(raw, true)?;
        participation(ctx, cid, raw, me).await?;
    }
    let cid = community_id(raw, true)?;
    gatekeeper_identifiers(raw, Some(&target), false)?;
    let outcome = ctx.voice().mute_speaker(cid, &target, muted).await;
    runtime("mute speaker", raw, outcome)?;
    if let Some(state) = ctx.communities() {
        state.fire_analytics(
            "MUTE_SPEAKER_IN_COMMUNITY_CALL",
            serde_json::json!({"call_id":raw,"user_id":me,"target_user_id":wire_target}),
        );
    }
    Ok(())
}

/// The `oneof` arms a community-voice refusal lands in.
enum Answer {
    Invalid(InvalidRequest),
    Forbidden(ForbiddenError),
    NotFound(NotFoundError),
    Conflict(ConflictingError),
    Internal(InternalServerError),
}

/// A refusal onto its arm; an internal one carries the procedure's own
/// upstream message (`failed`), the detail only logged. A database error
/// raised with `?` also answers the procedure's application error arm.
fn answer(refusal: Refusal, failed: &str) -> Result<Answer, SocialError> {
    Ok(match refusal {
        Refusal::Invalid(m) => Answer::Invalid(InvalidRequest { message: Some(m) }),
        Refusal::Forbidden(m) => Answer::Forbidden(ForbiddenError { message: Some(m) }),
        Refusal::NotFound(m) => Answer::NotFound(NotFoundError { message: Some(m) }),
        Refusal::Conflict(m) => Answer::Conflict(ConflictingError { message: Some(m) }),
        Refusal::Failed(detail) | Refusal::Internal(detail) | Refusal::RateLimited(detail) => {
            tracing::error!(%detail, "{failed}");
            Answer::Internal(InternalServerError {
                message: Some(
                    if failed == "Failed to mute/unmute speaker from community voice chat" {
                        format!("{failed}: {detail}")
                    } else {
                        failed.to_string()
                    },
                ),
            })
        }
    })
}

/// One procedure's response: `ok` from the operation's value, a refusal
/// onto the arms the response has (a not-found or conflict it lacks is its
/// internal error).
macro_rules! respond {
    ($module:ident, $response:ident, $result:expr, $failed:expr, $ok:expr) => {{
        let internal = |message: Option<String>| {
            $module::Response::InternalServerError(InternalServerError {
                message: message.or(Some($failed.to_string())),
            })
        };
        let response = match $result {
            Ok(value) => $module::Response::Ok($ok(value)),
            Err(refusal) => match answer(refusal, $failed)? {
                Answer::Invalid(i) => $module::Response::InvalidRequest(i),
                Answer::Forbidden(f) => $module::Response::ForbiddenError(f),
                Answer::NotFound(n) => respond!(@not_found $module, n, internal),
                Answer::Conflict(c) => respond!(@conflict $module, c, internal),
                Answer::Internal(i) => internal(i.message),
            },
        };
        Ok($response {
            response: Some(response),
        })
    }};
    (@not_found start_community_voice_chat_response, $n:ident, $internal:ident) => {{
        let _ = $n;
        $internal(None)
    }};
    (@not_found $module:ident, $n:ident, $internal:ident) => {
        $module::Response::NotFoundError($n)
    };
    (@conflict start_community_voice_chat_response, $c:ident, $internal:ident) => {
        start_community_voice_chat_response::Response::ConflictingError($c)
    };
    (@conflict $module:ident, $c:ident, $internal:ident) => {{
        let _ = $c;
        $internal(None)
    }};
}

impl SocialServiceImpl {
    pub(crate) async fn start_community_voice_chat(
        &self,
        request: StartCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<StartCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        respond!(
            start_community_voice_chat_response,
            StartCommunityVoiceChatResponse,
            start_community_voice_chat(&context.server_context, &me, &request.community_id, true)
                .await,
            "Failed to start community voice chat",
            |connection_url| start_community_voice_chat_response::Ok {
                credentials: Some(CommunityVoiceChatCredentials { connection_url }),
            }
        )
    }

    pub(crate) async fn join_community_voice_chat(
        &self,
        request: JoinCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<JoinCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        let community_id = request.community_id;
        respond!(
            join_community_voice_chat_response,
            JoinCommunityVoiceChatResponse,
            join_community_voice_chat(&context.server_context, &me, &community_id, true).await,
            "Failed to join community voice chat",
            |connection_url| join_community_voice_chat_response::Ok {
                voice_chat_id: community_id,
                credentials: Some(CommunityVoiceChatCredentials { connection_url }),
            }
        )
    }

    pub(crate) async fn request_to_speak_in_community_voice_chat(
        &self,
        request: RequestToSpeakInCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<RequestToSpeakInCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        let raising = request.is_raising_hand;
        respond!(
            request_to_speak_in_community_voice_chat_response,
            RequestToSpeakInCommunityVoiceChatResponse,
            request_to_speak(&context.server_context, &me, &request.community_id, raising).await,
            "Failed to update hand status in community voice chat",
            |()| request_to_speak_in_community_voice_chat_response::Ok {
                message: if raising {
                    "Request to speak sent successfully"
                } else {
                    "Hand lowered successfully"
                }
                .into(),
            }
        )
    }

    pub(crate) async fn promote_speaker_in_community_voice_chat(
        &self,
        request: PromoteSpeakerInCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<PromoteSpeakerInCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        respond!(
            promote_speaker_in_community_voice_chat_response,
            PromoteSpeakerInCommunityVoiceChatResponse,
            set_speaker(
                &context.server_context,
                &me,
                &request.community_id,
                &request.user_address,
                true,
            )
            .await,
            "Failed to promote speaker in community voice chat",
            |_| promote_speaker_in_community_voice_chat_response::Ok {
                message: "User promoted to speaker successfully".into(),
            }
        )
    }

    pub(crate) async fn demote_speaker_in_community_voice_chat(
        &self,
        request: DemoteSpeakerInCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<DemoteSpeakerInCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        respond!(
            demote_speaker_in_community_voice_chat_response,
            DemoteSpeakerInCommunityVoiceChatResponse,
            set_speaker(
                &context.server_context,
                &me,
                &request.community_id,
                &request.user_address,
                false,
            )
            .await,
            "Failed to demote speaker in community voice chat",
            |self_demote| demote_speaker_in_community_voice_chat_response::Ok {
                message: if self_demote {
                    "You have been demoted to listener successfully"
                } else {
                    "User demoted to listener successfully"
                }
                .into(),
            }
        )
    }

    pub(crate) async fn kick_player_from_community_voice_chat(
        &self,
        request: KickPlayerFromCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<KickPlayerFromCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        respond!(
            kick_player_from_community_voice_chat_response,
            KickPlayerFromCommunityVoiceChatResponse,
            kick_player(
                &context.server_context,
                &me,
                &request.community_id,
                &request.user_address,
            )
            .await,
            "Failed to kick player from community voice chat",
            |()| kick_player_from_community_voice_chat_response::Ok {
                message: "User kicked from voice chat successfully".into(),
            }
        )
    }

    pub(crate) async fn reject_speak_request_in_community_voice_chat(
        &self,
        request: RejectSpeakRequestInCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<RejectSpeakRequestInCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        respond!(
            reject_speak_request_in_community_voice_chat_response,
            RejectSpeakRequestInCommunityVoiceChatResponse,
            reject_speak_request(
                &context.server_context,
                &me,
                &request.community_id,
                &request.user_address,
            )
            .await,
            "Failed to reject speak request in community voice chat",
            |()| reject_speak_request_in_community_voice_chat_response::Ok {
                message: "Speak request rejected successfully".into(),
            }
        )
    }

    pub(crate) async fn end_community_voice_chat(
        &self,
        request: EndCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<EndCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        respond!(
            end_community_voice_chat_response,
            EndCommunityVoiceChatResponse,
            end_community_voice_chat(&context.server_context, &me, &request.community_id).await,
            "Failed to end community voice chat",
            |()| end_community_voice_chat_response::Ok {
                message: "Community voice chat ended successfully".into(),
            }
        )
    }

    pub(crate) async fn mute_speaker_from_community_voice_chat(
        &self,
        request: MuteSpeakerFromCommunityVoiceChatPayload,
        context: ProcedureContext<Context>,
    ) -> Result<MuteSpeakerFromCommunityVoiceChatResponse, SocialError> {
        let me = Self::caller(&context)?;
        let muted = request.muted;
        respond!(
            mute_speaker_from_community_voice_chat_response,
            MuteSpeakerFromCommunityVoiceChatResponse,
            mute_speaker(
                &context.server_context,
                &me,
                &request.community_id,
                &request.user_address,
                muted,
            )
            .await,
            "Failed to mute/unmute speaker from community voice chat",
            |()| mute_speaker_from_community_voice_chat_response::Ok { muted }
        )
    }
}
