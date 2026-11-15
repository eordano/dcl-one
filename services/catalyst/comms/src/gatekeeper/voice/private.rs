//! Private messaging and private voice-call workflows.
use super::*;

/// GET /private-messages/token: an explorer-signed fetch; the platform ban
/// probe (device-widened) refuses before minting; the token is subscribe-only
/// on the private-messages room and carries the wallet's DM privacy. The
/// connection record is a social write, skipped while the social switch is
/// off or held; the mint is admission and still answers.
pub async fn private_messages_token(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    switch: Option<Extension<crate::writes::Switch>>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let sf = verify_signed_fetch(
        &headers,
        "get",
        &SignedPath::at(&uri, "/private-messages/token"),
        &[EXPLORER_SIGNER],
        state.eip1654(),
    )
    .await?;
    let identity = sf.signer;
    if state.deny_list.is_denied(identity.as_str()).await {
        return Err(super::super::errors::unauthorized(
            "Access denied, wallet is deny-listed.",
        ));
    }
    let ip_address = get_request_ip(&headers);
    let device_id = device_identifier(&sf.metadata);
    let record = crate::writes::side_refusal(switch).is_none();
    let upsert = async {
        if !record {
            return;
        }
        if let Err(e) = state
            .moderation
            .upsert_connection(UpsertPlayerConnection {
                address: identity.clone(),
                ip_address,
                device_id: device_id.clone(),
            })
            .await
        {
            tracing::warn!(error = %e, address = %identity, "failed to store player connection info");
        }
    };
    let (banned, privacy) = tokio::join!(
        async {
            // Record only an admitted connection: a refused one must not replace the recorded device.
            let banned = state
                .moderation
                .is_banned_for_connection(&identity, device_id.as_deref())
                .await;
            if matches!(banned, Ok(false)) {
                upsert.await;
            }
            banned
        },
        state.social.private_messages_privacy(&identity)
    );
    let map_error = |e: sqlx::Error| {
        ApiError::from(AuthorityNotEstablished::store_unavailable(
            "social.user_bans",
            &e,
        ))
    };
    let banned = banned.map_err(map_error)?;
    let privacy = privacy.map_err(map_error)?;
    if banned {
        return Err(forbidden(
            super::super::moderation::store::PLATFORM_BANNED_MSG,
        ));
    }
    let metadata = serde_json::json!({ "private_messages_privacy": privacy }).to_string();
    let mut grants = join_grants(&state.settings.private_messages_room_id);
    grants.can_publish = false;
    grants.can_update_own_metadata = false;
    let adapter = mint(&state, identity.as_str(), grants, Some(metadata))?;
    Ok(Json(serde_json::json!({ "adapter": adapter })))
}

#[derive(Debug, Deserialize)]
pub struct PrivateMessagesPrivacyBody {
    pub private_messages_privacy: Option<String>,
}

/// PATCH /users/{address}/private-messages-privacy (service token): writes
/// the ONE `social.user_settings` row the friends module reads too.
pub async fn patch_private_messages_privacy(
    State(state): State<SharedState>,
    Path(address): Path<String>,
    Json(body): Json<PrivateMessagesPrivacyBody>,
) -> Result<Json<Value>, ApiError> {
    let address = Address::parse(&address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    let privacy = body
        .private_messages_privacy
        .as_deref()
        .map(|s| s.to_lowercase())
        .filter(|s| s == "all" || s == "only_friends")
        .ok_or_else(|| ApiError::bad_request("Invalid private_messages_privacy"))?;
    state
        .social
        .set_private_messages_privacy(&address, &privacy)
        .await?;
    Ok(Json(
        serde_json::json!({ "address": address.as_str(), "private_messages_privacy": privacy }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct PrivateVoiceChatBody {
    pub room_id: String,
    pub user_addresses: Vec<String>,
}

/// The credentials of a private call: one token per participant, both sides
/// checked against the platform ban first, then the call row opened with
/// both `not_connected`. `room_id` is the call id (`social.private_voice_chats`
/// keys it as a uuid, so the friends module's ids are the wire's).
pub async fn get_private_voice_chat_room_credentials(
    state: &CommsState,
    room_id: &str,
    user_addresses: &[Address],
) -> Result<BTreeMap<String, Value>, ApiError> {
    private_voice_chat_room_credentials(state, room_id, user_addresses, false).await
}

pub(in crate::gatekeeper) async fn pending_private_voice_chat_room_credentials(
    state: &CommsState,
    room_id: &str,
    user_addresses: &[Address],
) -> Result<BTreeMap<String, Value>, ApiError> {
    private_voice_chat_room_credentials(state, room_id, user_addresses, true).await
}

async fn private_voice_chat_room_credentials(
    state: &CommsState,
    room_id: &str,
    user_addresses: &[Address],
    pending_only: bool,
) -> Result<BTreeMap<String, Value>, ApiError> {
    let call_id = Uuid::parse_str(room_id.trim())
        .map_err(|_| ApiError::bad_request("Invalid request body, room_id must be a uuid"))?;
    let [caller, callee] = user_addresses else {
        return Err(ApiError::bad_request(
            "Invalid request body, user_addresses must name the two participants",
        ));
    };
    if caller == callee {
        return Err(ApiError::bad_request(
            "Invalid request body, user_addresses must name two different wallets",
        ));
    }
    refuse_deny_listed(state, user_addresses).await?;
    state
        .moderation
        .platform_ban_decision(user_addresses)
        .await
        .into_result()?;
    let room_name = private_voice_chat_room_name(&call_id);
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for addr in user_addresses {
        let mut grants = join_grants(&room_name);
        grants.can_update_own_metadata = false;
        let connection_url = mint(state, addr.as_str(), grants, None)?;
        out.insert(
            addr.as_str().to_string(),
            serde_json::json!({ "connection_url": connection_url }),
        );
    }
    if pending_only {
        if !state
            .voice_db
            .accept_pending_private_call(call_id, caller, callee)
            .await?
        {
            return Err(ApiError::not_found("Private voice chat intent not found"));
        }
    } else {
        match state
            .voice_db
            .create_private_call(call_id, caller, callee)
            .await
        {
            Ok(_) => {}
            Err(db::CreatePrivateCallError::PartiesDiffer) => {
                return Err(super::super::errors::conflict(
                    "Private voice chat room belongs to other participants",
                ));
            }
            Err(db::CreatePrivateCallError::Db(e)) => return Err(e.into()),
        }
    }
    Ok(out)
}

/// POST /private-voice-chat (service token).
pub async fn create_private_voice_chat(
    State(state): State<SharedState>,
    Json(body): Json<PrivateVoiceChatBody>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    if body.room_id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Invalid request body, missing room_id",
        ));
    }
    if body.user_addresses.is_empty() {
        return Err(ApiError::bad_request(
            "Invalid request body, missing user_addresses",
        ));
    }
    let addresses = body
        .user_addresses
        .iter()
        .map(|a| Address::parse(a))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::bad_request("Invalid request body, invalid user_addresses"))?;
    let out = get_private_voice_chat_room_credentials(&state, &body.room_id, &addresses).await?;
    Ok(Json(
        serde_json::to_value(out).unwrap_or(serde_json::json!({})),
    ))
}

#[derive(Debug, Deserialize)]
pub struct EndPrivateVoiceChatBody {
    pub address: Option<String>,
}

pub async fn end_private_voice_chat_by(
    state: &CommsState,
    room_id: &str,
    address: &Address,
) -> Result<EndedPrivateCall, ApiError> {
    let not_found = || ApiError::not_found(format!("Room {room_id} does not exist"));
    let call_id = Uuid::parse_str(room_id.trim()).map_err(|_| not_found())?;
    end_private_call(state, Some(call_id), address)
        .await?
        .ok_or_else(not_found)
}

/// Ends the accepted call `call_id` (or else the latest one `address` is in)
/// when `address` is one of its participants: the row goes first, then the
/// LiveKit room. A failed room delete is logged; the row is gone either way.
/// `None` when `address` is in no such call.
pub async fn end_private_call(
    state: &CommsState,
    call_id: Option<Uuid>,
    address: &Address,
) -> Result<Option<EndedPrivateCall>, ApiError> {
    let ended = match state
        .voice_db
        .delete_private_call_if_member(call_id, address)
        .await
    {
        Ok(ended) => ended,
        Err(DeleteRoomError::RoomDoesNotExist) => return Ok(None),
        Err(DeleteRoomError::Db(e)) => return Err(e.into()),
    };
    let room_name = private_voice_chat_room_name(&ended.call.id);
    if let Err(e) = state.room_service().delete_room(&room_name).await {
        tracing::warn!(error = %e, room = %room_name, "failed to delete livekit private voice room");
    }
    Ok(Some(ended))
}

/// DELETE /private-voice-chat/{id} (service token).
pub async fn end_private_voice_chat(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<EndPrivateVoiceChatBody>,
) -> Result<Json<Value>, ApiError> {
    require_livekit(&state)?;
    let address = body
        .address
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("Invalid request body, missing address"))?;
    let address = Address::parse(address)
        .map_err(|_| ApiError::bad_request("Invalid request body, invalid address"))?;
    let users_in_room = end_private_voice_chat_by(&state, &id, &address)
        .await?
        .users;
    Ok(Json(
        serde_json::json!({ "users_in_voice_chat": users_in_room }),
    ))
}

#[derive(Debug, serde::Serialize)]
pub struct VoiceChatStatusResponse {
    pub is_user_in_voice_chat: bool,
}

/// GET /users/{address}/voice-chat-status (service token).
pub async fn get_voice_chat_status(
    State(state): State<SharedState>,
    Path(address): Path<String>,
) -> Result<Json<VoiceChatStatusResponse>, ApiError> {
    let address = Address::parse(&address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    let is_user_in_voice_chat = state.voice_db.active_call_of(&address).await?.is_some();
    Ok(Json(VoiceChatStatusResponse {
        is_user_in_voice_chat,
    }))
}
