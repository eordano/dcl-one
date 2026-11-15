// from: shop-server@dc1a8be96:src/controllers/routes.ts (the notify block), src/controllers/schemas/{notify-request,notify-on-sale}.ts, src/controllers/handlers/{create-notify-request,get-notify-request,delete-notify-request,notify-on-sale-handler}.ts, src/ports/notify/component.ts, src/logic/notify/{consumer,on-sale}.ts
//! Availability subscriptions: shop-server's "notify me when available"
//! waitlist (SHOP.md, "Backend work" 6). A signed-in buyer asks to be
//! emailed when a sold-out or unlisted item is on sale again; the wallet is
//! the verified signed-fetch signer (a scene-signed chain is refused: a
//! scene the buyer walks into must not read their email or cancel their
//! alerts as that account), so a caller only ever reads, writes or cancels
//! their own row (`shop.item_notify_requests`).
//!
//! The two hops shop-server made over the network are local here
//! (`delivery`): the trigger is the `shop-notify-availability` job reading
//! the pending items' state from the items mirror and recording an on-sale
//! transition together with one outbox row per pending subscriber
//! (`shop.item_notify_outbox`, the SQS hop); the sender is the
//! `shop-notify-deliveries` job claiming due rows under a lease, mailing
//! each through `social::notifications::email` (the upstream
//! code-rendered template, web2 copy only) outside any transaction, and
//! marking the row sent and the request notified in one statement, so a
//! duplicate trigger or a redelivery never re-notifies. An in-process
//! caller (an indexer that sees the ITEM_PUBLISHED event) may call
//! [`item_on_sale`] directly: the upstream `POST /notify/item-on-sale`
//! without the bearer.
//!
//! Routes (`shop_router`, under `/shop-api`, the writes behind
//! `DATA_WRITES_ENABLED`): `POST|GET|DELETE /v1/notify-requests`, the
//! upstream wire shapes (`201 { ok: true }`, `{ subscribed, email? }`,
//! `{ unsubscribed: n }`, `{ error }` refusals).

pub mod delivery;
pub mod store;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Query, State as Extract};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::shared::signed_fetch::{self, PayloadPolicy, SignedFetchError, SignedPath};
use crate::shared::writes::DataWrites;

pub use delivery::{AvailabilityMailer, OnSaleItem, item_on_sale};
pub use store::{NotifyRequest, NotifyStatus, Subscriptions};

pub const DEFAULT_SHOP_BASE_URL: &str = "https://decentraland.org/shop";
pub const DEFAULT_DELIVERY_INTERVAL: Duration = Duration::from_secs(30);
pub const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct Settings {
    /// `SHOP_BASE_URL`: the deep link base of the item page and the opt-out page in the email.
    pub shop_base_url: String,
    /// `SHOP_NOTIFY_DELIVERY_INTERVAL_SECS`, `SHOP_NOTIFY_WATCH_INTERVAL_SECS`.
    pub delivery_interval: Duration,
    pub watch_interval: Duration,
    pub writes_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shop_base_url: DEFAULT_SHOP_BASE_URL.to_string(),
            delivery_interval: DEFAULT_DELIVERY_INTERVAL,
            watch_interval: DEFAULT_WATCH_INTERVAL,
            writes_enabled: false,
        }
    }
}

pub struct State {
    pub store: Subscriptions,
    pub mailer: Arc<dyn AvailabilityMailer>,
    pub settings: Settings,
    pub eip1654: Option<Arc<dyn Eip1654Validator>>,
}

pub type SharedState = Arc<State>;

impl State {
    pub fn new(
        pool: PgPool,
        mailer: Arc<dyn AvailabilityMailer>,
        settings: Settings,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        Self {
            store: Subscriptions::new(pool),
            mailer,
            settings,
            eip1654,
        }
    }

    /// The two loops: the availability watcher (the trigger) and the
    /// deliveries worker (the sender). Both run whatever the write gate
    /// says: a row already durable is delivered; with no sender configured
    /// every claimed row ends `skipped`, never a silent success.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs) {
        let configured = self.mailer.is_configured();
        if !configured {
            tracing::info!(
                "SENDGRID_API_KEY / SENDGRID_FROM_EMAIL unset: availability alerts are recorded as skipped"
            );
        }
        let state = self.clone();
        jobs.spawn_periodic_leased(
            "shop-notify-availability",
            PeriodicCfg::every(self.settings.watch_interval.max(Duration::from_secs(1)))
                .with_jitter(Duration::from_secs(5))
                .with_error_backoff(Duration::from_secs(600)),
            move |ctx| {
                let state = state.clone();
                async move {
                    let summary = delivery::watch_pass(state.store.pool()).await?;
                    if summary.transitions > 0 || summary.enqueued > 0 {
                        tracing::info!(?summary, "availability watch pass");
                    }
                    ctx.progress(format!("{summary:?}"));
                    Ok(())
                }
            },
        );
        let state = self.clone();
        jobs.spawn_periodic_leased(
            "shop-notify-deliveries",
            PeriodicCfg::every(self.settings.delivery_interval.max(Duration::from_secs(1)))
                .with_jitter(Duration::from_secs(5))
                .with_error_backoff(Duration::from_secs(300)),
            move |ctx| {
                let state = state.clone();
                async move {
                    let summary = delivery::run_pass(
                        state.store.pool(),
                        state.mailer.as_ref(),
                        &state.settings.shop_base_url,
                    )
                    .await?;
                    if summary.claimed > 0 {
                        tracing::info!(?summary, "availability deliveries pass");
                        ctx.progress(format!("{summary:?}"));
                    }
                    Ok(())
                }
            },
        );
    }
}

/// The waitlist routes; `main.rs` mounts them under `/shop-api` only (the upstream host was the sole alias).
pub fn shop_router(state: SharedState) -> Router {
    let enabled = state.settings.writes_enabled;
    let api = Router::new()
        .route(
            "/v1/notify-requests",
            get(read).post(create).delete(unsubscribe),
        )
        .with_state(state);
    DataWrites::guard(api, enabled)
}

#[derive(Debug)]
pub enum ApiError {
    Status(StatusCode, String),
    /// The static signed-fetch refusal (verifier internals never leak).
    Unauthorized,
    Internal(String),
}

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::Status(StatusCode::BAD_REQUEST, msg.into())
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("database: {e}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status, message) => (status, Json(json!({ "error": message }))).into_response(),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthorized", "message": "This endpoint requires a signed-fetch request (ADR-44)." })),
            )
                .into_response(),
            Self::Internal(detail) => {
                tracing::error!(error = %detail, "notify-requests route failed");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "Failed to process notify request" }))).into_response()
            }
        }
    }
}

fn signed_fetch_error(e: SignedFetchError) -> ApiError {
    match e {
        SignedFetchError::MalformedChain { ref detail } if e.is_metadata_refusal() => {
            ApiError::bad_request(format!("Invalid signer: {detail}"))
        }
        other => {
            tracing::debug!(error = %other, "signed-fetch verification failed for /notify-requests");
            ApiError::Unauthorized
        }
    }
}

/// The verified, lowercased signer over the path the request arrived on.
async fn require_signer(
    state: &State,
    headers: &HeaderMap,
    method: &Method,
    original: &OriginalUri,
) -> Result<String, ApiError> {
    signed_fetch::require_signer_at(
        headers,
        method.as_str().to_ascii_lowercase().as_str(),
        &SignedPath::at(&original.0, "/v1/notify-requests"),
        PayloadPolicy::V6ThenLegacy,
        state.eip1654.as_deref(),
    )
    .await
    .map(|s| s.into_string().to_ascii_lowercase())
    .map_err(signed_fetch_error)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreateNotifyRequest {
    pub contract_address: String,
    pub item_id: String,
    pub chain_id: i64,
    pub email: String,
}

/// ajv's `format: "email"` (the fast check): one `@`, no whitespace or
/// control characters, a local part and a dotted domain, at most 254 chars.
pub fn is_email(s: &str) -> bool {
    if s.len() > 254 || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((local, domain)) = s.rsplit_once('@') else {
        return false;
    };
    if local.is_empty() || local.len() > 64 || domain.is_empty() || !domain.contains('.') {
        return false;
    }
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    domain.split('.').all(label_ok)
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
}

/// Pending notify requests one wallet may hold.
pub const MAX_PENDING_PER_WALLET: i64 = 50;
/// Pending notify requests one email address may receive, across wallets.
pub const MAX_PENDING_PER_EMAIL: i64 = 20;

pub fn validate_create(req: &CreateNotifyRequest) -> Result<(), String> {
    if !crate::market::is_valid_eth_address(&req.contract_address) {
        return Err("contractAddress must be a 0x-prefixed 40-hex-char address".into());
    }
    if req.item_id.is_empty() || req.item_id.len() > 78 {
        return Err("itemId must be 1..78 characters".into());
    }
    if req.chain_id < 1 {
        return Err("chainId must be an integer >= 1".into());
    }
    if !is_email(&req.email) {
        return Err("email must be a valid email address".into());
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct ItemQuery {
    #[serde(rename = "contractAddress")]
    contract_address: Option<String>,
    #[serde(rename = "itemId")]
    item_id: Option<String>,
}

/// `POST /v1/notify-requests`: upsert the signer's row for the item, status
/// pending (a re-subscribe queues again). The email is PII: never logged.
async fn create(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let address = require_signer(&state, &headers, &Method::POST, &OriginalUri(original)).await?;
    let req: CreateNotifyRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
    validate_create(&req).map_err(ApiError::bad_request)?;
    let contract = req.contract_address.to_ascii_lowercase();
    let stored = state
        .store
        .upsert_capped(
            &address,
            &req.email,
            &contract,
            &req.item_id,
            req.chain_id as i32,
            MAX_PENDING_PER_WALLET,
            MAX_PENDING_PER_EMAIL,
        )
        .await?;
    if !stored {
        return Err(ApiError::Status(
            StatusCode::TOO_MANY_REQUESTS,
            "too many pending notify requests for this wallet or email".into(),
        ));
    }
    tracing::info!(address = %address, contract = %contract, item_id = %req.item_id, "notify request upserted");
    Ok((StatusCode::CREATED, Json(json!({ "ok": true }))).into_response())
}

/// `GET /v1/notify-requests?contractAddress=&itemId=`: `{ subscribed, email? }`
/// for the signer and the item; `subscribed` only while the row is pending.
async fn read(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<ItemQuery>,
) -> Result<Response, ApiError> {
    let address = require_signer(&state, &headers, &Method::GET, &OriginalUri(original)).await?;
    let (Some(contract), Some(item_id)) = (
        q.contract_address.as_deref().filter(|s| !s.is_empty()),
        q.item_id.as_deref().filter(|s| !s.is_empty()),
    ) else {
        return Err(ApiError::bad_request(
            "contractAddress and itemId are required query params",
        ));
    };
    if !crate::market::is_valid_eth_address(contract) {
        return Err(ApiError::bad_request(
            "contractAddress must be a 0x-prefixed 40-hex-char address",
        ));
    }
    let row = state
        .store
        .get(&address, &contract.to_ascii_lowercase(), item_id)
        .await?;
    let body = match row {
        Some(r) if r.status == NotifyStatus::Pending => {
            json!({ "subscribed": true, "email": r.email })
        }
        _ => json!({ "subscribed": false }),
    };
    Ok(Json(body).into_response())
}

/// `DELETE /v1/notify-requests[?contractAddress=&itemId=]`: opt out of one
/// item (both given) or every item (neither); exactly one is ambiguous.
/// Idempotent: `{ unsubscribed: <count> }`, 0 when nothing matched.
async fn unsubscribe(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<ItemQuery>,
) -> Result<Response, ApiError> {
    let address = require_signer(&state, &headers, &Method::DELETE, &OriginalUri(original)).await?;
    let contract = q
        .contract_address
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let item_id = q.item_id.as_deref().filter(|s| !s.is_empty());
    if contract.is_some() != item_id.is_some() {
        return Err(ApiError::bad_request(
            "Provide both contractAddress and itemId, or neither to unsubscribe from all items",
        ));
    }
    if let Some(c) = &contract
        && !crate::market::is_valid_eth_address(c)
    {
        return Err(ApiError::bad_request(
            "contractAddress must be a 0x-prefixed 40-hex-char address",
        ));
    }
    let scope = contract.as_deref().zip(item_id);
    let unsubscribed = state.store.unsubscribe(&address, scope).await?;
    tracing::info!(address = %address, scope = %scope.map(|(c, i)| format!("{c}:{i}")).unwrap_or_else(|| "all".into()), unsubscribed, "notify requests unsubscribed");
    Ok(Json(json!({ "unsubscribed": unsubscribed })).into_response())
}
