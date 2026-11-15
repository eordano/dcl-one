use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::AuthChain;
use crate::shared::signed_fetch::{self, PayloadPolicy, Signer};

use super::validation::validate_identity_chain;
use super::{
    ErrorBody, EvictedBody, bad_request, client_ip, ips_match, is_valid_uuid, normalize_ip,
};
use crate::api::Shared as AppState;

const IDENTITY_TTL_SECONDS: i64 = 3600;
const IDENTITY_STATUS_TTL_SECONDS: i64 = 14 * 24 * 3600;
const MAX_PENDING_IDENTITIES: usize = 10_000;
pub(super) const MAX_IDENTITIES_PER_SOURCE: usize = 100;
const MAX_IDENTITY_TOMBSTONES: usize = 50_000;
const IDENTITY_SIGNED_PATH: &str = "/identities";

#[derive(Debug, Clone)]
pub struct IdentityStatus {
    pub expiration: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub consumed: bool,
    pub signer: String,
    pub deletion_reason: Option<DeletionReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionReason {
    Consumed,
    Expired,
    IpMismatch,
}

#[derive(Debug, Clone)]
pub struct IdentityRecord {
    pub identity_id: String,
    pub identity: Value,
    pub ip_address: String,
    /// The creating network's `request_source` key, for the pending cap.
    pub source: Option<String>,
    pub is_mobile: bool,
    pub created_at: DateTime<Utc>,
    pub expiration: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct IdentityRequestBody {
    pub identity: Value,
    #[serde(rename = "isMobile", default)]
    pub is_mobile: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct StoredIdentity {
    #[serde(rename = "ephemeralIdentity")]
    ephemeral_identity: EphemeralIdentity,
    #[serde(rename = "authChain")]
    auth_chain: AuthChain,
}

#[derive(Debug, Deserialize)]
struct EphemeralIdentity {
    address: String,
    #[serde(rename = "privateKey")]
    private_key: String,
}

/// The stored identity's fields the handler never reads still have to be well-formed: an
/// AuthIdentity without a parseable `expiration` or an `ephemeralIdentity.publicKey` is refused.
fn identity_shape_error(identity: &Value) -> Option<String> {
    match identity.get("expiration") {
        None => return Some("missing field `expiration`".into()),
        Some(value) => {
            if let Err(err) = serde_json::from_value::<DateTime<Utc>>(value.clone()) {
                return Some(format!("expiration: {err}"));
            }
        }
    }
    match identity["ephemeralIdentity"].get("publicKey") {
        None => Some("missing field `publicKey`".into()),
        Some(value) if !value.is_string() => Some("publicKey: expected a string".into()),
        Some(_) => None,
    }
}

#[derive(Debug, Serialize)]
pub struct IdentityResponse {
    #[serde(rename = "identityId")]
    pub identity_id: String,
    pub expiration: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct IdentityIdValidationResponse {
    pub identity: Value,
}

pub(super) async fn create_identity(
    State(state): State<AppState>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    Json(body): Json<IdentityRequestBody>,
) -> Response {
    let request_signer = match verify_identity_post(&state, &headers, uri.path()).await {
        Ok(signer) => signer,
        Err(refusal) => return *refusal,
    };
    if body.identity.is_null() {
        return bad_request("AuthIdentity is required in request body");
    }
    let parsed: StoredIdentity = match serde_json::from_value(body.identity.clone()) {
        Ok(parsed) => parsed,
        Err(err) => return bad_request(&format!("Invalid AuthIdentity: {err}")),
    };
    if let Some(err) = identity_shape_error(&body.identity) {
        return bad_request(&format!("Invalid AuthIdentity: {err}"));
    }
    let (owner, final_authority) = match validate_identity_chain(&parsed.auth_chain) {
        Ok(authorities) => authorities,
        Err(msg) => return bad_request(&msg),
    };
    if !parsed
        .ephemeral_identity
        .address
        .eq_ignore_ascii_case(&final_authority)
    {
        return forbidden("Ephemeral wallet address does not match auth chain final authority");
    }
    if !request_signer.as_str().eq_ignore_ascii_case(&owner) {
        return forbidden("Request sender does not match identity owner");
    }
    let derives_address = parsed
        .ephemeral_identity
        .private_key
        .trim()
        .parse::<alloy_signer_local::PrivateKeySigner>()
        .is_ok_and(|wallet| {
            format!("{:#x}", wallet.address())
                .eq_ignore_ascii_case(&parsed.ephemeral_identity.address)
        });
    if !derives_address {
        return forbidden("Ephemeral private key does not match the provided address");
    }

    let signer = owner.to_lowercase();
    let identity_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    if state.auth_api.identities.len() >= MAX_PENDING_IDENTITIES {
        sweep_expired_identities(&state, now);
    }
    let source = super::request_source(
        &headers,
        peer.map(|axum::Extension(axum::extract::ConnectInfo(p))| p),
        &state.settings.trusted_proxy_cidrs,
    );
    let admission = state.auth_api.identity_admission.lock();
    let (mut live, mut from_source) = (0usize, 0usize);
    for entry in state.auth_api.identities.iter() {
        let record = entry.value();
        if record.expiration < now {
            continue;
        }
        live += 1;
        if source.is_some() && record.source == source {
            from_source += 1;
        }
    }
    if live >= MAX_PENDING_IDENTITIES {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody {
                error: "Too many pending identities, retry shortly".into(),
            }),
        )
            .into_response();
    }
    if from_source >= MAX_IDENTITIES_PER_SOURCE {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorBody {
                error: "Too many pending sign-ins from this network, try again later".into(),
            }),
        )
            .into_response();
    }
    let expiration = now + Duration::seconds(IDENTITY_TTL_SECONDS);
    let record = IdentityRecord {
        identity_id: identity_id.clone(),
        identity: body.identity,
        ip_address: client_ip(&headers),
        source,
        is_mobile: body.is_mobile.unwrap_or(false),
        created_at: now,
        expiration,
    };
    state
        .auth_api
        .identities
        .insert(identity_id.clone(), record);
    drop(admission);
    if state.auth_api.identity_status.len() < MAX_IDENTITY_TOMBSTONES {
        state.auth_api.identity_status.insert(
            identity_id.clone(),
            IdentityStatus {
                expiration,
                created_at: now,
                consumed: false,
                signer,
                deletion_reason: None,
            },
        );
    }
    (
        StatusCode::CREATED,
        Json(IdentityResponse {
            identity_id,
            expiration,
        }),
    )
        .into_response()
}

/// The signed POST of the identity's owner over `/identities` (the gateway
/// path `/auth-api/identities` counts: the old gateway sent it as
/// `x-original-path`, which this handler sets from the request's own path when
/// the client did not); a chain a scene signed on a player's behalf is refused.
async fn verify_identity_post(
    state: &AppState,
    headers: &HeaderMap,
    public_path: &str,
) -> Result<Signer, Box<Response>> {
    let mut headers = headers.clone();
    if !headers.contains_key(signed_fetch::ORIGINAL_PATH_HEADER)
        && let Ok(v) = public_path.parse()
    {
        headers.insert(signed_fetch::ORIGINAL_PATH_HEADER, v);
    }
    signed_fetch::require_signer(
        &headers,
        "post",
        IDENTITY_SIGNED_PATH,
        PayloadPolicy::V6Only,
        state.eip1654.get().map(|v| &**v),
    )
    .await
    .map_err(|err| {
        let (_, message) = err.http_status_and_message();
        Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": message,
                    "message": "This endpoint requires a signed fetch request. See ADR-44.",
                })),
            )
                .into_response(),
        )
    })
}

pub fn sweep_expired_identities(state: &AppState, now: DateTime<Utc>) {
    state
        .auth_api
        .identities
        .retain(|_, record| record.expiration >= now);
    let horizon = now - Duration::seconds(IDENTITY_STATUS_TTL_SECONDS);
    state
        .auth_api
        .identity_status
        .retain(|_, status| status.created_at >= horizon);
}

pub(super) async fn get_identity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !is_valid_uuid(&id) {
        return bad_request("Invalid identity format");
    }
    let now = Utc::now();

    let Some((_, record)) = state.auth_api.identities.remove(&id) else {
        return identity_status_response(&state, &id);
    };

    let signer = identity_owner(&record.identity).unwrap_or_default();

    if record.expiration < now {
        update_identity_status(
            &state,
            &id,
            false,
            Some(DeletionReason::Expired),
            &signer,
            now,
        );
        return (
            StatusCode::GONE,
            Json(ErrorBody {
                error: "Identity has expired".into(),
            }),
        )
            .into_response();
    }

    let request_ip = client_ip(&headers);
    if !record.is_mobile
        && !record.ip_address.is_empty()
        && !request_ip.is_empty()
        && !ips_match(&record.ip_address, &request_ip)
    {
        update_identity_status(
            &state,
            &id,
            false,
            Some(DeletionReason::IpMismatch),
            &signer,
            now,
        );
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody {
                error: "IP address mismatch".into(),
            }),
        )
            .into_response();
    }
    if !record.is_mobile
        && !record.ip_address.is_empty()
        && !request_ip.is_empty()
        && normalize_ip(&record.ip_address) != normalize_ip(&request_ip)
    {
        tracing::warn!(
            stored = %record.ip_address,
            request = %request_ip,
            %signer,
            "identity IP matched only by /24 prefix"
        );
    }

    update_identity_status(
        &state,
        &id,
        true,
        Some(DeletionReason::Consumed),
        &signer,
        now,
    );
    (
        StatusCode::OK,
        Json(IdentityIdValidationResponse {
            identity: record.identity,
        }),
    )
        .into_response()
}

fn identity_status_response(state: &AppState, id: &str) -> Response {
    let Some(status) = state
        .auth_api
        .identity_status
        .get(id)
        .map(|e| e.value().clone())
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: "Identity not found".into(),
            }),
        )
            .into_response();
    };
    match status.deletion_reason {
        Some(DeletionReason::Consumed) => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: "Identity was already consumed".into(),
            }),
        )
            .into_response(),
        Some(DeletionReason::Expired) => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: "Identity has expired".into(),
            }),
        )
            .into_response(),
        Some(DeletionReason::IpMismatch) => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: "Identity was deleted due to IP mismatch".into(),
            }),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(EvictedBody {
                error: "Identity was evicted".into(),
                created_at: status.created_at,
            }),
        )
            .into_response(),
    }
}

fn update_identity_status(
    state: &AppState,
    id: &str,
    consumed: bool,
    reason: Option<DeletionReason>,
    signer: &str,
    now: DateTime<Utc>,
) {
    if let Some(mut entry) = state.auth_api.identity_status.get_mut(id) {
        entry.consumed = consumed;
        entry.deletion_reason = reason;
    } else {
        state.auth_api.identity_status.insert(
            id.to_string(),
            IdentityStatus {
                expiration: now,
                created_at: now,
                consumed,
                signer: signer.to_string(),
                deletion_reason: reason,
            },
        );
    }
}

fn identity_owner(identity: &Value) -> Option<String> {
    identity
        .get("authChain")
        .and_then(|c| c.as_array())
        .and_then(|links| links.first())
        .and_then(|link| link.get("payload"))
        .and_then(|p| p.as_str())
        .map(|s| s.to_lowercase())
}

fn forbidden(msg: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}
