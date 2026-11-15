//! Community voice through the RPC handlers, the cases of upstream's
//! `logic/community-voice.spec.ts` and the community-voice
//! `rpc-server/services` specs (`decentraland/social-service-ea` at
//! `b6b2cefefdf727b2ed47ddb887e77d226a41eea6`) Catalyst implements but the start/join test does not reach:
//! who may end, mute, raise a hand, promote, demote, reject and kick, in a
//! public and a private community, and what reaches the runtime.

use super::*;

/// The variant a response holds, by name (`Ok`, `ForbiddenError`, ...).
fn variant<T: std::fmt::Debug>(response: &Option<T>) -> String {
    let debug = format!("{response:?}");
    let inner = debug.strip_prefix("Some(").unwrap_or(&debug);
    inner.split(['(', ' ', '{']).next().unwrap().to_string()
}

struct Rig {
    pool: PgPool,
    ctx: Context,
    voice: Arc<FakeVoiceRooms>,
    community: Uuid,
    cid: String,
    owner: String,
    moderator: String,
    member: String,
    banned: String,
    guest: String,
}

impl Rig {
    /// A community (`private` or not) with an owner, a moderator, a member,
    /// a banned wallet and a guest with no role.
    async fn new(private: bool) -> Option<Self> {
        Some(Self::with_pool(private, scratch_pool().await?).await)
    }

    async fn with_pool(private: bool, pool: PgPool) -> Self {
        let voice = Arc::new(FakeVoiceRooms::new(pool.clone()));
        let ctx = test_ctx(Db::new(pool.clone()), voice.clone());
        let (owner, moderator, member, banned, guest) = (
            addr("cvo"),
            addr("cvm"),
            addr("cvb"),
            addr("cvx"),
            addr("cvg"),
        );
        let community = seed_community(&pool, true, &owner, "owner").await;
        sqlx::query("UPDATE social.communities SET private = $2 WHERE id = $1")
            .bind(community)
            .bind(private)
            .execute(&pool)
            .await
            .unwrap();
        for (who, role) in [(&moderator, "moderator"), (&member, "member")] {
            sqlx::query("INSERT INTO social.community_members (community_id, member_address, role) VALUES ($1, $2, $3)")
                .bind(community)
                .bind(who)
                .bind(role)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO social.community_bans (community_id, banned_address, banned_by) VALUES ($1, $2, $3)")
            .bind(community)
            .bind(&banned)
            .bind(&owner)
            .execute(&pool)
            .await
            .unwrap();
        Self {
            pool,
            ctx,
            voice,
            cid: community.to_string(),
            community,
            owner,
            moderator,
            member,
            banned,
            guest,
        }
    }

    fn as_(&self, me: &str) -> ProcedureContext<Context> {
        procedure_ctx(&self.ctx, me, 61)
    }

    fn runtime_saw(&self, line: &str) -> bool {
        self.voice.calls.lock().iter().any(|c| c == line)
    }

    async fn mute(&self, me: &str, target: &str, muted: bool) -> String {
        let r = SocialServiceImpl
            .mute_speaker_from_community_voice_chat(
                MuteSpeakerFromCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                    user_address: target.to_string(),
                    muted,
                },
                self.as_(me),
            )
            .await
            .unwrap();
        if let Some(mute_speaker_from_community_voice_chat_response::Response::Ok(ok)) = &r.response
        {
            assert_eq!(ok.muted, muted);
        }
        variant(&r.response)
    }

    async fn hand(&self, me: &str, raising: bool) -> String {
        let r = SocialServiceImpl
            .request_to_speak_in_community_voice_chat(
                RequestToSpeakInCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                    is_raising_hand: raising,
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn promote(&self, me: &str, target: &str) -> String {
        let r = SocialServiceImpl
            .promote_speaker_in_community_voice_chat(
                PromoteSpeakerInCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                    user_address: target.to_string(),
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn demote(&self, me: &str, target: &str) -> String {
        let r = SocialServiceImpl
            .demote_speaker_in_community_voice_chat(
                DemoteSpeakerInCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                    user_address: target.to_string(),
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn reject(&self, me: &str, target: &str) -> String {
        let r = SocialServiceImpl
            .reject_speak_request_in_community_voice_chat(
                RejectSpeakRequestInCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                    user_address: target.to_string(),
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn kick(&self, me: &str, target: &str) -> String {
        let r = SocialServiceImpl
            .kick_player_from_community_voice_chat(
                KickPlayerFromCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                    user_address: target.to_string(),
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn start(&self, me: &str) -> String {
        let r = SocialServiceImpl
            .start_community_voice_chat(
                StartCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn join(&self, me: &str) -> String {
        let r = SocialServiceImpl
            .join_community_voice_chat(
                JoinCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn end(&self, me: &str) -> String {
        let r = SocialServiceImpl
            .end_community_voice_chat(
                EndCommunityVoiceChatPayload {
                    community_id: self.cid.clone(),
                },
                self.as_(me),
            )
            .await
            .unwrap();
        variant(&r.response)
    }

    async fn drop(self) {
        drop_community(&self.pool, self.community).await;
    }
}

fn checksummed(a: &str) -> String {
    a.to_uppercase().replacen("0X", "0x", 1)
}

fn voice_updates(
    rx: &mut tokio::sync::broadcast::Receiver<Arc<SocialEvent>>,
) -> Vec<CommunityVoiceChatUpdate> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|e| match e.as_ref() {
            SocialEvent::CommunityVoice(u) => Some(u.clone()),
            _ => None,
        })
        .collect()
}

/// A moderator starts like the owner, never over a live room; the owner or a
/// moderator ends a live room, a member never. As upstream's
/// `communityVoiceChatUpdateHandler` (`src/logic/updates.ts`): a public,
/// listed community's STARTED reaches every subscriber, `isMember` per
/// recipient, never the starter, and its ENDED the same audience; a private
/// or unlisted one's reaches its members only.
#[tokio::test]
async fn moderators_start_and_end_and_members_cannot() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let mut to_member = rig.ctx.pubsub().subscribe(&rig.member);
    let mut to_starter = rig.ctx.pubsub().subscribe(&rig.moderator);
    let mut to_guest = rig.ctx.pubsub().subscribe(&rig.guest);
    assert_eq!(rig.end(&rig.owner).await, "NotFoundError", "no live room");
    assert_eq!(rig.start(&rig.moderator).await, "Ok");
    assert!(voice_updates(&mut to_starter).is_empty());
    let started = |updates: Vec<CommunityVoiceChatUpdate>, member: bool| {
        updates.iter().any(|u| {
            u.community_id == rig.cid
                && u.status == CommunityVoiceChatStatus::CommunityVoiceChatStarted as i32
                && u.community_name == "Voice Soft Delete Community"
                && u.is_member == member
        })
    };
    assert!(started(voice_updates(&mut to_member), true));
    assert!(
        started(voice_updates(&mut to_guest), false),
        "public and listed: everyone"
    );
    // a live room is not started again (upstream `CommunityVoiceChatAlreadyActiveError`)
    assert_eq!(rig.start(&rig.owner).await, "ConflictingError");
    assert_eq!(rig.end(&rig.member).await, "ForbiddenError");
    assert_eq!(rig.end(&rig.guest).await, "ForbiddenError");
    assert_eq!(rig.end(&rig.moderator).await, "Ok");
    assert!(rig.runtime_saw(&format!(
        "end-community {} {}",
        rig.community, rig.moderator
    )));
    for rx in [&mut to_member, &mut to_guest] {
        let ended = voice_updates(rx);
        assert!(
            ended.iter().any(|u| u.community_id == rig.cid
                && u.status == CommunityVoiceChatStatus::CommunityVoiceChatEnded as i32
                && u.ended_at.is_some()
                && !u.is_member),
            "{ended:?}"
        );
    }
    assert_eq!(rig.end(&rig.owner).await, "NotFoundError", "ended already");
    rig.drop().await;

    // unlisted: members only, at start and at end
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    sqlx::query("UPDATE social.communities SET unlisted = TRUE WHERE id = $1")
        .bind(rig.community)
        .execute(&rig.pool)
        .await
        .unwrap();
    let mut to_member = rig.ctx.pubsub().subscribe(&rig.member);
    let mut to_guest = rig.ctx.pubsub().subscribe(&rig.guest);
    assert_eq!(rig.start(&rig.owner).await, "Ok");
    assert_eq!(rig.end(&rig.owner).await, "Ok");
    let member_saw = voice_updates(&mut to_member);
    assert_eq!(member_saw.len(), 2, "{member_saw:?}");
    assert!(member_saw.iter().all(|u| u.is_member));
    assert!(voice_updates(&mut to_guest).is_empty());
    rig.drop().await;
}

/// Muting another speaker needs a moderator who is not targeting the owner;
/// the refused ones never reach the runtime. Muting yourself is free,
/// unmuting yourself needs to be in the room's audience.
#[tokio::test]
async fn who_may_mute_whom() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let (owner, moderator, member, guest, banned) = (
        &rig.owner,
        &rig.moderator,
        &rig.member,
        &rig.guest,
        &rig.banned,
    );
    let c = rig.community;
    for (me, target, muted) in [
        (moderator, member, true),
        (moderator, member, false),
        (owner, moderator, true),
        (moderator, moderator, true),
        (moderator, banned, true),
    ] {
        assert_eq!(rig.mute(me, target, muted).await, "Ok", "{me} -> {target}");
        assert!(rig.runtime_saw(&format!("mute {c} {target} {muted}")));
    }
    // a checksummed target reaches the runtime lowercased
    assert_eq!(rig.mute(moderator, &checksummed(member), true).await, "Ok");
    assert!(rig.runtime_saw(&format!("mute {c} {member} true")));
    for (me, target) in [(moderator, owner), (member, guest), (guest, member)] {
        rig.voice.calls.lock().clear();
        assert_eq!(
            rig.mute(me, target, true).await,
            "ForbiddenError",
            "{me} -> {target}"
        );
        assert!(rig.voice.calls.lock().is_empty(), "{me} -> {target}");
    }
    // yourself: muting is free; unmuting in a public community too, but not banned
    for me in [member, guest, banned] {
        assert_eq!(rig.mute(me, me, true).await, "Ok", "{me}");
    }
    // unmuting yourself needs a live room (upstream
    // `validateCommunityVoiceChatParticipation`, `validation.ts:41-42`)
    assert_eq!(rig.mute(guest, guest, false).await, "NotFoundError");
    assert_eq!(rig.start(owner).await, "Ok");
    assert_eq!(rig.mute(guest, guest, false).await, "Ok");
    assert_eq!(rig.mute(banned, banned, false).await, "ForbiddenError");
    rig.drop().await;

    // a private community admits its members only
    let Some(rig) = Rig::new(true).await else {
        return;
    };
    assert_eq!(rig.start(&rig.owner).await, "Ok");
    assert_eq!(rig.mute(&rig.member, &rig.member, false).await, "Ok");
    assert_eq!(
        rig.mute(&rig.guest, &rig.guest, false).await,
        "ForbiddenError"
    );
    assert_eq!(rig.mute(&rig.guest, &rig.guest, true).await, "Ok");
    rig.drop().await;
}

/// Raising a hand needs to be in the audience; lowering it never does.
#[tokio::test]
async fn who_may_raise_a_hand() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let c = rig.community;
    // raising a hand needs a live room (upstream `validation.ts:41-42`)
    assert_eq!(rig.hand(&rig.guest, true).await, "NotFoundError");
    assert_eq!(rig.start(&rig.owner).await, "Ok");
    assert_eq!(rig.hand(&rig.guest, true).await, "Ok");
    assert!(rig.runtime_saw(&format!("request-to-speak {c} {} true", rig.guest)));
    rig.voice.calls.lock().clear();
    assert_eq!(rig.hand(&rig.banned, true).await, "ForbiddenError");
    assert!(rig.voice.calls.lock().is_empty());
    assert_eq!(rig.hand(&rig.banned, false).await, "Ok");
    rig.drop().await;

    let Some(rig) = Rig::new(true).await else {
        return;
    };
    assert_eq!(rig.start(&rig.owner).await, "Ok");
    assert_eq!(rig.hand(&rig.member, true).await, "Ok");
    rig.voice.calls.lock().clear();
    assert_eq!(rig.hand(&rig.guest, true).await, "ForbiddenError");
    assert!(rig.voice.calls.lock().is_empty());
    assert_eq!(rig.hand(&rig.guest, false).await, "Ok");
    rig.drop().await;
}

/// Promote, demote, reject and kick: a moderator or the owner acts on the
/// audience, never a moderator on the owner, never a member or a guest; in
/// a private community the target must be a member.
#[tokio::test]
async fn who_may_moderate_the_room() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let (owner, moderator, member, guest) = (&rig.owner, &rig.moderator, &rig.member, &rig.guest);
    let c = rig.community;
    assert_eq!(rig.promote(moderator, member).await, "Ok");
    assert!(rig.runtime_saw(&format!("set-speaker {c} {member} true")));
    assert_eq!(rig.promote(moderator, guest).await, "Ok");
    assert_eq!(rig.promote(owner, moderator).await, "Ok");
    assert_eq!(rig.demote(moderator, member).await, "Ok");
    assert!(rig.runtime_saw(&format!("set-speaker {c} {member} false")));
    assert_eq!(rig.demote(owner, moderator).await, "Ok");
    assert_eq!(rig.reject(moderator, guest).await, "Ok");
    assert!(rig.runtime_saw(&format!("reject-speak-request {c} {guest}")));
    assert_eq!(rig.reject(owner, moderator).await, "Ok");
    for kicked in [member, guest, &rig.banned] {
        assert_eq!(rig.kick(moderator, kicked).await, "Ok", "{kicked}");
        assert!(rig.runtime_saw(&format!("kick {c} {kicked}")));
    }
    for (me, target) in [(moderator, owner), (member, guest), (guest, member)] {
        assert_eq!(
            rig.promote(me, target).await,
            "ForbiddenError",
            "promote {me} {target}"
        );
        assert_eq!(
            rig.demote(me, target).await,
            "ForbiddenError",
            "demote {me} {target}"
        );
        assert_eq!(
            rig.reject(me, target).await,
            "ForbiddenError",
            "reject {me} {target}"
        );
    }
    for me in [member, guest] {
        assert_eq!(rig.kick(me, owner).await, "ForbiddenError", "kick {me}");
    }
    rig.drop().await;

    let Some(rig) = Rig::new(true).await else {
        return;
    };
    assert_eq!(rig.promote(&rig.moderator, &rig.member).await, "Ok");
    assert_eq!(
        rig.promote(&rig.moderator, &rig.guest).await,
        "ForbiddenError"
    );
    assert_eq!(
        rig.demote(&rig.moderator, &rig.guest).await,
        "ForbiddenError"
    );
    assert_eq!(
        rig.reject(&rig.moderator, &rig.guest).await,
        "ForbiddenError"
    );
    rig.drop().await;
}

/// Calls `$method` as `$me` and answers the response's Debug form, its
/// variant and message included.
macro_rules! said {
    ($rig:expr, $me:expr, $method:ident, $payload:expr) => {
        format!(
            "{:?}",
            SocialServiceImpl
                .$method($payload, $rig.as_($me))
                .await
                .unwrap()
                .response
                .unwrap()
        )
    };
}

/// A refusal arm as its Debug form shows it.
fn arm(variant: &str, message: &str) -> String {
    format!("{variant}({variant} {{ message: Some({message:?}) }})")
}

fn ok(message: &str) -> String {
    format!("Ok(Ok {{ message: {message:?} }})")
}

fn promote(cid: &str, target: &str) -> PromoteSpeakerInCommunityVoiceChatPayload {
    PromoteSpeakerInCommunityVoiceChatPayload {
        community_id: cid.to_string(),
        user_address: target.to_string(),
    }
}

fn demote(cid: &str, target: &str) -> DemoteSpeakerInCommunityVoiceChatPayload {
    DemoteSpeakerInCommunityVoiceChatPayload {
        community_id: cid.to_string(),
        user_address: target.to_string(),
    }
}

fn kick(cid: &str, target: &str) -> KickPlayerFromCommunityVoiceChatPayload {
    KickPlayerFromCommunityVoiceChatPayload {
        community_id: cid.to_string(),
        user_address: target.to_string(),
    }
}

fn reject(cid: &str, target: &str) -> RejectSpeakRequestInCommunityVoiceChatPayload {
    RejectSpeakRequestInCommunityVoiceChatPayload {
        community_id: cid.to_string(),
        user_address: target.to_string(),
    }
}

/// Upstream's identifiers, missing communities and unparsable ids
/// (`src/logic/community-voice/{moderation,validation}.ts`, the community
/// voice handlers at `b6b2cef`): an empty community id or target is an
/// invalid request; a missing community fails the first gate that reads it
/// (start: not a member; join: no room; promote: not a moderator; demote:
/// an invalid id); an id that is not a uuid fails where upstream reads it
/// first (the database: internal; the gatekeeper: invalid request).
#[tokio::test]
async fn identifiers_and_missing_communities_answer_as_upstream() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let (owner, member) = (&rig.owner, &rig.member);
    let cid = rig.cid.clone();
    let required = arm(
        "InvalidRequest",
        "Community ID is required and cannot be empty",
    );
    assert_eq!(
        said!(
            rig,
            owner,
            start_community_voice_chat,
            StartCommunityVoiceChatPayload {
                community_id: " ".into()
            }
        ),
        required
    );
    assert_eq!(
        said!(
            rig,
            owner,
            promote_speaker_in_community_voice_chat,
            promote("", member)
        ),
        required
    );
    assert_eq!(
        said!(
            rig,
            owner,
            promote_speaker_in_community_voice_chat,
            promote(&cid, " ")
        ),
        arm(
            "InvalidRequest",
            "User address is required and cannot be empty"
        )
    );
    assert_eq!(
        said!(
            rig,
            owner,
            mute_speaker_from_community_voice_chat,
            MuteSpeakerFromCommunityVoiceChatPayload {
                community_id: cid.clone(),
                user_address: String::new(),
                muted: true,
            }
        ),
        "InvalidRequest(InvalidRequest { message: Some(\"User address is required and cannot be empty\") })"
    );

    let missing = Uuid::new_v4().to_string();
    assert_eq!(
        said!(
            rig,
            owner,
            start_community_voice_chat,
            StartCommunityVoiceChatPayload {
                community_id: missing.clone()
            }
        ),
        arm(
            "ForbiddenError",
            &format!("User {owner} is not a member of community {missing}")
        )
    );
    assert_eq!(
        said!(
            rig,
            owner,
            join_community_voice_chat,
            JoinCommunityVoiceChatPayload {
                community_id: missing.clone()
            }
        ),
        arm(
            "NotFoundError",
            &format!("Community voice chat not found for community {missing}")
        )
    );
    assert_eq!(
        said!(
            rig,
            owner,
            promote_speaker_in_community_voice_chat,
            promote(&missing, member)
        ),
        arm(
            "ForbiddenError",
            "Only community owners and moderators can promote speakers"
        )
    );
    assert_eq!(
        said!(
            rig,
            owner,
            demote_speaker_in_community_voice_chat,
            demote(&missing, owner)
        ),
        required
    );

    let garbled = "not-a-uuid";
    assert_eq!(
        said!(
            rig,
            owner,
            promote_speaker_in_community_voice_chat,
            promote(garbled, member)
        ),
        arm(
            "InternalServerError",
            "Failed to promote speaker in community voice chat"
        )
    );
    assert_eq!(
        said!(
            rig,
            owner,
            join_community_voice_chat,
            JoinCommunityVoiceChatPayload {
                community_id: garbled.into()
            }
        ),
        arm("InternalServerError", "Failed to join community voice chat")
    );
    assert_eq!(
        said!(
            rig,
            member,
            request_to_speak_in_community_voice_chat,
            RequestToSpeakInCommunityVoiceChatPayload {
                community_id: garbled.into(),
                is_raising_hand: true,
            }
        ),
        arm("InvalidRequest", "Invalid Gatekeeper community ID")
    );
    rig.drop().await;
}

/// The refusal and success messages of the ban gates and self-demotion, as
/// upstream's `validation.ts` (`:92-95`, `:141-144`, `:207-209`) and
/// `moderation.ts:83-125` word them; the gate matrix itself is
/// `bans_override_retained_roles_without_preventing_revocation` and
/// `speakers_can_demote_themselves_without_moderator_authority`.
#[tokio::test]
async fn ban_and_self_demotion_answers_carry_upstreams_messages() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let (owner, moderator, member, guest, banned) = (
        &rig.owner,
        &rig.moderator,
        &rig.member,
        &rig.guest,
        &rig.banned,
    );
    let cid = rig.cid.clone();
    assert_eq!(
        said!(
            rig,
            moderator,
            promote_speaker_in_community_voice_chat,
            promote(&cid, member)
        ),
        ok("User promoted to speaker successfully")
    );
    assert_eq!(
        said!(
            rig,
            moderator,
            promote_speaker_in_community_voice_chat,
            promote(&cid, banned)
        ),
        arm(
            "ForbiddenError",
            &format!("User {banned} is not a member of community {cid}")
        )
    );
    assert_eq!(
        said!(
            rig,
            moderator,
            demote_speaker_in_community_voice_chat,
            demote(&cid, banned)
        ),
        arm(
            "ForbiddenError",
            &format!("User {banned} is not a member of community {cid}")
        )
    );
    assert_eq!(
        said!(
            rig,
            moderator,
            reject_speak_request_in_community_voice_chat,
            reject(&cid, banned)
        ),
        ok("Speak request rejected successfully")
    );
    assert_eq!(
        said!(
            rig,
            moderator,
            demote_speaker_in_community_voice_chat,
            demote(&cid, member)
        ),
        ok("User demoted to listener successfully")
    );
    // yourself: no moderator needed, a public guest included
    for me in [member, guest] {
        assert_eq!(
            said!(
                rig,
                me,
                demote_speaker_in_community_voice_chat,
                demote(&cid, me)
            ),
            ok("You have been demoted to listener successfully"),
            "{me}"
        );
    }
    assert_eq!(
        said!(
            rig,
            owner,
            kick_player_from_community_voice_chat,
            kick(&cid, member)
        ),
        ok("User kicked from voice chat successfully")
    );

    // a moderator whose role row survived its ban
    sqlx::query("INSERT INTO social.community_bans (community_id, banned_address, banned_by) VALUES ($1, $2, $3)")
        .bind(rig.community)
        .bind(moderator)
        .bind(owner)
        .execute(&rig.pool)
        .await
        .unwrap();
    assert_eq!(
        said!(
            rig,
            moderator,
            start_community_voice_chat,
            StartCommunityVoiceChatPayload {
                community_id: cid.clone()
            }
        ),
        arm("ForbiddenError", "Banned users cannot start voice chats")
    );
    for (answer, action) in [
        (
            said!(
                rig,
                moderator,
                promote_speaker_in_community_voice_chat,
                promote(&cid, member)
            ),
            "promote speakers",
        ),
        (
            said!(
                rig,
                moderator,
                demote_speaker_in_community_voice_chat,
                demote(&cid, member)
            ),
            "demote other speakers",
        ),
        (
            said!(
                rig,
                moderator,
                kick_player_from_community_voice_chat,
                kick(&cid, member)
            ),
            "kick players",
        ),
        (
            said!(
                rig,
                moderator,
                reject_speak_request_in_community_voice_chat,
                reject(&cid, member)
            ),
            "reject speak requests",
        ),
    ] {
        assert_eq!(
            answer,
            arm("ForbiddenError", &format!("Banned users cannot {action}"))
        );
    }
    assert_eq!(rig.start(owner).await, "Ok");
    assert_eq!(
        said!(
            rig,
            moderator,
            end_community_voice_chat,
            EndCommunityVoiceChatPayload {
                community_id: cid.clone()
            }
        ),
        arm("ForbiddenError", "Banned users cannot end voice chats")
    );
    rig.drop().await;

    // a private community: a non-member cannot demote even itself
    let Some(rig) = Rig::new(true).await else {
        return;
    };
    assert_eq!(
        said!(
            rig,
            &rig.guest,
            demote_speaker_in_community_voice_chat,
            demote(&rig.cid, &rig.guest)
        ),
        arm(
            "ForbiddenError",
            &format!(
                "User {} is not a member of community {}",
                rig.guest, rig.cid
            )
        )
    );
    rig.drop().await;
}

/// A runtime failure is the procedure's internal error with upstream's
/// message, not a logged ok (the community voice handlers'
/// `internalServerError`); an end the runtime refuses leaves the room live
/// and tells no one. The success answers carry upstream's messages.
#[tokio::test]
async fn runtime_failures_are_internal_errors() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let (owner, member) = (&rig.owner, &rig.member);
    let cid = rig.cid.clone();
    assert_eq!(rig.start(owner).await, "Ok");
    let hand = |raising| RequestToSpeakInCommunityVoiceChatPayload {
        community_id: cid.clone(),
        is_raising_hand: raising,
    };
    assert_eq!(
        said!(
            rig,
            member,
            request_to_speak_in_community_voice_chat,
            hand(true)
        ),
        ok("Request to speak sent successfully")
    );
    assert_eq!(
        said!(
            rig,
            member,
            request_to_speak_in_community_voice_chat,
            hand(false)
        ),
        ok("Hand lowered successfully")
    );
    let mut to_member = rig.ctx.pubsub().subscribe(member);
    rig.voice
        .fail_actions
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = |what: &str| arm("InternalServerError", &format!("Failed to {what}"));
    assert_eq!(
        said!(
            rig,
            owner,
            promote_speaker_in_community_voice_chat,
            promote(&cid, member)
        ),
        failed("promote speaker in community voice chat")
    );
    assert_eq!(
        said!(
            rig,
            owner,
            demote_speaker_in_community_voice_chat,
            demote(&cid, member)
        ),
        failed("demote speaker in community voice chat")
    );
    assert_eq!(
        said!(
            rig,
            owner,
            kick_player_from_community_voice_chat,
            kick(&cid, member)
        ),
        failed("kick player from community voice chat")
    );
    assert_eq!(
        said!(
            rig,
            owner,
            reject_speak_request_in_community_voice_chat,
            reject(&cid, member)
        ),
        failed("reject speak request in community voice chat")
    );
    assert_eq!(
        said!(
            rig,
            member,
            request_to_speak_in_community_voice_chat,
            hand(true)
        ),
        failed("update hand status in community voice chat")
    );
    assert_eq!(
        said!(
            rig,
            owner,
            mute_speaker_from_community_voice_chat,
            MuteSpeakerFromCommunityVoiceChatPayload {
                community_id: cid.clone(),
                user_address: member.clone(),
                muted: true,
            }
        ),
        failed(
            "mute/unmute speaker from community voice chat: voice runtime failed: test runtime failed"
        )
    );
    let end = || EndCommunityVoiceChatPayload {
        community_id: cid.clone(),
    };
    assert_eq!(
        said!(rig, owner, end_community_voice_chat, end()),
        failed("end community voice chat")
    );
    assert!(rig.voice.live.lock().contains(&rig.community), "still live");
    assert!(voice_updates(&mut to_member).is_empty(), "no ENDED");
    rig.voice
        .fail_actions
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        said!(rig, owner, end_community_voice_chat, end()),
        ok("Community voice chat ended successfully")
    );
    assert_eq!(voice_updates(&mut to_member).len(), 1);
    rig.drop().await;
}

/// Upstream `joinCommunityVoiceChat` (`community-voice.ts:279-299`): the ban
/// is read again once the seat exists; a ban that landed meanwhile evicts it
/// and refuses the join.
#[tokio::test]
async fn a_ban_landing_during_a_join_evicts_the_seat() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    assert_eq!(rig.start(&rig.owner).await, "Ok");
    *rig.voice.ban_during_grant.lock() = Some(rig.member.clone());
    assert_eq!(
        said!(
            rig,
            &rig.member,
            join_community_voice_chat,
            JoinCommunityVoiceChatPayload {
                community_id: rig.cid.clone()
            }
        ),
        arm(
            "ForbiddenError",
            &format!(
                "The user {} is banned from community {}",
                rig.member, rig.cid
            )
        )
    );
    assert!(rig.runtime_saw(&format!("kick {} {}", rig.community, rig.member)));
    rig.drop().await;
}

/// Upstream `getUserProfileData` (`community-voice.ts:69-100`, `:124`,
/// `:272`): the starter's and a joiner's name, claimed-name flag and picture
/// reach the room metadata; a wallet without a profile sends none.
#[tokio::test]
async fn the_profile_reaches_the_room_metadata() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let ctx = profiled_ctx(Db::new(rig.pool.clone()), rig.voice.clone());
    let entity = deploy_profile(&rig.pool, &rig.owner, "Owner").await;
    let start = SocialServiceImpl
        .start_community_voice_chat(
            StartCommunityVoiceChatPayload {
                community_id: rig.cid.clone(),
            },
            procedure_ctx(&ctx, &rig.owner, 62),
        )
        .await
        .unwrap();
    assert!(matches!(
        start.response,
        Some(start_community_voice_chat_response::Response::Ok(_))
    ));
    let join = SocialServiceImpl
        .join_community_voice_chat(
            JoinCommunityVoiceChatPayload {
                community_id: rig.cid.clone(),
            },
            procedure_ctx(&ctx, &rig.guest, 63),
        )
        .await
        .unwrap();
    assert!(matches!(
        join.response,
        Some(join_community_voice_chat_response::Response::Ok(_))
    ));
    let profiles = rig.voice.profiles.lock().clone();
    assert_eq!(
        profiles.get(&rig.owner).cloned().flatten(),
        Some(serde_json::json!({
            "name": "Owner",
            "has_claimed_name": true,
            "profile_picture_url": "bafkreiface",
        }))
    );
    assert_eq!(profiles.get(&rig.guest), Some(&None));
    drop_profile(&rig.pool, &rig.owner, &entity).await;
    rig.drop().await;
}

/// Upstream's community-voice polling (`community-voice-polling.ts:104-130`):
/// a room that ends without the end procedure (expired, finished) reaches
/// the audience its start recorded as ENDED on the next poll, once. A canonical
/// media closure cannot identify the source's independently cased cache keys.
#[tokio::test]
async fn a_room_closed_out_of_band_reaches_its_audience_as_ended() {
    use crate::comms::interfaces::SocialDirectory;
    // The poll inspects every tracked key. Its fake media view must not close
    // generations created concurrently by other fixtures in the scratch DB.
    let Some(database) =
        super::fanout::OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let rig = Rig::with_pool(false, database.pool.clone()).await;
    let directory = crate::comms_directory::Directory::new(rig.pool.clone());
    let mut to_member = rig.ctx.pubsub().subscribe(&rig.member);
    let mut to_guest = rig.ctx.pubsub().subscribe(&rig.guest);
    assert_eq!(rig.start(&rig.owner).await, "Ok");
    voice_updates(&mut to_member);
    voice_updates(&mut to_guest);
    let id = crate::CommunityId::from_uuid(rig.community);
    directory.community_voice_room_closed(id).await;
    assert!(voice_updates(&mut to_member).is_empty());
    assert!(voice_updates(&mut to_guest).is_empty());
    assert!(
        rig.ctx
            .db()
            .voice_notification(&rig.community.to_string())
            .await
            .unwrap()
            .is_some()
    );
    rig.voice.live.lock().clear();
    rig.ctx.poll_community_voice_notifications().await;
    for rx in [&mut to_member, &mut to_guest] {
        let ended = voice_updates(rx);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(
            ended[0].status,
            CommunityVoiceChatStatus::CommunityVoiceChatEnded as i32
        );
        assert!(ended[0].ended_at.is_some());
    }
    directory.community_voice_room_closed(id).await;
    rig.ctx.poll_community_voice_notifications().await;
    assert!(voice_updates(&mut to_member).is_empty(), "once");

    // ended by the procedure: the runtime's closed-room report adds nothing
    rig.voice.live.lock().clear();
    assert_eq!(rig.start(&rig.owner).await, "Ok");
    assert_eq!(rig.end(&rig.owner).await, "Ok");
    directory.community_voice_room_closed(id).await;
    let seen = voice_updates(&mut to_member);
    assert_eq!(seen.len(), 2, "STARTED and one ENDED: {seen:?}");
    rig.drop().await;
    database.drop().await;
}

/// Participation admits public guests, rejects bans even without a membership
/// row, and requires private membership, in a live room of an active
/// community (upstream validation.ts:35-55).
#[tokio::test]
async fn joining_public_and_private_communities_respects_membership_and_bans() {
    for private in [false, true] {
        let Some(rig) = Rig::new(private).await else {
            return;
        };
        assert_eq!(rig.start(&rig.owner).await, "Ok");
        assert_eq!(rig.join(&rig.member).await, "Ok");
        rig.voice.calls.lock().clear();
        assert_eq!(
            rig.join(&rig.guest).await,
            if private { "ForbiddenError" } else { "Ok" }
        );
        if private {
            assert!(rig.voice.calls.lock().is_empty());
        } else {
            assert!(rig.runtime_saw(&format!(
                "community-credentials {} {} none join",
                rig.community, rig.guest
            )));
        }
        for retained_role in [false, true] {
            if retained_role {
                sqlx::query("INSERT INTO social.community_members (community_id, member_address, role) VALUES ($1, $2, 'member')")
                    .bind(rig.community).bind(&rig.banned).execute(&rig.pool).await.unwrap();
            }
            rig.voice.calls.lock().clear();
            assert_eq!(rig.join(&rig.banned).await, "ForbiddenError");
            assert!(rig.voice.calls.lock().is_empty());
        }
        sqlx::query("UPDATE social.communities SET active = FALSE WHERE id = $1")
            .bind(rig.community)
            .execute(&rig.pool)
            .await
            .unwrap();
        assert_eq!(rig.join(&rig.owner).await, "NotFoundError");
        assert!(rig.voice.calls.lock().is_empty());
        rig.drop().await;
    }
}

/// Self-demotion gives up a grant: public guests, banned participants and
/// ordinary members may do it, while private outsiders may not (upstream
/// moderation.ts:83-127). It does not acquire a capability through participation.
#[tokio::test]
async fn speakers_can_demote_themselves_without_moderator_authority() {
    for private in [false, true] {
        let Some(rig) = Rig::new(private).await else {
            return;
        };
        for me in [&rig.member, &rig.guest, &rig.banned] {
            rig.voice.calls.lock().clear();
            let allowed = !private || me == &rig.member;
            assert_eq!(
                rig.demote(me, &checksummed(me)).await,
                if allowed { "Ok" } else { "ForbiddenError" }
            );
            assert_eq!(
                rig.runtime_saw(&format!("set-speaker {} {me} false", rig.community)),
                allowed
            );
            assert_eq!(rig.promote(me, me).await, "ForbiddenError");
        }
        rig.drop().await;
    }
}

/// Bans win over a retained role for hosts and moderators. A banned target can
/// still be denied a hand request, muted or removed, but receives no speaker
/// grants (upstream validation.ts:92-95, 141-144, 205-211).
#[tokio::test]
async fn bans_override_retained_roles_without_preventing_revocation() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    sqlx::query("INSERT INTO social.community_members (community_id, member_address, role) VALUES ($1, $2, 'member')")
        .bind(rig.community).bind(&rig.banned).execute(&rig.pool).await.unwrap();
    assert_eq!(
        rig.promote(&rig.moderator, &rig.banned).await,
        "ForbiddenError"
    );
    assert_eq!(
        rig.demote(&rig.moderator, &rig.banned).await,
        "ForbiddenError"
    );
    assert!(rig.voice.calls.lock().is_empty());
    assert_eq!(rig.reject(&rig.moderator, &rig.banned).await, "Ok");
    assert_eq!(rig.mute(&rig.moderator, &rig.banned, true).await, "Ok");
    assert_eq!(rig.kick(&rig.moderator, &rig.banned).await, "Ok");

    sqlx::query("INSERT INTO social.community_bans (community_id, banned_address, banned_by) VALUES ($1, $2, $3)")
        .bind(rig.community).bind(&rig.moderator).bind(&rig.owner).execute(&rig.pool).await.unwrap();
    rig.voice.calls.lock().clear();
    assert_eq!(rig.start(&rig.moderator).await, "ForbiddenError");
    assert_eq!(rig.end(&rig.moderator).await, "ForbiddenError");
    assert_eq!(
        rig.promote(&rig.moderator, &rig.member).await,
        "ForbiddenError"
    );
    assert_eq!(
        rig.demote(&rig.moderator, &rig.member).await,
        "ForbiddenError"
    );
    assert_eq!(
        rig.reject(&rig.moderator, &rig.member).await,
        "ForbiddenError"
    );
    assert_eq!(
        rig.mute(&rig.moderator, &rig.member, true).await,
        "ForbiddenError"
    );
    assert_eq!(
        rig.kick(&rig.moderator, &rig.member).await,
        "ForbiddenError"
    );
    assert!(rig.voice.calls.lock().is_empty());
    rig.drop().await;
}
