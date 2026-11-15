//! Voice webhook transitions, notifications and expiry cleanup.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    UnknownReason,
    ClientInitiated,
    DuplicateIdentity,
    ServerShutdown,
    ParticipantRemoved,
    RoomDeleted,
    StateMismatch,
    JoinFailure,
    Migration,
    SignalClose,
    RoomClosed,
    UserUnavailable,
    UserRejected,
    SipTrunkFailure,
    Other(i64),
}

impl DisconnectReason {
    fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::UnknownReason,
            1 => Self::ClientInitiated,
            2 => Self::DuplicateIdentity,
            3 => Self::ServerShutdown,
            4 => Self::ParticipantRemoved,
            5 => Self::RoomDeleted,
            6 => Self::StateMismatch,
            7 => Self::JoinFailure,
            8 => Self::Migration,
            9 => Self::SignalClose,
            10 => Self::RoomClosed,
            11 => Self::UserUnavailable,
            12 => Self::UserRejected,
            13 => Self::SipTrunkFailure,
            other => Self::Other(other),
        }
    }

    fn from_name(s: &str) -> Self {
        match s {
            "UNKNOWN_REASON" => Self::UnknownReason,
            "CLIENT_INITIATED" => Self::ClientInitiated,
            "DUPLICATE_IDENTITY" => Self::DuplicateIdentity,
            "SERVER_SHUTDOWN" => Self::ServerShutdown,
            "PARTICIPANT_REMOVED" => Self::ParticipantRemoved,
            "ROOM_DELETED" => Self::RoomDeleted,
            "STATE_MISMATCH" => Self::StateMismatch,
            "JOIN_FAILURE" => Self::JoinFailure,
            "MIGRATION" => Self::Migration,
            "SIGNAL_CLOSE" => Self::SignalClose,
            "ROOM_CLOSED" => Self::RoomClosed,
            "USER_UNAVAILABLE" => Self::UserUnavailable,
            "USER_REJECTED" => Self::UserRejected,
            "SIP_TRUNK_FAILURE" => Self::SipTrunkFailure,
            _ => Self::UnknownReason,
        }
    }

    pub fn parse(v: Option<&Value>) -> Self {
        match v {
            Some(Value::Number(n)) => Self::from_i64(n.as_i64().unwrap_or(0)),
            Some(Value::String(s)) => Self::from_name(s),
            _ => Self::UnknownReason,
        }
    }
}

async fn delete_livekit_rooms(state: &CommsState, room_names: &[String], what: &'static str) {
    use futures::StreamExt;
    if !state.livekit.configured || room_names.is_empty() {
        return;
    }
    let room_service = state.room_service();
    futures::stream::iter(room_names)
        .for_each_concurrent(KICK_CONCURRENCY, |room_name| {
            let room_service = &room_service;
            async move {
                if let Err(e) = room_service.delete_room(room_name).await {
                    tracing::warn!(error = %e, room = %room_name, "failed to delete {what}");
                }
            }
        })
        .await;
}

// Bound webhook media operations independently of the client's other call
// sites. DeleteRoom treats an already absent room as success.
pub(in crate::gatekeeper) async fn delete_webhook_room(
    state: &CommsState,
    room: &str,
) -> Result<(), ApiError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.room_service().delete_room(room),
    )
    .await
    .map_err(|_| ApiError::internal("webhook room deletion timed out"))?
    .map_err(|_| ApiError::internal("webhook room deletion failed"))
}

// Record the ended stream before deleting its source rows, using the provider
// event ID as a stable ranking key. Failed DB work retains the participant
// count and moderator state for a complete retry.
async fn finish_webhook_community(
    state: &CommsState,
    community_id: CommunityId,
    event_id: &str,
    delete_media: bool,
) -> Result<(), ApiError> {
    let users = state.voice_db.community_users_in_room(community_id).await?;
    if delete_media {
        delete_webhook_room(
            state,
            &crate::rooms::community_voice_chat_room_name(&community_id),
        )
        .await?;
    }
    if !users.is_empty() {
        let now = now_utc();
        let key = format!("community-streaming-ended-{event_id}");
        let event = community_streaming_ended_event(
            &community_id.to_string(),
            users.len() as i64,
            &key,
            now.and_utc().timestamp_millis(),
        );
        state
            .social
            .record_community_voice_ended(community_id, &key, &event, now)
            .await?;
    }
    state
        .voice_db
        .delete_community_voice_chat(community_id)
        .await?;
    state.social.community_voice_room_closed(community_id).await;
    Ok(())
}

pub async fn handle_private_participant_joined(
    state: &CommsState,
    user_address: &str,
    call_id: Uuid,
) -> Result<(), ApiError> {
    let prepared = state
        .voice_db
        .prepare_private_join(user_address, call_id)
        .await?;
    match &prepared.outcome {
        PrivateJoin::Inactive => {
            delete_webhook_room(state, &private_voice_chat_room_name(&call_id)).await?;
        }
        PrivateJoin::NotInRoom => {
            return Err(
                sqlx::Error::Protocol(format!("User {user_address} is not in a room")).into(),
            );
        }
        PrivateJoin::Joined { old_call } if *old_call != call_id => {
            delete_webhook_room(state, &private_voice_chat_room_name(old_call)).await?;
        }
        PrivateJoin::Joined { .. } => {}
    }
    prepared.commit().await?;
    Ok(())
}

pub async fn handle_private_participant_left(
    state: &CommsState,
    user_address: &str,
    call_id: Uuid,
    reason: DisconnectReason,
) -> Result<(), ApiError> {
    let room_name = private_voice_chat_room_name(&call_id);
    match reason {
        DisconnectReason::DuplicateIdentity => Ok(()),
        DisconnectReason::ClientInitiated => {
            tracing::debug!(user = user_address, room = %room_name, "user left a private room willingly, destroying it");
            delete_webhook_room(state, &room_name).await?;
            Ok(state
                .voice_db
                .mark_participant_disconnected(call_id, user_address)
                .await?)
        }
        DisconnectReason::RoomDeleted => Ok(state.voice_db.delete_private_call(call_id).await?),
        _ => Ok(state
            .voice_db
            .mark_participant_interrupted(call_id, user_address)
            .await?),
    }
}

pub async fn expire_private_voice_chats(state: &CommsState) -> Result<(), ApiError> {
    let expired = state.voice_db.delete_expired_private_calls().await?;
    let rooms: Vec<String> = expired.iter().map(private_voice_chat_room_name).collect();
    delete_livekit_rooms(state, &rooms, "expired private voice room").await;
    if !rooms.is_empty() {
        tracing::info!(count = rooms.len(), "deleted expired private voice chats");
    }
    Ok(())
}

const EVENT_TYPE_STREAMING: &str = "streaming";
const EVENT_SUBTYPE_COMMUNITY_STREAMING_ENDED: &str = "community-streaming-ended";

/// The @dcl/schemas `CommunityStreamingEnded` event document (the old SNS
/// payload) social's ranking reads.
pub(super) fn community_streaming_ended_event(
    community_id: &str,
    participant_count: i64,
    key: &str,
    timestamp_ms: i64,
) -> Value {
    serde_json::json!({
        "type": EVENT_TYPE_STREAMING,
        "subType": EVENT_SUBTYPE_COMMUNITY_STREAMING_ENDED,
        "key": key,
        "timestamp": timestamp_ms,
        "metadata": { "communityId": community_id, "totalParticipants": participant_count },
    })
}

/// Hands the ended stream to social's community ranking, idempotent on the
/// event key; upstream's notification processor ignored this event, so no
/// member notice is written. Rooms already deleted (count 0) are skipped.
async fn notify_community_streaming_ended(state: &CommsState, rooms: &[(CommunityId, i64)]) {
    let now = now_utc();
    let now_ms = now.and_utc().timestamp_millis();
    let mut count = 0usize;
    for (community_id, participant_count) in rooms {
        if *participant_count == 0 {
            tracing::debug!(%community_id, "skipping CommunityStreamingEnded since voice chat was already deleted");
            continue;
        }
        let key = format!("community-streaming-ended-{community_id}-{now_ms}");
        let event = community_streaming_ended_event(
            &community_id.to_string(),
            *participant_count,
            &key,
            now_ms,
        );
        let res = state
            .social
            .record_community_voice_ended(*community_id, &key, &event, now)
            .await;
        match res {
            Ok(_) => count += 1,
            Err(e) => {
                tracing::error!(error = %e, %community_id, "failed to record CommunityStreamingEnded")
            }
        }
    }
    if count > 0 {
        tracing::info!(rooms = count, "recorded CommunityStreamingEnded");
    }
}

/// Drops the LiveKit room and the presence rows together; the row count is
/// the participant count the ended notification reports.
pub(in crate::gatekeeper) async fn destroy_community_voice_chat(
    state: &CommsState,
    community_id: CommunityId,
    what: &'static str,
) -> Result<(), ApiError> {
    let room_name = crate::rooms::community_voice_chat_room_name(&community_id);
    let room_service = state.room_service();
    let (livekit, deleted) = tokio::join!(
        async {
            if state.livekit.configured {
                room_service.delete_room(&room_name).await
            } else {
                Ok(())
            }
        },
        state.voice_db.delete_community_voice_chat(community_id),
    );
    if let Err(e) = livekit {
        tracing::warn!(error = %e, room = %room_name, "failed to delete {what}");
    }
    let participant_count = deleted?;
    notify_community_streaming_ended(state, &[(community_id, participant_count)]).await;
    state.social.community_voice_room_closed(community_id).await;
    Ok(())
}

pub async fn handle_community_participant_joined(
    state: &CommsState,
    user_address: &str,
    community_id: CommunityId,
    sid: Option<&str>,
) -> Result<(), ApiError> {
    if sid.is_none() {
        tracing::warn!(user = user_address, %community_id, "community participant joined without a session id");
    }
    Ok(state
        .voice_db
        .update_community_user_status(
            community_id,
            user_address,
            VoiceChatUserStatus::Connected,
            sid,
        )
        .await?)
}

/// A `participant_left` from a previous session (a different sid, or one that
/// predates the row's last update) is stale: the user reconnected.
pub(super) fn is_stale_leave(
    current_sid: Option<&str>,
    leaving_sid: Option<&str>,
    status_updated_at_ms: i64,
    leave_event_time_ms: Option<i64>,
) -> bool {
    let is_from_previous_session =
        matches!((leaving_sid, current_sid), (Some(leaving), Some(current)) if current != leaving);
    let user_state_is_newer_than_leave = current_sid.is_none()
        && leave_event_time_ms
            .map(|t| status_updated_at_ms > t + STALE_LEAVE_SKEW_MS)
            .unwrap_or(false);
    is_from_previous_session || user_state_is_newer_than_leave
}

pub async fn handle_community_participant_left(
    state: &CommsState,
    user_address: &str,
    community_id: CommunityId,
    reason: DisconnectReason,
    sid: Option<&str>,
    leave_event_time_ms: Option<i64>,
    event_id: &str,
) -> Result<(), ApiError> {
    match reason {
        DisconnectReason::DuplicateIdentity => {
            tracing::debug!(user = user_address, %community_id, "ignoring community disconnect due to duplicate identity");
            Ok(())
        }
        DisconnectReason::RoomDeleted => {
            finish_webhook_community(state, community_id, event_id, false).await
        }
        DisconnectReason::ClientInitiated => {
            let users_in_room = state.voice_db.community_users_in_room(community_id).await?;
            let leaving_user = users_in_room.iter().find(|u| u.address == user_address);
            if let Some(leaving_user) = leaving_user
                && is_stale_leave(
                    leaving_user.sid.as_deref(),
                    sid,
                    leaving_user.status_updated_at,
                    leave_event_time_ms,
                )
            {
                tracing::info!(user = user_address, %community_id, "ignoring stale participant_left: a newer session is active");
                return Ok(());
            }
            if leaving_user.is_some_and(|u| u.is_moderator) {
                let now = now_ms();
                let remaining = users_in_room
                    .iter()
                    .filter(|u| {
                        u.is_moderator
                            && u.address != user_address
                            && state.voice_db.is_active_community_user(u, now)
                    })
                    .count();
                if remaining == 0 {
                    tracing::debug!(%community_id, "no active moderators left in community room, destroying it");
                    return finish_webhook_community(state, community_id, event_id, true).await;
                }
            }
            state
                .voice_db
                .update_community_user_status(
                    community_id,
                    user_address,
                    VoiceChatUserStatus::Disconnected,
                    None,
                )
                .await?;
            Ok(())
        }
        _ => Ok(state
            .voice_db
            .update_community_user_status(
                community_id,
                user_address,
                VoiceChatUserStatus::ConnectionInterrupted,
                None,
            )
            .await?),
    }
}

pub async fn expire_community_voice_chats(state: &CommsState) -> Result<(), ApiError> {
    let room_counts = state.voice_db.active_community_participant_counts().await?;
    let expired = state
        .voice_db
        .delete_expired_community_voice_chats()
        .await?;
    let ended: Vec<(CommunityId, i64)> = expired
        .iter()
        .map(|id| {
            tracing::info!(community = %id, "expiring community voice chat room");
            (*id, room_counts.get(id).copied().unwrap_or(0))
        })
        .collect();
    let rooms: Vec<String> = expired
        .iter()
        .map(crate::rooms::community_voice_chat_room_name)
        .collect();
    let ((), ()) = tokio::join!(
        delete_livekit_rooms(state, &rooms, "expired community voice room"),
        notify_community_streaming_ended(state, &ended)
    );
    for id in &expired {
        state.social.community_voice_room_closed(*id).await;
    }
    Ok(())
}

/// One sweep of both presence tables (the `comms-voice-expiration` job).
pub async fn expire_voice_chats(state: &CommsState) -> anyhow::Result<()> {
    expire_private_voice_chats(state)
        .await
        .map_err(|e| anyhow::anyhow!("private voice chat expiration: {e}"))?;
    expire_community_voice_chats(state)
        .await
        .map_err(|e| anyhow::anyhow!("community voice chat expiration: {e}"))?;
    Ok(())
}
