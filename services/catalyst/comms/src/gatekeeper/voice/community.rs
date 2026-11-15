//! Community voice admission, status and speaker moderation.
use super::*;

#[derive(Debug, Deserialize)]
pub struct CommunityVoiceChatBody {
    pub community_id: String,
    /// A claim: nothing here verifies the caller controls this wallet.
    pub user_address: String,
    /// A claim: nothing here verifies this role was ever stored.
    pub user_role: Option<String>,
    pub action: Option<String>,
    pub profile_data: Option<Value>,
}

/// The role name assumed when the body omits `user_role`; still a claim.
const CLAIMED_ROLE_WHEN_OMITTED: &str = "none";

/// Whether the role the platform service **claimed** is one this handler
/// grants publish rights and the persisted moderator flag for. Exactly
/// `owner | moderator` (lowercased): not `mod`, not `admin`.
pub(super) fn claimed_role_grants_speaker_rights(claimed_role_lowercased: &str) -> bool {
    matches!(claimed_role_lowercased, "owner" | "moderator")
}

pub(super) fn community_join_metadata(
    claimed_role: &str,
    is_speaker: bool,
    profile_data: Option<&Value>,
) -> serde_json::Map<String, Value> {
    let mut metadata = serde_json::Map::new();
    metadata.insert("role".into(), serde_json::json!(claimed_role));
    metadata.insert("isSpeaker".into(), serde_json::json!(is_speaker));
    metadata.insert("muted".into(), serde_json::json!(false));
    if let Some(profile) = profile_data.and_then(|v| v.as_object()) {
        if let Some(name) = profile.get("name") {
            metadata.insert("name".into(), name.clone());
        }
        if let Some(claimed) = profile.get("has_claimed_name") {
            metadata.insert("hasClaimedName".into(), claimed.clone());
        }
        if let Some(pic) = profile.get("profile_picture_url") {
            metadata.insert("profilePictureUrl".into(), pic.clone());
        }
    }
    metadata
}

fn parse_community_id(raw: &str) -> Result<CommunityId, ApiError> {
    if raw.trim().is_empty() {
        return Err(ApiError::bad_request(
            "The parameter communityId is required",
        ));
    }
    CommunityId::parse(raw)
        .map_err(|_| ApiError::bad_request("The parameter communityId is invalid"))
}

/// POST /community-voice-chat (service token). See the module note: the
/// claimed role decides speaker rights; the platform ban is re-checked.
pub async fn community_voice_chat_create_or_join(
    State(state): State<SharedState>,
    Json(body): Json<CommunityVoiceChatBody>,
) -> Result<Json<Value>, ApiError> {
    let connection_url = community_voice_credentials_for(&state, body).await?;
    Ok(Json(
        serde_json::json!({ "connection_url": connection_url }),
    ))
}

/// The grant of a community room for `body.user_address` with the role the
/// caller states (the route's body, or the social RPC's stored role through
/// `rooms`): the platform ban re-checked, the room row joined.
pub async fn community_voice_credentials_for(
    state: &CommsState,
    body: CommunityVoiceChatBody,
) -> Result<String, ApiError> {
    require_livekit(state)?;
    if body.community_id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "The property community_id is required",
        ));
    }
    let community_id = CommunityId::parse(&body.community_id)
        .map_err(|_| ApiError::bad_request("The property community_id is invalid"))?;
    let claimed_wallet = Address::parse(&body.user_address)
        .map_err(|_| ApiError::bad_request("The property user_address is invalid"))?;
    let claimed_role = body
        .user_role
        .as_deref()
        .unwrap_or(CLAIMED_ROLE_WHEN_OMITTED)
        .to_lowercase();
    refuse_deny_listed(state, std::slice::from_ref(&claimed_wallet)).await?;
    state
        .moderation
        .platform_ban_decision(std::slice::from_ref(&claimed_wallet))
        .await
        .into_result()?;
    let speaker_rights = claimed_role_grants_speaker_rights(&claimed_role);
    let action = body.action.as_deref().unwrap_or("join").to_lowercase();
    let is_speaker = action == "create" && speaker_rights;
    let room_name = crate::rooms::community_voice_chat_room_name(&community_id);
    let metadata = community_join_metadata(&claimed_role, is_speaker, body.profile_data.as_ref());
    let mut grants = join_grants(&room_name);
    grants.can_publish = is_speaker;
    grants.can_update_own_metadata = false;
    let connection_url = mint(
        state,
        claimed_wallet.as_str(),
        grants,
        Some(Value::Object(metadata).to_string()),
    )?;
    match state
        .voice_db
        .join_user_to_community_room(community_id, &claimed_wallet, speaker_rights)
        .await
    {
        Ok(()) => {}
        Err(CommunityJoinError::CommunityNotFound) => {
            return Err(ApiError::not_found("Community not found"));
        }
        Err(CommunityJoinError::Db(e)) => return Err(e.into()),
    }
    Ok(connection_url)
}

#[derive(Debug, serde::Serialize)]
pub struct CommunityVoiceChatStatusResponse {
    pub active: bool,
    pub participant_count: i64,
    pub moderator_count: i64,
}

pub(super) async fn community_status(
    state: &CommsState,
    community_id: CommunityId,
) -> Result<(bool, i64, i64), ApiError> {
    let users = state.voice_db.community_users_in_room(community_id).await?;
    let now = now_ms();
    let active_participants = users
        .iter()
        .filter(|u| state.voice_db.is_active_community_user(u, now))
        .count() as i64;
    let active_moderators = users
        .iter()
        .filter(|u| u.is_moderator && state.voice_db.is_active_community_user(u, now))
        .count() as i64;
    Ok((
        active_moderators > 0,
        active_participants,
        active_moderators,
    ))
}

/// GET /community-voice-chat/{id}/status (service token).
pub async fn community_voice_chat_status(
    State(state): State<SharedState>,
    Path(community_id): Path<String>,
) -> Result<Json<CommunityVoiceChatStatusResponse>, ApiError> {
    let community_id = parse_community_id(&community_id)?;
    let (active, participant_count, moderator_count) =
        community_status(&state, community_id).await?;
    let (participant_count, moderator_count) = if active {
        (participant_count, moderator_count)
    } else {
        (0, 0)
    };
    Ok(Json(CommunityVoiceChatStatusResponse {
        active,
        participant_count,
        moderator_count,
    }))
}

#[derive(Debug, Deserialize)]
pub struct BulkCommunityStatusBody {
    pub community_ids: Vec<String>,
}

const MAX_BULK_COMMUNITY_IDS: usize = 100;

pub(super) fn validate_bulk_community_ids(ids: &[String]) -> Result<(), ApiError> {
    if ids.len() > MAX_BULK_COMMUNITY_IDS {
        return Err(ApiError::bad_request(format!(
            "community_ids must contain at most {MAX_BULK_COMMUNITY_IDS} items"
        )));
    }
    Ok(())
}

/// POST /community-voice-chat/status (service token): one statement for the
/// whole set; an id that is not a uuid answers inactive.
pub async fn community_voice_chat_bulk_status(
    State(state): State<SharedState>,
    Json(body): Json<BulkCommunityStatusBody>,
) -> Result<Json<Value>, ApiError> {
    validate_bulk_community_ids(&body.community_ids)?;
    let parsed: Vec<Option<CommunityId>> = body
        .community_ids
        .iter()
        .map(|id| CommunityId::parse(id).ok())
        .collect();
    let ids: Vec<CommunityId> = parsed.iter().flatten().copied().collect();
    let by_id = state.voice_db.bulk_community_status(&ids).await?;
    let data: Vec<Value> = body
        .community_ids
        .iter()
        .zip(&parsed)
        .map(|(raw, id)| {
            let (participant_count, moderator_count) =
                id.and_then(|id| by_id.get(&id).copied()).unwrap_or((0, 0));
            serde_json::json!({
                "community_id": raw,
                "active": moderator_count > 0,
                "participant_count": participant_count,
                "moderator_count": moderator_count,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "data": data })))
}

/// GET /community-voice-chat/active (service token).
pub async fn community_voice_chat_active(
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let active = state.voice_db.all_active_community_voice_chats().await?;
    let data: Vec<Value> = active
        .into_iter()
        .map(|c| serde_json::json!({ "communityId": c.community_id.to_string(), "participantCount": c.participant_count, "moderatorCount": c.moderator_count }))
        .collect();
    let total = data.len();
    Ok(Json(serde_json::json!({ "data": data, "total": total })))
}

#[derive(Debug, Deserialize)]
pub struct EndCommunityVoiceChatBody {
    pub user_address: Option<String>,
}

/// DELETE /community-voice-chat/{id} (service token).
pub async fn community_voice_chat_end(
    State(state): State<SharedState>,
    Path(community_id): Path<String>,
    Json(body): Json<EndCommunityVoiceChatBody>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let community_id = parse_community_id(&community_id)?;
    if body
        .user_address
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(ApiError::bad_request(
            "The property user_address is required",
        ));
    }
    destroy_community_voice_chat(&state, community_id, "livekit community voice room").await?;
    Ok(Json(
        serde_json::json!({ "message": "Community voice chat ended successfully" }),
    ))
}

/// The shape check of the moderation handlers' path parameters: non-empty,
/// the address lowercased. Not an authorization (see the module note).
pub(super) fn community_and_claimed_target(
    community_id: &str,
    claimed_user_address: &str,
) -> Result<(CommunityId, String), ApiError> {
    let community_id = parse_community_id(community_id)?;
    let addr = claimed_user_address.to_lowercase();
    if addr.trim().is_empty() {
        return Err(ApiError::bad_request(
            "The parameter userAddress is required",
        ));
    }
    Ok((community_id, addr))
}

pub(in crate::gatekeeper) async fn merge_metadata(
    state: &CommsState,
    room_name: &str,
    address: &str,
    patch: serde_json::Map<String, Value>,
) -> Result<(), ApiError> {
    state
        .room_service()
        .merge_participant_metadata(room_name, address, patch)
        .await
        .map_err(|e| ApiError::internal(format!("livekit update participant metadata: {e}")))
}

pub(in crate::gatekeeper) fn patch(entries: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

pub async fn community_request_to_speak(
    State(state): State<SharedState>,
    Path((community_id, user_address)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let (community_id, target) = community_and_claimed_target(&community_id, &user_address)?;
    merge_metadata(
        &state,
        &crate::rooms::community_voice_chat_room_name(&community_id),
        &target,
        patch(&[("isRequestingToSpeak", serde_json::json!(true))]),
    )
    .await?;
    Ok(Json(
        serde_json::json!({ "message": "Request to speak sent successfully" }),
    ))
}

pub async fn community_reject_speak_request(
    State(state): State<SharedState>,
    Path((community_id, user_address)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let (community_id, target) = community_and_claimed_target(&community_id, &user_address)?;
    merge_metadata(
        &state,
        &crate::rooms::community_voice_chat_room_name(&community_id),
        &target,
        patch(&[("isRequestingToSpeak", serde_json::json!(false))]),
    )
    .await?;
    Ok(Json(
        serde_json::json!({ "message": "Speak request rejected successfully" }),
    ))
}

pub(in crate::gatekeeper) async fn set_speaker(
    state: &CommsState,
    community_id: CommunityId,
    target: &str,
    speaker: bool,
) -> Result<(), ApiError> {
    let room = crate::rooms::community_voice_chat_room_name(&community_id);
    let service = state.room_service();
    let result = service
        .merge_participant_metadata_with_permission(
            &room,
            target,
            patch(&[
                ("isRequestingToSpeak", serde_json::json!(false)),
                ("isSpeaker", serde_json::json!(speaker)),
            ]),
            serde_json::json!({ "canPublish": speaker, "canSubscribe": true, "canPublishData": true }),
        )
        .await;
    if let Err(error) = result {
        if !speaker {
            // A failed metadata read must not prevent revoking publishing. Omit
            // metadata so an unread value is never replaced. Permission is a full
            // replacement, so retain the authorized listener permissions. Only
            // demotion may fall back; a failed promotion never grants publishing.
            service
                .update_participant(
                    &room,
                    target,
                    None,
                    Some(serde_json::json!({
                        "canPublish": false, "canSubscribe": true, "canPublishData": true
                    })),
                )
                .await
                .map_err(|fallback| {
                    ApiError::internal(format!(
                        "livekit demotion incomplete: participant update: {error}; permission revocation: {fallback}"
                    ))
                })?;
            return Err(ApiError::internal(format!(
                "livekit demotion incomplete: permission revocation completed but participant update failed: {error}"
            )));
        }
        return Err(ApiError::internal(format!(
            "livekit update participant permissions: {error}"
        )));
    }
    Ok(())
}

pub async fn community_promote_speaker(
    State(state): State<SharedState>,
    Path((community_id, user_address)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let (community_id, target) = community_and_claimed_target(&community_id, &user_address)?;
    set_speaker(&state, community_id, &target, true).await?;
    Ok(Json(
        serde_json::json!({ "message": "User promoted to speaker successfully" }),
    ))
}

pub async fn community_demote_speaker(
    State(state): State<SharedState>,
    Path((community_id, user_address)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let (community_id, target) = community_and_claimed_target(&community_id, &user_address)?;
    set_speaker(&state, community_id, &target, false).await?;
    Ok(Json(
        serde_json::json!({ "message": "User demoted to listener successfully" }),
    ))
}

pub async fn community_kick_player(
    State(state): State<SharedState>,
    Path((community_id, user_address)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let (community_id, target) = community_and_claimed_target(&community_id, &user_address)?;
    kick_from_community_room(&state, community_id, &target).await?;
    Ok(Json(
        serde_json::json!({ "message": "User kicked from voice chat successfully" }),
    ))
}

/// Removes `target` from the community room: the LiveKit participant
/// (best-effort, logged) and the presence row (the failure that counts).
pub async fn kick_from_community_room(
    state: &CommsState,
    community_id: CommunityId,
    target: &str,
) -> Result<(), ApiError> {
    let room_name = crate::rooms::community_voice_chat_room_name(&community_id);
    let room_service = state.room_service();
    let (removed, deleted) = tokio::join!(
        room_service.remove_participant(&room_name, target),
        state.voice_db.remove_community_user(community_id, target)
    );
    if let Err(e) = removed {
        tracing::warn!(error = %e, room = %room_name, addr = %target, "failed to remove community voice participant");
    }
    deleted?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct MuteSpeakerBody {
    pub muted: bool,
}

pub async fn community_mute_speaker(
    State(state): State<SharedState>,
    Path((community_id, user_address)): Path<(String, String)>,
    Json(body): Json<MuteSpeakerBody>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let (community_id, target) = community_and_claimed_target(&community_id, &user_address)?;
    merge_metadata(
        &state,
        &crate::rooms::community_voice_chat_room_name(&community_id),
        &target,
        patch(&[("muted", serde_json::json!(body.muted))]),
    )
    .await?;
    let action = if body.muted { "muted" } else { "unmuted" };
    Ok(Json(
        serde_json::json!({ "message": format!("User {action} successfully") }),
    ))
}

/// GET /users/{address}/community-voice-chat-status (service token).
pub async fn check_user_community_status(
    State(state): State<SharedState>,
    Path(user_address): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let address =
        Address::parse(&user_address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    let is_in = state
        .voice_db
        .is_user_in_any_community_voice_chat(&address)
        .await?;
    Ok(Json(
        serde_json::json!({ "userAddress": address.as_str(), "isInCommunityVoiceChat": is_in }),
    ))
}
