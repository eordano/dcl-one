//! Shared LiveKit transport primitives behind the comms API (README,
//! "Explorer, worlds and social ownership"): token minting (offline: HS256
//! over the configured key/secret, from the shared `dcl-livekit-token`) and the
//! twirp RoomService client. LiveKit's media server is an external process;
//! everything here that dials it degrades explicitly (a logged best-effort
//! side effect, or a 503 where the caller needed the answer) and never takes
//! the process down. Policy (who gets which grant) lives in the handlers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;

const ADMIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

type HmacSha256 = Hmac<Sha256>;

pub use dcl_livekit_token::{
    AccessToken, LivekitError, TRACK_SOURCE_MICROPHONE, VideoGrants, ingress_admin_token,
    join_grants, room_admin_token, sign_hs256, with_not_before,
};

pub const DEV_API_KEY: &str = "devkey";
pub const DEV_API_SECRET: &str = "devsecret";

/// The upstream LiveKit dev-image placeholder credentials count as unset:
/// tokens minted against them parse locally but no real cluster accepts them.
pub fn is_placeholder_cred(value: &str) -> bool {
    let v = value.trim();
    v.eq_ignore_ascii_case(DEV_API_KEY) || v.eq_ignore_ascii_case(DEV_API_SECRET)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedCreds {
    Configured {
        api_key: String,
        api_secret: String,
    },
    /// No real credentials but the dev opt-in is set: mint against
    /// devkey/devsecret with `configured = false`.
    DevFallback,
    /// No real credentials and no opt-in: the service refuses to start.
    Unconfigured,
}

pub fn resolve_creds(api_key: String, api_secret: String, allow_dev_creds: bool) -> ResolvedCreds {
    let real = |v: &str| !v.trim().is_empty() && !is_placeholder_cred(v);
    if real(&api_key) && real(&api_secret) {
        ResolvedCreds::Configured {
            api_key,
            api_secret,
        }
    } else if allow_dev_creds {
        ResolvedCreds::DevFallback
    } else {
        ResolvedCreds::Unconfigured
    }
}

pub fn build_adapter_url(host: &str, token: &str) -> String {
    format!("livekit:{}?access_token={}", client_ws_url(host), token)
}

/// The two LiveKit endpoints: `client_url` is the signaling URL explorers dial
/// (adapter URLs); `api_url` is what this process calls for room admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivekitEndpoints {
    pub client_url: String,
    pub api_url: String,
}

fn split_scheme(host: &str) -> (Option<&str>, &str) {
    let host = host.trim().trim_end_matches('/');
    match host.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, host),
    }
}

pub fn client_ws_url(host: &str) -> String {
    let (scheme, rest) = split_scheme(host);
    let scheme = match scheme {
        Some("ws") | Some("http") => "ws",
        Some("wss") | Some("https") | None => "wss",
        Some(other) => other,
    };
    format!("{scheme}://{rest}")
}

/// The scheme mapping the LiveKit server SDK applies; trailing slashes dropped
/// so the twirp paths append.
pub fn api_base_url(host: &str) -> String {
    let (scheme, rest) = split_scheme(host);
    let scheme = match scheme {
        Some("ws") | Some("http") => "http",
        Some("wss") | Some("https") | None => "https",
        Some(other) => other,
    };
    format!("{scheme}://{rest}")
}

/// The URL the worlds `/status` comms block advertises for the signaling
/// host: upstream's `getLivekitStatusUrl` (`ws:` maps to `http:`, anything
/// else to `https:`, rendered by `URL.toString()`, so a bare host gains `/`).
pub fn status_url(client_url: &str) -> String {
    let base = api_base_url(client_url);
    if base
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('/'))
    {
        base
    } else {
        format!("{base}/")
    }
}

fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// An empty `host` passes through empty so a deployment that leaves LiveKit
/// off still boots; the missing configuration surfaces on the first room call.
pub fn resolve_endpoints(
    host: &str,
    api_host: Option<&str>,
    client_url: Option<&str>,
) -> LivekitEndpoints {
    let api_host = present(api_host);
    let client_url = present(client_url);
    if host.trim().is_empty() {
        return LivekitEndpoints {
            client_url: client_url.map(client_ws_url).unwrap_or_default(),
            api_url: api_host.map(api_base_url).unwrap_or_default(),
        };
    }
    LivekitEndpoints {
        client_url: client_ws_url(client_url.unwrap_or(host)),
        api_url: api_base_url(api_host.unwrap_or(host)),
    }
}

// ── participant identities ─

/// A LiveKit identity is the wallet, optionally suffixed (`0x..:session`);
/// anything that is not a wallet (the authoritative server) reads as none.
pub fn address_from_identity(identity: &str) -> Option<String> {
    let lower = identity.to_lowercase();
    let candidate: String = lower.chars().take(42).collect();
    crate::auth::is_eth_address(&candidate).then_some(candidate)
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
}

fn decode_body_digest(claim: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE};
    STANDARD
        .decode(claim)
        .or_else(|_| STANDARD_NO_PAD.decode(claim))
        .or_else(|_| URL_SAFE.decode(claim))
        .or_else(|_| URL_SAFE_NO_PAD.decode(claim))
        .ok()
        .filter(|b| b.len() == 32)
}

/// The LiveKit webhook bearer: HS256 by the API secret, `iss` = the API key,
/// unexpired (60 s leeway), and its `sha256` claim = the body's digest.
/// Returns the authenticated last accepted UNIX second, including that leeway.
pub fn verify_webhook_token(
    api_key: &str,
    api_secret: &str,
    body: &[u8],
    auth_header: &str,
) -> Option<u64> {
    let trimmed = auth_header.trim();
    let token = trimmed.strip_prefix("Bearer ").unwrap_or(trimmed).trim();
    let mut segments = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(sig_b64), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return None;
    };
    let Ok(header_bytes) = URL_SAFE_NO_PAD.decode(header_b64) else {
        return None;
    };
    let Ok(header) = serde_json::from_slice::<serde_json::Value>(&header_bytes) else {
        return None;
    };
    if header.get("alg").and_then(|a| a.as_str()) != Some("HS256") {
        return None;
    }
    let Ok(provided_sig) = URL_SAFE_NO_PAD.decode(sig_b64) else {
        return None;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(api_secret.as_bytes()) else {
        return None;
    };
    mac.update(header_b64.as_bytes());
    mac.update(b".");
    mac.update(payload_b64.as_bytes());
    if mac.verify_slice(&provided_sig).is_err() {
        return None;
    }
    let Ok(payload_bytes) = URL_SAFE_NO_PAD.decode(payload_b64) else {
        return None;
    };
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) else {
        return None;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return None;
    };
    let now = now.as_secs();
    const LEEWAY: u64 = 60;
    let exp = payload.get("exp").and_then(json_u64)?;
    if now > exp.saturating_add(LEEWAY) {
        return None;
    }
    if let Some(nbf) = payload.get("nbf").and_then(json_u64)
        && nbf > now.saturating_add(LEEWAY)
    {
        return None;
    }
    if payload.get("iss").and_then(|v| v.as_str()) != Some(api_key) {
        return None;
    }
    let claim = payload.get("sha256").and_then(|v| v.as_str())?;
    let expected = decode_body_digest(claim)?;
    crate::auth::admin_bearer::timing_safe_eq(&expected, &Sha256::digest(body))
        .then_some(exp.saturating_add(LEEWAY))
}

/// How many rooms a ban kick or a teardown touches at once.
pub const KICK_CONCURRENCY: usize = 8;

pub const BANNED_ADDRESSES_FIELD: &str = "bannedAddresses";
pub const SCENE_ADMINS_FIELD: &str = "sceneAdmins";

#[derive(Debug, Clone)]
pub struct ParticipantInfo {
    pub sid: String,
    pub identity: String,
    pub metadata: Option<String>,
    pub attributes: std::collections::BTreeMap<String, String>,
    /// `permission.canUpdateMetadata` (either proto-JSON spelling); `None`
    /// when the two spellings disagree or the value is not a bool.
    pub can_update_metadata: Option<bool>,
    /// `joinedAt`, unix seconds; `None` when absent or malformed.
    pub joined_at: Option<i64>,
}

/// What a removal found in the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Removed,
    Absent,
}

#[derive(Error)]
pub enum RoomServiceError {
    #[error("livekit removal confirmed absent participant")]
    RemovalParticipantAbsent,
    #[error("livekit removal rejected missing room")]
    RemovalRoomMissing,
    #[error("livekit removal definitely rejected")]
    RemovalRejected,
    #[error("livekit token mint failed")]
    Token(#[from] LivekitError),
    #[error("livekit request failed")]
    Request,
    #[error("livekit returned an invalid response")]
    InvalidResponse,
    #[error("livekit returned status {0}")]
    Status(u16),
    #[error("livekit room not found")]
    NotFound,
}

impl std::fmt::Debug for RoomServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let class = match self {
            Self::RemovalParticipantAbsent => "RemovalParticipantAbsent",
            Self::RemovalRoomMissing => "RemovalRoomMissing",
            Self::RemovalRejected => "RemovalRejected",
            Self::Token(_) => "Token",
            Self::Request => "Request",
            Self::InvalidResponse => "InvalidResponse",
            Self::Status(_) => "Status",
            Self::NotFound => "NotFound",
        };
        f.debug_tuple("RoomServiceError").field(&class).finish()
    }
}

// Bound complete admin responses before JSON allocation; never truncate inventory.
const ADMIN_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

async fn admin_body(response: reqwest::Response) -> Result<Vec<u8>, RoomServiceError> {
    bounded_admin_body(response, ADMIN_RESPONSE_BYTES).await
}

async fn bounded_admin_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, RoomServiceError> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(RoomServiceError::InvalidResponse);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            RoomServiceError::Request
        } else {
            RoomServiceError::InvalidResponse
        }
    })? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(RoomServiceError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Runs one admin call under [`ADMIN_REQUEST_TIMEOUT`]; an elapsed deadline is
/// a request failure.
async fn within_admin_deadline<T>(
    call: impl Future<Output = Result<T, RoomServiceError>>,
) -> Result<T, RoomServiceError> {
    tokio::time::timeout(ADMIN_REQUEST_TIMEOUT, call)
        .await
        .map_err(|_| RoomServiceError::Request)?
}

/// Sends one Twirp POST with the bearer token, a JSON body and the per-request
/// deadline. The caller classifies the status, because RoomService and Ingress
/// map failures differently.
async fn twirp_post(
    http: &reqwest::Client,
    url: String,
    token: &str,
    body: &serde_json::Value,
) -> Result<reqwest::Response, RoomServiceError> {
    http.post(url)
        .bearer_auth(token)
        .timeout(ADMIN_REQUEST_TIMEOUT)
        .json(body)
        .send()
        .await
        .map_err(|_| RoomServiceError::Request)
}

/// Reads a bounded response body as JSON.
async fn twirp_json(
    response: reqwest::Response,
    limit: usize,
) -> Result<serde_json::Value, RoomServiceError> {
    serde_json::from_slice(&bounded_admin_body(response, limit).await?)
        .map_err(|_| RoomServiceError::InvalidResponse)
}

// Protobuf JSON may omit repeated fields for empty lists. A present null or
// wrong type, or a non-object envelope, is unknown rather than terminal empty.
fn inventory<'a>(
    body: &'a serde_json::Value,
    field: &str,
) -> Result<&'a [serde_json::Value], RoomServiceError> {
    let object = body.as_object().ok_or(RoomServiceError::InvalidResponse)?;
    match object.get(field) {
        None => Ok(&[]),
        Some(serde_json::Value::Array(values)) => Ok(values),
        Some(_) => Err(RoomServiceError::InvalidResponse),
    }
}

fn room_inventory(body: &serde_json::Value) -> Result<&[serde_json::Value], RoomServiceError> {
    let rooms = inventory(body, "rooms")?;
    for room in rooms {
        let object = room.as_object().ok_or(RoomServiceError::InvalidResponse)?;
        if !object
            .get("name")
            .and_then(|name| name.as_str())
            .is_some_and(|name| !name.is_empty())
        {
            return Err(RoomServiceError::InvalidResponse);
        }
        for field in ["sid", "metadata"] {
            if object.get(field).is_some_and(|value| !value.is_string()) {
                return Err(RoomServiceError::InvalidResponse);
            }
        }
    }
    Ok(rooms)
}

pub fn parse_room_metadata(
    metadata_str: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    match metadata_str.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}

pub fn metadata_with_appended(
    mut metadata: serde_json::Map<String, serde_json::Value>,
    field: &str,
    value: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut arr: Vec<serde_json::Value> = match metadata.get(field) {
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    if arr.iter().any(|v| v.as_str() == Some(value)) {
        return None;
    }
    arr.push(serde_json::Value::String(value.to_string()));
    metadata.insert(field.to_string(), serde_json::Value::Array(arr));
    Some(metadata)
}

pub fn metadata_with_removed(
    mut metadata: serde_json::Map<String, serde_json::Value>,
    field: &str,
    value: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let arr: Vec<serde_json::Value> = match metadata.get(field) {
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ => return None,
    };
    let filtered: Vec<serde_json::Value> = arr
        .iter()
        .filter(|v| v.as_str() != Some(value))
        .cloned()
        .collect();
    if filtered.len() == arr.len() {
        return None;
    }
    metadata.insert(field.to_string(), serde_json::Value::Array(filtered));
    Some(metadata)
}

fn room_metadata_write_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The twirp RoomService over the operator-configured API host. The host is
/// configuration, never user input, and is commonly a private address, so it
/// goes through the shared client unguarded (`Outbound::raw`).
pub struct RoomServiceClient<'a> {
    pub http: &'a reqwest::Client,
    pub api_url: String,
    pub api_key: String,
    pub api_secret: String,
}

/// Strict recovery records preserve source identity/session bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OccupancyRoom {
    pub name: String,
    pub sid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OccupancyParticipant {
    pub identity: String,
    pub sid: String,
    /// When the participant joined, in milliseconds (`joinedAtMs`, else
    /// `joinedAt` seconds); 0 when LiveKit sent neither.
    pub joined_at_ms: i64,
}

/// A protojson int64, which LiveKit's Twirp JSON writes as a string.
fn json_i64(value: Option<&serde_json::Value>) -> Option<i64> {
    match value? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn recovery_field(value: &serde_json::Value, field: &str) -> Result<String, RoomServiceError> {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 512 && !v.contains('\0'))
        .map(str::to_owned)
        .ok_or(RoomServiceError::InvalidResponse)
}

fn recovery_rooms(body: &serde_json::Value) -> Result<Vec<OccupancyRoom>, RoomServiceError> {
    let values = inventory(body, "rooms")?;
    if values.len() > 4096 {
        return Err(RoomServiceError::InvalidResponse);
    }
    let mut names = std::collections::HashSet::new();
    let mut sids = std::collections::HashSet::new();
    values
        .iter()
        .map(|value| {
            let name = recovery_field(value, "name")?;
            let sid = recovery_field(value, "sid")?;
            if !names.insert(name.clone()) || !sids.insert(sid.clone()) {
                return Err(RoomServiceError::InvalidResponse);
            }
            Ok(OccupancyRoom { name, sid })
        })
        .collect()
}

fn recovery_participants(
    body: &serde_json::Value,
) -> Result<Vec<OccupancyParticipant>, RoomServiceError> {
    let values = inventory(body, "participants")?;
    if values.len() > 100_000 {
        return Err(RoomServiceError::InvalidResponse);
    }
    let mut identities = std::collections::HashSet::new();
    let mut sids = std::collections::HashSet::new();
    values
        .iter()
        .map(|value| {
            let identity = recovery_field(value, "identity")?;
            let sid = recovery_field(value, "sid")?;
            if !identities.insert(identity.clone()) || !sids.insert(sid.clone()) {
                return Err(RoomServiceError::InvalidResponse);
            }
            let joined_at_ms = json_i64(value.get("joinedAtMs"))
                .filter(|ms| *ms > 0)
                .or_else(|| json_i64(value.get("joinedAt")).map(|s| s.saturating_mul(1000)))
                .unwrap_or(0);
            Ok(OccupancyParticipant {
                identity,
                sid,
                joined_at_ms,
            })
        })
        .collect()
}

#[cfg(test)]
mod recovery_contract_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complete_empty_is_distinct_from_invalid_and_duplicate_inventory() {
        for empty in [json!({}), json!({"rooms":[]})] {
            assert!(recovery_rooms(&empty).unwrap().is_empty());
        }
        for bad in [
            json!(null),
            json!([]),
            json!({"rooms":null}),
            json!({"rooms":{}}),
            json!({"rooms":[{}]}),
            json!({"rooms":[{"name":"world-a"}]}),
            json!({"rooms":[{"name":"world-a","sid":""}]}),
            json!({"rooms":[{"name":"world-a","sid":"1"},{"name":"world-a","sid":"2"}]}),
            json!({"rooms":[{"name":"a","sid":"1"},{"name":"b","sid":"1"}]}),
        ] {
            assert!(recovery_rooms(&bad).is_err(), "{bad}");
        }
        for empty in [json!({}), json!({"participants":[]})] {
            assert!(recovery_participants(&empty).unwrap().is_empty());
        }
        for bad in [
            json!({"participants":null}),
            json!({"participants":[{"identity":"a"}]}),
            json!({"participants":[{"identity":"a","sid":"1"},{"identity":"a","sid":"2"}]}),
            json!({"participants":[{"identity":"a","sid":"1"},{"identity":"b","sid":"1"}]}),
        ] {
            assert!(recovery_participants(&bad).is_err(), "{bad}");
        }
        assert!(recovery_rooms(&json!({"rooms":[{"name":"a".repeat(513),"sid":"1"}]})).is_err());
        assert!(
            recovery_participants(
                &json!({"participants":[{"identity":"a","sid":"s".repeat(513)}]})
            )
            .is_err()
        );
        let raw = recovery_participants(
            &json!({"participants":[{"identity":"a","sid":"1"},{"identity":"A","sid":"2"}]}),
        )
        .unwrap();
        assert_eq!(raw[0].identity, "a");
        assert_eq!(raw[1].identity, "A");
        let joined = recovery_participants(&json!({"participants":[
            {"identity":"a","sid":"1","joinedAt":"1700000000","joinedAtMs":"1700000000123"},
            {"identity":"b","sid":"2","joinedAt":1700000001},
            {"identity":"c","sid":"3"},
        ]}))
        .unwrap();
        assert_eq!(
            joined.iter().map(|p| p.joined_at_ms).collect::<Vec<_>>(),
            [1_700_000_000_123, 1_700_000_001_000, 0]
        );
    }

    #[test]
    fn strict_inventory_accepts_exact_room_and_record_limits_then_refuses_excess() {
        let mut rooms = (0..4096)
            .map(|n| json!({"name":format!("room-{n}"),"sid":format!("RM_{n}")}))
            .collect::<Vec<_>>();
        assert_eq!(
            recovery_rooms(&json!({"rooms":&rooms})).unwrap().len(),
            4096
        );
        rooms.push(json!({"name":"room-excess","sid":"RM_excess"}));
        assert!(recovery_rooms(&json!({"rooms":rooms})).is_err());
        let mut participants = (0..100_000)
            .map(|n| json!({"identity":format!("identity-{n}"),"sid":format!("PA_{n}")}))
            .collect::<Vec<_>>();
        // Parser bound is independent of the stricter HTTP response byte cap;
        // aggregate scan capacity is tested across several bounded responses.
        assert_eq!(
            recovery_participants(&json!({"participants":&participants}))
                .unwrap()
                .len(),
            100_000
        );
        participants.push(json!({"identity":"identity-excess","sid":"PA_excess"}));
        assert!(recovery_participants(&json!({"participants":participants})).is_err());
        assert!(
            recovery_rooms(&json!({"rooms":[{"name":"n".repeat(512),"sid":"s".repeat(512)}]}))
                .is_ok()
        );
        assert!(
            recovery_participants(
                &json!({"participants":[{"identity":"i".repeat(512),"sid":"s".repeat(512)}]})
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn recovery_stream_rejects_two_mebibytes_even_without_length_header() {
        use axum::{Router, body::Body, routing::post};
        use futures::stream;
        let app = Router::new().route(
            "/twirp/livekit.RoomService/ListRooms",
            post(|| async {
                Body::from_stream(stream::iter(vec![
                    Ok::<_, std::io::Error>(vec![b' '; 1024 * 1024]),
                    Ok(vec![b' '; 1024 * 1024 + 1]),
                ]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let client = RoomServiceClient::new(&http, &url, "key", "secret");
        assert!(matches!(
            client.occupancy_rooms().await,
            Err(RoomServiceError::InvalidResponse)
        ));
        server.abort();
    }

    #[tokio::test]
    async fn cutoff_removal_requires_exact_twirp_confirmation_and_classifies_rejections() {
        use axum::{Router, http::StatusCode, routing::post};
        for (status, code, message, expected) in [
            (404, "not_found", "participant does not exist", "absent"),
            (404, "not_found", "requested room does not exist", "room"),
            (404, "not_found", "generic proxy not found", "unknown"),
            (404, "different", "participant does not exist", "unknown"),
            (429, "resource_exhausted", "rate limited", "rejected"),
            (500, "internal", "unavailable", "unknown"),
        ] {
            let app = Router::new().route(
                "/twirp/livekit.RoomService/RemoveParticipant",
                post(move || async move {
                    (
                        StatusCode::from_u16(status).unwrap(),
                        axum::Json(serde_json::json!({"code": code, "msg": message})),
                    )
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let http = reqwest::Client::new();
            let client = RoomServiceClient::new(&http, &url, "key", "secret");
            let actual = match client
                .remove_participant_with_cutoff("room", "wallet", Some(105))
                .await
            {
                Ok(Removal::Absent) => "absent",
                Err(RoomServiceError::RemovalRoomMissing) => "room",
                Err(RoomServiceError::RemovalRejected) => "rejected",
                Err(_) => "unknown",
                Ok(Removal::Removed) => "removed",
            };
            assert_eq!(actual, expected, "{status}/{code}/{message}");
            // Existing non-cutoff callers retain their idempotent absent behavior.
            if status == 404 {
                assert!(client.remove_participant("room", "wallet").await.is_ok());
            }
            server.abort();
        }
    }
}

impl<'a> RoomServiceClient<'a> {
    pub fn new(http: &'a reqwest::Client, api_url: &str, api_key: &str, api_secret: &str) -> Self {
        Self {
            http,
            api_url: api_base_url(api_url),
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
        }
    }

    fn endpoint(&self, method: &str) -> String {
        format!("{}/twirp/livekit.RoomService/{method}", self.api_url)
    }

    async fn call(
        &self,
        method: &str,
        room: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RoomServiceError> {
        self.call_bounded(method, room, body, ADMIN_RESPONSE_BYTES)
            .await
    }

    async fn call_bounded(
        &self,
        method: &str,
        room: &str,
        body: serde_json::Value,
        limit: usize,
    ) -> Result<serde_json::Value, RoomServiceError> {
        let strict_removal = method == "RemoveParticipant" && body.get("revokeTokenTs").is_some();
        within_admin_deadline(async {
            let token = room_admin_token(&self.api_key, &self.api_secret, room)?;
            let resp = twirp_post(self.http, self.endpoint(method), &token, &body).await?;
            let status = resp.status();
            if strict_removal && !status.is_success() {
                let error = twirp_json(resp, limit).await?;
                return Err(
                    match (
                        status.as_u16(),
                        error["code"].as_str(),
                        error["msg"].as_str(),
                    ) {
                        (404, Some("not_found"), Some("participant does not exist")) => {
                            RoomServiceError::RemovalParticipantAbsent
                        }
                        (404, Some("not_found"), Some("requested room does not exist")) => {
                            RoomServiceError::RemovalRoomMissing
                        }
                        (429, Some("resource_exhausted"), _) => RoomServiceError::RemovalRejected,
                        _ => RoomServiceError::Status(status.as_u16()),
                    },
                );
            }
            if status.as_u16() == 404 {
                return Err(RoomServiceError::NotFound);
            }
            if !status.is_success() {
                let bytes = bounded_admin_body(resp, limit).await?;
                let txt = String::from_utf8_lossy(&bytes);
                if txt.contains("not_found") || txt.contains("does not exist") {
                    return Err(RoomServiceError::NotFound);
                }
                return Err(RoomServiceError::Status(status.as_u16()));
            }
            twirp_json(resp, limit).await
        })
        .await
    }

    /// Complete, SID-preserving reads for occupancy only. Caller includes permit
    /// wait and this call in its two-second operation deadline.
    pub async fn occupancy_rooms(&self) -> Result<Vec<OccupancyRoom>, RoomServiceError> {
        recovery_rooms(
            &self
                .call_bounded("ListRooms", "", serde_json::json!({}), 2 * 1024 * 1024)
                .await?,
        )
    }

    pub async fn occupancy_participants(
        &self,
        room: &str,
    ) -> Result<Vec<OccupancyParticipant>, RoomServiceError> {
        recovery_participants(
            &self
                .call_bounded(
                    "ListParticipants",
                    room,
                    serde_json::json!({"room": room}),
                    2 * 1024 * 1024,
                )
                .await?,
        )
    }

    pub async fn delete_room(&self, room: &str) -> Result<(), RoomServiceError> {
        match self
            .call("DeleteRoom", room, serde_json::json!({ "room": room }))
            .await
        {
            Ok(_) | Err(RoomServiceError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn remove_participant(
        &self,
        room: &str,
        identity: &str,
    ) -> Result<(), RoomServiceError> {
        match self
            .call(
                "RemoveParticipant",
                room,
                serde_json::json!({ "room": room, "identity": identity }),
            )
            .await
        {
            Ok(_) | Err(RoomServiceError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Removes a participant and requests a token cutoff (enforced by LiveKit
    /// Cloud only). An absent participant is reported, not folded into
    /// success: the island takeover counts "had already left" apart from
    /// "removed". The boundary is seconds, as a string (proto3 JSON int64).
    pub async fn remove_participant_with_cutoff(
        &self,
        room: &str,
        identity: &str,
        revoke_tokens_minted_before: Option<i64>,
    ) -> Result<Removal, RoomServiceError> {
        let mut body = serde_json::json!({ "room": room, "identity": identity });
        if let Some(boundary) = revoke_tokens_minted_before {
            body["revokeTokenTs"] = serde_json::Value::String(boundary.to_string());
        }
        match self.call("RemoveParticipant", room, body).await {
            Ok(_) => Ok(Removal::Removed),
            Err(RoomServiceError::RemovalParticipantAbsent) => Ok(Removal::Absent),
            Err(RoomServiceError::NotFound) => Ok(Removal::Absent),
            Err(e) => Err(e),
        }
    }

    /// Create an empty room only after a definite missing-room removal rejection. Creation
    /// does not confirm revocation; callers must revalidate and issue a new removal.
    pub async fn ensure_cutoff_room(&self, room: &str) -> Result<(), RoomServiceError> {
        self.call(
            "CreateRoom",
            room,
            serde_json::json!({"name": room, "emptyTimeout": 60}),
        )
        .await?;
        Ok(())
    }

    /// Whether the identity is in the room (case-insensitive); an absent room
    /// or participant is `false`, every other failure propagates.
    pub async fn holds_participant(
        &self,
        room: &str,
        identity: &str,
    ) -> Result<bool, RoomServiceError> {
        let participants = match self.list_participants(room).await {
            Ok(participants) => participants,
            Err(RoomServiceError::NotFound) => return Ok(false),
            Err(e) => return Err(e),
        };
        let target = identity.to_lowercase();
        Ok(participants
            .iter()
            .any(|p| p.identity.to_lowercase() == target))
    }

    pub async fn get_participant(
        &self,
        room: &str,
        identity: &str,
    ) -> Result<Option<ParticipantInfo>, RoomServiceError> {
        match self
            .call(
                "GetParticipant",
                room,
                serde_json::json!({ "room": room, "identity": identity }),
            )
            .await
        {
            Ok(v) => Ok(Some(parse_participant(&v))),
            Err(RoomServiceError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn list_participants(
        &self,
        room: &str,
    ) -> Result<Vec<ParticipantInfo>, RoomServiceError> {
        let body = self
            .call(
                "ListParticipants",
                room,
                serde_json::json!({ "room": room }),
            )
            .await?;
        inventory(&body, "participants")?
            .iter()
            .map(|participant| {
                let object = participant
                    .as_object()
                    .ok_or(RoomServiceError::InvalidResponse)?;
                if !object
                    .get("identity")
                    .and_then(|identity| identity.as_str())
                    .is_some_and(|identity| !identity.is_empty())
                {
                    return Err(RoomServiceError::InvalidResponse);
                }
                for field in ["sid", "metadata"] {
                    if object.get(field).is_some_and(|value| !value.is_string()) {
                        return Err(RoomServiceError::InvalidResponse);
                    }
                }
                if object
                    .get("permission")
                    .is_some_and(|value| !value.is_object())
                {
                    return Err(RoomServiceError::InvalidResponse);
                }
                Ok(parse_participant(participant))
            })
            .collect()
    }

    /// The roster's identities; a LiveKit failure reads as an empty room (the
    /// old contract of the participants route).
    pub async fn list_participant_identities(&self, room: &str) -> Vec<String> {
        match self.list_participants(room).await {
            Ok(list) => list.into_iter().map(|p| p.identity).collect(),
            Err(e) => {
                tracing::debug!(error = %e, room, "ListParticipants failed; answering an empty roster");
                Vec::new()
            }
        }
    }

    pub async fn update_participant(
        &self,
        room: &str,
        identity: &str,
        metadata: Option<&str>,
        permission: Option<serde_json::Value>,
    ) -> Result<(), RoomServiceError> {
        let mut body = serde_json::Map::new();
        body.insert("room".into(), serde_json::json!(room));
        body.insert("identity".into(), serde_json::json!(identity));
        if let Some(m) = metadata {
            body.insert("metadata".into(), serde_json::json!(m));
        }
        if let Some(p) = permission {
            body.insert("permission".into(), p);
        }
        match self
            .call("UpdateParticipant", room, serde_json::Value::Object(body))
            .await
        {
            Ok(_) | Err(RoomServiceError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn merge_participant_metadata(
        &self,
        room: &str,
        identity: &str,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), RoomServiceError> {
        let metadata = self.merged_metadata(room, identity, patch).await?;
        self.update_participant(room, identity, Some(&metadata), None)
            .await
    }

    /// One UpdateParticipant carrying both the merged metadata and the permission.
    pub async fn merge_participant_metadata_with_permission(
        &self,
        room: &str,
        identity: &str,
        patch: serde_json::Map<String, serde_json::Value>,
        permission: serde_json::Value,
    ) -> Result<(), RoomServiceError> {
        let metadata = self.merged_metadata(room, identity, patch).await?;
        self.update_participant(room, identity, Some(&metadata), Some(permission))
            .await
    }

    async fn merged_metadata(
        &self,
        room: &str,
        identity: &str,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, RoomServiceError> {
        let existing = self
            .get_participant(room, identity)
            .await?
            .and_then(|p| p.metadata);
        let mut merged: serde_json::Map<String, serde_json::Value> = existing
            .as_deref()
            .and_then(|m| serde_json::from_str(m).ok())
            .unwrap_or_default();
        for (k, v) in patch {
            merged.insert(k, v);
        }
        Ok(serde_json::Value::Object(merged).to_string())
    }

    pub async fn list_rooms(&self) -> Result<Vec<String>, RoomServiceError> {
        let body = self.call("ListRooms", "", serde_json::json!({})).await?;
        Ok(room_inventory(&body)?
            .iter()
            .map(|room| room["name"].as_str().expect("validated name").to_owned())
            .collect())
    }

    pub async fn get_room_info(
        &self,
        room: &str,
    ) -> Result<Option<serde_json::Value>, RoomServiceError> {
        let body = self
            .call("ListRooms", room, serde_json::json!({ "names": [room] }))
            .await?;
        let rooms = room_inventory(&body)?;
        if rooms.len() > 1
            || rooms
                .first()
                .is_some_and(|found| found["name"].as_str() != Some(room))
        {
            return Err(RoomServiceError::InvalidResponse);
        }
        Ok(rooms.first().cloned())
    }

    async fn write_room_metadata(
        &self,
        room: &str,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), RoomServiceError> {
        let metadata_str = serde_json::Value::Object(metadata).to_string();
        match self
            .call(
                "UpdateRoomMetadata",
                room,
                serde_json::json!({ "room": room, "metadata": metadata_str }),
            )
            .await
        {
            Ok(_) | Err(RoomServiceError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Reconcile owned fields while preserving presenters and other metadata.
    pub async fn merge_room_metadata(
        &self,
        room: &str,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), RoomServiceError> {
        let _guard = room_metadata_write_lock().lock().await;
        let Some(info) = self.get_room_info(room).await? else {
            return Ok(());
        };
        let mut metadata = parse_room_metadata(info.get("metadata").and_then(|m| m.as_str()));
        metadata.extend(fields);
        self.write_room_metadata(room, metadata).await
    }

    /// Initialize a room only when its current incarnation matches the event.
    /// Unlike the best-effort merger, absence and malformed metadata/acknowledgements
    /// are errors: a caller must not install a successful-refresh cooldown for them.
    /// LiveKit addresses updates by name; its API offers no SID compare-and-set.
    pub async fn merge_room_metadata_for_incarnation(
        &self,
        room: &str,
        expected_sid: Option<&str>,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<String>, RoomServiceError> {
        let _guard = room_metadata_write_lock().lock().await;
        let info = self
            .get_room_info(room)
            .await?
            .ok_or(RoomServiceError::NotFound)?;
        let sid = info
            .get("sid")
            .and_then(serde_json::Value::as_str)
            .filter(|sid| !sid.is_empty() && sid.len() <= 128)
            .ok_or(RoomServiceError::InvalidResponse)?;
        if expected_sid.is_some_and(|expected| expected != sid) {
            // A delayed event for a retired incarnation cannot update its successor.
            return Ok(None);
        }
        let mut metadata = match info.get("metadata") {
            None => serde_json::Map::new(),
            Some(serde_json::Value::String(value)) if value.is_empty() => serde_json::Map::new(),
            Some(serde_json::Value::String(value)) => {
                serde_json::from_str(value).map_err(|_| RoomServiceError::InvalidResponse)?
            }
            _ => return Err(RoomServiceError::InvalidResponse),
        };
        metadata.extend(fields);
        let updated = self
            .call(
                "UpdateRoomMetadata",
                room,
                serde_json::json!({"room": room, "metadata": serde_json::Value::Object(metadata).to_string()}),
            )
            .await?;
        if updated.get("sid").and_then(serde_json::Value::as_str) != Some(sid)
            || updated.get("name").and_then(serde_json::Value::as_str) != Some(room)
        {
            return Err(RoomServiceError::InvalidResponse);
        }
        Ok(Some(sid.to_owned()))
    }

    pub async fn append_to_room_metadata_array(
        &self,
        room: &str,
        field: &str,
        value: &str,
    ) -> Result<(), RoomServiceError> {
        let _guard = room_metadata_write_lock().lock().await;
        let Some(info) = self.get_room_info(room).await? else {
            return Ok(());
        };
        let metadata = parse_room_metadata(info.get("metadata").and_then(|m| m.as_str()));
        if let Some(updated) = metadata_with_appended(metadata, field, value) {
            self.write_room_metadata(room, updated).await?;
        }
        Ok(())
    }

    pub async fn remove_from_room_metadata_array(
        &self,
        room: &str,
        field: &str,
        value: &str,
    ) -> Result<(), RoomServiceError> {
        let _guard = room_metadata_write_lock().lock().await;
        let Some(info) = self.get_room_info(room).await? else {
            return Ok(());
        };
        let metadata = parse_room_metadata(info.get("metadata").and_then(|m| m.as_str()));
        if let Some(updated) = metadata_with_removed(metadata, field, value) {
            self.write_room_metadata(room, updated).await?;
        }
        Ok(())
    }
}

pub const INGRESS_INPUT_RTMP: &str = "RTMP_INPUT";

/// An ingress as the scene-stream routes need it. `stream_key` is a
/// credential: never logged, never in a `Debug` rendering.
#[derive(Clone, Default)]
pub struct IngressInfo {
    pub ingress_id: String,
    pub url: Option<String>,
    pub stream_key: Option<String>,
}

impl std::fmt::Debug for IngressInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngressInfo")
            .field("ingress_id", &self.ingress_id)
            .field("url", &self.url)
            .field("stream_key", &self.stream_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// The twirp Ingress service (RTMP ingress for a scene's stream) over the
/// same operator-configured API host as [`RoomServiceClient`], so through
/// the shared client unguarded (`Outbound::raw`).
pub struct IngressClient<'a> {
    pub http: &'a reqwest::Client,
    pub api_url: String,
    pub api_key: String,
    pub api_secret: String,
}

impl<'a> IngressClient<'a> {
    pub fn new(http: &'a reqwest::Client, api_url: &str, api_key: &str, api_secret: &str) -> Self {
        Self {
            http,
            api_url: api_base_url(api_url),
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
        }
    }

    fn endpoint(&self, method: &str) -> String {
        format!("{}/twirp/livekit.Ingress/{method}", self.api_url)
    }

    async fn call(
        &self,
        method: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RoomServiceError> {
        within_admin_deadline(async {
            let token = ingress_admin_token(&self.api_key, &self.api_secret)?;
            let resp = twirp_post(self.http, self.endpoint(method), &token, &body).await?;
            let status = resp.status();
            if !status.is_success() {
                let bytes = admin_body(resp).await?;
                let txt = String::from_utf8_lossy(&bytes);
                if status.as_u16() == 404 || txt.contains("not_found") {
                    return Err(RoomServiceError::NotFound);
                }
                return Err(RoomServiceError::Status(status.as_u16()));
            }
            twirp_json(resp, ADMIN_RESPONSE_BYTES).await
        })
        .await
    }

    pub async fn list_ingress(&self, room: &str) -> Result<Vec<IngressInfo>, RoomServiceError> {
        let body = self
            .call("ListIngress", serde_json::json!({ "roomName": room }))
            .await?;
        Ok(body
            .get("items")
            .and_then(|i| i.as_array())
            .map(|arr| arr.iter().map(parse_ingress).collect())
            .unwrap_or_default())
    }

    /// Exact ingress state for reconciliation. Unknown states preserve the SQL flag.
    pub async fn is_ingress_streaming(
        &self,
        ingress_id: &str,
    ) -> Result<Option<bool>, RoomServiceError> {
        let body = match tokio::time::timeout(
            Duration::from_secs(5),
            self.call(
                "ListIngress",
                serde_json::json!({ "ingressId": ingress_id }),
            ),
        )
        .await
        .map_err(|_| RoomServiceError::Request)?
        {
            Ok(body) => body,
            Err(RoomServiceError::NotFound) => return Ok(Some(false)),
            Err(error) => return Err(error),
        };
        ingress_streaming_state(&body, ingress_id)
    }

    pub async fn create_ingress(
        &self,
        room: &str,
        participant_identity: &str,
    ) -> Result<IngressInfo, RoomServiceError> {
        let body = self
            .call(
                "CreateIngress",
                serde_json::json!({
                    "inputType": INGRESS_INPUT_RTMP,
                    "name": format!("{room}-ingress"),
                    "roomName": room,
                    "participantIdentity": participant_identity,
                }),
            )
            .await?;
        Ok(parse_ingress(&body))
    }

    /// The room's existing ingress, else a new RTMP one.
    pub async fn get_or_create_ingress(
        &self,
        room: &str,
        participant_identity: &str,
    ) -> Result<IngressInfo, RoomServiceError> {
        if let Some(existing) = self.list_ingress(room).await?.into_iter().next() {
            return Ok(existing);
        }
        self.create_ingress(room, participant_identity).await
    }

    /// An ingress LiveKit no longer knows is already deleted.
    pub async fn delete_ingress(&self, ingress_id: &str) -> Result<(), RoomServiceError> {
        match self
            .call(
                "DeleteIngress",
                serde_json::json!({ "ingressId": ingress_id }),
            )
            .await
        {
            Ok(_) | Err(RoomServiceError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn ingress_streaming_state(
    body: &serde_json::Value,
    ingress_id: &str,
) -> Result<Option<bool>, RoomServiceError> {
    let items = body
        .get("items")
        .and_then(|value| value.as_array())
        .ok_or(RoomServiceError::InvalidResponse)?;
    if items.is_empty() {
        return Ok(Some(false));
    }
    let status = items
        .iter()
        .find(|item| item.get("ingressId").and_then(|v| v.as_str()) == Some(ingress_id))
        .and_then(|item| item.get("state"))
        .and_then(|state| state.get("status"));
    Ok(match status {
        Some(serde_json::Value::String(value)) => match value.as_str() {
            "ENDPOINT_BUFFERING" | "ENDPOINT_PUBLISHING" => Some(true),
            "ENDPOINT_INACTIVE" | "ENDPOINT_ERROR" | "ENDPOINT_COMPLETE" => Some(false),
            _ => None,
        },
        Some(serde_json::Value::Number(value)) => match value.as_i64() {
            Some(1 | 2) => Some(true),
            Some(0 | 3 | 4) => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn parse_ingress(v: &serde_json::Value) -> IngressInfo {
    IngressInfo {
        ingress_id: v
            .get("ingressId")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        url: v.get("url").and_then(|s| s.as_str()).map(String::from),
        stream_key: v
            .get("streamKey")
            .and_then(|s| s.as_str())
            .map(String::from),
    }
}

fn parse_participant(p: &serde_json::Value) -> ParticipantInfo {
    ParticipantInfo {
        sid: p
            .get("sid")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .into(),
        identity: p
            .get("identity")
            .and_then(|i| i.as_str())
            .unwrap_or_default()
            .to_string(),
        attributes: p
            .get("attributes")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
        metadata: p.get("metadata").and_then(|m| m.as_str()).map(String::from),
        can_update_metadata: p
            .get("permission")
            .and_then(|p| p.as_object())
            .and_then(
                |p| match (p.get("canUpdateMetadata"), p.get("can_update_metadata")) {
                    (Some(a), Some(b)) if a != b => None,
                    (Some(value), _) | (_, Some(value)) => value.as_bool(),
                    (None, None) => Some(false),
                },
            ),
        joined_at: p.get("joinedAt").and_then(|t| match t {
            serde_json::Value::String(s) => s.parse().ok(),
            other => other.as_i64(),
        }),
    }
}

/// The LiveKit credentials and endpoints this process mints against.
#[derive(Clone)]
pub struct LiveKit {
    pub api_key: String,
    pub api_secret: String,
    /// The signaling URL handed to explorers (adapter URLs).
    pub client_url: String,
    /// The API base this process calls for room admin; never handed out.
    pub api_url: String,
    /// False on the dev placeholders: tokens mint, no room call is attempted.
    pub configured: bool,
}

impl std::fmt::Debug for LiveKit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveKit")
            .field("api_key", &self.api_key)
            .field("api_secret", &"<set>")
            .field("client_url", &self.client_url)
            .field("api_url", &self.api_url)
            .field("configured", &self.configured)
            .finish()
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn decode_jwt_payload(jwt: &str) -> serde_json::Value {
    let payload_b64 = jwt.split('.').nth(1).expect("jwt payload segment");
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).expect("base64url");
    serde_json::from_slice(&bytes).expect("payload json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcl_livekit_token::unix_now;

    #[test]
    fn placeholder_creds_are_detected_case_insensitively_and_count_as_unset() {
        for v in ["devkey", "devsecret", "DEVKEY", "DevSecret", " devkey "] {
            assert!(is_placeholder_cred(v), "{v:?}");
        }
        for v in ["", "APIabc", "devkey2", "supersecret", "prod-devkey"] {
            assert!(!is_placeholder_cred(v), "{v:?}");
        }
        assert_eq!(
            resolve_creds("APIabc".into(), "supersecret".into(), false),
            ResolvedCreds::Configured {
                api_key: "APIabc".into(),
                api_secret: "supersecret".into()
            }
        );
        for (k, s) in [
            ("devkey", "devsecret"),
            ("DEVKEY", "DEVSECRET"),
            ("devkey", "supersecret"),
            ("APIabc", "devsecret"),
            ("", ""),
            ("   ", "   "),
            ("APIabc", ""),
        ] {
            assert_eq!(
                resolve_creds(k.into(), s.into(), false),
                ResolvedCreds::Unconfigured,
                "({k:?}, {s:?})"
            );
            assert_eq!(
                resolve_creds(k.into(), s.into(), true),
                ResolvedCreds::DevFallback,
                "({k:?}, {s:?})"
            );
        }
    }

    #[test]
    fn adapter_url_keeps_ws_schemes_and_maps_http_to_its_websocket_twin() {
        assert_eq!(
            build_adapter_url("lk.example.com", "t"),
            "livekit:wss://lk.example.com?access_token=t"
        );
        assert_eq!(
            build_adapter_url("ws://127.0.0.1:7880", "t"),
            "livekit:ws://127.0.0.1:7880?access_token=t"
        );
        assert_eq!(
            build_adapter_url("wss://lk.example.com", "t"),
            "livekit:wss://lk.example.com?access_token=t"
        );
        assert_eq!(
            build_adapter_url("https://lk.example.com/", "t"),
            "livekit:wss://lk.example.com?access_token=t"
        );
        assert_eq!(
            build_adapter_url("http://127.0.0.1:7880", "t"),
            "livekit:ws://127.0.0.1:7880?access_token=t"
        );
    }

    #[test]
    fn api_base_url_maps_scheme_like_the_livekit_server_sdk() {
        assert_eq!(api_base_url("lk.example.com/"), "https://lk.example.com");
        assert_eq!(
            api_base_url("wss://lk.example.com"),
            "https://lk.example.com"
        );
        assert_eq!(
            api_base_url("https://lk.example.com"),
            "https://lk.example.com"
        );
        assert_eq!(api_base_url("ws://127.0.0.1:7880"), "http://127.0.0.1:7880");
        assert_eq!(
            api_base_url("http://127.0.0.1:7880"),
            "http://127.0.0.1:7880"
        );
    }

    #[test]
    fn status_url_renders_like_the_url_class() {
        assert_eq!(
            status_url("wss://dcl.livekit.cloud"),
            "https://dcl.livekit.cloud/"
        );
        assert_eq!(status_url("ws://127.0.0.1:7880"), "http://127.0.0.1:7880/");
        assert_eq!(
            status_url("wss://lk.example.com/rtc"),
            "https://lk.example.com/rtc"
        );
    }

    #[test]
    fn endpoints_derive_from_one_host_and_are_overridable_on_their_own() {
        assert_eq!(
            resolve_endpoints("livekit.local", None, None),
            LivekitEndpoints {
                client_url: "wss://livekit.local".into(),
                api_url: "https://livekit.local".into()
            }
        );
        assert_eq!(
            resolve_endpoints("ws://127.0.0.1:7880", None, None),
            LivekitEndpoints {
                client_url: "ws://127.0.0.1:7880".into(),
                api_url: "http://127.0.0.1:7880".into()
            }
        );
        let split = resolve_endpoints(
            "livekit.example.com",
            Some("http://livekit-api.internal:7880"),
            None,
        );
        assert_eq!(split.client_url, "wss://livekit.example.com");
        assert_eq!(split.api_url, "http://livekit-api.internal:7880");
        let split = resolve_endpoints(
            "livekit-api.internal:7880",
            None,
            Some("wss://livekit.example.com"),
        );
        assert_eq!(split.client_url, "wss://livekit.example.com");
        assert_eq!(split.api_url, "https://livekit-api.internal:7880");
        assert_eq!(
            resolve_endpoints("livekit.local", Some("  "), Some("")),
            resolve_endpoints("livekit.local", None, None)
        );
        assert_eq!(
            resolve_endpoints("", None, None),
            LivekitEndpoints {
                client_url: String::new(),
                api_url: String::new()
            }
        );
    }

    #[test]
    fn address_from_identity_truncates_and_validates() {
        let addr = "0x1234567890abcdef1234567890abcdef12345678";
        assert_eq!(address_from_identity(addr).as_deref(), Some(addr));
        assert_eq!(
            address_from_identity(&format!("{addr}:session")).as_deref(),
            Some(addr)
        );
        assert_eq!(address_from_identity("authoritative-server"), None);
    }

    #[test]
    fn webhook_token_round_trips_and_refuses_a_wrong_body_key_or_issuer() {
        let body = br#"{"event":"participant_left"}"#;
        let mint = |key: &str, secret: &str, digest_of: &[u8]| {
            let now = dcl_livekit_token::unix_now().unwrap();
            let payload = serde_json::json!({
                "exp": now + 300, "nbf": now, "iss": key,
                "sha256": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(digest_of)),
            });
            sign_hs256(
                secret,
                &serde_json::to_vec(&dcl_livekit_token::jwt_header()).unwrap(),
                &serde_json::to_vec(&payload).unwrap(),
            )
            .unwrap()
        };
        let good = mint("devkey", "devsecret", body);
        let deadline =
            verify_webhook_token("devkey", "devsecret", body, &format!("Bearer {good}")).unwrap();
        assert!(deadline >= unix_now().unwrap() + 359);
        assert_eq!(
            verify_webhook_token("devkey", "devsecret", body, &good),
            Some(deadline)
        );
        assert!(verify_webhook_token("devkey", "devsecret", b"tampered", &good).is_none());
        assert!(verify_webhook_token("devkey", "other", body, &good).is_none());
        assert!(verify_webhook_token("otherkey", "devsecret", body, &good).is_none());
        assert!(verify_webhook_token("devkey", "devsecret", body, "not.a.jwt.at.all").is_none());
    }

    #[tokio::test]
    async fn admin_deadline_covers_headers_and_success_or_error_body_for_both_clients() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for ingress in [false, true] {
            for mode in [0, 200, 500] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let (seen, received) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut data = Vec::new();
                    loop {
                        let mut bytes = [0; 1024];
                        let n = socket.read(&mut bytes).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&bytes[..n]);
                        if data.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    if mode != 0 {
                        socket.write_all(format!("HTTP/1.1 {mode} fixture\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{").as_bytes()).await.unwrap();
                    }
                    let _ = seen.send(());
                    std::future::pending::<()>().await;
                });
                let request = tokio::spawn(async move {
                    let http = reqwest::Client::new();
                    if ingress {
                        matches!(
                            IngressClient::new(&http, &url, "APIkey", "secret")
                                .list_ingress("room")
                                .await,
                            Err(RoomServiceError::Request)
                        )
                    } else {
                        matches!(
                            RoomServiceClient::new(&http, &url, "APIkey", "secret")
                                .list_rooms()
                                .await,
                            Err(RoomServiceError::Request)
                        )
                    }
                });
                received.await.unwrap();
                tokio::task::yield_now().await;
                tokio::time::pause();
                let start = tokio::time::Instant::now();
                tokio::time::advance(Duration::from_secs(6)).await;
                assert!(request.await.unwrap(), "ingress={ingress}, mode={mode}");
                assert!(
                    start.elapsed() <= Duration::from_secs(6),
                    "entire body must share the five-second budget"
                );
                tokio::time::resume();
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn terminal_inventory_rejects_malformed_success_and_preserves_proto_empty() {
        let http = reqwest::Client::new();
        for body in [
            "null",
            "[]",
            r#"{"rooms":null}"#,
            r#"{"rooms":false}"#,
            r#"{"rooms":[null]}"#,
            r#"{"rooms":[{}]}"#,
            r#"{"rooms":[{"name":7}]}"#,
            r#"{"rooms":[{"name":"r","sid":false}]}"#,
        ] {
            let (url, _) = capture_once("200 OK", body).await;
            assert!(
                matches!(
                    RoomServiceClient::new(&http, &url, "key", "secret")
                        .list_rooms()
                        .await,
                    Err(RoomServiceError::InvalidResponse)
                ),
                "{body}"
            );
            let (url, _) = capture_once("200 OK", body).await;
            assert!(
                matches!(
                    RoomServiceClient::new(&http, &url, "key", "secret")
                        .get_room_info("r")
                        .await,
                    Err(RoomServiceError::InvalidResponse)
                ),
                "{body}"
            );
        }
        for body in [
            "null",
            r#"{"participants":null}"#,
            r#"{"participants":false}"#,
            r#"{"participants":[null]}"#,
            r#"{"participants":[{}]}"#,
            r#"{"participants":[{"identity":2}]}"#,
            r#"{"participants":[{"identity":"wallet","sid":7}]}"#,
        ] {
            let (url, _) = capture_once("200 OK", body).await;
            assert!(
                matches!(
                    RoomServiceClient::new(&http, &url, "key", "secret")
                        .list_participants("r")
                        .await,
                    Err(RoomServiceError::InvalidResponse)
                ),
                "{body}"
            );
        }
        for body in ["{}", r#"{"rooms":[]}"#] {
            let (url, _) = capture_once("200 OK", body).await;
            assert!(
                RoomServiceClient::new(&http, &url, "key", "secret")
                    .list_rooms()
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        for body in ["{}", r#"{"participants":[]}"#] {
            let (url, _) = capture_once("200 OK", body).await;
            assert!(
                RoomServiceClient::new(&http, &url, "key", "secret")
                    .list_participants("r")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        ADMIN_RESPONSE_BYTES + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        assert!(matches!(
            RoomServiceClient::new(&http, &url, "key", "secret")
                .list_rooms()
                .await,
            Err(RoomServiceError::InvalidResponse)
        ));
        task.abort();
    }

    #[test]
    fn room_metadata_arrays_append_once_and_remove_only_present_values() {
        let m = parse_room_metadata(Some(r#"{"bannedAddresses":["0xa"]}"#));
        assert!(metadata_with_appended(m.clone(), BANNED_ADDRESSES_FIELD, "0xa").is_none());
        let added = metadata_with_appended(m.clone(), BANNED_ADDRESSES_FIELD, "0xb").unwrap();
        assert_eq!(added["bannedAddresses"], serde_json::json!(["0xa", "0xb"]));
        assert!(metadata_with_removed(m.clone(), BANNED_ADDRESSES_FIELD, "0xz").is_none());
        assert!(metadata_with_removed(m.clone(), SCENE_ADMINS_FIELD, "0xa").is_none());
        let removed = metadata_with_removed(m, BANNED_ADDRESSES_FIELD, "0xa").unwrap();
        assert_eq!(removed["bannedAddresses"], serde_json::json!([]));
        assert!(parse_room_metadata(Some("not json")).is_empty());
        assert!(parse_room_metadata(None).is_empty());
    }

    struct Captured {
        line: String,
        auth: String,
        body: serde_json::Value,
    }

    async fn capture_once(
        resp_status: &'static str,
        resp_body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<Captured>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut total = 0;
            loop {
                let n = sock.read(&mut buf[total..]).await.unwrap();
                if n == 0 {
                    break;
                }
                total += n;
                let text = String::from_utf8_lossy(&buf[..total]);
                if let Some(hdr_end) = text.find("\r\n\r\n") {
                    let content_len = text[..hdr_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if total >= hdr_end + 4 + content_len {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf[..total]).to_string();
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
            let line = head.lines().next().unwrap_or("").to_string();
            let auth = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("authorization:")
                        .map(|_| {
                            l.split_once(':')
                                .map(|x| x.1)
                                .unwrap_or("")
                                .trim()
                                .to_string()
                        })
                })
                .unwrap_or_default();
            let body_json: serde_json::Value =
                serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            let resp = format!(
                "HTTP/1.1 {resp_status}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{resp_body}",
                resp_body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            let _ = tx.send(Captured {
                line,
                auth,
                body: body_json,
            });
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn list_participant_identities_twirp_request_and_grant_shape() {
        let (host, rx) = capture_once(
            "200 OK",
            r#"{"participants":[{"identity":"0x1234567890ABCDEF1234567890abcdef12345678:wss"},{"identity":"authoritative-server"},{"identity":"0xffff567890abcdef1234567890abcdef12345678"}]}"#,
        )
        .await;
        let http = reqwest::Client::new();
        let identities = RoomServiceClient::new(&http, &host, "devkey", "devsecret")
            .list_participant_identities("world-foo.eth-bafkreiabc")
            .await;
        assert_eq!(
            identities,
            vec![
                "0x1234567890ABCDEF1234567890abcdef12345678:wss".to_string(),
                "authoritative-server".to_string(),
                "0xffff567890abcdef1234567890abcdef12345678".to_string(),
            ]
        );
        let cap = rx.await.unwrap();
        assert_eq!(
            cap.line,
            "POST /twirp/livekit.RoomService/ListParticipants HTTP/1.1"
        );
        assert_eq!(
            cap.body,
            serde_json::json!({ "room": "world-foo.eth-bafkreiabc" })
        );
        let claims = decode_jwt_payload(cap.auth.strip_prefix("Bearer ").expect("bearer prefix"));
        assert_eq!(claims["iss"], "devkey");
        assert_eq!(claims["video"]["roomAdmin"], true);
        assert_eq!(claims["video"]["room"], "world-foo.eth-bafkreiabc");
    }

    #[tokio::test]
    async fn list_participant_identities_swallows_livekit_failures() {
        let (host, _rx) = capture_once("500 Internal Server Error", "boom").await;
        let http = reqwest::Client::new();
        assert!(
            RoomServiceClient::new(&http, &host, "devkey", "devsecret")
                .list_participant_identities("scene:abc")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn delete_room_posts_twirp_deleteroom_and_treats_404_as_success() {
        let (host, rx) = capture_once("200 OK", "{}").await;
        let http = reqwest::Client::new();
        RoomServiceClient::new(&http, &host, "devkey", "devsecret")
            .delete_room("voice-chat-private-c1")
            .await
            .unwrap();
        let cap = rx.await.unwrap();
        assert_eq!(
            cap.line,
            "POST /twirp/livekit.RoomService/DeleteRoom HTTP/1.1"
        );
        assert_eq!(
            cap.body,
            serde_json::json!({ "room": "voice-chat-private-c1" })
        );
        let (host, _rx) = capture_once("404 Not Found", "not_found").await;
        RoomServiceClient::new(&http, &host, "devkey", "devsecret")
            .delete_room("gone")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn remove_and_update_participant_send_the_upstream_bodies() {
        let (host, rx) = capture_once("200 OK", "{}").await;
        let http = reqwest::Client::new();
        RoomServiceClient::new(&http, &host, "devkey", "devsecret")
            .remove_participant("voice-chat-community-c1", "0xabc")
            .await
            .unwrap();
        let cap = rx.await.unwrap();
        assert_eq!(
            cap.line,
            "POST /twirp/livekit.RoomService/RemoveParticipant HTTP/1.1"
        );
        assert_eq!(
            cap.body,
            serde_json::json!({ "room": "voice-chat-community-c1", "identity": "0xabc" })
        );

        let (host, rx) = capture_once("200 OK", "{}").await;
        RoomServiceClient::new(&http, &host, "devkey", "devsecret")
            .update_participant(
                "voice-chat-community-c1",
                "0xabc",
                None,
                Some(serde_json::json!({ "canPublish": true })),
            )
            .await
            .unwrap();
        let cap = rx.await.unwrap();
        assert_eq!(
            cap.line,
            "POST /twirp/livekit.RoomService/UpdateParticipant HTTP/1.1"
        );
        assert_eq!(cap.body["permission"]["canPublish"], true);
        assert!(cap.body.get("metadata").is_none());
    }

    #[tokio::test]
    async fn get_participant_maps_not_found_to_none_and_a_500_to_a_status_error() {
        let http = reqwest::Client::new();
        let (host, _rx) = capture_once("404 Not Found", "not_found").await;
        assert!(
            RoomServiceClient::new(&http, &host, "devkey", "devsecret")
                .get_participant("r", "0xabc")
                .await
                .unwrap()
                .is_none()
        );
        let (host, _rx) = capture_once("500 Internal Server Error", "boom").await;
        assert!(matches!(
            RoomServiceClient::new(&http, &host, "devkey", "devsecret")
                .list_participants("r")
                .await,
            Err(RoomServiceError::Status(500))
        ));
    }

    #[tokio::test]
    async fn participant_metadata_read_failure_never_writes_an_empty_base() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let count = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).contains("GetParticipant"));
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            drop(socket);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let http = reqwest::Client::new();
        assert!(
            RoomServiceClient::new(&http, &host, "key", "secret")
                .merge_participant_metadata("room", "wallet", serde_json::Map::new())
                .await
                .is_err()
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn room_metadata_refresh_preserves_presenters_and_unrelated_fields() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut updated = serde_json::Value::Null;
            for n in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0);
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            if n == 1 {
                                updated = serde_json::from_slice(&bytes[end + 4..end + 4 + length])
                                    .unwrap();
                            }
                            break;
                        }
                    }
                }
                let response = if n == 0 {
                    serde_json::json!({"rooms":[{"name":"scene:main:scene","sid":"RM_fixture","metadata":serde_json::json!({"presenters":["existing-presenter"],"other":42,"sceneAdmins":["stale"]}).to_string()}]}).to_string()
                } else {
                    "{}".to_string()
                };
                let wire = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}",
                    response.len()
                );
                socket.write_all(wire.as_bytes()).await.unwrap();
            }
            updated
        });
        let http = reqwest::Client::new();
        RoomServiceClient::new(&http, &host, "devkey", "devsecret")
            .merge_room_metadata(
                "scene:main:scene",
                serde_json::json!({"sceneAdmins":["owner","admin"],"bannedAddresses":[]})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .await
            .unwrap();
        let request = task.await.unwrap();
        let metadata: serde_json::Value =
            serde_json::from_str(request["metadata"].as_str().unwrap()).unwrap();
        assert_eq!(
            metadata["presenters"],
            serde_json::json!(["existing-presenter"])
        );
        assert_eq!(metadata["other"], 42);
        assert_eq!(
            metadata["sceneAdmins"],
            serde_json::json!(["owner", "admin"])
        );
        assert_eq!(metadata["bannedAddresses"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn exact_ingress_reconciliation_maps_absence_and_errors_conservatively() {
        let http = reqwest::Client::new();
        let (host, rx) = capture_once(
            "200 OK",
            r#"{"items":[{"ingressId":"IN_1","state":{"status":"ENDPOINT_PUBLISHING"}}]}"#,
        )
        .await;
        assert_eq!(
            IngressClient::new(&http, &host, "devkey", "devsecret")
                .is_ingress_streaming("IN_1")
                .await
                .unwrap(),
            Some(true)
        );
        let cap = rx.await.unwrap();
        assert_eq!(cap.line, "POST /twirp/livekit.Ingress/ListIngress HTTP/1.1");
        assert_eq!(cap.body, serde_json::json!({"ingressId":"IN_1"}));
        let (host, _rx) = capture_once("404 Not Found", "not_found").await;
        assert_eq!(
            IngressClient::new(&http, &host, "devkey", "devsecret")
                .is_ingress_streaming("gone")
                .await
                .unwrap(),
            Some(false)
        );
        let (host, _rx) = capture_once("500 Internal Server Error", "error").await;
        assert!(matches!(
            IngressClient::new(&http, &host, "devkey", "devsecret")
                .is_ingress_streaming("IN_1")
                .await,
            Err(RoomServiceError::Status(500))
        ));
    }

    #[tokio::test]
    async fn ingress_calls_carry_the_ingress_admin_grant_and_the_upstream_bodies() {
        let http = reqwest::Client::new();
        let (host, rx) = capture_once("200 OK", r#"{"items":[]}"#).await;
        assert!(
            IngressClient::new(&http, &host, "devkey", "devsecret")
                .list_ingress("scene:main:bafkrei1")
                .await
                .unwrap()
                .is_empty()
        );
        let cap = rx.await.unwrap();
        assert_eq!(cap.line, "POST /twirp/livekit.Ingress/ListIngress HTTP/1.1");
        assert_eq!(
            cap.body,
            serde_json::json!({ "roomName": "scene:main:bafkrei1" })
        );
        let claims = decode_jwt_payload(cap.auth.strip_prefix("Bearer ").expect("bearer prefix"));
        assert_eq!(
            claims["video"],
            serde_json::json!({ "ingressAdmin": true, "roomList": true })
        );

        let (host, rx) = capture_once(
            "200 OK",
            r#"{"ingressId":"IN_1","url":"rtmp://ingest/live","streamKey":"sk_1"}"#,
        )
        .await;
        let info = IngressClient::new(&http, &host, "devkey", "devsecret")
            .create_ingress("scene:main:bafkrei1", "u-streamer")
            .await
            .unwrap();
        assert_eq!(
            (
                info.ingress_id.as_str(),
                info.url.as_deref(),
                info.stream_key.as_deref()
            ),
            ("IN_1", Some("rtmp://ingest/live"), Some("sk_1"))
        );
        assert!(
            !format!("{info:?}").contains("sk_1"),
            "the stream key never reaches a Debug rendering"
        );
        let cap = rx.await.unwrap();
        assert_eq!(
            cap.line,
            "POST /twirp/livekit.Ingress/CreateIngress HTTP/1.1"
        );
        assert_eq!(
            cap.body,
            serde_json::json!({ "inputType": "RTMP_INPUT", "name": "scene:main:bafkrei1-ingress", "roomName": "scene:main:bafkrei1", "participantIdentity": "u-streamer" })
        );

        let (host, _rx) = capture_once("404 Not Found", "not_found").await;
        IngressClient::new(&http, &host, "devkey", "devsecret")
            .delete_ingress("IN_gone")
            .await
            .expect("an unknown ingress is already deleted");
        let (host, _rx) = capture_once("500 Internal Server Error", "boom").await;
        assert!(matches!(
            IngressClient::new(&http, &host, "devkey", "devsecret")
                .delete_ingress("IN_1")
                .await,
            Err(RoomServiceError::Status(500))
        ));
    }

    #[test]
    fn metadata_permissions_accept_both_proto_json_spellings_and_fail_closed_on_conflicts() {
        for (permission, expected) in [
            (serde_json::json!({"can_update_metadata": true}), Some(true)),
            (serde_json::json!({"canUpdateMetadata": true}), Some(true)),
            (
                serde_json::json!({"can_update_metadata": false}),
                Some(false),
            ),
            (serde_json::json!({"canUpdateMetadata": false}), Some(false)),
            (serde_json::json!({}), Some(false)),
            (serde_json::Value::Null, None),
            (serde_json::json!({"can_update_metadata": "false"}), None),
            (
                serde_json::json!({"canUpdateMetadata": false, "can_update_metadata": true}),
                None,
            ),
            (
                serde_json::json!({"canUpdateMetadata": true, "can_update_metadata": false}),
                None,
            ),
        ] {
            let participant = parse_participant(&serde_json::json!({"permission": permission}));
            assert_eq!(participant.can_update_metadata, expected);
        }
        assert_eq!(
            parse_participant(&serde_json::json!({})).can_update_metadata,
            None
        );
    }

    #[tokio::test]
    async fn a_cutoff_removal_reports_absent_apart_from_removed() {
        let http = reqwest::Client::new();
        let (host, rx) = capture_once("200 OK", "{}").await;
        assert_eq!(
            RoomServiceClient::new(&http, &host, "devkey", "devsecret")
                .remove_participant_with_cutoff("island-c1", "0xabc", Some(1_700_000_001))
                .await
                .unwrap(),
            Removal::Removed
        );
        let cap = rx.await.unwrap();
        assert_eq!(
            cap.body,
            serde_json::json!({ "room": "island-c1", "identity": "0xabc", "revokeTokenTs": "1700000001" })
        );
        let (host, _rx) = capture_once("404 Not Found", "not_found").await;
        assert_eq!(
            RoomServiceClient::new(&http, &host, "devkey", "devsecret")
                .remove_participant_with_cutoff("island-c1", "0xabc", None)
                .await
                .unwrap(),
            Removal::Absent
        );
    }

    #[tokio::test]
    async fn list_rooms_parses_names() {
        let (host, _rx) =
            capture_once("200 OK", r#"{"rooms":[{"name":"r1"},{"name":"r2"}]}"#).await;
        let http = reqwest::Client::new();
        assert_eq!(
            RoomServiceClient::new(&http, &host, "devkey", "devsecret")
                .list_rooms()
                .await
                .unwrap(),
            vec!["r1".to_string(), "r2".to_string()]
        );
    }
}

#[cfg(test)]
mod streaming_state_tests {
    use super::*;
    #[test]
    fn ingress_states_are_conservative_and_exact() {
        for (status, expected) in [
            (serde_json::json!("ENDPOINT_BUFFERING"), Some(true)),
            (serde_json::json!(2), Some(true)),
            (serde_json::json!(0), Some(false)),
            (serde_json::json!("ENDPOINT_COMPLETE"), Some(false)),
            (serde_json::json!(99), None),
        ] {
            let body =
                serde_json::json!({"items":[{"ingressId":"wanted", "state":{"status":status}}]});
            assert_eq!(ingress_streaming_state(&body, "wanted").unwrap(), expected);
            assert_eq!(ingress_streaming_state(&body, "other").unwrap(), None);
        }
        assert_eq!(
            ingress_streaming_state(&serde_json::json!({"items":[]}), "gone").unwrap(),
            Some(false)
        );
        assert!(ingress_streaming_state(&serde_json::json!({}), "gone").is_err());
    }
}
