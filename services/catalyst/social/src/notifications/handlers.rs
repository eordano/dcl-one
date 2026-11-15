//! The routes. Every wallet-scoped route verifies the signed-fetch chain
//! over the path the client requested (`OriginalUri`, so the router works
//! nested under `/notifications` or at a vhost root; a proxy that rewrites
//! the path forwards the public one as `x-original-path`, and a public path
//! that ends with the route, `/notifications` on notifications.<domain>,
//! verifies as the route; `SignedPath::from_uris`); the broadcast is
//! the bearer-gated admin route.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use uuid::Uuid;

use super::admin::RequireAdmin;
use super::email::{self, CODE_LEN};
use super::errors::{ApiError, NotificationsError, parse_json};
use super::store::SetEmailOutcome;
use super::types::{
    BroadcastResponse, CommunityOptOutStatus, MarkReadResponse, NotificationsListResponse,
    Subscription, is_known_type, subscription_update,
};
use crate::identity::Address;
use crate::shared::signed_fetch;

type AppState = Arc<super::State>;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 50;
const MAX_ADDRESSES: usize = 10_000;
const SCOPE_COMMUNITY: &str = "community";

/// The verified signer, or the signed-fetch refusal as the envelope (400
/// malformed, 401 unauthenticated, 503 when a contract wallet cannot be
/// asked; the old Rust port collapsed all three to 401). Current payload
/// only, as upstream's every route but `PUT /notifications/read`.
async fn signer(
    state: &AppState,
    headers: &HeaderMap,
    method: &str,
    uri: &OriginalUri,
    local: &Uri,
) -> Result<Address, NotificationsError> {
    verify(
        state,
        headers,
        method,
        uri,
        local,
        signed_fetch::PayloadPolicy::V6Only,
    )
    .await
}

async fn verify(
    state: &AppState,
    headers: &HeaderMap,
    method: &str,
    uri: &OriginalUri,
    local: &Uri,
    policy: signed_fetch::PayloadPolicy,
) -> Result<Address, NotificationsError> {
    signed_fetch::require_signer_at(
        headers,
        method,
        &signed_fetch::SignedPath::from_uris(uri, local),
        policy,
        state.eip1654.as_deref(),
    )
    .await
    .map(|s| Address::from_signer(&s))
    .map_err(|e| {
        tracing::warn!(error = %e, "signed-fetch rejected");
        let (status, error) = e.http_status_and_message();
        NotificationsError::SignedFetch { status, error }
    })
}

/// Upstream's `status-handler.ts`: unset values answer `""`.
pub async fn get_status(State(state): State<AppState>) -> Json<JsonValue> {
    Json(json!({
        "version": state.settings.current_version.as_deref().unwrap_or(""),
        "currentTime": chrono::Utc::now().timestamp_millis(),
        "commitHash": state.settings.commit_hash.as_deref().unwrap_or(""),
    }))
}

/// JS `parseInt(s, 10)`: leading whitespace, a sign, then the longest digit
/// run (saturating); `None` is NaN.
fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (negative, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let n = rest.as_bytes()[..digits].iter().fold(0i64, |n, d| {
        n.saturating_mul(10).saturating_add(i64::from(d - b'0'))
    });
    Some(if negative { -n } else { n })
}

/// Upstream's reading of the query: `onlyUnread` by presence (`?onlyUnread`
/// and `onlyUnread=false` both filter), `limit` 1-50 else 20, `from` (epoch
/// ms, inclusive) only when positive; nothing here refuses a request.
fn list_params(query: Option<&str>) -> (i64, Option<i64>, bool) {
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .into_owned()
        .collect();
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let limit = get("limit")
        .and_then(parse_int)
        .filter(|n| (1..=MAX_LIMIT).contains(n))
        .unwrap_or(DEFAULT_LIMIT);
    let from = get("from").and_then(parse_int).filter(|&n| n > 0);
    (limit, from, get("onlyUnread").is_some())
}

pub async fn get_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
    RawQuery(query): RawQuery,
) -> Result<Json<NotificationsListResponse>, NotificationsError> {
    let signer = signer(&state, &headers, "get", &uri, &local).await?;
    let (limit, from, only_unread) = list_params(query.as_deref());
    let items = state
        .store
        .list_and_touch(&signer, limit, from, only_unread)
        .await?;
    Ok(Json(NotificationsListResponse {
        notifications: items,
    }))
}

/// The one route that also accepts the pre-6.0 payload, behind the
/// canonical-`signer` guard (upstream's `READ_NOTIFICATIONS_CANONICAL_METADATA_KEYS`):
/// decentraland-dapps and godot-explorer still sign it here.
pub async fn put_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
    body: Bytes,
) -> Result<impl IntoResponse, NotificationsError> {
    let policy = signed_fetch::PayloadPolicy::V6ThenLegacy;
    let signer = verify(&state, &headers, "put", &uri, &local, policy).await?;
    let body = parse_json(&body)?;
    let ids = match body.get("notificationIds").and_then(JsonValue::as_array) {
        Some(ids) if !ids.is_empty() => ids,
        _ => return Err(ApiError::bad_request("Missing notificationIds").into()),
    };
    let ids: Vec<Uuid> = ids
        .iter()
        .map(|id| id.as_str().and_then(|s| Uuid::parse_str(s).ok()))
        .collect::<Option<_>>()
        .ok_or_else(|| ApiError::bad_request("invalid notification id"))?;
    let updated = state.store.mark_read(&signer, &ids).await?;
    Ok(Json(MarkReadResponse { updated }))
}

pub async fn get_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
) -> Result<Json<Subscription>, NotificationsError> {
    let signer = signer(&state, &headers, "get", &uri, &local).await?;
    Ok(Json(state.store.get_subscription(&signer).await?))
}

pub async fn put_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
    body: Bytes,
) -> Result<StatusCode, NotificationsError> {
    let signer = signer(&state, &headers, "put", &uri, &local).await?;
    let details = subscription_update(&parse_json(&body)?).map_err(ApiError::bad_request)?;
    state
        .store
        .put_subscription_details(&signer, &details)
        .await?;
    state.track(
        "subscription_changed",
        serde_json::json!({ "address": signer.as_str(), "subscription_details": details }),
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SetEmailBody {
    pub email: Option<String>,
    #[serde(rename = "isCreditsWorkflow")]
    pub is_credits_workflow: bool,
}

pub async fn put_set_email(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
    Json(body): Json<SetEmailBody>,
) -> Result<impl IntoResponse, NotificationsError> {
    let signer = signer(&state, &headers, "put", &uri, &local).await?;
    let email = match body.email.as_deref() {
        Some("") => "",
        Some(e) if email::is_valid_email(e) => e,
        _ => return Err(ApiError::bad_request(email::INVALID_EMAIL).into()),
    };
    let blacklisted = !email.is_empty() && state.email.is_domain_blacklisted(email);
    match state
        .store
        .set_email(&signer, email, body.is_credits_workflow, blacklisted)
        .await?
    {
        SetEmailOutcome::NoEmailSent => {}
        SetEmailOutcome::SendConfirmation { source, code } => {
            let user_name = crate::rpc::profiles::Profiles::new(Some(state.store.pool().clone()))
                .get_profile(signer.as_str())
                .await
                .map(|p| p.name);
            state
                .email
                .send_confirmation(source, email, signer.as_str(), &code, user_name.as_deref())
                .await?;
            state.track(
                "email_validation_started",
                serde_json::json!({
                    "address": signer.as_str(),
                    "email_to_validate": email,
                    "is_credits_workflow": body.is_credits_workflow,
                }),
            );
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConfirmEmailBody {
    pub address: Option<String>,
    pub code: Option<String>,
    #[serde(rename = "turnstileToken")]
    pub turnstile_token: Option<String>,
    pub source: Option<String>,
}

/// Unsigned by design (the link is opened from a mail client); the code and
/// the optional Turnstile challenge are the credential.
pub async fn put_confirm_email(
    State(state): State<AppState>,
    Json(body): Json<ConfirmEmailBody>,
) -> Result<impl IntoResponse, NotificationsError> {
    let address = body
        .address
        .as_deref()
        .and_then(|a| Address::parse(a).ok())
        .ok_or_else(|| ApiError::bad_request("Missing address"))?;
    let code = body
        .code
        .as_deref()
        .filter(|c| c.len() == CODE_LEN)
        .ok_or_else(|| ApiError::bad_request("Missing code"))?;
    let is_credits = body.source.as_deref() == Some("credits");
    let (verified, error_codes) = email::verify_turnstile(
        &state.http,
        state.email.turnstile_secret(),
        body.turnstile_token.as_deref(),
    )
    .await?;
    if !verified {
        state.track(
            "email_validation_failed",
            serde_json::json!({
                "address": address.as_str(),
                "error": "captcha_validation_failed",
                "is_credits_workflow": is_credits,
                "error_codes": serde_json::to_string(&error_codes).unwrap_or_default(),
            }),
        );
        return Err(ApiError::unauthorized("Invalid captcha").into());
    }
    let pending = state
        .store
        .find_unconfirmed_email(&address)
        .await?
        .ok_or_else(|| ApiError::bad_request("No unconfirmed email for this address"))?;
    if !super::signing::constant_time_eq(pending.1.as_bytes(), code.as_bytes()) {
        return Err(ApiError::bad_request("Invalid code").into());
    }
    if state.email.is_domain_blacklisted(&pending.0) {
        return Err(ApiError::bad_request(email::INVALID_EMAIL).into());
    }
    if state.store.confirm_email(&address, code).await?.is_none() {
        return Err(ApiError::bad_request("Invalid code").into());
    }
    state.track(
        "email_validated",
        serde_json::json!({
            "address": address.as_str(),
            "validated_email": pending.0,
            "is_credits_workflow": is_credits,
        }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_opt_out(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
    Path((scope, scope_id)): Path<(String, String)>,
) -> Result<Json<CommunityOptOutStatus>, NotificationsError> {
    let signer = signer(&state, &headers, "get", &uri, &local).await?;
    if scope != SCOPE_COMMUNITY {
        return Err(ApiError::bad_request("Invalid scope").into());
    }
    let opted_out = state.store.is_opted_out(&signer, &scope, &scope_id).await?;
    Ok(Json(CommunityOptOutStatus {
        scope,
        scope_id,
        opted_out,
    }))
}

/// Upstream deletes without checking the scope: an unknown one is a 204 no-op.
pub async fn delete_opt_out(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
    Path((scope, scope_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, NotificationsError> {
    let signer = signer(&state, &headers, "delete", &uri, &local).await?;
    state
        .store
        .delete_opt_out(&signer, &scope, &scope_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Upstream's `CreateNotificationOptOutSchema` (ajv, first error):
/// `{scope: "community", scopeId: non-empty string}`, nothing else.
fn opt_out_schema_error(body: &JsonValue) -> Option<JsonValue> {
    let error = |path: &str, schema: &str, keyword: &str, params: JsonValue, message: &str| {
        Some(
            json!([{ "instancePath": path, "schemaPath": schema, "keyword": keyword,
            "params": params, "message": message }]),
        )
    };
    let Some(obj) = body.as_object() else {
        return error(
            "",
            "#/type",
            "type",
            json!({ "type": "object" }),
            "must be object",
        );
    };
    for key in ["scope", "scopeId"] {
        if !obj.contains_key(key) {
            let message = format!("must have required property '{key}'");
            return error(
                "",
                "#/required",
                "required",
                json!({ "missingProperty": key }),
                &message,
            );
        }
    }
    if let Some(extra) = obj.keys().find(|k| *k != "scope" && *k != "scopeId") {
        return error(
            "",
            "#/additionalProperties",
            "additionalProperties",
            json!({ "additionalProperty": extra }),
            "must NOT have additional properties",
        );
    }
    match obj["scope"].as_str() {
        None => {
            let params = json!({ "type": "string" });
            return error(
                "/scope",
                "#/properties/scope/type",
                "type",
                params,
                "must be string",
            );
        }
        Some(SCOPE_COMMUNITY) => {}
        Some(_) => {
            return error(
                "/scope",
                "#/properties/scope/enum",
                "enum",
                json!({ "allowedValues": [SCOPE_COMMUNITY] }),
                "must be equal to one of the allowed values",
            );
        }
    }
    match obj["scopeId"].as_str() {
        None => {
            let params = json!({ "type": "string" });
            error(
                "/scopeId",
                "#/properties/scopeId/type",
                "type",
                params,
                "must be string",
            )
        }
        Some("") => error(
            "/scopeId",
            "#/properties/scopeId/minLength",
            "minLength",
            json!({ "limit": 1 }),
            "must NOT have fewer than 1 characters",
        ),
        Some(_) => None,
    }
}

pub async fn post_opt_out(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: OriginalUri,
    local: Uri,
    body: Bytes,
) -> Result<impl IntoResponse, NotificationsError> {
    let signer = signer(&state, &headers, "post", &uri, &local).await?;
    let body = serde_json::from_slice::<JsonValue>(&body)
        .map_err(|_| NotificationsError::Schema(JsonValue::Null))?;
    if let Some(data) = opt_out_schema_error(&body) {
        return Err(NotificationsError::Schema(data));
    }
    let scope = SCOPE_COMMUNITY.to_string();
    let scope_id = body["scopeId"].as_str().unwrap_or_default().to_string();
    state
        .store
        .create_opt_out(&signer, &scope, &scope_id)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CommunityOptOutStatus {
            scope,
            scope_id,
            opted_out: true,
        }),
    ))
}

const MAX_BROADCAST_ID_CHARS: usize = 128;

#[derive(Debug, Deserialize)]
pub struct BroadcastBody {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub metadata: JsonValue,
    #[serde(default)]
    pub addresses: Option<Vec<String>>,
    /// The idempotency key of this broadcast (`notifications_broadcast_uidx`):
    /// a retried POST with the same id reaches no recipient twice. Absent, the
    /// server mints a uuid per request, as the old admin route did.
    #[serde(rename = "broadcastId", default)]
    pub broadcast_id: Option<String>,
}

pub async fn post_broadcast(
    admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<BroadcastBody>,
) -> Result<impl IntoResponse, NotificationsError> {
    if !is_known_type(&body.kind) {
        return Err(
            ApiError::bad_request(format!("unknown notification type: {}", body.kind)).into(),
        );
    }
    if !body.metadata.is_null() && !body.metadata.is_object() {
        return Err(ApiError::bad_request("metadata must be an object").into());
    }
    let metadata = if body.metadata.is_null() {
        serde_json::json!({})
    } else {
        body.metadata
    };
    let addresses: Option<Vec<Address>> = match body.addresses {
        Some(list) => {
            if list.is_empty() {
                return Err(ApiError::bad_request(
                    "addresses must be non-empty when provided (omit it to broadcast to all)",
                )
                .into());
            }
            if list.len() > MAX_ADDRESSES {
                return Err(ApiError::bad_request(format!(
                    "too many addresses: {} (max {})",
                    list.len(),
                    MAX_ADDRESSES
                ))
                .into());
            }
            let mut parsed = Vec::with_capacity(list.len());
            for a in &list {
                parsed.push(
                    Address::parse(a)
                        .map_err(|_| ApiError::bad_request(format!("invalid address: {}", a)))?,
                );
            }
            Some(parsed)
        }
        None => None,
    };
    let broadcast_id = match body.broadcast_id.as_deref().map(str::trim) {
        None => Uuid::new_v4().to_string(),
        Some(id) if !id.is_empty() && id.chars().count() <= MAX_BROADCAST_ID_CHARS => {
            id.to_string()
        }
        Some(_) => {
            return Err(ApiError::bad_request(format!(
                "broadcastId must be 1 to {MAX_BROADCAST_ID_CHARS} characters"
            ))
            .into());
        }
    };
    let inserted = state
        .store
        .broadcast(&broadcast_id, &body.kind, &metadata, addresses.as_deref())
        .await?;
    tracing::info!(actor = %admin.audit_actor_description(), kind = %body.kind, recipients = inserted, "broadcast");
    Ok((
        StatusCode::OK,
        Json(BroadcastResponse {
            ok: true,
            broadcast_id,
            kind: body.kind,
            recipients: inserted,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::list_params;

    #[test]
    fn the_list_query_reads_as_upstream_parse_int_and_has() {
        assert_eq!(list_params(None), (20, None, false));
        assert_eq!(list_params(Some("onlyUnread")), (20, None, true));
        assert_eq!(list_params(Some("onlyUnread=false")), (20, None, true));
        assert_eq!(list_params(Some("limit=5&from=0")), (5, None, false));
        assert_eq!(list_params(Some("limit=50&from=12")), (50, Some(12), false));
        assert_eq!(list_params(Some("limit=51")).0, 20);
        assert_eq!(list_params(Some("limit=0")).0, 20);
        assert_eq!(list_params(Some("limit=abc&from=abc")), (20, None, false));
        assert_eq!(list_params(Some("limit=7px&from=+9x")), (7, Some(9), false));
        assert_eq!(list_params(Some("from=-5")).1, None);
        assert_eq!(
            list_params(Some("from=99999999999999999999999")).1,
            Some(i64::MAX)
        );
    }
}
