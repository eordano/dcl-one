//! Voice: the private and community voice rooms (the private-messages room
//! token, private calls, community rooms and their expiry sweep), their
//! store (`db`) and the rooms trait social consumes (`rooms`); their
//! presence comes from the LiveKit webhook (`gatekeeper::webhook`).
//!
//! Token minting is offline. Webhook lifecycle cleanup must succeed before
//! acknowledgement; token and maintenance paths retain their existing policies.
//!
//! # Trust boundary of the bearer-gated routes (intentional)
//!
//! The `/private-voice-chat*`, `/community-voice-chat*`, `/users/{address}/
//! *voice-chat-status` and `/users/{address}/private-messages-privacy` routes
//! establish no per-user identity: their only gate is the platform service
//! token (`COMMS_GATEKEEPER_AUTH_TOKEN`, checked by the router layer), and the
//! wallet, the community and the claimed role come verbatim from the body.
//! Upstream puts community-role resolution in the calling platform service;
//! comms is the downstream executor. A leak of that one token is moderator
//! authority over every community at once. The friends/communities modules
//! calling these in-process inherit the same contract until they call the
//! logic functions directly.

pub mod db;
pub mod rooms;

use crate::shared::now_utc;
use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Extension, OriginalUri, Path, State};
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{Address, AuthorityNotEstablished, CommunityId};

use super::auth::{EXPLORER_SIGNER, device_identifier, get_request_ip, verify_signed_fetch};
use super::errors::{ApiError, forbidden, service_unavailable};
use super::livekit::{AccessToken, KICK_CONCURRENCY, build_adapter_url, join_grants};
use super::moderation::store::UpsertPlayerConnection;
use super::{CommsState, SharedState};
use crate::rooms::private_voice_chat_room_name;
use crate::shared::signed_fetch::SignedPath;
use db::{CommunityJoinError, DeleteRoomError, EndedPrivateCall, PrivateJoin, VoiceChatUserStatus};

const STALE_LEAVE_SKEW_MS: i64 = 1000;

pub(super) fn now_ms() -> i64 {
    now_utc().and_utc().timestamp_millis()
}

pub(super) fn require_livekit(state: &CommsState) -> Result<(), ApiError> {
    if state.livekit.configured {
        Ok(())
    } else {
        Err(service_unavailable(
            "LiveKit is not configured (LIVEKIT_API_KEY / LIVEKIT_API_SECRET unset)",
        ))
    }
}

fn mint(
    state: &CommsState,
    identity: &str,
    grants: super::livekit::VideoGrants,
    metadata: Option<String>,
) -> Result<String, ApiError> {
    let mut token = AccessToken::new(
        &state.livekit.api_key,
        &state.livekit.api_secret,
        identity,
        grants,
    );
    if let Some(m) = metadata {
        token = token.with_metadata(m);
    }
    let jwt = token
        .to_jwt()
        .map_err(|e| ApiError::internal(format!("livekit token: {e}")))?;
    Ok(build_adapter_url(&state.livekit.client_url, &jwt))
}

pub const DENY_LISTED_MSG: &str = "Access denied, deny-listed wallet.";

/// Voice mints refuse a deny-listed participant like the other mint paths.
async fn refuse_deny_listed(state: &CommsState, addresses: &[Address]) -> Result<(), ApiError> {
    for address in addresses {
        if state.deny_list.is_denied(address.as_str()).await {
            return Err(super::errors::unauthorized(DENY_LISTED_MSG));
        }
    }
    Ok(())
}

mod private;
pub(super) use private::pending_private_voice_chat_room_credentials;
pub use private::{
    EndPrivateVoiceChatBody, PrivateMessagesPrivacyBody, PrivateVoiceChatBody,
    VoiceChatStatusResponse, create_private_voice_chat, end_private_call, end_private_voice_chat,
    end_private_voice_chat_by, get_private_voice_chat_room_credentials, get_voice_chat_status,
    patch_private_messages_privacy, private_messages_token,
};
mod community;
use community::community_status;
pub use community::{
    BulkCommunityStatusBody, CommunityVoiceChatBody, CommunityVoiceChatStatusResponse,
    EndCommunityVoiceChatBody, MuteSpeakerBody, check_user_community_status,
    community_demote_speaker, community_kick_player, community_mute_speaker,
    community_promote_speaker, community_reject_speak_request, community_request_to_speak,
    community_voice_chat_active, community_voice_chat_bulk_status,
    community_voice_chat_create_or_join, community_voice_chat_end, community_voice_chat_status,
    community_voice_credentials_for, kick_from_community_room,
};
pub(super) use community::{merge_metadata, patch, set_speaker};
mod lifecycle;
#[cfg(test)]
pub(super) use lifecycle::delete_webhook_room;
pub(super) use lifecycle::destroy_community_voice_chat;
pub use lifecycle::{
    DisconnectReason, expire_community_voice_chats, expire_private_voice_chats, expire_voice_chats,
    handle_community_participant_joined, handle_community_participant_left,
    handle_private_participant_joined, handle_private_participant_left,
};

#[cfg(test)]
use community::{
    claimed_role_grants_speaker_rights, community_and_claimed_target, community_join_metadata,
    validate_bulk_community_ids,
};
#[cfg(test)]
use lifecycle::{community_streaming_ended_event, is_stale_leave};

#[cfg(test)]
mod speaker_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_deserialize_as_upstream() {
        let body: PrivateVoiceChatBody = serde_json::from_str(
            r#"{ "room_id": "call-1", "user_addresses": ["0xAAA", "0xBBB"] }"#,
        )
        .unwrap();
        assert_eq!(body.room_id, "call-1");
        assert_eq!(body.user_addresses, vec!["0xAAA", "0xBBB"]);
        let with: EndPrivateVoiceChatBody =
            serde_json::from_str(r#"{ "address": "0xabc" }"#).unwrap();
        assert_eq!(with.address.as_deref(), Some("0xabc"));
        let without: EndPrivateVoiceChatBody = serde_json::from_str("{}").unwrap();
        assert!(without.address.is_none());
        let full: CommunityVoiceChatBody = serde_json::from_str(
            r#"{ "community_id": "c1", "user_address": "0xabc", "user_role": "owner", "action": "create",
                 "profile_data": { "name": "Foo", "has_claimed_name": true, "profile_picture_url": "http://x/y.png" } }"#,
        )
        .unwrap();
        assert_eq!(full.user_role.as_deref(), Some("owner"));
        assert_eq!(full.action.as_deref(), Some("create"));
        let minimal: CommunityVoiceChatBody =
            serde_json::from_str(r#"{ "community_id": "c1", "user_address": "0xabc" }"#).unwrap();
        assert!(
            minimal.user_role.is_none()
                && minimal.action.is_none()
                && minimal.profile_data.is_none()
        );
        let mute: MuteSpeakerBody = serde_json::from_str(r#"{ "muted": true }"#).unwrap();
        assert!(mute.muted);
        let bulk: BulkCommunityStatusBody =
            serde_json::from_str(r#"{ "community_ids": ["a", "b"] }"#).unwrap();
        assert_eq!(bulk.community_ids, vec!["a", "b"]);
    }

    #[test]
    fn bulk_community_ids_bounded_at_100() {
        let at_limit: Vec<String> = (0..100).map(|i| format!("c{i}")).collect();
        assert!(validate_bulk_community_ids(&at_limit).is_ok());
        let over: Vec<String> = (0..101).map(|i| format!("c{i}")).collect();
        assert_eq!(validate_bulk_community_ids(&over).unwrap_err().code, 400);
    }

    #[test]
    fn the_claimed_role_that_grants_speaker_rights_is_exactly_owner_or_moderator() {
        assert!(claimed_role_grants_speaker_rights("owner"));
        assert!(claimed_role_grants_speaker_rights("moderator"));
        for rejected in [
            "member", "none", "mod", "admin", "Owner", "OWNER", "banned", "",
        ] {
            assert!(
                !claimed_role_grants_speaker_rights(rejected),
                "{rejected:?}"
            );
        }
    }

    #[test]
    fn community_join_metadata_shapes() {
        let md = community_join_metadata("owner", true, None);
        assert_eq!(md["role"], "owner");
        assert_eq!(md["isSpeaker"], true);
        assert_eq!(md["muted"], false);
        assert_eq!(md.len(), 3);
        let profile = serde_json::json!({ "name": "Foo", "has_claimed_name": true, "profile_picture_url": "http://x/y.png" });
        let md = community_join_metadata("member", false, Some(&profile));
        assert_eq!(md["isSpeaker"], false);
        assert_eq!(md["name"], "Foo");
        assert_eq!(md["hasClaimedName"], true);
        assert_eq!(md["profilePictureUrl"], "http://x/y.png");
        let md =
            community_join_metadata("member", false, Some(&serde_json::json!({ "name": "Foo" })));
        assert!(!md.contains_key("hasClaimedName") && !md.contains_key("profilePictureUrl"));
    }

    #[test]
    fn the_path_parameter_shape_check_keeps_its_messages_order_and_lowercasing() {
        let id = CommunityId::new().to_string();
        assert_eq!(
            community_and_claimed_target("  ", "0xAbC")
                .unwrap_err()
                .code,
            400
        );
        assert_eq!(
            community_and_claimed_target("not-a-uuid", "0xAbC")
                .unwrap_err()
                .code,
            400
        );
        assert_eq!(
            community_and_claimed_target(&id, "   ").unwrap_err().code,
            400
        );
        assert_eq!(
            community_and_claimed_target(&id, "0xAbC").unwrap().1,
            "0xabc"
        );
    }

    #[test]
    fn disconnect_reason_parses_numeric_and_proto3_json_names() {
        assert_eq!(
            DisconnectReason::parse(Some(&serde_json::json!(1))),
            DisconnectReason::ClientInitiated
        );
        assert_eq!(
            DisconnectReason::parse(Some(&serde_json::json!(2))),
            DisconnectReason::DuplicateIdentity
        );
        assert_eq!(
            DisconnectReason::parse(Some(&serde_json::json!(5))),
            DisconnectReason::RoomDeleted
        );
        assert_eq!(
            DisconnectReason::parse(Some(&serde_json::json!(99))),
            DisconnectReason::Other(99)
        );
        assert_eq!(
            DisconnectReason::parse(Some(&serde_json::json!("CLIENT_INITIATED"))),
            DisconnectReason::ClientInitiated
        );
        assert_eq!(
            DisconnectReason::parse(Some(&serde_json::json!("ROOM_DELETED"))),
            DisconnectReason::RoomDeleted
        );
        assert_eq!(
            DisconnectReason::parse(None),
            DisconnectReason::UnknownReason
        );
        assert_eq!(
            DisconnectReason::parse(Some(&Value::Null)),
            DisconnectReason::UnknownReason
        );
    }

    #[test]
    fn community_streaming_ended_event_matches_dcl_schemas_shape() {
        let event = community_streaming_ended_event(
            "my-community",
            7,
            "community-streaming-ended-my-community-1718900000000",
            1_718_900_000_000,
        );
        assert_eq!(event["type"], "streaming");
        assert_eq!(event["subType"], "community-streaming-ended");
        assert_eq!(
            event["key"],
            "community-streaming-ended-my-community-1718900000000"
        );
        assert_eq!(event["timestamp"], 1_718_900_000_000i64);
        assert_eq!(event["metadata"]["communityId"], "my-community");
        assert_eq!(event["metadata"]["totalParticipants"], 7);
        assert_eq!(event["metadata"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn stale_leave_rules() {
        assert!(is_stale_leave(Some("new"), Some("old"), 0, None));
        assert!(is_stale_leave(Some("new"), Some("old"), 0, Some(1_000_000)));
        assert!(!is_stale_leave(Some("s1"), Some("s1"), 0, None));
        assert!(!is_stale_leave(Some("s1"), Some("s1"), 0, Some(0)));
        assert!(is_stale_leave(None, Some("s"), 10_000, Some(5_000)));
        assert!(is_stale_leave(None, None, 10_000, Some(5_000)));
        assert!(!is_stale_leave(None, Some("s"), 5_500, Some(5_000)));
        assert!(!is_stale_leave(
            None,
            Some("s"),
            5_000 + STALE_LEAVE_SKEW_MS,
            Some(5_000)
        ));
        assert!(is_stale_leave(
            None,
            Some("s"),
            5_000 + STALE_LEAVE_SKEW_MS + 1,
            Some(5_000)
        ));
        assert!(!is_stale_leave(None, None, 999_999, None));
        assert!(!is_stale_leave(Some("s"), Some("s"), 10_000_000, Some(0)));
    }

    #[test]
    fn response_key_casing_matches_upstream() {
        let single = serde_json::to_value(CommunityVoiceChatStatusResponse {
            active: true,
            participant_count: 3,
            moderator_count: 1,
        })
        .unwrap();
        let obj = single.as_object().unwrap();
        assert_eq!(obj.len(), 3);
        assert!(obj.contains_key("participant_count") && obj.contains_key("moderator_count"));
        let status = serde_json::to_value(VoiceChatStatusResponse {
            is_user_in_voice_chat: false,
        })
        .unwrap();
        assert_eq!(status["is_user_in_voice_chat"], false);
    }
}
