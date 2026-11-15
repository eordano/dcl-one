// from: shop-server@dc1a8be96:src/logic/outfits.ts, src/adapters/db/outfits.ts, src/controllers/routes.ts (the outfits block, outfitCreatorsMiddleware), src/controllers/schemas/outfit.ts, src/controllers/handlers/{get-outfits,get-outfits-all,get-outfit,post-outfit,put-outfit,delete-outfit,post-outfit-thumbnail,get-outfit-thumbnail}.ts
//! Outfits: the curated shoppable wearable sets of the Shop (SHOP.md,
//! "Backend work" 6, the shop-server fold), the one public-facing surface
//! the old treasury service had. Reads are unauthenticated; writes are
//! ADR-44 signed fetch (the author is the verified signer, never the path or
//! the body) AND gated by the creator allowlist (`SHOP_OUTFIT_CREATORS`, the
//! server-side twin of upstream's `dapps-shop-outfit-creators` flag). A
//! draft is visible to an allowlisted signer only; everyone else gets the
//! same 404 (a 403 would confirm it exists). Thumbnail bytes go to the ONE
//! blob store under their `hash_bytes_v1` cid; the sha256 hex of the bytes
//! stays the wire id (`shop.outfit_thumbnails`, schemas/30_shop.sql).
//!
//! Routes (`shop_router`, nested under `/shop-api` by `shop::router`; the
//! writes behind `DATA_WRITES_ENABLED`): `GET /v1/outfits`,
//! `GET /v1/outfits/all`, `POST /v1/outfits/thumbnails`,
//! `GET /v1/outfits/thumbnails/{hash}`, `GET|PUT|DELETE /v1/outfits/{id}`,
//! `POST /v1/outfits`. Wire shapes, status codes and error labels are the
//! upstream handlers' (`{ outfits }`, `{ outfit }`, `{ hash }`, `{ ok: true }`,
//! `{ ok: false, error: <label> }`).

pub mod store;

#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Path, Query, State as Extract};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, extract::Request};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::shared::signed_fetch::{self, PayloadPolicy, SignedFetchError, SignedPath};
use crate::shared::writes::DataWrites;
use crate::storage::Storage;

pub use store::{Outfit, OutfitItemRef, OutfitThumbnail, Outfits};

/// Thumbnail upload ceiling (load-bearing upstream: its server had no body cap of its own).
pub const MAX_THUMBNAIL_BYTES: usize = 1024 * 1024;
pub const MIN_PUBLISH_ITEMS: usize = 1;
pub const MAX_OUTFIT_ITEMS: usize = 20;
/// Hard page-size ceiling for the listings.
pub const OUTFITS_MAX_LIMIT: i64 = 50;

/// `SHOP_OUTFIT_CREATORS` (comma or newline separated wallets, compared
/// lowercase) and the write gate.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub creators: Vec<String>,
    pub writes_enabled: bool,
}

impl Settings {
    /// Upstream `parseAddressListVariant`: split on commas and newlines,
    /// trim, lowercase, keep the 0x + 40 hex ones; an unparsable list is an
    /// empty allowlist (fail closed), never a partial one silently.
    pub fn parse_creators(raw: &str) -> Vec<String> {
        let mut out: Vec<String> = raw
            .split([',', '\n', '\r'])
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| crate::market::is_valid_eth_address(s))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn is_creator(&self, address: &str) -> bool {
        let address = address.to_ascii_lowercase();
        self.creators.contains(&address)
    }
}

pub struct State {
    pub store: Outfits,
    pub storage: Arc<Storage>,
    pub settings: Settings,
    pub eip1654: Option<Arc<dyn Eip1654Validator>>,
}

pub type SharedState = Arc<State>;

impl State {
    pub fn new(
        pool: PgPool,
        storage: Arc<Storage>,
        settings: Settings,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        if settings.creators.is_empty() {
            tracing::info!(
                "SHOP_OUTFIT_CREATORS is empty: every outfit write answers 403 (the allowlist fails closed)"
            );
        }
        Self {
            store: Outfits::new(pool),
            storage,
            settings,
            eip1654,
        }
    }
}

/// The outfits routes at their upstream paths; fixed segments (`/all`,
/// `/thumbnails`) are registered before `/{id}` so they never match as ids.
pub fn shop_router(state: SharedState) -> Router {
    let enabled = state.settings.writes_enabled;
    let api = Router::new()
        .route("/v1/outfits", get(list_published).post(create))
        .route("/v1/outfits/all", get(list_all))
        .route("/v1/outfits/thumbnails", post(upload_thumbnail))
        .route("/v1/outfits/thumbnails/{hash}", get(thumbnail))
        .route("/v1/outfits/{id}", get(one).put(replace).delete(remove))
        .with_state(state);
    DataWrites::guard(api, enabled)
}

/// The upstream envelopes: `{ ok: false, error: <label> }` for a handler
/// refusal, `{ error: 'unauthorized', message }` for a failed signed fetch
/// (the static body, so verifier internals never reach the caller),
/// `{ ok: false, error: 'Internal server error' }` for a failure.
#[derive(Debug)]
pub enum ApiError {
    Labelled {
        status: StatusCode,
        label: &'static str,
        message: Option<String>,
    },
    Unauthorized,
    Internal(String),
}

impl ApiError {
    fn labelled(status: StatusCode, label: &'static str) -> Self {
        Self::Labelled {
            status,
            label,
            message: None,
        }
    }
    fn bad_request(label: &'static str, message: impl Into<String>) -> Self {
        Self::Labelled {
            status: StatusCode::BAD_REQUEST,
            label,
            message: Some(message.into()),
        }
    }
    pub fn not_found() -> Self {
        Self::labelled(StatusCode::NOT_FOUND, "not_found")
    }
    pub fn not_allowed() -> Self {
        Self::labelled(StatusCode::FORBIDDEN, "not_allowed")
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
            Self::Labelled { status, label, message } => {
                let mut body = json!({ "ok": false, "error": label });
                if let Some(m) = message {
                    body["message"] = Value::String(m);
                }
                (status, Json(body)).into_response()
            }
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthorized", "message": "This endpoint requires a signed-fetch request (ADR-44)." })),
            )
                .into_response(),
            Self::Internal(detail) => {
                tracing::error!(error = %detail, "outfits route failed");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "ok": false, "error": "Internal server error" }))).into_response()
            }
        }
    }
}

/// A refusal before signature verification (a scene-signed chain, a
/// non-canonical `signer`) is a 400 as upstream's `rejectIfSigner`; every
/// other refusal is the static 401 body.
fn signed_fetch_error(e: SignedFetchError) -> ApiError {
    match e {
        SignedFetchError::MalformedChain { ref detail } if e.is_metadata_refusal() => {
            ApiError::bad_request("invalid_signer", detail.clone())
        }
        other => {
            tracing::debug!(error = %other, "signed-fetch verification failed for an outfits route");
            ApiError::Unauthorized
        }
    }
}

/// The verified signer of a route that must be signed, over the path the
/// request arrived on (`OriginalUri`: `/shop-api/v1/outfits/...`).
async fn require_signer(
    state: &State,
    headers: &HeaderMap,
    method: &Method,
    original: &OriginalUri,
    local: &str,
) -> Result<String, ApiError> {
    signed_fetch::require_signer_at(
        headers,
        method.as_str().to_ascii_lowercase().as_str(),
        &SignedPath::at(&original.0, local),
        PayloadPolicy::V6ThenLegacy,
        state.eip1654.as_deref(),
    )
    .await
    .map(|s| s.into_string())
    .map_err(signed_fetch_error)
}

/// Signed AND allowlisted: the authoring gate (401 unverified, 403 not a creator).
async fn require_creator(
    state: &State,
    headers: &HeaderMap,
    method: &Method,
    original: &OriginalUri,
    local: &str,
) -> Result<String, ApiError> {
    let author = require_signer(state, headers, method, original, local).await?;
    if !state.settings.is_creator(&author) {
        return Err(ApiError::not_allowed());
    }
    Ok(author)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OutfitRequest {
    pub id: String,
    pub name: String,
    pub thumbnail_hash: String,
    pub items: Vec<OutfitItemRef>,
    pub body_shape: String,
    pub gradient_from: String,
    pub gradient_to: String,
    pub published: bool,
}

fn is_v4_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => matches!(c, b'0'..=b'9' | b'a'..=b'f'),
        })
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// The ajv schema of `POST /v1/outfits` and `PUT /v1/outfits/{id}`, as a
/// list of what is wrong (the first refusal is the 400's message).
pub fn validate_request(req: &OutfitRequest) -> Result<(), String> {
    if !is_v4_uuid(&req.id) {
        return Err("id must be a lowercase v4 uuid".into());
    }
    let name_len = req.name.chars().count();
    if !(1..=60).contains(&name_len)
        || req
            .name
            .chars()
            .any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
    {
        return Err("name must be 1..60 characters without control characters".into());
    }
    if !(req.thumbnail_hash.is_empty() || is_sha256_hex(&req.thumbnail_hash)) {
        return Err("thumbnailHash must be '' or 64 lowercase hex characters".into());
    }
    if req.items.len() > MAX_OUTFIT_ITEMS {
        return Err(format!(
            "items must hold at most {MAX_OUTFIT_ITEMS} entries"
        ));
    }
    for item in &req.items {
        if !crate::market::is_valid_eth_address(&item.contract_address) {
            return Err(format!(
                "items[].contractAddress {:?} is not a 0x + 40 hex address",
                item.contract_address
            ));
        }
        if item.item_id.is_empty()
            || item.item_id.len() > 78
            || !item.item_id.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(format!(
                "items[].itemId {:?} must be 1..78 digits",
                item.item_id
            ));
        }
    }
    if !matches!(req.body_shape.as_str(), "male" | "female" | "unisex") {
        return Err("bodyShape must be male, female or unisex".into());
    }
    for (field, value) in [
        ("gradientFrom", &req.gradient_from),
        ("gradientTo", &req.gradient_to),
    ] {
        if !(value.is_empty() || is_hex_color(value)) {
            return Err(format!("{field} must be '' or #rrggbb"));
        }
    }
    Ok(())
}

/// Duplicate pairs make an outfit lie about its size; ajv could not express this check.
pub fn has_unique_items(items: &[OutfitItemRef]) -> bool {
    let mut keys: Vec<String> = items
        .iter()
        .map(|i| format!("{}-{}", i.contract_address.to_ascii_lowercase(), i.item_id))
        .collect();
    let n = keys.len();
    keys.sort();
    keys.dedup();
    keys.len() == n
}

/// The publish bar: a thumbnail, both backdrop stops (a published thumbnail
/// is uploaded transparent and composited over them) and 1..20 items.
/// Drafts are exempt: they only need a name.
pub fn is_publishable(
    thumbnail_hash: &str,
    gradient_from: &str,
    gradient_to: &str,
    items: usize,
) -> bool {
    !thumbnail_hash.is_empty()
        && !gradient_from.is_empty()
        && !gradient_to.is_empty()
        && (MIN_PUBLISH_ITEMS..=MAX_OUTFIT_ITEMS).contains(&items)
}

/// The stored form: item contract addresses and gradient stops lowercased
/// (the item order is the authored order the studio and the cards render).
pub fn normalize(mut req: OutfitRequest) -> OutfitRequest {
    for item in &mut req.items {
        item.contract_address = item.contract_address.to_ascii_lowercase();
    }
    req.gradient_from = req.gradient_from.to_ascii_lowercase();
    req.gradient_to = req.gradient_to.to_ascii_lowercase();
    req
}

/// The image type from the magic bytes: the Content-Type header is
/// caller-controlled and therefore meaningless. `None` for anything but
/// png / jpeg / webp.
pub fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a] {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Whether the (possibly unsigned) caller may see an outfit: published, or an allowlisted author view.
pub fn can_read(outfit: &Outfit, caller: Option<&str>, settings: &Settings) -> bool {
    outfit.published || caller.is_some_and(|c| settings.is_creator(c))
}

/// Upstream `Number(param ?? default) || default`: a value that is not a
/// finite number, or zero, is the default.
fn js_number_or(raw: Option<&String>, default: i64) -> i64 {
    match raw {
        None => default,
        Some(s) => match catalyst_shared::js::number_from_str(s) {
            n if n.is_finite() && n.trunc() != 0.0 => n.trunc() as i64,
            _ => default,
        },
    }
}

fn no_store(body: Value) -> Response {
    let mut res = Json(body).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn parse_body(bytes: &Bytes) -> Result<OutfitRequest, ApiError> {
    let req: OutfitRequest = serde_json::from_slice(bytes)
        .map_err(|e| ApiError::bad_request("invalid_body", e.to_string()))?;
    validate_request(&req).map_err(|m| ApiError::bad_request("invalid_body", m))?;
    Ok(normalize(req))
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<String>,
    offset: Option<String>,
}

/// Public listing: published outfits, newest first. Uncached so an unpublish takes effect immediately.
async fn list_published(
    Extract(state): Extract<SharedState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let limit = js_number_or(q.limit.as_ref(), OUTFITS_MAX_LIMIT);
    let offset = js_number_or(q.offset.as_ref(), 0);
    let outfits = state.store.list(true, limit, offset).await?;
    Ok(no_store(json!({ "outfits": outfits })))
}

/// The authoring view: every outfit including drafts. Allowlist-gated.
async fn list_all(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Response, ApiError> {
    require_creator(
        &state,
        &headers,
        &Method::GET,
        &OriginalUri(original),
        "/v1/outfits/all",
    )
    .await?;
    let limit = js_number_or(q.limit.as_ref(), OUTFITS_MAX_LIMIT);
    let offset = js_number_or(q.offset.as_ref(), 0);
    let outfits = state.store.list(false, limit, offset).await?;
    Ok(no_store(json!({ "outfits": outfits })))
}

/// One outfit. Signed fetch is optional: published outfits are public,
/// drafts are visible to allowlisted signers only; everyone else gets the
/// same 404 (a draft must not leak, not even as a 403).
async fn one(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let outfit = state.store.get(&id).await?;
    let caller = signed_fetch::optional_signer_at(
        &headers,
        "get",
        &SignedPath::at(&original, format!("/v1/outfits/{id}")),
        PayloadPolicy::V6ThenLegacy,
        state.eip1654.as_deref(),
    )
    .await
    .map(|s| s.into_string());
    match outfit {
        Some(outfit) if can_read(&outfit, caller.as_deref(), &state.settings) => {
            Ok(no_store(json!({ "outfit": outfit })))
        }
        _ => Err(ApiError::not_found()),
    }
}

/// Creates an outfit. Idempotent on the client-generated id: a replay returns the stored row with 200.
async fn create(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let author = require_creator(
        &state,
        &headers,
        &Method::POST,
        &OriginalUri(original),
        "/v1/outfits",
    )
    .await?;
    let req = parse_body(&body)?;
    if !has_unique_items(&req.items) {
        return Err(ApiError::labelled(StatusCode::BAD_REQUEST, "invalid_items"));
    }
    if req.published
        && !is_publishable(
            &req.thumbnail_hash,
            &req.gradient_from,
            &req.gradient_to,
            req.items.len(),
        )
    {
        return Err(ApiError::labelled(
            StatusCode::BAD_REQUEST,
            "not_publishable",
        ));
    }
    let (outfit, inserted) = state.store.create(&req, &author).await?;
    let status = if inserted {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!({ "outfit": outfit }))).into_response())
}

/// Full replace, last write wins. Any allowlisted address may edit any
/// outfit (the upstream team rule); the original author is retained.
async fn replace(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    require_creator(
        &state,
        &headers,
        &Method::PUT,
        &OriginalUri(original),
        &format!("/v1/outfits/{id}"),
    )
    .await?;
    let req = parse_body(&body)?;
    if req.id != id {
        return Err(ApiError::labelled(StatusCode::BAD_REQUEST, "id_mismatch"));
    }
    if !has_unique_items(&req.items) {
        return Err(ApiError::labelled(StatusCode::BAD_REQUEST, "invalid_items"));
    }
    if req.published
        && !is_publishable(
            &req.thumbnail_hash,
            &req.gradient_from,
            &req.gradient_to,
            req.items.len(),
        )
    {
        return Err(ApiError::labelled(
            StatusCode::BAD_REQUEST,
            "not_publishable",
        ));
    }
    match state.store.replace(&req).await? {
        Some(outfit) => Ok(Json(json!({ "outfit": outfit })).into_response()),
        None => Err(ApiError::not_found()),
    }
}

/// Deletes an outfit. Idempotent; thumbnails are content-addressed and possibly shared, so they stay.
async fn remove(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    require_creator(
        &state,
        &headers,
        &Method::DELETE,
        &OriginalUri(original),
        &format!("/v1/outfits/{id}"),
    )
    .await?;
    state.store.remove(&id).await?;
    Ok(Json(json!({ "ok": true })).into_response())
}

/// Stores a thumbnail from a raw image body (no multipart): 411 without a
/// Content-Length, 413 over the cap (declared or actual), 415 when the
/// magic bytes are not png/jpeg/webp (an empty body reports here, not as
/// too large). Content-addressed by sha256, so re-uploads dedupe.
async fn upload_thumbnail(
    Extract(state): Extract<SharedState>,
    OriginalUri(original): OriginalUri,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let author = require_creator(
        &state,
        &parts.headers,
        &Method::POST,
        &OriginalUri(original),
        "/v1/outfits/thumbnails",
    )
    .await?;
    let Some(declared) = parts.headers.get(header::CONTENT_LENGTH) else {
        return Err(ApiError::labelled(
            StatusCode::LENGTH_REQUIRED,
            "length_required",
        ));
    };
    let declared: Option<usize> = declared.to_str().ok().and_then(|s| s.trim().parse().ok());
    if !declared.is_some_and(|n| n <= MAX_THUMBNAIL_BYTES) {
        return Err(ApiError::labelled(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
        ));
    }
    let bytes = match axum::body::to_bytes(body, MAX_THUMBNAIL_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return Err(ApiError::labelled(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
            ));
        }
    };
    let Some(content_type) = sniff_image_type(&bytes) else {
        return Err(ApiError::labelled(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_type",
        ));
    };
    let hash = hex::encode(Sha256::digest(&bytes));
    let cid = crate::auth::hash_bytes_v1(&bytes);
    state
        .storage
        .store(&cid, bytes.clone())
        .await
        .map_err(|e| ApiError::Internal(format!("blob store: {e}")))?;
    state
        .store
        .put_thumbnail(
            &OutfitThumbnail {
                hash: hash.clone(),
                blob_cid: cid,
                content_type: content_type.to_string(),
                size: bytes.len() as i32,
            },
            &author,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "hash": hash }))).into_response())
}

/// Serves thumbnail bytes: hash-addressed content never changes, hence the
/// immutable cache; `nosniff` keeps a polyglot an image and nothing else.
async fn thumbnail(
    Extract(state): Extract<SharedState>,
    Path(hash): Path<String>,
) -> Result<Response, ApiError> {
    let hash = hash.to_ascii_lowercase();
    if !is_sha256_hex(&hash) {
        return Err(ApiError::labelled(StatusCode::BAD_REQUEST, "invalid_hash"));
    }
    let Some(row) = state.store.get_thumbnail(&hash).await? else {
        return Err(ApiError::not_found());
    };
    let bytes = match state.storage.retrieve(&row.blob_cid).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            tracing::error!(hash = %hash, cid = %row.blob_cid, "outfit thumbnail row without its blob");
            return Err(ApiError::not_found());
        }
        Err(e) => return Err(ApiError::Internal(format!("blob store: {e}"))),
    };
    let mut res = Response::new(Body::from(bytes));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&row.content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(row.size as u64));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(res)
}
