//! `/auth-api`: the explorer and web sign-in relay. A client posts a signing
//! request, a wallet page recovers it by id and posts the outcome, and the
//! requester polls for it; a signed identity is parked for a device to
//! pick up once. Everything is process state with a short TTL (5 min
//! requests, 1 h identities), as the old service kept it.

use axum::Extension;
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::api::Shared as AppState;
use crate::auth::AuthChain;
use crate::shared::client_ip::{
    client_ip, is_internal, is_trusted_proxy, normalize_ip, single_real_ip, source_key,
};

mod identities;
mod validation;

pub use identities::{DeletionReason, IdentityRecord, IdentityStatus, sweep_expired_identities};
use identities::{create_identity, get_identity};
use validation::{validate_auth_chain, validate_request_message};

const REQUEST_TTL_SECONDS: i64 = 300;
/// An expired request still answers 410 for this long before the sweep drops it.
const REQUEST_EXPIRED_GRACE_SECONDS: i64 = 300;
const MAX_PENDING_REQUESTS: usize = 10_000;
const MAX_REQUESTS_PER_SOURCE: usize = 100;
const DEFAULT_BODY_SIZE_BYTES: usize = 16 * 1024;
const IDENTITY_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const CLIENT_HEADER: &str = "x-dcl-client";
const CORRELATION_HEADER: &str = "x-correlation-id";

fn client_tag(headers: &HeaderMap) -> &str {
    headers
        .get(CLIENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 80
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
        })
        .unwrap_or("unknown")
}

async fn record_request_telemetry(req: Request, next: Next) -> Response {
    let client = client_tag(req.headers()).to_owned();
    let correlation_id = req
        .headers()
        .get(CORRELATION_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::new_v4)
        .to_string();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let mut response = next.run(req).await;
    let status = response.status();
    let response_class = format!("{}xx", status.as_u16() / 100);
    if let Ok(value) = HeaderValue::from_str(&correlation_id) {
        response.headers_mut().insert(CORRELATION_HEADER, value);
    }
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(CORRELATION_HEADER),
    );
    tracing::info!(
        target: "catalyst::auth_api_request",
        %client,
        %correlation_id,
        %method,
        %path,
        status = status.as_u16(),
        %response_class,
        "auth-api request"
    );
    response
}

/// Prunes expired identities and aged tombstones on an interval instead of on
/// every POST: one periodic job.
pub fn spawn_identity_sweeper(state: &AppState, jobs: &crate::jobs::Jobs) {
    let state = std::sync::Arc::downgrade(state);
    jobs.spawn_periodic(
        "auth-api-identity-sweep",
        crate::jobs::PeriodicCfg::every(IDENTITY_SWEEP_INTERVAL).after_first_interval(),
        move |_ctx| {
            let state = state.clone();
            async move {
                if let Some(state) = state.upgrade() {
                    let now = Utc::now();
                    sweep_expired_identities(&state, now);
                    sweep_expired_requests(&state, now);
                }
                Ok(())
            }
        },
    );
}

pub struct AuthApiState {
    pub requests: DashMap<String, ChallengeRecord>,
    pub identities: DashMap<String, IdentityRecord>,
    pub identity_status: DashMap<String, IdentityStatus>,
    /// Held across a cap check and its insert, so concurrent creates cannot
    /// overshoot the caps.
    pub(crate) request_admission: parking_lot::Mutex<()>,
    pub(crate) identity_admission: parking_lot::Mutex<()>,
}

impl Default for AuthApiState {
    fn default() -> Self {
        Self {
            requests: DashMap::new(),
            identities: DashMap::new(),
            identity_status: DashMap::new(),
            request_admission: parking_lot::Mutex::new(()),
            identity_admission: parking_lot::Mutex::new(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChallengeRecord {
    pub request_id: String,
    pub method: String,
    pub params: Vec<Value>,
    pub sender: Option<String>,
    pub code: u32,
    pub challenge: String,
    pub created_at: DateTime<Utc>,
    pub expiration: DateTime<Utc>,
    pub requires_validation: bool,
    pub status: ChallengeStatus,
    /// The creating network's [`request_source`] key, for the pending cap.
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ChallengeStatus {
    Pending,
    Signed {
        outcome: Box<OutcomeResponseMessage>,
    },
    Fulfilled,
}

#[derive(Debug, Deserialize)]
pub struct CreateRequestBody {
    pub method: String,
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    #[serde(rename = "authChain", default)]
    pub auth_chain: Option<AuthChain>,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

#[derive(Debug, Serialize)]
pub struct CreateRequestResponse {
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub expiration: DateTime<Utc>,
    pub code: u32,
    pub challenge: String,
}

#[derive(Debug, Serialize)]
pub struct RecoverResponse {
    pub expiration: DateTime<Utc>,
    pub code: u32,
    pub method: String,
    pub params: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    pub challenge: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OutcomeError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HttpOutcomeMessage {
    pub sender: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OutcomeError>,
    #[serde(rename = "authChain", default, skip_serializing_if = "Option::is_none")]
    pub auth_chain: Option<AuthChain>,
}

#[derive(Debug, Serialize, Clone)]
pub struct OutcomeResponseMessage {
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OutcomeError>,
    #[serde(rename = "authChain", skip_serializing_if = "Option::is_none")]
    pub auth_chain: Option<AuthChain>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct EvictedBody {
    pub error: String,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ValidationStatusResponse {
    #[serde(rename = "requiresValidation")]
    pub requires_validation: bool,
}

#[derive(Debug, Serialize)]
pub struct LiveResponse {
    pub timestamp: i64,
}

/// The sign-in relay under the gateway's `/auth-api` prefix (the old
/// `rewrite ^/auth-api/?(.*)$ /auth/$1`): `/auth-api/requests`,
/// `/auth-api/v2/requests/{id}[/outcome,/validation]`, `/auth-api/identities`.
/// The old bearer-guarded `/admin/auth/*` routes are the console's
/// `explorer-api/{challenges-list,challenge-get,challenge-revoke,
/// identities-list,identity-revoke}` calls (`ops::admin::explore`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health/live", get(health_live))
        .route("/health", get(health_live))
        .route("/requests", post(create_request))
        .route(
            "/requests/{id}",
            get(get_request_outcome).post(post_request_outcome_legacy),
        )
        .route("/v2/requests/{id}", get(get_request_v2))
        .route("/v2/requests/{id}/outcome", post(post_request_outcome))
        .route(
            "/v2/requests/{id}/validation",
            post(post_request_validation).get(get_request_validation),
        )
        .route("/identities", post(create_identity))
        .route("/identities/{id}", get(get_identity))
        .layer(middleware::from_fn(cors_preflight))
        .layer(middleware::from_fn(enforce_body_cap))
        .layer(middleware::from_fn(record_request_telemetry))
        .layer(DefaultBodyLimit::max(DEFAULT_BODY_SIZE_BYTES))
}

async fn cors_preflight(req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
            .header(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "content-type, x-dcl-client, x-correlation-id",
            )
            .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORRELATION_HEADER)
            .header(header::ACCESS_CONTROL_MAX_AGE, "3600")
            .body(Body::empty())
            .expect("static preflight response");
    }
    next.run(req).await
}

async fn enforce_body_cap(req: Request, next: Next) -> Response {
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|len| len > DEFAULT_BODY_SIZE_BYTES) {
        return payload_too_large();
    }
    let res = next.run(req).await;
    if res.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return payload_too_large();
    }
    res
}

fn challenge_view(record: &ChallengeRecord) -> Value {
    let status = match &record.status {
        ChallengeStatus::Pending => "pending",
        ChallengeStatus::Signed { .. } => "signed",
        ChallengeStatus::Fulfilled => "fulfilled",
    };
    json!({
        "requestId": record.request_id,
        "method": record.method,
        "sender": record.sender,
        "code": record.code,
        "createdAt": record.created_at,
        "expiration": record.expiration,
        "requiresValidation": record.requires_validation,
        "status": status,
    })
}

/// The console's `explorer-api/challenges-list`.
pub fn admin_list_challenges(state: &AppState) -> Value {
    let items: Vec<Value> = state
        .auth_api
        .requests
        .iter()
        .map(|e| challenge_view(e.value()))
        .collect();
    json!({ "count": items.len(), "challenges": items })
}

/// The console's `explorer-api/challenge-get`.
pub fn admin_get_challenge(state: &AppState, id: &str) -> (StatusCode, Value) {
    match state.auth_api.requests.get(id) {
        Some(entry) => (StatusCode::OK, challenge_view(entry.value())),
        None => (
            StatusCode::NOT_FOUND,
            json!({ "error": "challenge_not_found", "requestId": id }),
        ),
    }
}

/// The console's `explorer-api/challenge-revoke`.
pub fn admin_revoke_challenge(state: &AppState, id: &str) -> (StatusCode, Value) {
    let removed = state.auth_api.requests.remove(id).is_some();
    let status = if removed {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    (
        status,
        json!({ "ok": removed, "requestId": id, "revoked": removed }),
    )
}

/// The console's `explorer-api/identities-list`.
pub fn admin_list_identities(state: &AppState) -> Value {
    let items: Vec<Value> = state
        .auth_api
        .identities
        .iter()
        .map(|e| {
            let r = e.value();
            json!({
                "identityId": r.identity_id,
                "ipAddress": r.ip_address,
                "isMobile": r.is_mobile,
                "createdAt": r.created_at,
                "expiration": r.expiration,
            })
        })
        .collect();
    json!({ "count": items.len(), "identities": items })
}

/// The console's `explorer-api/identity-revoke`.
pub fn admin_revoke_identity(state: &AppState, id: &str) -> (StatusCode, Value) {
    let removed = state.auth_api.identities.remove(id).is_some();
    let status = if removed {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    (
        status,
        json!({ "ok": removed, "identityId": id, "revoked": removed }),
    )
}

async fn health_live() -> impl IntoResponse {
    Json(LiveResponse {
        timestamp: Utc::now().timestamp_millis(),
    })
}

pub fn sweep_expired_requests(state: &AppState, now: DateTime<Utc>) {
    let horizon = now - Duration::seconds(REQUEST_EXPIRED_GRACE_SECONDS);
    state
        .auth_api
        .requests
        .retain(|_, record| record.expiration >= horizon);
}

/// The network a request comes from, for the pending caps, IPv6 by its /64:
/// a trusted proxy's `X-Real-IP` (the proxy itself when it sends none), or a
/// public peer. An untrusted private peer is most likely an unlisted gateway
/// whose address would put every client in one bucket, so it, like a request
/// without a socket, has no source and only the global cap applies. The
/// request chain is not bound to the request, so the caps never key on the
/// signer.
pub(super) fn request_source(
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    trusted_proxies: &[ipnet::IpNet],
) -> Option<String> {
    request_source_with(headers, peer, trusted_proxies, &UNTRUSTED_PEER_WARNED)
}

fn request_source_with(
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    trusted_proxies: &[ipnet::IpNet],
    warned: &std::sync::atomic::AtomicBool,
) -> Option<String> {
    let peer = peer?.ip().to_canonical();
    let ip = if is_trusted_proxy(peer, trusted_proxies) {
        single_real_ip(headers).unwrap_or(peer)
    } else if is_internal(peer) {
        warn_untrusted_internal_peer_once(peer, warned);
        return None;
    } else {
        peer
    };
    Some(source_key(&ip.to_string()))
}

/// Once per process, as content warns for deploys.
static UNTRUSTED_PEER_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Once per process, for any untrusted private or loopback peer: a proxy
/// missing from the setting, or clients that all arrive from private
/// addresses. `true` when this call logged.
fn warn_untrusted_internal_peer_once(
    peer: std::net::IpAddr,
    warned: &std::sync::atomic::AtomicBool,
) -> bool {
    use std::sync::atomic::Ordering;
    if warned.load(Ordering::Relaxed) || warned.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        %peer,
        "/auth-api: a private or loopback peer is not in HTTP_TRUSTED_PROXY_CIDRS, so \
         sign-ins from it get no per-network cap; list the backend proxy networks there"
    );
    true
}

/// Refuses a new request once the relay or its network holds too many
/// unexpired requests whose outcome has not been delivered.
fn request_capacity_refusal(
    state: &AppState,
    source: Option<&str>,
    now: DateTime<Utc>,
) -> Option<Response> {
    if state.auth_api.requests.len() >= MAX_PENDING_REQUESTS {
        sweep_expired_requests(state, now);
    }
    // Delivered records only answer 410; under pressure the oldest go,
    // just enough to admit one more.
    let len = state.auth_api.requests.len();
    if len >= MAX_PENDING_REQUESTS {
        let mut delivered: Vec<(DateTime<Utc>, String)> = state
            .auth_api
            .requests
            .iter()
            .filter(|e| matches!(e.value().status, ChallengeStatus::Fulfilled))
            .map(|e| (e.value().created_at, e.key().clone()))
            .collect();
        let excess = len + 1 - MAX_PENDING_REQUESTS;
        if delivered.len() > excess {
            delivered.select_nth_unstable(excess - 1);
            delivered.truncate(excess);
        }
        for (_, id) in delivered {
            state.auth_api.requests.remove(&id);
        }
    }
    let (mut live, mut from_source) = (0usize, 0usize);
    for entry in state.auth_api.requests.iter() {
        let record = entry.value();
        if record.expiration < now || matches!(record.status, ChallengeStatus::Fulfilled) {
            continue;
        }
        live += 1;
        // The caps bound requests in flight, not how many are created: a
        // sender can still free its own slot by posting and fetching an outcome.
        if source.is_some() && record.source.as_deref() == source {
            from_source += 1;
        }
    }
    if live >= MAX_PENDING_REQUESTS {
        return Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorBody {
                    error: "Too many pending requests, retry shortly".into(),
                }),
            )
                .into_response(),
        );
    }
    (from_source >= MAX_REQUESTS_PER_SOURCE).then(|| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorBody {
                error: "Too many pending requests from this network, try again later".into(),
            }),
        )
            .into_response()
    })
}

async fn create_request(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    headers: HeaderMap,
    Json(body): Json<CreateRequestBody>,
) -> Response {
    let validated = match validate_request_message(&body) {
        Ok(validated) => validated,
        Err(msg) => return bad_request(&msg),
    };
    let sender = match validate_auth_chain(validated.auth_chain) {
        Ok(verified) => verified.sender.to_lowercase(),
        Err(msg) => return bad_request(&msg),
    };
    let now = Utc::now();
    let source = request_source(
        &headers,
        peer.map(|Extension(ConnectInfo(p))| p),
        &state.settings.trusted_proxy_cidrs,
    );
    let admission = state.auth_api.request_admission.lock();
    if let Some(refusal) = request_capacity_refusal(&state, source.as_deref(), now) {
        return refusal;
    }

    let request_id = Uuid::new_v4().to_string();
    let expiration = now + Duration::seconds(REQUEST_TTL_SECONDS);
    let code = random_pairing_code();
    let challenge = build_challenge(&request_id, code, &now, &body.method, validated.params);

    let record = ChallengeRecord {
        request_id: request_id.clone(),
        method: body.method.clone(),
        params: validated.params.to_vec(),
        sender: Some(sender),
        code,
        challenge: challenge.clone(),
        created_at: now,
        expiration,
        requires_validation: false,
        status: ChallengeStatus::Pending,
        source,
    };

    state.auth_api.requests.insert(request_id.clone(), record);
    drop(admission);

    (
        StatusCode::CREATED,
        Json(CreateRequestResponse {
            request_id,
            expiration,
            code,
            challenge,
        }),
    )
        .into_response()
}

async fn get_request_v2(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let now = Utc::now();
    if let Some(reason) = evict_if_expired(&state, &id, now) {
        return reason;
    }
    let Some(entry) = state.auth_api.requests.get(&id) else {
        return not_found(&id);
    };
    let record = entry.value();
    if matches!(record.status, ChallengeStatus::Fulfilled) {
        return gone_already_fulfilled(&id);
    }
    let body = RecoverResponse {
        expiration: record.expiration,
        code: record.code,
        method: record.method.clone(),
        params: record.params.clone(),
        sender: record.sender.clone(),
        challenge: record.challenge.clone(),
    };
    (StatusCode::OK, Json(body)).into_response()
}

async fn get_request_outcome(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let now = Utc::now();
    if let Some(reason) = evict_if_expired(&state, &id, now) {
        return reason;
    }

    let outcome_opt = {
        let Some(entry) = state.auth_api.requests.get(&id) else {
            return not_found(&id);
        };
        let record = entry.value();
        match &record.status {
            ChallengeStatus::Fulfilled => return gone_already_fulfilled(&id),
            ChallengeStatus::Pending => None,
            ChallengeStatus::Signed { outcome } => Some((**outcome).clone()),
        }
    };

    match outcome_opt {
        None => StatusCode::NO_CONTENT.into_response(),
        Some(outcome) => {
            // Delivered: only the 410 remains, so the record frees its slot
            // and keeps nothing else.
            if let Some(mut entry) = state.auth_api.requests.get_mut(&id) {
                entry.status = ChallengeStatus::Fulfilled;
                entry.params = Vec::new();
                entry.challenge = String::new();
            }
            (StatusCode::OK, Json(outcome)).into_response()
        }
    }
}

async fn post_request_outcome(
    state: State<AppState>,
    path: Path<String>,
    body: Json<HttpOutcomeMessage>,
) -> Response {
    submit_outcome(state, path, body).await
}

async fn post_request_outcome_legacy(
    state: State<AppState>,
    path: Path<String>,
    body: Json<HttpOutcomeMessage>,
) -> Response {
    submit_outcome(state, path, body).await
}

async fn submit_outcome(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<HttpOutcomeMessage>,
) -> Response {
    if let Err(msg) = validate_outcome_body(&body) {
        return bad_request(&msg);
    }

    if let Some(chain) = body.auth_chain.as_ref()
        && let Err(msg) = validate_auth_chain(chain)
    {
        return bad_request(&msg);
    }

    let now = Utc::now();
    if let Some(reason) = evict_if_expired(&state, &id, now) {
        return reason;
    }

    let Some(mut entry) = state.auth_api.requests.get_mut(&id) else {
        return not_found(&id);
    };
    let record = entry.value_mut();

    match record.status {
        ChallengeStatus::Fulfilled => return gone_already_fulfilled(&id),
        ChallengeStatus::Signed { .. } => {
            return bad_request(&format!(
                "Request with id \"{}\" already has a response",
                id
            ));
        }
        ChallengeStatus::Pending => {}
    }

    let outcome = OutcomeResponseMessage {
        request_id: id.clone(),
        sender: body.sender.to_lowercase(),
        result: body.result,
        error: body.error,
        auth_chain: body.auth_chain,
    };
    record.status = ChallengeStatus::Signed {
        outcome: Box::new(outcome),
    };

    StatusCode::OK.into_response()
}

async fn post_request_validation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let now = Utc::now();
    if let Some(reason) = evict_if_expired(&state, &id, now) {
        return reason;
    }
    let Some(mut entry) = state.auth_api.requests.get_mut(&id) else {
        return not_found(&id);
    };
    let record = entry.value_mut();
    if matches!(record.status, ChallengeStatus::Fulfilled) {
        return gone_already_fulfilled(&id);
    }
    record.requires_validation = true;
    StatusCode::NO_CONTENT.into_response()
}

async fn get_request_validation(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let now = Utc::now();
    if let Some(reason) = evict_if_expired(&state, &id, now) {
        return reason;
    }
    let Some(entry) = state.auth_api.requests.get(&id) else {
        return not_found(&id);
    };
    let record = entry.value();
    if matches!(record.status, ChallengeStatus::Fulfilled) {
        return gone_already_fulfilled(&id);
    }
    (
        StatusCode::OK,
        Json(ValidationStatusResponse {
            requires_validation: record.requires_validation,
        }),
    )
        .into_response()
}

fn random_pairing_code() -> u32 {
    loop {
        let bytes = *Uuid::new_v4().as_bytes();
        for (i, byte) in bytes.into_iter().enumerate() {
            if i == 6 || i == 8 {
                continue;
            }
            if byte < 200 {
                return u32::from(byte % 100);
            }
        }
    }
}

fn build_challenge(
    request_id: &str,
    code: u32,
    now: &DateTime<Utc>,
    method: &str,
    params: &[Value],
) -> String {
    let params_blob = serde_json::to_string(params).unwrap_or_else(|_| "[]".into());
    format!(
        "Decentraland Login\nRequest: {}\nCode: {}\nMethod: {}\nParams: {}\nIssued: {}",
        request_id,
        code,
        method,
        params_blob,
        now.to_rfc3339()
    )
}

fn evict_if_expired(state: &AppState, id: &str, now: DateTime<Utc>) -> Option<Response> {
    let expired = state
        .auth_api
        .requests
        .get(id)
        .map(|e| e.value().expiration < now)
        .unwrap_or(false);
    if expired {
        state.auth_api.requests.remove(id);
        Some(
            (
                StatusCode::GONE,
                Json(ErrorBody {
                    error: format!("Request with id \"{}\" has expired", id),
                }),
            )
                .into_response(),
        )
    } else {
        None
    }
}

fn not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: format!("Request with id \"{}\" not found", id),
        }),
    )
        .into_response()
}

fn gone_already_fulfilled(id: &str) -> Response {
    (
        StatusCode::GONE,
        Json(ErrorBody {
            error: format!("Request with id \"{}\" has already been fulfilled", id),
        }),
    )
        .into_response()
}

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

fn payload_too_large() -> Response {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header(header::CONNECTION, "close")
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("Payload Too Large"))
        .expect("static payload-too-large response")
}

fn validate_outcome_body(body: &HttpOutcomeMessage) -> Result<(), String> {
    if !crate::auth::is_eth_address(&body.sender) {
        return Err("sender must be a valid address".into());
    }
    match (body.result.is_some(), body.error.is_some()) {
        (true, false) | (false, true) => Ok(()),
        (false, false) => Err("Outcome must include either a result or an error".into()),
        (true, true) => Err("Outcome must include either a result or an error, not both".into()),
    }
}

fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36
        && Uuid::parse_str(s).is_ok_and(|u| {
            u.get_version_num() == 4 && matches!(u.get_variant(), uuid::Variant::RFC4122)
        })
}

fn ips_match(stored: &str, request: &str) -> bool {
    if stored.is_empty() || request.is_empty() {
        return false;
    }
    let a = normalize_ip(stored);
    let b = normalize_ip(request);
    if a == b {
        return true;
    }
    let octets_a: Vec<&str> = a.split('.').collect();
    let octets_b: Vec<&str> = b.split('.').collect();
    if octets_a.len() == 4
        && octets_b.len() == 4
        && octets_a.iter().all(|o| o.parse::<u8>().is_ok())
        && octets_b.iter().all(|o| o.parse::<u8>().is_ok())
    {
        return octets_a[..3] == octets_b[..3];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn client_tag_accepts_versioned_known_clients_and_marks_everything_else_unknown() {
        assert_eq!(
            client_tag(&headers(&[(CLIENT_HEADER, "dcl-web/1")])),
            "dcl-web/1"
        );
        assert_eq!(
            client_tag(&headers(&[(CLIENT_HEADER, "dcl-native/0.5.0")])),
            "dcl-native/0.5.0"
        );
        assert_eq!(client_tag(&headers(&[])), "unknown");
        assert_eq!(
            client_tag(&headers(&[(CLIENT_HEADER, "spaces are invalid")])),
            "unknown"
        );
    }

    #[tokio::test]
    async fn request_telemetry_returns_the_supplied_or_generated_correlation_id() {
        use tower::ServiceExt;

        let app = Router::new()
            .route("/", get(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn(record_request_telemetry));
        let supplied = "4ef97fa3-9dcf-4630-9bc4-5f742706e87a";
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(CLIENT_HEADER, "dcl-web/1")
                    .header(CORRELATION_HEADER, supplied)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[CORRELATION_HEADER], supplied);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS],
            CORRELATION_HEADER
        );

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let generated = response.headers()[CORRELATION_HEADER].to_str().unwrap();
        assert!(Uuid::parse_str(generated).is_ok());
    }

    #[test]
    fn pairing_code_within_range() {
        for _ in 0..1000 {
            assert!(random_pairing_code() < 100);
        }
    }

    #[test]
    fn pairing_code_independent_of_challenge_timestamp() {
        let now = Utc::now();
        let stamp_code = now.timestamp_subsec_nanos() % 100;
        let codes: std::collections::HashSet<u32> =
            (0..128).map(|_| random_pairing_code()).collect();
        assert!(codes.len() > 1);
        assert!(codes.iter().any(|c| *c != stamp_code));
        let challenge = build_challenge("req-1", 42, &now, "personal_sign", &[]);
        assert!(challenge.contains("Code: 42"));
    }

    #[test]
    fn request_ttl_matches_the_upstream_default() {
        assert_eq!(REQUEST_TTL_SECONDS, 300);
    }

    fn outcome(
        sender: &str,
        result: Option<Value>,
        error: Option<OutcomeError>,
    ) -> HttpOutcomeMessage {
        HttpOutcomeMessage {
            sender: sender.into(),
            result,
            error,
            auth_chain: None,
        }
    }

    #[test]
    fn outcome_sender_must_be_an_eth_address() {
        let sig = Some(Value::from("0xdeadbeef"));
        assert!(
            validate_outcome_body(&outcome(
                "0x1234567890123456789012345678901234567890",
                sig.clone(),
                None
            ))
            .is_ok()
        );
        for bad in [
            "",
            "not-an-address",
            "0x123",
            "1234567890123456789012345678901234567890",
            "0x123456789012345678901234567890123456789z",
        ] {
            assert_eq!(
                validate_outcome_body(&outcome(bad, sig.clone(), None)).unwrap_err(),
                "sender must be a valid address"
            );
        }
    }

    #[test]
    fn uuid_accepts_hyphenated_v4_in_either_case() {
        assert!(is_valid_uuid("8c545b52-c08e-4c39-a854-46e2b5b03c48"));
        assert!(is_valid_uuid("8C545B52-C08E-4C39-A854-46E2B5B03C48"));
    }

    #[test]
    fn uuid_rejects_non_v4_versions_and_nil() {
        assert!(!is_valid_uuid("c232ab00-9414-11ec-b909-0242ac120002"));
        assert!(!is_valid_uuid("00000000-0000-0000-0000-000000000000"));
    }

    #[test]
    fn uuid_rejects_non_hyphenated_encodings() {
        assert!(!is_valid_uuid("8c545b52c08e4c39a85446e2b5b03c48"));
        assert!(!is_valid_uuid("{8c545b52-c08e-4c39-a854-46e2b5b03c48}"));
        assert!(!is_valid_uuid(
            "urn:uuid:8c545b52-c08e-4c39-a854-46e2b5b03c48"
        ));
    }

    #[test]
    fn uuid_rejects_non_rfc4122_variant_nibbles() {
        assert!(!is_valid_uuid("8c545b52-c08e-4c39-c854-46e2b5b03c48"));
        assert!(!is_valid_uuid("8c545b52-c08e-4c39-7854-46e2b5b03c48"));
    }

    #[test]
    fn outcome_requires_exactly_one_of_result_or_error() {
        let addr = "0x1234567890123456789012345678901234567890";
        let err = OutcomeError {
            code: 4001,
            message: "User rejected the request".into(),
            data: None,
        };
        assert!(validate_outcome_body(&outcome(addr, Some(Value::from("0xsig")), None)).is_ok());
        assert!(validate_outcome_body(&outcome(addr, None, Some(err.clone()))).is_ok());
        assert_eq!(
            validate_outcome_body(&outcome(addr, None, None)).unwrap_err(),
            "Outcome must include either a result or an error"
        );
        assert_eq!(
            validate_outcome_body(&outcome(addr, Some(Value::from("0xsig")), Some(err)))
                .unwrap_err(),
            "Outcome must include either a result or an error, not both"
        );
    }

    async fn router() -> Router {
        routes().with_state(crate::api::tests_support::state_without_db().await)
    }

    fn json_request(method: &str, uri: &str, body: Vec<u8>) -> Request {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, body.len())
            .header("x-real-ip", "203.0.113.9")
            .body(Body::from(body))
            .unwrap()
    }

    fn json_post(uri: &str, body: Vec<u8>) -> Request {
        json_request("POST", uri, body)
    }

    async fn assert_upstream_payload_too_large(res: Response, uri: &str) {
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
        assert_eq!(
            res.headers()
                .get(header::CONNECTION)
                .and_then(|v| v.to_str().ok()),
            Some("close"),
            "{uri}"
        );
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"Payload Too Large", "{uri}");
    }

    fn padded_request_body(padding: usize) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "method": "eth_sendTransaction",
            "params": [format!("0x{}", "ab".repeat(padding))],
        }))
        .unwrap()
    }

    fn delegated(ephemeral: &str) -> AuthChain {
        use alloy_signer::SignerSync;
        let signer = crate::shared::signed_fetch::test_support::wallet();
        let message = format!(
            "Decentraland Login\nEphemeral address: {ephemeral}\nExpiration: 2100-01-01T00:00:00.000Z"
        );
        let signature = format!(
            "0x{}",
            hex::encode(
                signer
                    .sign_message_sync(message.as_bytes())
                    .unwrap()
                    .as_bytes()
            )
        );
        vec![
            crate::auth::AuthLink {
                link_type: crate::auth::AuthLinkType::SIGNER,
                payload: crate::shared::signed_fetch::test_support::address(),
                signature: None,
            },
            crate::auth::AuthLink {
                link_type: crate::auth::AuthLinkType::EcdsaEphemeral,
                payload: message,
                signature: Some(signature),
            },
        ]
    }

    async fn call(app: &Router, req: Request) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn get_req(uri: &str) -> Request {
        Request::builder()
            .uri(uri)
            .header("x-real-ip", "203.0.113.9")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_sign_in_request_is_recovered_answered_and_fulfilled_once() {
        let app = Router::new().nest("/auth-api", router().await);
        let ephemeral = alloy_signer_local::PrivateKeySigner::random();
        let chain = delegated(&format!("{:#x}", ephemeral.address()));
        let body = serde_json::to_vec(
            &json!({ "method": "personal_sign", "params": ["hello"], "authChain": chain }),
        )
        .unwrap();
        let (status, created) = call(&app, json_post("/auth-api/requests", body)).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let id = created["requestId"].as_str().unwrap().to_string();
        assert!(
            created["challenge"]
                .as_str()
                .unwrap()
                .contains(&format!("Request: {id}"))
        );

        let (status, recovered) = call(&app, get_req(&format!("/auth-api/v2/requests/{id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(recovered["method"], "personal_sign");
        assert_eq!(
            recovered["sender"],
            crate::shared::signed_fetch::test_support::address()
        );
        let (status, _) = call(&app, get_req(&format!("/auth-api/requests/{id}"))).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "pending");

        let (status, _) = call(
            &app,
            json_post(
                &format!("/auth-api/v2/requests/{id}/validation"),
                Vec::new(),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, v) = call(
            &app,
            get_req(&format!("/auth-api/v2/requests/{id}/validation")),
        )
        .await;
        assert_eq!(v["requiresValidation"], true);

        let outcome = serde_json::to_vec(
            &json!({ "sender": "0x1234567890123456789012345678901234567890", "result": "0xsig" }),
        )
        .unwrap();
        let (status, _) = call(
            &app,
            json_post(
                &format!("/auth-api/v2/requests/{id}/outcome"),
                outcome.clone(),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, again) = call(
            &app,
            json_post(&format!("/auth-api/v2/requests/{id}/outcome"), outcome),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{again}");
        let (status, answered) = call(&app, get_req(&format!("/auth-api/requests/{id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answered["result"], "0xsig");
        assert_eq!(answered["requestId"], id);
        let (status, gone) = call(&app, get_req(&format!("/auth-api/requests/{id}"))).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(
            gone["error"],
            format!("Request with id \"{id}\" has already been fulfilled")
        );
        let (status, _) = call(
            &app,
            get_req("/auth-api/requests/8c545b52-c08e-4c39-a854-46e2b5b03c48"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_identity_is_parked_by_its_owner_and_picked_up_once_from_the_same_network() {
        let state = crate::api::tests_support::state_without_db().await;
        let app = Router::new().nest("/auth-api", routes().with_state(state.clone()));
        let ephemeral = alloy_signer_local::PrivateKeySigner::random();
        let eph_address = format!("{:#x}", ephemeral.address());
        let identity = json!({
            "ephemeralIdentity": { "address": eph_address, "privateKey": format!("0x{}", hex::encode(ephemeral.to_bytes())), "publicKey": "0x04" },
            "expiration": "2100-01-01T00:00:00.000Z",
            "authChain": delegated(&eph_address),
        });
        let body = serde_json::to_vec(&json!({ "identity": identity })).unwrap();
        let unsigned = call(&app, json_post("/auth-api/identities", body.clone())).await;
        assert_eq!(unsigned.0, StatusCode::UNAUTHORIZED, "{}", unsigned.1);

        let headers = crate::shared::signed_fetch::test_support::signed_headers(
            "post",
            "/auth-api/identities",
            "{}",
            true,
        )
        .await;
        let mut req = json_post("/auth-api/identities", body);
        req.headers_mut().extend(headers);
        let (status, created) = call(&app, req).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let id = created["identityId"].as_str().unwrap().to_string();
        assert_eq!(admin_list_identities(&state)["count"], 1);

        let other_network = Request::builder()
            .uri(format!("/auth-api/identities/{id}"))
            .header("x-real-ip", "198.51.100.7")
            .body(Body::empty())
            .unwrap();
        let (status, err) = call(&app, other_network).await;
        assert_eq!(
            (status, err["error"].as_str()),
            (StatusCode::FORBIDDEN, Some("IP address mismatch"))
        );
        let (status, err) = call(&app, get_req(&format!("/auth-api/identities/{id}"))).await;
        assert_eq!(
            (status, err["error"].as_str()),
            (
                StatusCode::NOT_FOUND,
                Some("Identity was deleted due to IP mismatch")
            )
        );
    }

    fn from_peer(mut req: Request, peer: &str) -> Request {
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
        req
    }

    fn record(id: &str, source: &str, expiration: DateTime<Utc>) -> ChallengeRecord {
        ChallengeRecord {
            request_id: id.into(),
            method: "personal_sign".into(),
            params: Vec::new(),
            sender: Some("0x1234567890123456789012345678901234567890".into()),
            code: 1,
            challenge: String::new(),
            created_at: Utc::now(),
            expiration,
            requires_validation: false,
            status: ChallengeStatus::Pending,
            source: Some(source.into()),
        }
    }

    #[tokio::test]
    async fn a_network_is_capped_on_pending_requests_and_a_replayed_chain_locks_no_one_out() {
        let state = crate::api::tests_support::state_without_db().await;
        let app = Router::new().nest("/auth-api", routes().with_state(state.clone()));
        let ephemeral = alloy_signer_local::PrivateKeySigner::random();
        let body = serde_json::to_vec(&json!({ "method": "personal_sign", "params": ["hi"], "authChain": delegated(&format!("{:#x}", ephemeral.address())) })).unwrap();
        let post = |peer: &str| from_peer(json_post("/auth-api/requests", body.clone()), peer);
        for _ in 0..MAX_REQUESTS_PER_SOURCE {
            let (status, created) = call(&app, post("203.0.113.5:4000")).await;
            assert_eq!(status, StatusCode::CREATED, "{created}");
        }
        let (status, _) = call(&app, post("203.0.113.5:4001")).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let (status, _) = call(&app, post("198.51.100.7:4000")).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "the same chain from another network is not refused"
        );
        let (status, _) = call(&app, json_post("/auth-api/requests", body.clone())).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "a request without a source has no per-network cap"
        );
        for _ in 0..MAX_REQUESTS_PER_SOURCE {
            let (status, _) = call(&app, post("10.0.0.9:4000")).await;
            assert_eq!(
                status,
                StatusCode::CREATED,
                "an unlisted private gateway does not put every client in one bucket"
            );
        }

        let ids: Vec<String> = state
            .auth_api
            .requests
            .iter()
            .filter(|e| e.value().source.as_deref() == Some("203.0.113.5"))
            .map(|e| e.key().clone())
            .collect();
        let outcome = serde_json::to_vec(
            &json!({ "sender": "0x1234567890123456789012345678901234567890", "error": { "code": 1, "message": "no" } }),
        )
        .unwrap();
        for id in &ids {
            let (status, _) = call(
                &app,
                json_post(
                    &format!("/auth-api/v2/requests/{id}/outcome"),
                    outcome.clone(),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, _) = call(&app, post("203.0.113.5:4002")).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "an answer posted to the request does not free its slot"
        );
        for id in &ids {
            let (status, _) = call(&app, get_req(&format!("/auth-api/requests/{id}"))).await;
            assert_eq!(status, StatusCode::OK, "the requester picks up its outcome");
        }
        let (status, _) = call(&app, post("203.0.113.5:4003")).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "a delivered outcome frees the slot"
        );
        let (status, _) = call(&app, get_req(&format!("/auth-api/requests/{}", ids[0]))).await;
        assert_eq!(
            status,
            StatusCode::GONE,
            "a delivered request still answers 410"
        );
    }

    #[test]
    fn an_untrusted_private_peer_without_x_real_ip_warns() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let peer = |s: &str| Some(s.parse::<std::net::SocketAddr>().unwrap());
        let warned = AtomicBool::new(false);
        let public = request_source_with(&HeaderMap::new(), peer("203.0.113.5:1"), &[], &warned);
        assert_eq!(public.as_deref(), Some("203.0.113.5"));
        assert!(
            !warned.load(Ordering::Relaxed),
            "a public peer is not warned about"
        );
        let private = request_source_with(&HeaderMap::new(), peer("172.17.0.1:1"), &[], &warned);
        assert_eq!(private, None);
        assert!(warned.load(Ordering::Relaxed));
    }

    #[test]
    fn the_untrusted_peer_warning_is_logged_once() {
        let warned = std::sync::atomic::AtomicBool::new(false);
        let peer: std::net::IpAddr = "10.0.0.5".parse().unwrap();
        assert!(warn_untrusted_internal_peer_once(peer, &warned));
        assert!(!warn_untrusted_internal_peer_once(peer, &warned));
    }

    #[test]
    fn request_sources_group_ipv6_by_64_and_trust_only_listed_proxies() {
        let none = HeaderMap::new();
        let peer = |s: &str| Some(s.parse::<std::net::SocketAddr>().unwrap());
        assert_eq!(
            request_source(&none, peer("[2001:db8:1:2::9]:1"), &[]),
            request_source(&none, peer("[2001:db8:1:2:abcd::1]:1"), &[])
        );
        assert_ne!(
            request_source(&none, peer("[2001:db8:1:2::9]:1"), &[]),
            request_source(&none, peer("[2001:db8:1:3::9]:1"), &[])
        );
        let spoofed = headers(&[("x-real-ip", "198.51.100.7")]);
        assert_eq!(
            request_source(&spoofed, peer("203.0.113.5:1"), &[]).as_deref(),
            Some("203.0.113.5"),
            "a public untrusted peer's header is ignored"
        );
        let proxies = ["10.0.0.0/8".parse().unwrap()];
        assert_eq!(
            request_source(&spoofed, peer("10.0.0.5:1"), &proxies).as_deref(),
            Some("198.51.100.7")
        );
        assert_eq!(
            request_source(&none, peer("10.0.0.5:1"), &proxies).as_deref(),
            Some("10.0.0.5"),
            "a trusted proxy without X-Real-IP is its own source"
        );
        assert_eq!(
            request_source(&spoofed, peer("10.0.0.5:1"), &[]),
            None,
            "an unlisted private gateway gives no source"
        );
        assert_eq!(request_source(&spoofed, None, &proxies), None);
    }

    #[tokio::test]
    async fn the_global_cap_counts_only_unexpired_requests() {
        let state = crate::api::tests_support::state_without_db().await;
        let now = Utc::now();
        let in_grace = now - Duration::seconds(1);
        for i in 0..MAX_PENDING_REQUESTS {
            let id = format!("r{i}");
            state
                .auth_api
                .requests
                .insert(id.clone(), record(&id, &format!("s{i}"), in_grace));
        }
        assert!(
            request_capacity_refusal(&state, Some("new"), now).is_none(),
            "expired requests in their grace window hold no slot"
        );
        let live = now + Duration::seconds(REQUEST_TTL_SECONDS);
        for mut entry in state.auth_api.requests.iter_mut() {
            entry.expiration = live;
        }
        assert_eq!(
            request_capacity_refusal(&state, None, now).map(|r| r.status()),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_creates_never_overshoot_a_network_cap() {
        let state = crate::api::tests_support::state_without_db().await;
        let app = Router::new().nest("/auth-api", routes().with_state(state.clone()));
        let now = Utc::now();
        for i in 0..MAX_REQUESTS_PER_SOURCE - 3 {
            let id = format!("r{i}");
            state.auth_api.requests.insert(
                id.clone(),
                record(
                    &id,
                    "203.0.113.5",
                    now + Duration::seconds(REQUEST_TTL_SECONDS),
                ),
            );
        }
        let ephemeral = alloy_signer_local::PrivateKeySigner::random();
        let body = serde_json::to_vec(&json!({ "method": "personal_sign", "params": ["hi"], "authChain": delegated(&format!("{:#x}", ephemeral.address())) })).unwrap();
        let calls: Vec<_> = (0..32)
            .map(|i| {
                let app = app.clone();
                let req = from_peer(
                    json_post("/auth-api/requests", body.clone()),
                    &format!("203.0.113.5:{}", 5000 + i),
                );
                tokio::spawn(async move { call(&app, req).await.0 })
            })
            .collect();
        let mut created = 0;
        for call in calls {
            if call.await.unwrap() == StatusCode::CREATED {
                created += 1;
            }
        }
        assert_eq!(created, 3, "only the free slots are taken");
        let held = state
            .auth_api
            .requests
            .iter()
            .filter(|e| e.value().source.as_deref() == Some("203.0.113.5"))
            .count();
        assert_eq!(held, MAX_REQUESTS_PER_SOURCE);
    }

    #[tokio::test]
    async fn delivered_requests_go_first_when_the_relay_is_full() {
        let state = crate::api::tests_support::state_without_db().await;
        let now = Utc::now();
        let live = now + Duration::seconds(REQUEST_TTL_SECONDS);
        for i in 0..MAX_PENDING_REQUESTS {
            let id = format!("r{i}");
            let mut r = record(&id, &format!("s{i}"), live);
            r.status = ChallengeStatus::Fulfilled;
            r.created_at = now - Duration::seconds((MAX_PENDING_REQUESTS - i) as i64);
            state.auth_api.requests.insert(id, r);
        }
        assert!(request_capacity_refusal(&state, Some("new"), now).is_none());
        assert_eq!(
            state.auth_api.requests.len(),
            MAX_PENDING_REQUESTS - 1,
            "only enough delivered records go to admit one more"
        );
        assert!(
            !state.auth_api.requests.contains_key("r0"),
            "the oldest delivered record goes first"
        );
        assert!(state.auth_api.requests.contains_key("r1"));
    }

    #[tokio::test]
    async fn the_sweep_keeps_requests_through_their_grace_window() {
        let state = crate::api::tests_support::state_without_db().await;
        let now = Utc::now();
        state.auth_api.requests.insert(
            "old".into(),
            record(
                "old",
                "",
                now - Duration::seconds(REQUEST_EXPIRED_GRACE_SECONDS + 1),
            ),
        );
        state.auth_api.requests.insert(
            "grace".into(),
            record("grace", "", now - Duration::seconds(1)),
        );
        state.auth_api.requests.insert(
            "live".into(),
            record("live", "", now + Duration::seconds(60)),
        );
        sweep_expired_requests(&state, now);
        let mut left: Vec<String> = state
            .auth_api
            .requests
            .iter()
            .map(|e| e.key().clone())
            .collect();
        left.sort();
        assert_eq!(left, ["grace", "live"]);
    }

    #[tokio::test]
    async fn a_network_is_capped_on_pending_identities_but_a_signer_is_not() {
        let state = crate::api::tests_support::state_without_db().await;
        let app = Router::new().nest("/auth-api", routes().with_state(state.clone()));
        let post = || async {
            let ephemeral = alloy_signer_local::PrivateKeySigner::random();
            let eph_address = format!("{:#x}", ephemeral.address());
            let identity = json!({
                "ephemeralIdentity": { "address": eph_address, "privateKey": format!("0x{}", hex::encode(ephemeral.to_bytes())), "publicKey": "0x04" },
                "expiration": "2100-01-01T00:00:00.000Z",
                "authChain": delegated(&eph_address),
            });
            let body = serde_json::to_vec(&json!({ "identity": identity })).unwrap();
            let headers = crate::shared::signed_fetch::test_support::signed_headers(
                "post",
                "/auth-api/identities",
                "{}",
                true,
            )
            .await;
            let mut req = json_post("/auth-api/identities", body);
            req.headers_mut().extend(headers);
            req
        };
        for _ in 0..8 {
            let (status, body) = call(&app, from_peer(post().await, "198.51.100.7:1")).await;
            assert_eq!(
                status,
                StatusCode::CREATED,
                "one signer is not capped: {body}"
            );
        }
        let now = Utc::now();
        for i in 0..identities::MAX_IDENTITIES_PER_SOURCE {
            state.auth_api.identities.insert(
                format!("i{i}"),
                IdentityRecord {
                    identity_id: format!("i{i}"),
                    identity: Value::Null,
                    ip_address: String::new(),
                    source: Some("203.0.113.5".into()),
                    is_mobile: false,
                    created_at: now,
                    expiration: now + Duration::seconds(3600),
                },
            );
        }
        let (status, _) = call(&app, from_peer(post().await, "203.0.113.5:1")).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let (status, _) = call(&app, from_peer(post().await, "198.51.100.7:2")).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn the_console_calls_list_and_revoke_pending_requests() {
        let state = crate::api::tests_support::state_without_db().await;
        let app = Router::new().nest("/auth-api", routes().with_state(state.clone()));
        let ephemeral = alloy_signer_local::PrivateKeySigner::random();
        let body = serde_json::to_vec(&json!({ "method": "eth_signTypedData_v4", "params": [], "authChain": delegated(&format!("{:#x}", ephemeral.address())) })).unwrap();
        let (_, created) = call(&app, json_post("/auth-api/requests", body)).await;
        let id = created["requestId"].as_str().unwrap().to_string();
        assert_eq!(admin_list_challenges(&state)["count"], 1);
        assert_eq!(admin_get_challenge(&state, &id).1["status"], "pending");
        assert_eq!(admin_revoke_challenge(&state, &id).0, StatusCode::OK);
        assert_eq!(admin_revoke_challenge(&state, &id).0, StatusCode::NOT_FOUND);
        let (status, _) = call(&app, get_req(&format!("/auth-api/v2/requests/{id}"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn body_cap_matches_the_upstream_shared_route_cap() {
        assert_eq!(DEFAULT_BODY_SIZE_BYTES, 16 * 1024);
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused_on_every_posting_route() {
        use tower::ServiceExt;

        let app = router().await;
        let oversized = padded_request_body(DEFAULT_BODY_SIZE_BYTES);
        assert!(oversized.len() > DEFAULT_BODY_SIZE_BYTES);

        for uri in [
            "/requests",
            "/requests/8c545b52-c08e-4c39-a854-46e2b5b03c48",
            "/v2/requests/8c545b52-c08e-4c39-a854-46e2b5b03c48/outcome",
            "/v2/requests/8c545b52-c08e-4c39-a854-46e2b5b03c48/validation",
            "/identities",
        ] {
            let res = app
                .clone()
                .oneshot(json_post(uri, oversized.clone()))
                .await
                .unwrap();
            assert_upstream_payload_too_large(res, uri).await;
        }
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_on_a_reading_free_get_route() {
        use tower::ServiceExt;

        let app = router().await;
        let oversized = padded_request_body(DEFAULT_BODY_SIZE_BYTES);
        let uri = "/requests/8c545b52-c08e-4c39-a854-46e2b5b03c48";

        let res = app
            .oneshot(json_request("GET", uri, oversized))
            .await
            .unwrap();
        assert_upstream_payload_too_large(res, uri).await;
    }

    #[tokio::test]
    async fn an_undeclared_oversized_body_is_refused_with_the_same_envelope() {
        use tower::ServiceExt;

        let app = router().await;
        let uri = "/requests";
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-real-ip", "203.0.113.9")
            .body(Body::from(padded_request_body(DEFAULT_BODY_SIZE_BYTES)))
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_upstream_payload_too_large(res, uri).await;
    }

    #[tokio::test]
    async fn a_body_of_exactly_the_cap_is_not_refused() {
        use tower::ServiceExt;

        let app = router().await;
        let at_cap = vec![b' '; DEFAULT_BODY_SIZE_BYTES];

        let res = app.oneshot(json_post("/requests", at_cap)).await.unwrap();
        assert_ne!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn bodies_under_the_cap_reach_the_handler() {
        use tower::ServiceExt;

        let app = router().await;
        let accepted = padded_request_body(1024);
        assert!(accepted.len() < DEFAULT_BODY_SIZE_BYTES);

        let res = app.oneshot(json_post("/requests", accepted)).await.unwrap();
        assert_ne!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
