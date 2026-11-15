//! `POST /livekit-webhook`: the one LiveKit webhook route and its dispatch.
//! The verified participant events drive the voice rooms' presence
//! (`gatekeeper::voice`), the ingress events the stream rows
//! (`gatekeeper::streaming`), then the live presence and the room event sink.
//! The room event store receives every verified participant event.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use super::errors::{ApiError, unauthorized};
use super::livekit::{address_from_identity, verify_webhook_token};
use super::voice::{
    DisconnectReason, handle_community_participant_joined, handle_community_participant_left,
    handle_private_participant_joined, handle_private_participant_left, require_livekit,
};
use super::{CommsState, SharedState};
use crate::rooms::{community_id_from_room_name, private_call_id_from_room_name};

// A rolling history bounds memory without imposing a global event quota.
// At overload the oldest protection is lost; lifecycle application still runs.
// IDs expire at their authenticated JWT acceptance deadline. Active identities
// are kept independently until displaced/evicted, never expired by the join JWT.
const MAX_ENTRIES: usize = 100_000;
const LOCK_STRIPES: usize = 64;
const METADATA_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

struct History<V> {
    values: HashMap<String, (V, u64)>,
    order: BTreeMap<u64, String>,
    sequence: u64,
    capacity: usize,
}

impl<V> Default for History<V> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            order: BTreeMap::new(),
            sequence: 0,
            capacity: MAX_ENTRIES,
        }
    }
}

impl<V> History<V> {
    fn get(&self, key: &str) -> Option<&V> {
        self.values.get(key).map(|(value, _)| value)
    }
    fn insert(&mut self, key: String, value: V) {
        if let Some((_, order)) = self.values.remove(&key) {
            self.order.remove(&order);
        }
        while self.values.len() >= self.capacity {
            if let Some((_, key)) = self.order.pop_first() {
                self.values.remove(&key);
            }
        }
        self.sequence += 1;
        self.order.insert(self.sequence, key.clone());
        self.values.insert(key, (value, self.sequence));
    }
    fn remove(&mut self, key: &str) -> bool {
        let Some((_, order)) = self.values.remove(key) else {
            return false;
        };
        self.order.remove(&order);
        true
    }
}

/// What the webhook does with a verified event.
#[derive(Debug, PartialEq, Eq)]
enum Prepared {
    Apply,
    /// A redelivery of an applied event.
    Duplicate,
    /// A participant event an ordered session supersedes for comms.
    Superseded,
}

#[derive(Default)]
struct WebhookState {
    applied: History<u64>,
    participants: History<(i64, bool)>,
    current: History<(String, i64, bool)>,
    metadata: History<(String, tokio::time::Instant)>,
    /// Event IDs whose store failed before the sink observed them.
    unobserved: History<()>,
}

pub(super) struct WebhookCoordinator {
    state: Mutex<WebhookState>,
    locks: [tokio::sync::Mutex<()>; LOCK_STRIPES],
}

impl Default for WebhookCoordinator {
    fn default() -> Self {
        Self {
            state: Mutex::new(WebhookState::default()),
            locks: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
        }
    }
}

fn stripe(key: &str) -> usize {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hash);
    hash.finish() as usize % LOCK_STRIPES
}

#[derive(Debug)]
struct ParticipantEvent {
    sid: String,
    identity: String,
    room: String,
    current_key: String,
    voice_address: String,
}

#[derive(Debug)]
enum EventKind {
    RoomStarted,
    RoomFinished,
    ParticipantJoined(ParticipantEvent),
    ParticipantLeft(ParticipantEvent, DisconnectReason),
    IngressStarted,
    IngressEnded,
    Ignored,
}

#[derive(Debug)]
struct ValidatedEvent {
    id: String,
    at: i64,
    name: String,
    kind: EventKind,
    room: Option<String>,
    room_sid: Option<String>,
    ingress_id: Option<String>,
}

impl ValidatedEvent {
    fn parse(event: &Value) -> Result<Self, ApiError> {
        let id = event_id(event);
        let at = event_time_ms(event);
        if id.is_none_or(|id| id.len() > 128) || at.is_none() {
            return Err(ApiError::bad_request(
                "missing livekit webhook id or createdAt",
            ));
        }
        let name = event
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let room = room_name(event);
        let room_sid = room_sid(event).map(str::to_owned);
        let kind = match name {
            "participant_joined" | "participant_left" => {
                let sid = participant_sid(event);
                let identity = participant_identity(event);
                if sid.as_ref().is_none_or(|sid| sid.len() > 128)
                    || room.as_ref().is_none_or(|room| room.len() > 512)
                    || identity
                        .as_ref()
                        .is_none_or(|identity| identity.len() > 512)
                {
                    return Err(ApiError::bad_request(
                        "missing livekit participant event fields",
                    ));
                }
                let identity = identity.unwrap();
                let room = room.clone().unwrap();
                let participant = ParticipantEvent {
                    sid: sid.unwrap(),
                    current_key: identity_key(&room, &identity),
                    room,
                    voice_address: address_from_identity(&identity)
                        .unwrap_or_else(|| identity.to_lowercase().chars().take(42).collect()),
                    identity,
                };
                if name == "participant_joined" {
                    EventKind::ParticipantJoined(participant)
                } else {
                    EventKind::ParticipantLeft(
                        participant,
                        DisconnectReason::parse(
                            event
                                .get("participant")
                                .and_then(|p| p.get("disconnectReason")),
                        ),
                    )
                }
            }
            "room_started" => {
                if room
                    .as_ref()
                    .is_none_or(|room| room.is_empty() || room.len() > 512)
                    || room_sid.as_ref().is_some_and(|sid| sid.len() > 128)
                {
                    return Err(ApiError::bad_request("invalid livekit room event fields"));
                }
                EventKind::RoomStarted
            }
            "room_finished" => EventKind::RoomFinished,
            "ingress_started" => EventKind::IngressStarted,
            "ingress_ended" => EventKind::IngressEnded,
            _ => EventKind::Ignored,
        };
        Ok(Self {
            id: id.unwrap().to_owned(),
            at: at.unwrap(),
            name: name.to_owned(),
            kind,
            room,
            room_sid,
            ingress_id: event
                .get("ingressInfo")
                .and_then(|i| i.get("ingressId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    fn participant(&self) -> Option<&ParticipantEvent> {
        match &self.kind {
            EventKind::ParticipantJoined(participant)
            | EventKind::ParticipantLeft(participant, _) => Some(participant),
            _ => None,
        }
    }

    fn joined(&self) -> bool {
        matches!(self.kind, EventKind::ParticipantJoined(_))
    }
    fn left(&self) -> bool {
        matches!(self.kind, EventKind::ParticipantLeft(_, _))
    }
}

// Host identities and counted players retain separate case-insensitive session keys.
fn identity_key(room: &str, identity: &str) -> String {
    let class = if crate::presence::counts_as_player(identity) {
        "player"
    } else {
        "server"
    };
    format!("{}:{room}{class}:{}", room.len(), identity.to_lowercase())
}

#[cfg(test)]
fn current_key(event: &Value) -> Option<String> {
    Some(identity_key(
        &room_name(event)?,
        &participant_identity(event)?,
    ))
}

impl WebhookState {
    fn metadata_due(&self, room: &str, sid: Option<&str>) -> bool {
        !sid.is_some_and(|sid| {
            self.metadata.get(room).is_some_and(|(current, completed)| {
                current == sid && completed.elapsed() < METADATA_COOLDOWN
            })
        })
    }

    fn prepare(&mut self, event: &ValidatedEvent, deadline: u64, now: u64) -> Prepared {
        if let Some(previous) = self.applied.get(&event.id).copied() {
            if now <= previous {
                self.applied
                    .insert(event.id.clone(), deadline.max(previous));
                return Prepared::Duplicate;
            }
            self.applied.remove(&event.id);
        }
        if let Some(participant) = event.participant() {
            let sid = &participant.sid;
            let at = event.at;
            if self
                .participants
                .get(sid)
                .is_some_and(|(latest, left)| at < *latest || (at == *latest && *left))
            {
                return Prepared::Superseded;
            }
            if let Some((current_sid, joined, left)) = self.current.get(&participant.current_key) {
                if event.left() && current_sid != sid {
                    // Preserve the old session tombstone against delayed equal-second joins.
                    self.participants.insert(sid.clone(), (at, true));
                    return Prepared::Superseded;
                }
                if event.joined()
                    && (at < *joined
                        || (at == *joined && (sid < current_sid || (sid == current_sid && *left))))
                {
                    return Prepared::Superseded;
                }
            }
        }
        Prepared::Apply
    }

    fn applied(&mut self, event: &ValidatedEvent, deadline: u64) {
        self.unobserved.remove(&event.id);
        self.applied.insert(event.id.clone(), deadline);
        if let Some(participant) = event.participant() {
            let left = event.left();
            self.participants
                .insert(participant.sid.clone(), (event.at, left));
            self.current.insert(
                participant.current_key.clone(),
                (participant.sid.clone(), event.at, left),
            );
        }
    }
}

fn event_id(event: &Value) -> Option<&str> {
    event
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// A verified event is acknowledged only after its effects have succeeded.
pub async fn livekit_webhook(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    require_livekit(&state)?;
    let auth = headers
        .get("authorization")
        .or_else(|| headers.get("x-livekit-signature"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let deadline = verify_webhook_token(
        &state.livekit.api_key,
        &state.livekit.api_secret,
        &body,
        auth,
    )
    .ok_or_else(|| unauthorized("invalid livekit webhook signature"))?;
    let event: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("invalid livekit webhook JSON"))?;
    let event = ValidatedEvent::parse(&event)?;
    tracing::info!(event = event.name, "livekit webhook received");
    if matches!(event.kind, EventKind::RoomStarted | EventKind::RoomFinished)
        && let Some(room) = &event.room
    {
        state.presence.invalidate_room(room);
    }
    if matches!(event.kind, EventKind::RoomFinished)
        && let (Some(room), Some(sid)) = (&event.room, &event.room_sid)
    {
        super::removal::room_closed(&state.pool, room, sid).await?;
    }
    // Room completion effects precede ignored-event acknowledgement.
    if matches!(event.kind, EventKind::RoomFinished | EventKind::Ignored) {
        return Ok(axum::http::StatusCode::OK.into_response());
    }
    let resource = event
        .room
        .as_deref()
        .or(event.ingress_id.as_deref())
        .unwrap_or_default();
    let id = &event.id;
    let (a, b) = (stripe(id), stripe(&resource));
    let _first = state.webhooks.locks[a.min(b)].lock().await;
    let _second = if a != b {
        Some(state.webhooks.locks[a.max(b)].lock().await)
    } else {
        None
    };
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    if now > deadline {
        return Err(unauthorized("expired livekit webhook signature"));
    }
    let prepared = state
        .webhooks
        .state
        .lock()
        .unwrap()
        .prepare(&event, deadline, now);
    if prepared != Prepared::Apply {
        // The store orders sessions itself: a superseded event still counts there.
        if prepared == Prepared::Superseded
            && state.room_event_store.is_some()
            && let Some(room_event) = room_event(&state, &event).await?
        {
            store_room_event(&state, &room_event).await?;
            // A failed store's redelivery can be superseded by then; the sink still gets it once.
            let owed = state.webhooks.state.lock().unwrap().unobserved.remove(id);
            if owed && let Some(sink) = state.room_events.as_ref() {
                sink.observe_room_event(&room_event);
            }
        }
        return Ok(axum::http::StatusCode::OK.into_response());
    }
    // Invalidate before any awaited side effect/resolution. Even a handler
    // retry cannot let a concurrent read certify a room whose event is known.
    if let Some(participant) = event.participant() {
        state.presence.invalidate_room(&participant.room);
    }
    if (matches!(event.kind, EventKind::RoomStarted) || event.joined())
        && let Some(room) = &event.room
    {
        let sid = event.room_sid.as_deref().filter(|sid| sid.len() <= 128);
        let due = state
            .webhooks
            .state
            .lock()
            .unwrap()
            .metadata_due(&room, sid);
        if due {
            // Already serialized by this room's webhook stripe. Record only a
            // confirmed same-incarnation write; failures remain retryable.
            let updated =
                super::moderation::maintenance::initialize_room(state.as_ref(), &room, sid).await?;
            if let Some(updated) = updated.filter(|updated| sid == Some(updated.as_str())) {
                state
                    .webhooks
                    .state
                    .lock()
                    .unwrap()
                    .metadata
                    .insert(room.clone(), (updated, tokio::time::Instant::now()));
            }
        }
    }
    dispatch(&state, &event).await?;
    if let Some(room_event) = room_event(&state, &event).await? {
        if let Err(error) = store_room_event(&state, &room_event).await {
            state
                .webhooks
                .state
                .lock()
                .unwrap()
                .unobserved
                .insert(id.to_owned(), ());
            return Err(error);
        }
        if let Some(sink) = state.room_events.as_ref() {
            sink.observe_room_event(&room_event);
        }
    }
    state
        .webhooks
        .state
        .lock()
        .unwrap()
        .applied(&event, deadline);
    Ok(axum::http::StatusCode::OK.into_response())
}

/// The participant event the store and the sink consume, with one resolved
/// identity. A world scene's complete name is resolved without guessing a
/// hyphen boundary; a failed resolution leaves the webhook unapplied for retry.
async fn room_event(
    state: &CommsState,
    event: &ValidatedEvent,
) -> Result<Option<crate::protocol::bus::room_events::RoomEvent>, ApiError> {
    use crate::protocol::bus::room_events::{RoomEvent, RoomEventKind, RoomType};
    let Some(participant) = event.participant() else {
        return Ok(None);
    };
    let room = &participant.room;
    let Some(parsed) = crate::rooms::resolve_room_name(state.scenes.as_ref(), &room)
        .await
        .map_err(|error| {
            tracing::warn!(%error, room, "canonical room identity unavailable");
            ApiError::http(503, "Room identity is temporarily unavailable")
        })?
    else {
        return Ok(None);
    };
    Ok(Some(RoomEvent {
        kind: if event.joined() {
            RoomEventKind::Joined
        } else {
            RoomEventKind::Left
        } as i32,
        room: room.clone(),
        room_type: if parsed.realm.is_world() {
            RoomType::World
        } else {
            RoomType::Scene
        } as i32,
        realm_name: parsed
            .realm
            .world()
            .map(|w| w.key())
            .unwrap_or_else(|| parsed.realm.as_str())
            .to_owned(),
        scene_id: parsed
            .scene_id
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or("")
            .to_owned(),
        identity: participant.identity.clone(),
        participant_sid: participant.sid.clone(),
        event_time_ms: event.at as u64,
        event_id: event.id.clone(),
    }))
}

/// Stores the event before the webhook is acknowledged; a failure is a 503.
async fn store_room_event(
    state: &CommsState,
    event: &crate::protocol::bus::room_events::RoomEvent,
) -> Result<(), ApiError> {
    let Some(store) = state.room_event_store.as_ref() else {
        return Ok(());
    };
    store.store_room_event(event).await.map_err(|error| {
        tracing::warn!(%error, event_id = event.event_id, "room event not stored");
        ApiError::http(503, "Room event not stored")
    })
}

async fn dispatch(state: &CommsState, event: &ValidatedEvent) -> Result<(), ApiError> {
    match &event.kind {
        EventKind::IngressStarted | EventKind::IngressEnded => {
            super::streaming::ingress_event(
                state,
                if matches!(event.kind, EventKind::IngressStarted) {
                    super::streaming::IngressEvent::Started
                } else {
                    super::streaming::IngressEvent::Ended
                },
                event.ingress_id.as_deref(),
            )
            .await?;
        }
        EventKind::ParticipantJoined(participant) => {
            if let Some(call_id) = private_call_id_from_room_name(&participant.room) {
                handle_private_participant_joined(state, &participant.voice_address, call_id)
                    .await?;
            } else if let Some(community_id) = community_id_from_room_name(&participant.room) {
                handle_community_participant_joined(
                    state,
                    &participant.voice_address,
                    community_id,
                    Some(&participant.sid),
                )
                .await?;
            }
        }
        EventKind::ParticipantLeft(participant, reason) => {
            if let Some(call_id) = private_call_id_from_room_name(&participant.room) {
                handle_private_participant_left(
                    state,
                    &participant.voice_address,
                    call_id,
                    *reason,
                )
                .await?;
            } else if let Some(community_id) = community_id_from_room_name(&participant.room) {
                handle_community_participant_left(
                    state,
                    &participant.voice_address,
                    community_id,
                    *reason,
                    Some(&participant.sid),
                    Some(event.at),
                    &event.id,
                )
                .await?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn room_name(event: &Value) -> Option<String> {
    event
        .get("room")
        .and_then(|r| r.get("name"))
        .and_then(|n| n.as_str())
        .map(String::from)
}

fn room_sid(event: &Value) -> Option<&str> {
    event
        .get("room")?
        .get("sid")?
        .as_str()
        .filter(|sid| !sid.is_empty())
}

fn participant_identity(event: &Value) -> Option<String> {
    event
        .get("participant")
        .and_then(|p| p.get("identity"))
        .and_then(|i| i.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[cfg(test)]
fn participant_address(event: &Value) -> Option<String> {
    let identity = event
        .get("participant")
        .and_then(|p| p.get("identity"))
        .and_then(|i| i.as_str())?;
    Some(
        address_from_identity(identity)
            .unwrap_or_else(|| identity.to_lowercase().chars().take(42).collect::<String>()),
    )
}

fn participant_sid(event: &Value) -> Option<String> {
    event
        .get("participant")
        .and_then(|p| p.get("sid"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn event_time_ms(event: &Value) -> Option<i64> {
    let created_at = event
        .get("createdAt")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);
    if created_at > 0 {
        if created_at < 1_000_000_000_000 {
            Some(created_at * 1000)
        } else {
            Some(created_at)
        }
    } else {
        None
    }
}

#[cfg(test)]
fn leave_event_time_ms(event: &Value) -> i64 {
    event_time_ms(event).unwrap_or_else(super::voice::now_ms)
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD as B64_STANDARD, URL_SAFE_NO_PAD};
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};

    /// Mints what LiveKit actually sends: an HS256 JWT over the body digest,
    /// bounded by `exp`/`nbf`. Sign with the wrong `secret` to mint the
    /// token a 401 needs.
    pub fn webhook_jwt(body: &[u8], key: &str, secret: &str) -> String {
        let now = chrono::Utc::now().timestamp() as u64;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let claims = serde_json::json!({ "iss": key, "nbf": now, "exp": now + 300, "sha256": B64_STANDARD.encode(Sha256::digest(body)) });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let signing_input = format!("{header}.{payload}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(signing_input.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }
}

#[cfg(test)]
mod initial_metadata_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn participant_event(id: &str, kind: &str, sid: &str, at: i64) -> Value {
        serde_json::json!({"id": id, "event": kind, "createdAt": at,
            "room": {"name": "world-test.dcl.eth"},
            "participant": {"sid": sid, "identity": "wallet"}})
    }

    #[test]
    fn parsed_events_preserve_validation_precedence_and_optional_fields() {
        for (event, error) in [
            (
                serde_json::json!({"event":"participant_joined"}),
                "missing livekit webhook id or createdAt",
            ),
            (
                serde_json::json!({"id":"id","createdAt":1,"event":"participant_joined"}),
                "missing livekit participant event fields",
            ),
            (
                serde_json::json!({"id":"id","createdAt":1,"event":"room_started","room":{"name":""}}),
                "invalid livekit room event fields",
            ),
        ] {
            assert_eq!(ValidatedEvent::parse(&event).unwrap_err().message, error);
        }
        for kind in [
            "room_finished",
            "track_published",
            "ingress_started",
            "ingress_ended",
        ] {
            let parsed = ValidatedEvent::parse(
                &serde_json::json!({"id":"id","createdAt":"123","event":kind}),
            )
            .unwrap();
            assert_eq!(parsed.at, 123000);
            assert!(parsed.room.is_none());
            assert!(parsed.ingress_id.is_none());
            assert!(parsed.participant().is_none());
        }
        let parsed = ValidatedEvent::parse(&serde_json::json!({"id":"id","createdAt":1718900000123i64,"event":"participant_left","room":{"name":""},"participant":{"identity":"0x1234567890ABCDEF1234567890abcdef12345678:wss","sid":"sid","disconnectReason":"CLIENT_INITIATED"}})).unwrap();
        assert_eq!(parsed.at, 1718900000123);
        let EventKind::ParticipantLeft(participant, reason) = parsed.kind else {
            panic!("expected leave")
        };
        assert_eq!(
            participant.identity,
            "0x1234567890ABCDEF1234567890abcdef12345678:wss"
        );
        assert_eq!(
            participant.voice_address,
            "0x1234567890abcdef1234567890abcdef12345678"
        );
        assert_eq!(reason, DisconnectReason::ClientInitiated);
        assert_eq!(participant.room, "");
    }

    #[test]
    fn retries_ordering_and_equal_second_leave_precedence() {
        let mut state = WebhookState::default();
        let join = participant_event("j", "participant_joined", "PA_1", 100);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&join).unwrap(), 500, 100),
            Prepared::Apply
        );
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&join).unwrap(), 500, 100),
            Prepared::Apply
        );
        state.applied(&ValidatedEvent::parse(&join).unwrap(), 500);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&join).unwrap(), 600, 100),
            Prepared::Duplicate
        );
        assert_eq!(state.applied.get("j"), Some(&600));
        let old_leave = participant_event("old", "participant_left", "PA_1", 99);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&old_leave).unwrap(), 500, 100),
            Prepared::Superseded
        );
        let leave = participant_event("l", "participant_left", "PA_1", 100);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&leave).unwrap(), 500, 100),
            Prepared::Apply
        );
        state.applied(&ValidatedEvent::parse(&leave).unwrap(), 500);
        let same_second_join = participant_event("j2", "participant_joined", "PA_1", 100);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&same_second_join).unwrap(), 500, 100),
            Prepared::Superseded
        );
        let newer = participant_event("newer", "participant_joined", "PA_2", 101);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&newer).unwrap(), 500, 100),
            Prepared::Apply
        );
        state.applied(&ValidatedEvent::parse(&newer).unwrap(), 500);
        let delayed_old = participant_event("old-sid", "participant_left", "PA_1", 102);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&delayed_old).unwrap(), 500, 100),
            Prepared::Superseded
        );
    }

    #[test]
    fn equal_second_same_sid_leave_wins_both_delivery_orders() {
        for reverse in [false, true] {
            let mut state = WebhookState::default();
            let mut events = [
                participant_event("join", "participant_joined", "PA_same", 100),
                participant_event("leave", "participant_left", "PA_same", 100),
            ];
            if reverse {
                events.reverse();
            }
            for event in events {
                if state.prepare(&ValidatedEvent::parse(&event).unwrap(), 500, 100)
                    == Prepared::Apply
                {
                    state.applied(&ValidatedEvent::parse(&event).unwrap(), 500);
                }
            }
            assert_eq!(state.participants.get("PA_same"), Some(&(100_000, true)));
        }
    }

    #[test]
    fn equal_second_session_choice_is_deterministic_in_both_orders() {
        for reverse in [false, true] {
            let mut state = WebhookState::default();
            let mut events = [
                participant_event("a", "participant_joined", "PA_A", 100),
                participant_event("b", "participant_joined", "PA_B", 100),
            ];
            if reverse {
                events.reverse();
            }
            for event in events {
                if state.prepare(&ValidatedEvent::parse(&event).unwrap(), 500, 100)
                    == Prepared::Apply
                {
                    state.applied(&ValidatedEvent::parse(&event).unwrap(), 500);
                }
            }
            let key =
                current_key(&participant_event("a", "participant_joined", "PA_A", 100)).unwrap();
            assert_eq!(state.current.get(&key).unwrap().0, "PA_B");
            let old_leave = participant_event("left-a", "participant_left", "PA_A", 100);
            assert_eq!(
                state.prepare(&ValidatedEvent::parse(&old_leave).unwrap(), 500, 100),
                Prepared::Superseded
            );
        }
    }

    #[test]
    fn bounded_rolling_history_keeps_unrelated_lifecycle_events_progressing() {
        let mut state = WebhookState::default();
        state.applied.capacity = 8;
        state.participants.capacity = 8;
        state.current.capacity = 8;
        for n in 0..1000 {
            let mut event = participant_event(
                &format!("j{n}"),
                "participant_joined",
                &format!("PA_{n}"),
                100 + n,
            );
            event["participant"]["identity"] = Value::String(format!("wallet{n}"));
            assert_eq!(
                state.prepare(&ValidatedEvent::parse(&event).unwrap(), 1000, 100),
                Prepared::Apply
            );
            state.applied(&ValidatedEvent::parse(&event).unwrap(), 1000);
            let leave = serde_json::json!({"id": format!("l{n}"), "event": "participant_left", "createdAt": 101+n,
                "room": event["room"], "participant": event["participant"]});
            assert_eq!(
                state.prepare(&ValidatedEvent::parse(&leave).unwrap(), 1000, 100),
                Prepared::Apply
            );
            state.applied(&ValidatedEvent::parse(&leave).unwrap(), 1000);
            assert!(state.applied.values.len() <= 8);
            assert!(state.participants.values.len() <= 8);
            assert!(state.current.values.len() <= 8);
        }
        let ingress =
            serde_json::json!({"id": "ingress", "event": "ingress_started", "createdAt": 100});
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&ingress).unwrap(), 2000, 1001),
            Prepared::Apply
        );
        state.applied(&ValidatedEvent::parse(&ingress).unwrap(), 2000);
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&ingress).unwrap(), 2000, 2000),
            Prepared::Duplicate
        );
        assert_eq!(
            state.prepare(&ValidatedEvent::parse(&ingress).unwrap(), 3000, 2001),
            Prepared::Apply
        );
    }

    #[test]
    fn webhook_field_readers() {
        let event = serde_json::json!({
            "event": "participant_left",
            "room": { "name": "voice-chat-private-x" },
            "participant": { "identity": "0x1234567890ABCDEF1234567890abcdef12345678:wss", "sid": "PA_1", "disconnectReason": "CLIENT_INITIATED" },
            "createdAt": 1_718_900_000,
        });
        assert_eq!(room_name(&event).as_deref(), Some("voice-chat-private-x"));
        assert_eq!(
            participant_address(&event).as_deref(),
            Some("0x1234567890abcdef1234567890abcdef12345678")
        );
        assert_eq!(participant_sid(&event).as_deref(), Some("PA_1"));
        assert_eq!(leave_event_time_ms(&event), 1_718_900_000_000);
        let ms_event = serde_json::json!({ "createdAt": "1718900000123" });
        assert_eq!(leave_event_time_ms(&ms_event), 1_718_900_000_123);
        assert!(leave_event_time_ms(&serde_json::json!({})) > 1_700_000_000_000);
    }
}
