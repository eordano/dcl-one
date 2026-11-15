//! The route handlers: every one verifies the signed fetch and resolves the
//! scene context first ([`super::resolve_authorized`]), then reads or writes
//! only the resolved [`SceneNamespace`]; an upsert answers its 411/413
//! before the authorization ([`super::resolve_authorized_upsert`]), then, as
//! upstream's schema validator, its 415 and body checks, then the key and the
//! player address. A written value is stored and echoed as the text
//! `JSON.stringify(JSON.parse(body).value)` gives ([`super::js_json`]); values
//! are served as the jsonb text Postgres returns, spliced into the envelope
//! byte for byte (a stored `0`, `false`, `""` or `null` is a 200, never a
//! 404).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use serde_json::{Value, json};

use crate::shared::identity::Address;
use crate::shared::query_params::js_parse_int;

use super::errors::ApiError;
use super::js_json::{self, Doc};
use super::store::{StorageEntry, value_size_bytes};
use super::{AuthPolicy, SharedState, resolve_authorized, resolve_authorized_upsert};

pub const MAX_LIMIT: i64 = 100;
pub const MAX_OFFSET: i64 = 100_000;
pub const CONFIRM_DELETE_ALL_HEADER: &str = "x-confirm-delete-all";

pub const MAX_KEY_LENGTH: usize = 255;

pub const BODY_ENVELOPE_SLACK_BYTES: i64 = 1024;

/// 1..=255 code points (the old varchar(255) measured characters; the
/// schema's `octet_length(key) <= 1020` CHECK is the byte bound of that),
/// and no NUL, which no SQL statement may receive.
pub fn validate_key(key: &str) -> Result<(), ApiError> {
    if key.is_empty() || (key.len() > MAX_KEY_LENGTH && key.chars().count() > MAX_KEY_LENGTH) {
        return Err(ApiError::bad_request(format!(
            "Key must be between 1 and {MAX_KEY_LENGTH} characters"
        )));
    }
    if key.contains('\0') {
        return Err(ApiError::bad_request("Key must not contain NUL characters"));
    }
    Ok(())
}

/// Upstream's body-size middleware: the declared `Content-Length` against
/// the namespace's value cap plus the envelope slack; no `Content-Length`
/// and no `Transfer-Encoding` is an empty body (RFC 9112 §6.3), a chunked
/// or unreadable length a 411.
pub fn check_content_length(headers: &HeaderMap, max_value_size: i64) -> Result<(), ApiError> {
    let content_length = match headers.get(header::CONTENT_LENGTH) {
        Some(v) => v.to_str().ok().and_then(|v| v.trim().parse::<i64>().ok()),
        None if headers.contains_key(header::TRANSFER_ENCODING) => None,
        None => Some(0),
    };
    let Some(content_length) = content_length.filter(|v| *v >= 0) else {
        return Err(ApiError::LengthRequired(
            "Requests with a body must include a valid Content-Length header".to_string(),
        ));
    };
    let max_body_size = max_value_size + BODY_ENVELOPE_SLACK_BYTES;
    if content_length > max_body_size {
        return Err(ApiError::PayloadTooLarge(format!(
            "Request body exceeds the maximum allowed size ({max_body_size} bytes)"
        )));
    }
    Ok(())
}

/// jsonb cannot hold `\u0000`; the escape sequence is refused unless it is
/// itself escaped (`\\u0000` is literal text).
pub fn reject_nul_characters(serialized: &str) -> Result<(), ApiError> {
    let bytes = serialized.as_bytes();
    let needle = b"\\u0000";
    let mut from = 0;
    while let Some(pos) = find_from(bytes, needle, from) {
        let backslashes_before = bytes[..pos]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count();
        if backslashes_before % 2 == 0 {
            return Err(ApiError::bad_request(
                "Values must not contain the \\u0000 (NUL) character",
            ));
        }
        from = pos + 1;
    }
    Ok(())
}

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

#[derive(Debug, Clone)]
pub struct Pagination {
    pub limit: i64,
    pub offset: i64,
    pub prefix: Option<String>,
}

/// URLSearchParams.get uses the first occurrence of a repeated key.
pub(super) fn query_params(query: Option<&str>) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        params
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    params
}

/// As `@dcl/http-commons` 2.0.1: `limit` in 1..=100 (anything else is 100),
/// `offset` >= 0 clamped to [`MAX_OFFSET`] (anything else 0), both parsed with
/// `parseInt` semantics; `prefix` when non-empty; a `page` parameter is
/// ignored, as the old service did.
pub fn parse_pagination(params: &HashMap<String, String>) -> Pagination {
    let int = |name: &str| params.get(name).and_then(|v| js_parse_int(v, false));
    let limit = match int("limit") {
        Some(n) if n > 0 && n <= MAX_LIMIT => n,
        _ => MAX_LIMIT,
    };
    let offset = match int("offset") {
        Some(n) if n >= 0 => n.min(MAX_OFFSET),
        _ => 0,
    };
    let prefix = params.get("prefix").cloned().filter(|s| !s.is_empty());
    Pagination {
        limit,
        offset,
        prefix,
    }
}

pub fn require_confirm_delete_all(headers: &HeaderMap) -> Result<(), ApiError> {
    if headers.get(CONFIRM_DELETE_ALL_HEADER).is_none() {
        return Err(ApiError::bad_request(
            "Missing required header: X-Confirm-Delete-All",
        ));
    }
    Ok(())
}

/// `@dcl/schema-validator-component`'s check on the request's first
/// `Content-Type` (Node keeps only the first of a repeated one).
pub fn is_json_content_type(headers: &HeaderMap) -> bool {
    crate::content::validation::is_json_content_type(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    )
}

/// Which upsert schema the body must satisfy.
#[derive(Clone, Copy)]
pub enum UpsertSchema {
    /// `{ value: <any JSON value> }`.
    Storage,
    /// `{ value: <string> }`.
    Env,
}

/// The schema validator's 415 and body checks: the parsed document and its
/// `value` item.
pub async fn read_upsert_body(
    headers: &HeaderMap,
    body: axum::body::Body,
    max_value_size: i64,
    schema: UpsertSchema,
) -> Result<(Doc, usize), ApiError> {
    if !is_json_content_type(headers) {
        return Err(ApiError::UnsupportedMediaType);
    }
    // The declared length already passed `check_content_length`.
    let limit = usize::try_from(max_value_size + BODY_ENVELOPE_SLACK_BYTES).unwrap_or(usize::MAX);
    let bytes = axum::body::to_bytes(body, limit).await.map_err(|error| {
        let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
        while let Some(current) = cause {
            if current.is::<http_body_util::LengthLimitError>() {
                return ApiError::PayloadTooLarge(format!(
                    "Request body exceeds the maximum allowed size ({limit} bytes)"
                ));
            }
            cause = current.source();
        }
        ApiError::InvalidBody {
            message: "Invalid JSON body".into(),
            data: None,
        }
    })?;
    let doc =
        js_json::parse(&js_json::decode_body(&bytes)).map_err(|message| ApiError::InvalidBody {
            message,
            data: None,
        })?;
    let checked = match schema {
        UpsertSchema::Storage => doc.validate_storage_body(),
        UpsertSchema::Env => doc.validate_env_body(),
    };
    match checked {
        Ok(value) => Ok((doc, value)),
        Err(errors) => Err(ApiError::InvalidBody {
            message: "Invalid JSON body".into(),
            data: Some(errors),
        }),
    }
}

/// `params.player_address.toLowerCase()` then `EthAddress.validate`: never
/// trimmed.
pub fn parse_player(raw: &str) -> Result<Address, ApiError> {
    Address::parse_untrimmed(&raw.to_ascii_lowercase())
        .map_err(|_| ApiError::bad_request("Invalid player address"))
}

#[derive(Debug)]
pub struct RawJson(pub String);

impl axum::response::IntoResponse for RawJson {
    fn into_response(self) -> axum::response::Response {
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            self.0,
        )
            .into_response()
    }
}

pub fn raw_value_response(serialized: &str) -> RawJson {
    RawJson(format!("{{\"value\":{serialized}}}"))
}

pub fn raw_paginated_response(
    entries: &[StorageEntry],
    limit: i64,
    offset: i64,
    total: i64,
) -> RawJson {
    let mut data = String::from("[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            data.push(',');
        }
        data.push_str("{\"key\":");
        data.push_str(&serde_json::to_string(&e.key).expect("strings always serialize"));
        data.push_str(",\"value\":");
        data.push_str(&e.value);
        data.push('}');
    }
    data.push(']');
    RawJson(format!(
        "{{\"data\":{data},\"pagination\":{{\"limit\":{limit},\"offset\":{offset},\"total\":{total}}}}}"
    ))
}

pub fn get_value_response(value: Option<Arc<str>>) -> Result<RawJson, ApiError> {
    match value {
        Some(v) => Ok(raw_value_response(&v)),
        None => Err(ApiError::not_found("Value not found")),
    }
}

/// Body of `GET /usage/world`, `GET /usage/env` and `GET /usage/players/{player}`.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "world-storage/")
)]
pub struct UsageResponse {
    #[serde(rename = "usedBytes")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub used_bytes: i64,
    #[serde(rename = "maxTotalSizeBytes")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub max_total_size_bytes: i64,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "world-storage/")
)]
pub struct PaginationInfo {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub limit: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub offset: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: i64,
}

/// Body of `GET /env` (variable names) and `GET /players` (addresses).
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "world-storage/")
)]
pub struct KeyListResponse {
    pub data: Vec<String>,
    pub pagination: PaginationInfo,
}

/// One row of a values listing: the stored key plus its arbitrary JSON value.
#[cfg(feature = "ts")]
#[derive(ts_rs::TS)]
#[ts(export, export_to = "world-storage/")]
pub struct StorageValueRow {
    pub key: String,
    #[ts(type = "unknown")]
    pub value: (),
}

/// Body of `GET /values` and `GET /players/{player}`, as
/// [`raw_paginated_response`] splices it.
#[cfg(feature = "ts")]
#[derive(ts_rs::TS)]
#[ts(export, export_to = "world-storage/")]
pub struct ValuesListResponse {
    pub data: Vec<StorageValueRow>,
    pub pagination: PaginationInfo,
}

fn original_of(parts: &axum::http::request::Parts) -> Option<&Uri> {
    parts.extensions.get::<OriginalUri>().map(|o| &o.0)
}

pub async fn ping() -> &'static str {
    "pong"
}

pub async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

pub async fn world_get(
    State(state): State<SharedState>,
    Path(key): Path<String>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<RawJson, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::LOGS_ACCESS,
    )
    .await?;
    validate_key(&key)?;
    get_value_response(state.store.world_get(&ctx.namespace, &key).await?)
}

pub async fn world_upsert(
    State(state): State<SharedState>,
    Path(key): Path<String>,
    req: axum::extract::Request,
) -> Result<RawJson, ApiError> {
    let (parts, body) = req.into_parts();
    let ctx = resolve_authorized_upsert(
        &state,
        &parts.headers,
        &parts.uri,
        original_of(&parts),
        AuthPolicy::LOGS_ACCESS,
        state.settings.world_limits.max_value_size_bytes,
    )
    .await?;
    let max = state.settings.world_limits.max_value_size_bytes;
    let (doc, value) = read_upsert_body(&parts.headers, body, max, UpsertSchema::Storage).await?;
    validate_key(&key)?;

    let serialized = doc.stringify(value);
    drop(doc);
    reject_nul_characters(&serialized)?;

    state
        .store
        .world_upsert_with_quota(
            &ctx.namespace,
            &key,
            &serialized,
            state.settings.world_limits,
        )
        .await?;
    Ok(raw_value_response(&serialized))
}

pub async fn world_delete(
    State(state): State<SharedState>,
    Path(key): Path<String>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "delete",
        &uri,
        Some(&original),
        AuthPolicy::LOGS_ACCESS,
    )
    .await?;
    validate_key(&key)?;
    state.store.world_delete(&ctx.namespace, &key).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn world_list(
    State(state): State<SharedState>,
    RawQuery(params): RawQuery,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<RawJson, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::LOGS_ACCESS,
    )
    .await?;
    let p = parse_pagination(&query_params(params.as_deref()));
    let (entries, total) = state
        .store
        .world_list_page(&ctx.namespace, p.limit, p.offset, p.prefix.as_deref())
        .await?;
    Ok(raw_paginated_response(&entries, p.limit, p.offset, total))
}

pub async fn world_clear(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "delete",
        &uri,
        Some(&original),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
    )
    .await?;
    require_confirm_delete_all(&headers)?;
    state.store.world_delete_all(&ctx.namespace).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn player_get(
    State(state): State<SharedState>,
    Path((player, key)): Path<(String, String)>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<RawJson, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::LOGS_ACCESS,
    )
    .await?;
    validate_key(&key)?;
    let player = parse_player(&player)?;
    get_value_response(
        state
            .store
            .player_get(&ctx.namespace, &player, &key)
            .await?,
    )
}

pub async fn player_upsert(
    State(state): State<SharedState>,
    Path((player, key)): Path<(String, String)>,
    req: axum::extract::Request,
) -> Result<RawJson, ApiError> {
    let (parts, body) = req.into_parts();
    let ctx = resolve_authorized_upsert(
        &state,
        &parts.headers,
        &parts.uri,
        original_of(&parts),
        AuthPolicy::LOGS_ACCESS,
        state.settings.player_limits.max_value_size_bytes,
    )
    .await?;
    let max = state.settings.player_limits.max_value_size_bytes;
    let (doc, value) = read_upsert_body(&parts.headers, body, max, UpsertSchema::Storage).await?;
    validate_key(&key)?;
    let player = parse_player(&player)?;

    let serialized = doc.stringify(value);
    drop(doc);
    reject_nul_characters(&serialized)?;

    state
        .store
        .player_upsert_with_quota(
            &ctx.namespace,
            &player,
            &key,
            &serialized,
            state.settings.player_limits,
        )
        .await?;
    Ok(raw_value_response(&serialized))
}

pub async fn player_delete(
    State(state): State<SharedState>,
    Path((player, key)): Path<(String, String)>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "delete",
        &uri,
        Some(&original),
        AuthPolicy::LOGS_ACCESS,
    )
    .await?;
    validate_key(&key)?;
    let player = parse_player(&player)?;
    state
        .store
        .player_delete(&ctx.namespace, &player, &key)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn player_list(
    State(state): State<SharedState>,
    Path(player): Path<String>,
    RawQuery(params): RawQuery,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<RawJson, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::LOGS_ACCESS,
    )
    .await?;
    let player = parse_player(&player)?;
    let p = parse_pagination(&query_params(params.as_deref()));
    let (entries, total) = state
        .store
        .player_list_page(
            &ctx.namespace,
            &player,
            p.limit,
            p.offset,
            p.prefix.as_deref(),
        )
        .await?;
    Ok(raw_paginated_response(&entries, p.limit, p.offset, total))
}

pub async fn player_clear(
    State(state): State<SharedState>,
    Path(player): Path<String>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "delete",
        &uri,
        Some(&original),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
    )
    .await?;
    let player = parse_player(&player)?;
    require_confirm_delete_all(&headers)?;
    state
        .store
        .player_delete_all_for_player(&ctx.namespace, &player)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn players_list(
    State(state): State<SharedState>,
    RawQuery(params): RawQuery,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<KeyListResponse>, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::LOGS_ACCESS,
    )
    .await?;
    let p = parse_pagination(&query_params(params.as_deref()));
    let (players, total) = state
        .store
        .player_list_players_page(&ctx.namespace, p.limit, p.offset)
        .await?;
    Ok(Json(KeyListResponse {
        data: players,
        pagination: PaginationInfo {
            limit: p.limit,
            offset: p.offset,
            total,
        },
    }))
}

pub async fn players_clear_all(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "delete",
        &uri,
        Some(&original),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
    )
    .await?;
    require_confirm_delete_all(&headers)?;
    state.store.player_delete_all(&ctx.namespace).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn env_get(
    State(state): State<SharedState>,
    Path(key): Path<String>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::AUTHORIZED_ADDRESSES_OR_SCOPED_DELEGATION,
    )
    .await?;
    validate_key(&key)?;
    let encryptor = state.encryptor()?;
    match state.store.env_get_enc(&ctx.namespace, &key).await? {
        Some(blob) => Ok(Json(json!({ "value": encryptor.decrypt(&blob)? }))),
        None => Err(ApiError::not_found("Value not found")),
    }
}

pub async fn env_upsert(
    State(state): State<SharedState>,
    Path(key): Path<String>,
    req: axum::extract::Request,
) -> Result<StatusCode, ApiError> {
    let (parts, body) = req.into_parts();
    let ctx = resolve_authorized_upsert(
        &state,
        &parts.headers,
        &parts.uri,
        original_of(&parts),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
        state.settings.env_limits.max_value_size_bytes,
    )
    .await?;
    let max = state.settings.env_limits.max_value_size_bytes;
    let (doc, value) = read_upsert_body(&parts.headers, body, max, UpsertSchema::Env).await?;
    validate_key(&key)?;
    let value = doc
        .string(value)
        .expect("the env schema admits only a string");
    drop(doc);
    let encryptor = state.encryptor()?;

    let size = value_size_bytes(&value);
    let enc = encryptor.encrypt(&value)?;
    state
        .store
        .env_upsert_with_quota(&ctx.namespace, &key, &enc, size, state.settings.env_limits)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn env_delete(
    State(state): State<SharedState>,
    Path(key): Path<String>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "delete",
        &uri,
        Some(&original),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
    )
    .await?;
    validate_key(&key)?;
    state.store.env_delete(&ctx.namespace, &key).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn env_list_keys(
    State(state): State<SharedState>,
    RawQuery(params): RawQuery,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<KeyListResponse>, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
    )
    .await?;
    let p = parse_pagination(&query_params(params.as_deref()));
    let (keys, total) = state
        .store
        .env_list_keys_page(&ctx.namespace, p.limit, p.offset, p.prefix.as_deref())
        .await?;
    Ok(Json(KeyListResponse {
        data: keys,
        pagination: PaginationInfo {
            limit: p.limit,
            offset: p.offset,
            total,
        },
    }))
}

pub async fn env_clear(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "delete",
        &uri,
        Some(&original),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
    )
    .await?;
    require_confirm_delete_all(&headers)?;
    state.store.env_delete_all(&ctx.namespace).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn usage_world(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<UsageResponse>, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::DEFAULT,
    )
    .await?;
    let info = state.store.world_size_info(&ctx.namespace).await?;
    Ok(Json(UsageResponse {
        used_bytes: info.total_size,
        max_total_size_bytes: state.settings.world_limits.max_total_size_bytes,
    }))
}

pub async fn usage_player(
    State(state): State<SharedState>,
    Path(player): Path<String>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<UsageResponse>, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::DEFAULT,
    )
    .await?;
    let player = parse_player(&player)?;
    let info = state
        .store
        .player_size_info(&ctx.namespace, &player)
        .await?;
    Ok(Json(UsageResponse {
        used_bytes: info.total_size,
        max_total_size_bytes: state.settings.player_limits.max_total_size_bytes,
    }))
}

pub async fn usage_env(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<UsageResponse>, ApiError> {
    let ctx = resolve_authorized(
        &state,
        &headers,
        "get",
        &uri,
        Some(&original),
        AuthPolicy::OWNERS_DEPLOYERS_ONLY,
    )
    .await?;
    let info = state.store.env_size_info(&ctx.namespace).await?;
    Ok(Json(UsageResponse {
        used_bytes: info.total_size,
        max_total_size_bytes: state.settings.env_limits.max_total_size_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn key_length_is_validated_in_code_points() {
        assert!(validate_key("k").is_ok());
        assert!(validate_key(&"k".repeat(255)).is_ok());
        assert!(
            validate_key(&"\u{E9}".repeat(255)).is_ok(),
            "255 code points, 510 bytes"
        );
        assert!(matches!(validate_key(""), Err(ApiError::BadRequest(_))));
        assert!(matches!(
            validate_key(&"k".repeat(256)),
            Err(ApiError::BadRequest(_))
        ));
        assert!(matches!(
            validate_key(&"\u{E9}".repeat(256)),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[test]
    fn nul_characters_are_rejected_but_escaped_backslash_text_is_not() {
        let with_nul = serde_json::to_string(&json!({ "a": "b\u{0}c" })).unwrap();
        assert!(matches!(
            reject_nul_characters(&with_nul),
            Err(ApiError::BadRequest(_))
        ));

        let literal_text = serde_json::to_string(&json!({ "a": "\\u0000" })).unwrap();
        assert!(reject_nul_characters(&literal_text).is_ok());

        let backslash_then_nul = serde_json::to_string(&json!({ "a": "\\\u{0}" })).unwrap();
        assert!(reject_nul_characters(&backslash_then_nul).is_err());

        assert!(reject_nul_characters(r#"{"a":"plain"}"#).is_ok());
    }

    #[test]
    fn content_length_precheck_maps_to_411_and_413() {
        let mut headers = HeaderMap::new();
        assert!(
            check_content_length(&headers, 100).is_ok(),
            "no length and no transfer-encoding is an empty body"
        );
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        let err = check_content_length(&headers, 100).unwrap_err();
        assert!(matches!(err, ApiError::LengthRequired(_)));
        assert_eq!(err.into_response().status(), StatusCode::LENGTH_REQUIRED);

        headers.insert("content-length", "not-a-number".parse().unwrap());
        assert!(matches!(
            check_content_length(&headers, 100),
            Err(ApiError::LengthRequired(_))
        ));

        headers.insert(
            "content-length",
            (100 + BODY_ENVELOPE_SLACK_BYTES)
                .to_string()
                .parse()
                .unwrap(),
        );
        assert!(check_content_length(&headers, 100).is_ok());

        headers.insert(
            "content-length",
            (101 + BODY_ENVELOPE_SLACK_BYTES)
                .to_string()
                .parse()
                .unwrap(),
        );
        let err = check_content_length(&headers, 100).unwrap_err();
        assert!(matches!(err, ApiError::PayloadTooLarge(_)));
        assert_eq!(err.into_response().status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn an_undeclared_body_past_the_limit_is_413() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let body = axum::body::Body::from(vec![b' '; 101 + BODY_ENVELOPE_SLACK_BYTES as usize]);
        let err = read_upsert_body(&headers, body, 100, UpsertSchema::Storage)
            .await
            .err()
            .unwrap();
        assert_eq!(err.into_response().status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn an_interrupted_body_is_400_without_transport_details() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let chunks = futures::stream::iter([
            Ok(bytes::Bytes::from_static(br#"{"value":"#)),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "private transport detail",
            )),
        ]);
        let error = read_upsert_body(
            &headers,
            axum::body::Body::from_stream(chunks),
            100,
            UpsertSchema::Storage,
        )
        .await
        .err()
        .unwrap();
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"ok": false, "message": "Invalid JSON body"})
        );
    }

    #[tokio::test]
    async fn schema_error_responses_preserve_lone_surrogates() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        for (body, expected) in [
            (
                r#"{"value":{"\ud800/~":1e400}}"#,
                r#""instancePath":"/value/\ud800~1~0""#,
            ),
            (
                r#"{"value":1,"\udc00":2}"#,
                r#""additionalProperty":"\udc00""#,
            ),
        ] {
            let error = read_upsert_body(
                &headers,
                axum::body::Body::from(body),
                4096,
                UpsertSchema::Storage,
            )
            .await
            .err()
            .unwrap();
            let response = error.into_response();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), 16384)
                .await
                .unwrap();
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains('\u{fffd}'), "{text}");
            assert!(super::super::js::JsValue::parse(text).is_some());
        }
    }

    #[test]
    fn keys_refuse_nul_after_the_length_check() {
        assert_eq!(
            validate_key("a\0b").unwrap_err().to_string(),
            "Key must not contain NUL characters"
        );
        assert_eq!(
            validate_key("").unwrap_err().to_string(),
            "Key must be between 1 and 255 characters"
        );
    }

    #[test]
    fn json_content_types_follow_the_schema_validator() {
        let check = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for v in values {
                headers.append("content-type", v.parse().unwrap());
            }
            is_json_content_type(&headers)
        };
        for ok in [
            "application/json",
            "application/json; charset=utf-8",
            " APPLICATION/JSON ",
            "application/json;",
            "application/vnd.api+json",
            "text/x+json; v=1",
        ] {
            assert!(check(&[ok]), "{ok:?}");
        }
        for refused in [
            "",
            "text/plain",
            "application/jsonx",
            "application/+json",
            "+json",
            "a/b/c+json",
            "application/json, text/plain",
        ] {
            assert!(!check(&[refused]), "{refused:?}");
        }
        assert!(!check(&[]));
        // Node keeps only the first `Content-Type` of a request.
        assert!(check(&["application/json", "text/plain"]));
        assert!(!check(&["text/plain", "application/json"]));
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn pagination_uses_js_whitespace_and_the_first_query_value() {
        let p = parse_pagination(&query_params(Some("limit=5&limit=9&offset=2&offset=7")));
        assert_eq!((p.limit, p.offset), (5, 2));
        // `parseInt` skips U+FEFF but not U+0085.
        let p = parse_pagination(&query_params(Some("limit=%EF%BB%BF7")));
        assert_eq!(p.limit, 7);
        let p = parse_pagination(&query_params(Some("limit=%C2%857")));
        assert_eq!(p.limit, MAX_LIMIT);
    }

    #[test]
    fn pagination_caps_and_coerces_like_upstream() {
        let p = parse_pagination(&params(&[]));
        assert_eq!((p.limit, p.offset), (MAX_LIMIT, 0));
        assert_eq!(
            parse_pagination(&params(&[("limit", "500")])).limit,
            MAX_LIMIT
        );
        let p = parse_pagination(&params(&[
            ("limit", "50"),
            ("offset", "10"),
            ("prefix", "a"),
        ]));
        assert_eq!(
            (p.limit, p.offset, p.prefix.as_deref()),
            (50, 10, Some("a"))
        );
        for bad in ["abc", "0", "-1"] {
            assert_eq!(
                parse_pagination(&params(&[("limit", bad)])).limit,
                MAX_LIMIT
            );
        }
        for bad in ["abc", "-1"] {
            assert_eq!(parse_pagination(&params(&[("offset", bad)])).offset, 0);
        }
        assert_eq!(parse_pagination(&params(&[("prefix", "")])).prefix, None);
    }

    #[test]
    fn pagination_parses_leading_integers_and_clamps_the_offset_like_http_commons() {
        let huge = "1".repeat(21);
        for ((limit, offset), expected) in [
            (("10abc", "0"), (10, 0)),
            (("10", "5.9"), (10, 5)),
            (("10", "999999999999"), (10, MAX_OFFSET)),
            (("10", huge.as_str()), (10, MAX_OFFSET)),
            ((" +7", "-3"), (7, 0)),
            (("-0", "100000"), (MAX_LIMIT, MAX_OFFSET)),
        ] {
            let p = parse_pagination(&params(&[("limit", limit), ("offset", offset)]));
            assert_eq!(
                (p.limit, p.offset),
                expected,
                "limit={limit} offset={offset}"
            );
        }
    }

    #[test]
    fn pagination_ignores_a_page_parameter() {
        let p = parse_pagination(&params(&[("limit", "10"), ("page", "3")]));
        assert_eq!(
            (p.limit, p.offset),
            (10, 0),
            "world-storage listing paginates on limit/offset only"
        );
    }

    #[test]
    fn player_addresses_are_lowercased_never_trimmed() {
        assert_eq!(
            parse_player("0xABCDEF0123456789abcdef0123456789ABCDEF01")
                .unwrap()
                .as_str(),
            "0xabcdef0123456789abcdef0123456789abcdef01"
        );
        for bad in [
            "",
            " ",
            " 0xabcdef0123456789abcdef0123456789abcdef01",
            "0xabcdef0123456789abcdef0123456789abcdef01 ",
            "0x123",
            "not-an-address",
            "0x000000000000000000000000000000000000000g",
            "0000000000000000000000000000000000000000",
        ] {
            assert!(
                matches!(parse_player(bad), Err(ApiError::BadRequest(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn stored_falsy_value_is_200_not_404() {
        for v in [json!(0), json!(false), json!(""), json!(null)] {
            let serialized: Arc<str> = Arc::from(serde_json::to_string(&v).unwrap());
            let body = get_value_response(Some(serialized))
                .expect("stored falsy value should yield a 200 body")
                .0;
            assert_eq!(body, serde_json::to_string(&json!({ "value": v })).unwrap());
        }
    }

    fn gnarly_values() -> Vec<Value> {
        vec![
            json!({"quotes":"a\"b","backslash":"c\\d","nl":"e\nf","tab":"g\th"}),
            json!({"unicode":"h\u{E9}llo \u{1F30D} \u{2028}","empty":{},"nested":{"n":[1,2.5,null,true],"s":""}}),
            json!([1, "two", {"three": 3}, []]),
            json!("\\u0041 literal escape text"),
            json!(-0.5),
        ]
    }

    #[test]
    fn raw_value_splice_is_byte_identical_to_the_decoded_path() {
        for v in gnarly_values() {
            let serialized = serde_json::to_string(&v).unwrap();
            assert_eq!(
                raw_value_response(&serialized).0,
                serde_json::to_string(&json!({ "value": v })).unwrap()
            );
        }
    }

    #[test]
    fn raw_paginated_splice_is_byte_identical_to_the_decoded_path() {
        let keys = ["plain", "with\"quote", "back\\slash", "uni\u{1F511}\n"];
        let entries: Vec<StorageEntry> = keys
            .iter()
            .zip(gnarly_values())
            .map(|(k, v)| StorageEntry {
                key: k.to_string(),
                value: serde_json::to_string(&v).unwrap(),
            })
            .collect();
        let decoded: Vec<Value> = keys
            .iter()
            .zip(gnarly_values())
            .map(|(k, v)| json!({ "key": k, "value": v }))
            .collect();
        assert_eq!(
            raw_paginated_response(&entries, 100, 0, 4).0,
            serde_json::to_string(
                &json!({ "data": decoded, "pagination": { "limit": 100, "offset": 0, "total": 4 } })
            )
            .unwrap()
        );
        assert_eq!(
            raw_paginated_response(&[], 10, 5, 0).0,
            r#"{"data":[],"pagination":{"limit":10,"offset":5,"total":0}}"#
        );
    }

    #[test]
    fn raw_value_splice_passes_postgres_spaced_text_verbatim() {
        let pg_text = "{\"a\": 1, \"b\": [1, 2]}";
        let body = raw_value_response(pg_text).0;
        assert!(body.contains(pg_text));
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({ "value": { "a": 1, "b": [1, 2] } })
        );
    }

    #[test]
    fn absent_key_is_404() {
        let err = get_value_response(None).expect_err("absent key should be an error");
        assert!(matches!(err, ApiError::NotFound(_)));
        assert_eq!(err.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn usage_and_key_list_serialize_like_the_upstream_json() {
        assert_eq!(
            serde_json::to_value(UsageResponse {
                used_bytes: 5,
                max_total_size_bytes: 10
            })
            .unwrap(),
            json!({ "usedBytes": 5, "maxTotalSizeBytes": 10 })
        );
        assert_eq!(
            serde_json::to_value(KeyListResponse {
                data: vec!["a".into()],
                pagination: PaginationInfo {
                    limit: 100,
                    offset: 0,
                    total: 1
                }
            })
            .unwrap(),
            json!({ "data": ["a"], "pagination": { "limit": 100, "offset": 0, "total": 1 } })
        );
    }

    fn json_put(content_type: Option<&str>, body: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(ct) = content_type {
            headers.insert("content-type", ct.parse().unwrap());
        }
        headers.insert("content-length", body.len().to_string().parse().unwrap());
        headers
    }

    async fn read(
        content_type: Option<&str>,
        body: &str,
        schema: UpsertSchema,
    ) -> Result<String, ApiError> {
        let headers = json_put(content_type, body);
        let (doc, value) = read_upsert_body(
            &headers,
            axum::body::Body::from(body.to_string()),
            1024,
            schema,
        )
        .await?;
        Ok(match schema {
            UpsertSchema::Storage => doc.stringify(value),
            UpsertSchema::Env => doc.string(value).unwrap(),
        })
    }

    async fn refusal(err: ApiError) -> (StatusCode, Value) {
        let resp = err.into_response();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn upsert_bodies_answer_like_the_schema_validator() {
        let json = Some("application/json");
        assert_eq!(
            read(
                json,
                r#"{"value":{"b":1.0,"a":[1e2,-0]}}"#,
                UpsertSchema::Storage
            )
            .await
            .unwrap(),
            r#"{"b":1,"a":[100,0]}"#
        );
        assert_eq!(
            read(json, r#"{"value":"s"}"#, UpsertSchema::Env)
                .await
                .unwrap(),
            "s"
        );
        for missing in [None, Some("text/plain")] {
            let err = read(missing, r#"{"value":1}"#, UpsertSchema::Storage)
                .await
                .unwrap_err();
            assert_eq!(
                refusal(err).await,
                (
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    json!({ "ok": false, "message": "Content-Type must be application/json" })
                )
            );
        }
        let err = read(json, "{", UpsertSchema::Storage).await.unwrap_err();
        assert_eq!(
            refusal(err).await,
            (
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "message": "Expected property name or '}' in JSON at position 1 (line 1 column 2)" })
            )
        );
        let err = read(json, r#"{"value":1,"extra":2}"#, UpsertSchema::Storage)
            .await
            .unwrap_err();
        assert_eq!(
            refusal(err).await,
            (
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "message": "Invalid JSON body", "data": [{
                    "instancePath": "", "schemaPath": "#/additionalProperties",
                    "keyword": "additionalProperties", "params": { "additionalProperty": "extra" },
                    "message": "must NOT have additional properties"
                }] })
            )
        );
        let err = read(json, r#"{"value":5}"#, UpsertSchema::Env)
            .await
            .unwrap_err();
        assert_eq!(refusal(err).await.1["data"][0]["message"], "must be string");
    }
}
