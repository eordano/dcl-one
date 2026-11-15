//! `POST /entities` (root and `/content`): the local deploy. A
//! `multipart/form-data` body carries the `entityId` field, the auth chain
//! (an `authChain` JSON field or the `authChain[i][type|payload|signature]`
//! fields, checked as `@dcl/schemas`' `AuthChain.validate` checks it) and
//! the files, the entity file among them. The write deployer
//! ([`super::write_deployer::WriteDeployer::deploy_entity`]) hashes the
//! uploads, refuses a denylisted id, checks the signature, then upstream's
//! server checks, runs the content validator (the LAND leg through
//! `land_authz`) and persists the whole deployment in ONE call of the
//! hash-named deploy function, `content.local_entities` provenance included
//! for a local scene.
//!
//! Answers: 200 `{creationTimestamp}`, the stored `local_timestamp`; an
//! entity already deployed answers its stored one before any signature,
//! TTL or rate-limit check, as upstream's idempotent deploy does. 400
//! `{errors}` for a refused deployment and `{error}` for a malformed form,
//! 413 `{error}` for one past a form limit (upstream's multipart limits and
//! texts, the total capped at [`MAX_DEPLOYMENT_BYTES`]), 408 `{error}` for a
//! body still arriving at the upload timeout (`MULTIPART_UPLOAD_TIMEOUT_MS`,
//! upstream's text with what arrived); 503 `{errors}` with `Retry-After`
//! when this node could not decide (a source unset or down, the store
//! unreadable), its cause logged, not answered, or did not decide a read
//! form within `DEPLOYMENT_PROCESSING_TIMEOUT_MS` (worlds' text). The gates of
//! [`super::writes`] come before any of it (404 read-only, 503 disabled),
//! then the 503 with `Retry-After: 60` while the node is bootstrapping.
//! Before the body is read, in upstream's order: a client address past its
//! daily quota 429, a body that does not fit the in-memory upload budget
//! 503, and a client with its uploads or bytes in flight 429
//! ([`super::upload_admission`], per process), each with `Retry-After`; a
//! body that is not a form, or declares more than a form can hold, then
//! answers 400 or 413. The quota counts a request once its form is read,
//! as upstream's `withDeploymentQuota`, and gives back only the ones the
//! deployer could not decide for its own state (its 503), which upstream
//! keeps. A contract-wallet signature this node cannot check (503) and an
//! unsynced copy refusing the entity's type (503) still count: the client
//! chose them.
//! Ahead of the bootstrapping gate, every request (partial batches
//! included) counts toward upstream's per-client burst limit, 200 per
//! 60 s by default, a 429 `{"error":"Too many requests"}` with
//! `Retry-After` past it; the gateway's coarser `limit_req zone=deploy`
//! (`infra/gateway/gateway.py`) sits above it. The per-pointer TTLs are the
//! deployer's.
//!
//! A form with `partial=true` is a batch of a partial upload
//! ([`super::partial_deployments`]): 202 `{missing}` until the upload is
//! complete, then 200. `?partial=true` declares it before the body, so the
//! quota is not checked on admission but once the form is read (the upload
//! counted once, when created, and a batch refused with a 400 before joining
//! it); the query flag on a form without the field is a 400. Once the fields
//! before the first file are read, a batch from a source that spent its
//! quota and not resuming its signer's own upload, or from an account over
//! its byte rate, is turned away (429) before any file is buffered.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::multipart::MultipartError;
use axum::extract::{FromRequest, Multipart, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use serde_json::{Value, json};

use super::SharedState;
use super::errors::{AppError, AppResult};
use super::partial_deployments;
use super::write_deployer::DeployFailure;
use super::writes;
use crate::auth::MAX_AUTH_CHAIN_LINKS;

/// Upstream's `MAX_UPLOAD_FILE_COUNT` and `MAX_UPLOAD_FILE_SIZE` defaults.
pub const MAX_DEPLOY_FILES: usize = 3000;
pub const MAX_DEPLOY_FILE_BYTES: usize = 100 * 1024 * 1024;

/// The bytes of every file and field value a form may carry (upstream's
/// `MAX_UPLOAD_TOTAL_SIZE`, 2 GiB there): the old `MAX_DEPLOYMENT_SIZE_BYTES`
/// default. The gateway's body cap on the route (`CONTENT_DEPLOY_CAP` in
/// `infra/gateway/gateway.py`) sits just above [`max_deployment_body_bytes`];
/// a gateway test reads these constants.
pub const MAX_DEPLOYMENT_BYTES: usize = 200 * 1024 * 1024;

/// The most framing a part adds (upstream `MULTIPART_PART_FRAMING_BYTES`):
/// a 16 KiB header block plus a boundary line and CRLFs.
const MULTIPART_PART_FRAMING_BYTES: usize = 16 * 1024 + 128;

/// The largest body a form within the limits can have, framing included
/// (upstream `maxMultipartBodySize`); the route's body limit.
pub fn max_deployment_body_bytes(max_fields: usize) -> usize {
    MAX_DEPLOYMENT_BYTES + (MAX_DEPLOY_FILES + max_fields + 1) * MULTIPART_PART_FRAMING_BYTES
}

fn invalid(msg: impl Into<String>) -> AppError {
    AppError::InvalidRequest(msg.into())
}

fn too_large(msg: impl Into<String>) -> AppError {
    AppError::PayloadTooLarge(msg.into())
}

/// Upstream's 413 for a body past [`max_deployment_body_bytes`].
fn body_too_large(max_fields: usize) -> AppError {
    too_large(format!(
        "The request body is too large. The maximum allowed total upload size is {MAX_DEPLOYMENT_BYTES} bytes plus multipart framing ({} bytes in all).",
        max_deployment_body_bytes(max_fields)
    ))
}

/// A form without the `entityId` field.
const MISSING_ENTITY_ID_MESSAGE: &str = "Missing required field: entityId";

/// A body that is not a `multipart/form-data` form.
const NOT_MULTIPART_MESSAGE: &str = "Invalid request: expected a multipart/form-data body";

/// A read error of the form: past the route's body limit, upstream's 413;
/// any other parser failure its fixed 400, the parser's text logged.
fn read_error(e: MultipartError, max_fields: usize) -> AppError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return body_too_large(max_fields);
    }
    tracing::debug!(error = %e, "POST /entities: unreadable multipart form");
    invalid("Invalid multipart/form-data request")
}

/// The chain as the form spells it: an `authChain` JSON array, or the
/// indexed `authChain[i][type|payload|signature]` fields; `None` when
/// neither is present.
pub fn extract_auth_chain_from_fields(
    fields: &BTreeMap<String, String>,
) -> Result<Option<Value>, String> {
    if let Some(chain_str) = fields.get("authChain") {
        let chain: Value =
            serde_json::from_str(chain_str).map_err(|_| "Invalid auth chain".to_string())?;
        let arr = chain
            .as_array()
            .ok_or_else(|| "Invalid auth chain".to_string())?;
        if arr.len() > MAX_AUTH_CHAIN_LINKS {
            return Err(too_long_message());
        }
        return Ok(Some(chain));
    }

    let mut biggest_index: i64 = -1;
    for key in fields.keys() {
        if let Some(rest) = key.strip_prefix("authChain[")
            && let Some(idx_str) = rest.split(']').next()
            && let Ok(idx) = idx_str.parse::<i64>()
            && idx > biggest_index
        {
            biggest_index = idx;
        }
    }

    if biggest_index == -1 {
        return Ok(None);
    }
    if biggest_index >= MAX_AUTH_CHAIN_LINKS as i64 {
        return Err(too_long_message());
    }

    let mut chain = Vec::new();
    for i in 0..=biggest_index {
        let payload = indexed_field(fields, i, "payload")?;
        let signature = indexed_field(fields, i, "signature")?;
        let link_type = indexed_field(fields, i, "type")?;
        chain.push(json!({
            "type": link_type,
            "payload": payload,
            "signature": signature,
        }));
    }
    Ok(Some(Value::Array(chain)))
}

fn indexed_field<'a>(
    fields: &'a BTreeMap<String, String>,
    i: i64,
    part: &str,
) -> Result<&'a String, String> {
    fields
        .get(&format!("authChain[{i}][{part}]"))
        .ok_or_else(|| format!("Missing auth chain element at index {i}"))
}

fn too_long_message() -> String {
    format!("Auth chain is too long; the maximum allowed is {MAX_AUTH_CHAIN_LINKS} elements")
}

/// The `type` values `@dcl/schemas` admits in an auth link.
const AUTH_LINK_TYPES: [&str; 5] = [
    "SIGNER",
    "ECDSA_EPHEMERAL",
    "ECDSA_SIGNED_ENTITY",
    "ECDSA_EIP_1654_EPHEMERAL",
    "ECDSA_EIP_1654_SIGNED_ENTITY",
];

/// `AuthChain.validate` of `@dcl/schemas`: at least one link, each an
/// object with a known `type` and a string `payload`; `signature` a string
/// or null, required on every link but a SIGNER, whose signature is absent,
/// null or empty.
fn is_valid_auth_chain(chain: &Value) -> bool {
    let Some(links) = chain.as_array().filter(|links| !links.is_empty()) else {
        return false;
    };
    links.iter().all(|link| {
        let Some(link) = link.as_object() else {
            return false;
        };
        let Some(link_type) = link
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| AUTH_LINK_TYPES.contains(t))
        else {
            return false;
        };
        let signer = link_type == "SIGNER";
        link.get("payload").is_some_and(Value::is_string)
            && match link.get("signature") {
                None => signer,
                Some(Value::Null) => true,
                Some(Value::String(signature)) => !signer || signature.is_empty(),
                Some(_) => false,
            }
    })
}

/// The chain of a deploy form, as upstream's handler takes it: a form
/// without one is upstream's `No auth chain can be derivated` (sic), and a
/// chain `AuthChain.validate` refuses its `Invalid auth chain`.
fn auth_chain_of(fields: &BTreeMap<String, String>) -> AppResult<Value> {
    let chain = extract_auth_chain_from_fields(fields)
        .map_err(invalid)?
        .ok_or_else(|| invalid("No auth chain can be derivated"))?;
    if !is_valid_auth_chain(&chain) {
        return Err(invalid("Invalid auth chain"));
    }
    Ok(chain)
}

/// A part is a file when it names one or carries a non-text body: the Explorer
/// sends each file as a bare `application/octet-stream` part named by its hash.
fn is_file_part(file_name: Option<&str>, content_type: Option<&str>) -> bool {
    file_name.is_some()
        || content_type.is_some_and(|t| !t.trim_start().to_ascii_lowercase().starts_with("text/"))
}

/// A form as read, or turned away before its files were buffered.
enum Form {
    Read(BTreeMap<String, String>, Vec<(String, Bytes)>),
    Refused(Response),
}

/// Reads the whole form with upstream's multipart limits and answers
/// (`multipart.ts`): text fields by name, at most `max_fields` of up to
/// `max_field_bytes` each, and file parts in order with their field names,
/// each up to [`MAX_DEPLOY_FILE_BYTES`], at most [`MAX_DEPLOY_FILES`] of
/// them, all values together up to [`MAX_DEPLOYMENT_BYTES`]; a limit passed
/// is a 413, a name sent twice a 400. `before_files` sees the fields read
/// before the first file part (the clients send theirs first) and may
/// refuse the form there.
async fn read_form(
    multipart: &mut Multipart,
    max_fields: usize,
    max_field_bytes: usize,
    before_files: impl AsyncFnOnce(&BTreeMap<String, String>) -> Option<Response>,
) -> AppResult<Form> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut files: Vec<(String, Bytes)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut field_count = 0usize;
    let mut total_bytes = 0usize;
    let mut before_files = Some(before_files);
    let duplicate = |name: &str| invalid(format!("Duplicate form field '{name}'"));
    let mut count_total = |bytes: usize| {
        total_bytes = total_bytes.saturating_add(bytes);
        if total_bytes > MAX_DEPLOYMENT_BYTES {
            return Err(too_large(format!(
                "The request body is too large. The maximum allowed total upload size is {MAX_DEPLOYMENT_BYTES} bytes."
            )));
        }
        Ok(())
    };

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| read_error(e, max_fields))?
    {
        let name = field.name().unwrap_or("").to_string();

        if is_file_part(field.file_name(), field.content_type()) {
            if let Some(gate) = before_files.take()
                && let Some(refused) = gate(&fields).await
            {
                return Ok(Form::Refused(refused));
            }
            if files.len() >= MAX_DEPLOY_FILES {
                return Err(too_large(format!(
                    "Too many files in the request. The maximum allowed is {MAX_DEPLOY_FILES}."
                )));
            }
            if !seen.insert(name.clone()) {
                return Err(duplicate(&name));
            }
            // Upstream prints busboy's file name, `undefined` for a part
            // without one (the Explorer names its parts by hash only).
            let file_name = field.file_name().unwrap_or("undefined").to_string();
            let mut buf = BytesMut::new();
            while let Some(chunk) = field.chunk().await.map_err(|e| read_error(e, max_fields))? {
                if buf.len().saturating_add(chunk.len()) > MAX_DEPLOY_FILE_BYTES {
                    return Err(too_large(format!(
                        "File '{file_name}' is too large. The maximum allowed size per file is {MAX_DEPLOY_FILE_BYTES} bytes."
                    )));
                }
                count_total(chunk.len())?;
                buf.extend_from_slice(&chunk);
            }
            files.push((name, buf.freeze()));
        } else {
            if field_count >= max_fields {
                return Err(too_large(format!(
                    "Too many form fields in the request. The maximum allowed is {max_fields}."
                )));
            }
            field_count += 1;
            let mut buf = Vec::new();
            while let Some(chunk) = field.chunk().await.map_err(|e| read_error(e, max_fields))? {
                if buf.len().saturating_add(chunk.len()) > max_field_bytes {
                    return Err(too_large(format!(
                        "Field '{name}' is too large. The maximum allowed size per field is {max_field_bytes} bytes."
                    )));
                }
                buf.extend_from_slice(&chunk);
            }
            if !seen.insert(name.clone()) {
                return Err(duplicate(&name));
            }
            count_total(buf.len())?;
            // busboy decodes a value as UTF-8 with replacement characters.
            fields.insert(name, String::from_utf8_lossy(&buf).into_owned());
        }
    }
    Ok(Form::Read(fields, files))
}

/// The `type` of the uploaded entity file (the JSON upload hashing to
/// `entity_id`), for the unsynced copy's per-type gate; the deployer checks
/// the same file in full.
fn entity_type_of(files: &[Bytes], entity_id: &str) -> Option<String> {
    entity_file_of(files, entity_id).and_then(|v| v.get("type")?.as_str().map(str::to_string))
}

fn entity_file_of(files: &[Bytes], entity_id: &str) -> Option<Value> {
    files
        .iter()
        .filter(|f| f.first() == Some(&b'{'))
        .find(|f| crate::auth::verify_hash(f, entity_id))
        .and_then(|f| serde_json::from_slice::<Value>(f).ok())
}

/// Whether an undecided deploy's quota charge is given back: only when its
/// access check fits one Checker batch and asks only Decentraland's own
/// contracts (LAND, estates, the NAME registrar). A larger request, or a
/// contract the entity names (a collection, a worn item's NFT), can drive the
/// check past its deadline or into a non-revert failure.
fn is_refundable(files: &[Bytes], entity_id: &str) -> bool {
    entity_file_of(files, entity_id).is_some_and(|entity| {
        !has_nul(&entity)
            && !asks_named_contracts(&entity)
            && access_calls_of(&entity) <= crate::validator::onchain::CALLS_PER_BATCH
    })
}

/// Whether a string or key holds a NUL, which the metadata column refuses at the
/// write: that 503 is the request's own.
fn has_nul(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(has_nul),
        Value::Object(map) => map.iter().any(|(k, v)| k.contains('\0') || has_nul(v)),
        _ => false,
    }
}

/// Whether the access check calls contracts the entity names: a wearable's or
/// emote's collection, or an on-chain item a profile or outfit wears.
fn asks_named_contracts(entity: &Value) -> bool {
    if matches!(
        entity.get("type").and_then(Value::as_str),
        Some("wearable" | "emote")
    ) {
        return true;
    }
    let on_chain = |urn: &Value| {
        let urn = urn.as_str().or_else(|| urn.get("urn")?.as_str());
        urn.is_some_and(|u| {
            crate::validator::onchain::items::item_ref(u)
                .on_l1()
                .is_some()
        })
    };
    let worn = |list: Option<&Value>| {
        list.and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(on_chain))
    };
    let Some(metadata) = entity.get("metadata") else {
        return false;
    };
    let avatars = metadata.get("avatars").and_then(Value::as_array);
    let outfits = metadata.get("outfits").and_then(Value::as_array);
    avatars.into_iter().flatten().any(|a| {
        let a = a.get("avatar");
        worn(a.and_then(|a| a.get("wearables"))) || worn(a.and_then(|a| a.get("emotes")))
    }) || outfits
        .into_iter()
        .flatten()
        .any(|o| worn(o.get("outfit").and_then(|o| o.get("wearables"))))
}

/// An upper bound on the on-chain sub-calls the access check asks for `entity`:
/// its pointers (LAND parcels), a wearable's or emote's content hashes, and the
/// names and items its profiles and outfits claim.
fn access_calls_of(entity: &Value) -> usize {
    let len = |v: Option<&Value>| v.and_then(Value::as_array).map_or(0, Vec::len);
    let mut calls = len(entity.get("pointers"));
    if matches!(
        entity.get("type").and_then(Value::as_str),
        Some("wearable" | "emote")
    ) {
        calls += len(entity.get("content"));
    }
    let Some(metadata) = entity.get("metadata") else {
        return calls;
    };
    for avatar in metadata
        .get("avatars")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        calls += usize::from(avatar.get("hasClaimedName") == Some(&Value::Bool(true)));
        let worn = avatar.get("avatar");
        calls += len(worn.and_then(|a| a.get("wearables")));
        calls += len(worn.and_then(|a| a.get("emotes")));
    }
    for outfit in metadata
        .get("outfits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        calls += len(outfit.get("outfit").and_then(|o| o.get("wearables")));
    }
    calls + len(metadata.get("namesForExtraSlots"))
}

pub fn deployment_failure_response(failure: DeployFailure) -> Response {
    let unavailable = !matches!(failure, DeployFailure::Rejected(_, _));
    let (status, errors) = match failure {
        DeployFailure::Rejected(_, errors) => (StatusCode::BAD_REQUEST, errors),
        DeployFailure::Unavailable(errors) | DeployFailure::Unverifiable(errors) => {
            (StatusCode::SERVICE_UNAVAILABLE, errors)
        }
    };
    let response = (status, Json(json!({ "errors": errors }))).into_response();
    if unavailable {
        return with_retry_after(
            response,
            header::HeaderValue::from_static(writes::UNAVAILABLE_RETRY_AFTER_SECS),
        );
    }
    response
}

/// The 503 of a read form not decided within `DEPLOYMENT_PROCESSING_TIMEOUT_MS`
/// (worlds' text). Dropping the deploy there is safe: a client disconnect
/// drops it at the same points, its guards release on drop, stored blobs are
/// content-addressed and the write is one statement, so it either committed
/// (a retry then answers the stored timestamp) or did not.
fn processing_timed_out(timeout: Duration) -> Response {
    tracing::warn!(
        timeout_ms = timeout.as_millis() as u64,
        "POST /entities: deployment processing timed out"
    );
    deployment_failure_response(DeployFailure::Unavailable(vec![format!(
        "Deployment processing exceeded the {}ms deadline.",
        timeout.as_millis()
    )]))
}

/// `response` with a `Retry-After` of `value`.
fn with_retry_after(mut response: Response, value: header::HeaderValue) -> Response {
    response.headers_mut().insert(header::RETRY_AFTER, value);
    response
}

/// 429 with `Retry-After` for a client past its burst limit or daily quota.
fn quota_exceeded_response(retry_after_secs: u64) -> Response {
    with_retry_after(
        super::errors::json_error(StatusCode::TOO_MANY_REQUESTS, "Too many requests"),
        header::HeaderValue::from(retry_after_secs),
    )
}

/// The bytes a body can occupy: its declared length, else the body cap.
fn declared_body_bytes(headers: &HeaderMap) -> u64 {
    declared_length(headers)
        .unwrap_or(u64::MAX)
        .min(MAX_DEPLOYMENT_BYTES as u64)
}

/// The declared `Content-Length`, if any.
fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Any other method on `/entities`: 405 with `Allow: POST`, or, read-only,
/// the not-found of upstream's `READ_ONLY` node, which has no such route.
pub async fn other_method(State(state): State<SharedState>) -> Response {
    if state.is_read_only() {
        return writes::read_only_response();
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, header::HeaderValue::from_static("POST"))],
    )
        .into_response()
}

/// The client the `POST /entities` limits count. Behind the gateway the
/// socket peer is a proxy in `HTTP_TRUSTED_PROXY_CIDRS`, and its single
/// `X-Real-IP` names the client (the proxy itself when missing); any other
/// peer is counted by its own address, since the route is reachable without
/// the gateway and a forwarding header from it is the client's own choice.
/// Without a socket (in-process callers and tests) the forwarded address is
/// read as before.
fn deploy_client(
    headers: &HeaderMap,
    peer: Option<std::net::IpAddr>,
    trusted_proxies: &[ipnet::IpNet],
) -> String {
    use crate::shared::client_ip::{client_ip, is_trusted_proxy, single_real_ip};
    let Some(peer) = peer.map(|p| p.to_canonical()) else {
        return client_ip(headers);
    };
    if is_trusted_proxy(peer, trusted_proxies) {
        single_real_ip(headers).unwrap_or(peer).to_string()
    } else {
        if looks_like_an_untrusted_proxy(headers, peer) {
            warn_untrusted_proxy_once(peer);
        }
        peer.to_string()
    }
}

/// An untrusted private or loopback peer that forwards `X-Real-IP`: most
/// likely the gateway or another backend proxy missing from
/// `HTTP_TRUSTED_PROXY_CIDRS`, so every deployer behind it shares its limits.
/// A direct client on a public address, or a local one sending no header, is
/// not one.
fn looks_like_an_untrusted_proxy(headers: &HeaderMap, peer: std::net::IpAddr) -> bool {
    use crate::shared::client_ip::{is_internal, single_real_ip};
    // A header equal to the peer is the server's own pin, not a proxy's.
    is_internal(peer)
        && headers.contains_key("x-real-ip")
        && single_real_ip(headers) != Some(peer.to_canonical())
}

/// Once per process: a node without the setting cannot tell its proxy from a
/// client, so it says so instead of refusing to start.
fn warn_untrusted_proxy_once(peer: std::net::IpAddr) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            %peer,
            "POST /entities: a private or loopback peer forwards X-Real-IP but is not in \
             HTTP_TRUSTED_PROXY_CIDRS, so its clients share one deploy limit and daily quota; \
             list the backend proxy networks there"
        );
    }
}

pub async fn create_entity(
    State(state): State<SharedState>,
    RawQuery(query): RawQuery,
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> AppResult<Response> {
    let request_started = std::time::Instant::now();
    if let Some(refused) = writes::refuse(&state) {
        return Ok(refused);
    }
    let Some(deployer) = state.deployer().cloned() else {
        return Ok(writes::disabled_response());
    };
    let client_ip = deploy_client(
        &headers,
        connect.map(|axum::Extension(axum::extract::ConnectInfo(addr))| addr.ip()),
        &state.settings.trusted_proxy_cidrs,
    );
    // Upstream's burst limit: every request counts, none is given back.
    if let Err(retry_after) = state.deploy_burst.try_consume(&client_ip) {
        return Ok(quota_exceeded_response(retry_after));
    }
    if !state.sync.takes_some_deploys() {
        return Ok(writes::bootstrapping_response());
    }
    // A declared partial batch is checked against the daily quota once its
    // form is read ([`partial_deployments`]): a source that spent its quota
    // may still resume its own uploads. Any other request of a spent source
    // is turned away here, unread and uncounted.
    let declared_partial = partial_deployments::is_declared(query.as_deref());
    if !declared_partial && let Some(retry_after) = state.deploy_quota.exhausted(&client_ip) {
        return Ok(quota_exceeded_response(retry_after));
    }

    let body_bytes = declared_body_bytes(&headers);
    let Some(_reservation) = state.upload_budget.try_reserve(body_bytes) else {
        return Ok(with_retry_after(
            super::errors::json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                super::upload_admission::BUDGET_EXHAUSTED_MESSAGE,
            ),
            header::HeaderValue::from_static(writes::UNAVAILABLE_RETRY_AFTER_SECS),
        ));
    };
    let Ok(_source_lease) = state.source_uploads.try_acquire(&client_ip, body_bytes) else {
        return Ok(with_retry_after(
            super::errors::json_error(
                StatusCode::TOO_MANY_REQUESTS,
                super::upload_admission::SOURCE_BUSY_MESSAGE,
            ),
            header::HeaderValue::from(super::upload_admission::SOURCE_BUSY_RETRY_AFTER_SECS),
        ));
    };
    let received = Arc::new(AtomicU64::new(0));
    let multipart = Multipart::from_request(counting_body(request, Arc::clone(&received)), &())
        .await
        .map_err(|_| invalid(NOT_MULTIPART_MESSAGE))?;
    let max_fields = state.settings.max_upload_field_count;
    if declared_length(&headers).is_some_and(|n| n > max_deployment_body_bytes(max_fields) as u64) {
        return Err(body_too_large(max_fields));
    }

    let request = Request {
        headers: &headers,
        client_ip: &client_ip,
        declared_partial,
        // Undeclared, a spent source was turned away above.
        quota_exhausted: if declared_partial {
            state.deploy_quota.exhausted(&client_ip)
        } else {
            None
        },
        started: request_started,
        received: &received,
    };
    deploy_form(&state, deployer, &request, multipart).await
}

/// `request` with its body's bytes added to `received` as they arrive.
fn counting_body(
    request: axum::extract::Request,
    received: Arc<AtomicU64>,
) -> axum::extract::Request {
    request.map(|body| {
        axum::body::Body::from_stream(body.into_data_stream().inspect(move |chunk| {
            if let Ok(chunk) = chunk {
                received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
        }))
    })
}

/// Upstream's 408 for a body that did not arrive within `timeout`, with
/// what did arrive (`uploadTimedOutMessage`).
fn upload_timed_out(timeout: Duration, received: u64, declared: Option<u64>) -> Response {
    let timeout_ms = timeout.as_millis() as f64;
    let received_text = match declared {
        Some(declared) => format!("{received} of {declared}"),
        None => received.to_string(),
    };
    let bytes_per_second = if timeout_ms > 0.0 {
        received as f64 * 1000.0 / timeout_ms
    } else {
        0.0
    };
    super::errors::json_error(
        StatusCode::REQUEST_TIMEOUT,
        &format!(
            "The upload did not finish within {} s: received {received_text} bytes ({} KiB/s). Retry on a faster connection or send smaller batches.",
            js_fixed(timeout_ms / 1000.0, 3),
            js_fixed(bytes_per_second / 1024.0, 2),
        ),
    )
}

/// `String(Number(value.toFixed(digits)))`: the value rounded to `digits`
/// decimals, without trailing zeros.
fn js_fixed(value: f64, digits: usize) -> String {
    let fixed = format!("{value:.digits$}");
    if fixed.contains('.') {
        fixed
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        fixed
    }
}

/// The 200 of a deployed entity: its stored `local_timestamp` in ms.
fn created_response(creation_timestamp: i64) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "creationTimestamp": creation_timestamp })),
    )
        .into_response()
}

/// What `deploy_form` needs of the request besides its body.
struct Request<'a> {
    headers: &'a HeaderMap,
    client_ip: &'a str,
    declared_partial: bool,
    /// The daily quota's Retry-After when the source had spent it on arrival.
    quota_exhausted: Option<u64>,
    started: std::time::Instant,
    /// The body's bytes read so far.
    received: &'a AtomicU64,
}

/// An admitted deploy's answer: [`Answer::Undecided`] is the deployer's
/// 503 for this node's own state on a request small enough not to have
/// caused it, which the client's quota gets back.
enum Answer {
    Decided(Response),
    Undecided(Response),
}

/// Reads an admitted form, within the upload timeout, and deploys it.
async fn deploy_form(
    state: &SharedState,
    deployer: std::sync::Arc<super::write_deployer::WriteDeployer>,
    request: &Request<'_>,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let settings = &state.settings;
    let is_partial = |fields: &BTreeMap<String, String>| {
        fields.get("partial").map(String::as_str) == Some("true")
    };
    let read = read_form(
        &mut multipart,
        settings.max_upload_field_count,
        settings.max_upload_field_bytes,
        async |fields: &BTreeMap<String, String>| {
            if !request.declared_partial && !is_partial(fields) {
                return None;
            }
            let entity_id = fields.get("entityId")?;
            let chain = auth_chain_of(fields).ok()?;
            let signer = chain.get(0)?.get("payload")?.as_str()?;
            partial_deployments::admit_before_files(
                &deployer,
                &settings.partial_uploads,
                entity_id,
                signer,
                request.quota_exhausted,
            )
            .await
        },
    );
    let timeout = settings.multipart_upload_timeout;
    let Ok(form) = tokio::time::timeout(timeout, read).await else {
        return Ok(upload_timed_out(
            timeout,
            request.received.load(Ordering::Relaxed),
            declared_length(request.headers),
        ));
    };
    let (fields, files) = match form? {
        Form::Read(fields, files) => (fields, files),
        Form::Refused(response) => return Ok(response),
    };
    if is_partial(&fields) {
        let answer = partial_batch(state, &deployer, request, fields, files).await;
        return Ok(answer.unwrap_or_else(IntoResponse::into_response));
    }
    // Otherwise the query flag would carry a regular deploy past the quota.
    if request.declared_partial {
        return Err(invalid(
            "The 'partial=true' query parameter requires the 'partial=true' form field",
        ));
    }
    // Counted once the form is read, as upstream's `withDeploymentQuota`:
    // a form refused by the parser or its limits is not.
    let charge = match state.deploy_quota.try_consume(request.client_ip) {
        Ok(charge) => charge,
        Err(retry_after) => return Ok(quota_exceeded_response(retry_after)),
    };
    let deploy = deploy_regular(state, &deployer, request, fields, files);
    let Ok(answer) = tokio::time::timeout(settings.deployment_processing_timeout, deploy).await
    else {
        // Counted: the client may have chosen what stalled (its contract
        // wallet's RPC), and the deploy may yet have committed.
        return Ok(processing_timed_out(settings.deployment_processing_timeout));
    };
    Ok(match answer {
        Ok(Answer::Decided(response)) => response,
        Ok(Answer::Undecided(response)) => {
            if let Some(charge) = charge {
                charge.refund();
            }
            response
        }
        Err(e) => e.into_response(),
    })
}

/// A regular (single-request) deploy of a read form.
async fn deploy_regular(
    state: &SharedState,
    deployer: &super::write_deployer::WriteDeployer,
    request: &Request<'_>,
    fields: BTreeMap<String, String>,
    files: Vec<(String, Bytes)>,
) -> AppResult<Answer> {
    let headers = request.headers;
    let request_started = request.started;
    let entity_id = fields
        .get("entityId")
        .ok_or_else(|| invalid(MISSING_ENTITY_ID_MESSAGE))?
        .clone();
    let auth_chain = auth_chain_of(&fields)?;
    // Deploys are idempotent: a replay answers the stored timestamp with no
    // signature, TTL or rate-limit check (its chain may have expired since).
    // A failed lookup goes on to the deploy, whose write answers a deployed
    // entity the same way.
    match deployer.deployed_timestamp(&entity_id).await {
        Ok(Some(creation_timestamp)) => {
            tracing::info!(entity_id = %entity_id, "POST /entities - Entity already deployed");
            return Ok(Answer::Decided(created_response(creation_timestamp)));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(entity_id = %entity_id, error = %e, "POST /entities - replay lookup failed");
        }
    }
    let files: Vec<Bytes> = files.into_iter().map(|(_, bytes)| bytes).collect();
    if !state.sync.takes_deploys() {
        let Some(entity_type) = entity_type_of(&files, &entity_id) else {
            return Ok(Answer::Decided(deployment_failure_response(
                DeployFailure::Rejected(
                    crate::write_deployer::RejectionKind::Validation,
                    vec![
                        "The entity file was not part of the uploaded files or has no type."
                            .to_string(),
                    ],
                ),
            )));
        };
        if !state.sync.takes_deploys_of(&entity_type) {
            return Ok(Answer::Decided(writes::bootstrapping_response()));
        }
    }

    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    tracing::info!(
        entity_id = %entity_id,
        user_agent = %user_agent,
        file_count = files.len(),
        "POST /entities - Deploying entity (multipart)"
    );

    let refundable = is_refundable(&files, &entity_id);
    let multipart_us = request_started.elapsed().as_micros() as u64;
    let deploy_started = std::time::Instant::now();
    match deployer
        .deploy_entity(files, &entity_id, auth_chain, "LOCAL")
        .await
    {
        Ok(creation_timestamp) => {
            tracing::info!(entity_id = %entity_id, "POST /entities - Deployment successful");
            state.deployments_cache.clear();
            tracing::debug!(
                target: "catalyst_perf",
                phase = "publish",
                entity_id,
                multipart_us,
                deploy_us = deploy_started.elapsed().as_micros() as u64,
                elapsed_us = request_started.elapsed().as_micros() as u64
            );
            Ok(Answer::Decided(created_response(creation_timestamp)))
        }
        Err(failure) => {
            tracing::warn!(entity_id = %entity_id, errors = ?failure.errors(), "POST /entities - Deployment failed");
            let undecided = refundable && matches!(failure, DeployFailure::Unavailable(_));
            let response = deployment_failure_response(failure);
            Ok(if undecided {
                Answer::Undecided(response)
            } else {
                Answer::Decided(response)
            })
        }
    }
}

/// A batch of a partial upload. As upstream's `withDeploymentQuota`, the
/// daily quota counts the upload once, when its first batch creates it, and
/// a batch refused with a 400 before joining its upload (a missing
/// `entityId` or a bad chain included); its 429s, 503s and replays are not
/// counted. A spent source may only resume the signer's own upload.
async fn partial_batch(
    state: &SharedState,
    deployer: &super::write_deployer::WriteDeployer,
    request: &Request<'_>,
    fields: BTreeMap<String, String>,
    files: Vec<(String, Bytes)>,
) -> AppResult<Response> {
    if !state.sync.takes_deploys_of("scene") {
        return Ok(writes::bootstrapping_response());
    }
    // Counted for good: the charge is never refunded.
    let count = || {
        let _ = state.deploy_quota.try_consume(request.client_ip);
    };
    let Some(entity_id) = fields.get("entityId").cloned() else {
        count();
        return Err(invalid(MISSING_ENTITY_ID_MESSAGE));
    };
    let auth_chain = match auth_chain_of(&fields) {
        Ok(chain) => chain,
        Err(e) => {
            count();
            return Err(e);
        }
    };
    tracing::info!(
        entity_id = %entity_id,
        file_count = files.len(),
        "POST /entities - Partial deployment batch"
    );
    let batch = partial_deployments::Batch {
        entity_id,
        auth_chain,
        files,
        requested_at_ms: chrono::Utc::now().timestamp_millis()
            - request.started.elapsed().as_millis() as i64,
    };
    let mut admission = partial_deployments::Admission::default();
    let timeout = state.settings.deployment_processing_timeout;
    let response = tokio::time::timeout(
        timeout,
        partial_deployments::handle(
            deployer,
            &state.settings.partial_uploads,
            batch,
            request.quota_exhausted,
            &mut admission,
        ),
    )
    .await
    .unwrap_or_else(|_| processing_timed_out(timeout));
    if admission.created || (response.status() == StatusCode::BAD_REQUEST && !admission.admitted) {
        count();
    }
    if response.status() == StatusCode::OK {
        state.deployments_cache.clear();
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn link_fields(count: usize) -> BTreeMap<String, String> {
        let mut f = BTreeMap::new();
        for i in 0..count {
            f.insert(format!("authChain[{i}][type]"), "SIGNER".to_string());
            f.insert(format!("authChain[{i}][payload]"), format!("0xp{i}"));
            f.insert(format!("authChain[{i}][signature]"), format!("0xs{i}"));
        }
        f
    }

    #[test]
    fn a_private_peer_forwarding_x_real_ip_looks_like_an_untrusted_proxy() {
        let mut forwarded = HeaderMap::new();
        forwarded.insert("x-real-ip", header::HeaderValue::from_static("203.0.113.9"));
        for peer in [
            "10.1.2.3",
            "172.18.0.4",
            "192.168.1.2",
            "127.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "::1",
            "fd00::4",
            "fe80::1",
            "::ffff:172.18.0.4",
        ] {
            let peer = peer.parse().unwrap();
            assert!(looks_like_an_untrusted_proxy(&forwarded, peer), "{peer}");
            assert!(
                !looks_like_an_untrusted_proxy(&HeaderMap::new(), peer),
                "no header: {peer}"
            );
        }
        for peer in ["198.51.100.7", "100.128.0.1", "2001:db8::1"] {
            let peer = peer.parse().unwrap();
            assert!(!looks_like_an_untrusted_proxy(&forwarded, peer), "{peer}");
        }
        let mut pinned = HeaderMap::new();
        pinned.insert("x-real-ip", header::HeaderValue::from_static("172.18.0.4"));
        assert!(
            !looks_like_an_untrusted_proxy(&pinned, "172.18.0.4".parse().unwrap()),
            "the server's own pin"
        );
    }

    #[test]
    fn access_calls_count_parcels_names_and_items() {
        let scene = serde_json::json!({"type": "scene", "pointers": ["0,0", "0,1"]});
        assert_eq!(access_calls_of(&scene), 2);
        let profile = serde_json::json!({
            "type": "profile",
            "pointers": ["0xabc"],
            "metadata": {"avatars": [
                {"hasClaimedName": true, "avatar": {"wearables": ["a", "b"], "emotes": [{"urn": "c"}]}},
                {"hasClaimedName": false, "avatar": {"wearables": []}}
            ]}
        });
        assert_eq!(access_calls_of(&profile), 5);
        let outfits = serde_json::json!({
            "type": "outfits",
            "pointers": ["0xabc:outfits"],
            "metadata": {"outfits": [{"outfit": {"wearables": ["a", "b"]}}], "namesForExtraSlots": ["n"]}
        });
        assert_eq!(access_calls_of(&outfits), 4);
        let wearable =
            serde_json::json!({"type": "wearable", "pointers": ["urn"], "content": [{}, {}]});
        assert_eq!(access_calls_of(&wearable), 3);
    }

    #[test]
    fn metadata_the_write_refuses_is_never_refunded() {
        let with = |description: &str| {
            serde_json::json!({"type": "scene", "pointers": ["0,0"],
                "metadata": {"avatars": [{"description": description}]}})
        };
        assert!(has_nul(&with("escaped \0 is invalid")));
        assert!(!has_nul(&with(r"literal \u0000 is valid")));
        let key = serde_json::json!({"metadata": {"a\u{0}b": 1}});
        assert!(has_nul(&key));
    }

    #[test]
    fn checks_on_contracts_the_entity_names_are_never_refunded() {
        let wearing = |urn: &str| {
            serde_json::json!({
                "type": "profile",
                "pointers": ["0xabc"],
                "metadata": {"avatars": [{"avatar": {"wearables": [urn], "emotes": []}}]}
            })
        };
        assert!(!asks_named_contracts(&wearing(
            "urn:decentraland:off-chain:base-avatars:eyebrows_00"
        )));
        assert!(asks_named_contracts(&wearing(
            "urn:decentraland:matic:collections-v2:0x0002000000000000000000000000000000000001:0:1"
        )));
        let emote = serde_json::json!({"type": "emote", "pointers": ["urn"]});
        assert!(asks_named_contracts(&emote));
        let scene = serde_json::json!({"type": "scene", "pointers": ["0,0"]});
        assert!(!asks_named_contracts(&scene));
    }

    #[test]
    fn a_part_is_a_file_when_named_as_one_or_not_text() {
        assert!(is_file_part(Some("profile.json"), None));
        assert!(is_file_part(Some("face.png"), Some("image/png")));
        assert!(is_file_part(None, Some("application/octet-stream")));
        assert!(!is_file_part(None, None));
        assert!(!is_file_part(None, Some("text/plain; charset=utf-8")));
        assert!(!is_file_part(None, Some("Text/Plain")));
    }

    #[test]
    fn extract_auth_chain_from_fields_parses_indexed_keys() {
        let f = fields(&[
            ("authChain[0][type]", "SIGNER"),
            ("authChain[0][payload]", "0xabc"),
            ("authChain[0][signature]", ""),
            ("authChain[1][type]", "ECDSA_SIGNED_ENTITY"),
            ("authChain[1][payload]", "QmEntity"),
            ("authChain[1][signature]", "0xdeadbeef"),
        ]);
        let out = extract_auth_chain_from_fields(&f).expect("parser ok");
        let arr = out.expect("Some(array)");
        let arr = arr.as_array().expect("is array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "SIGNER");
        assert_eq!(arr[0]["payload"], "0xabc");
        assert_eq!(arr[0]["signature"], "");
        assert_eq!(arr[1]["type"], "ECDSA_SIGNED_ENTITY");
        assert_eq!(arr[1]["payload"], "QmEntity");
        assert_eq!(arr[1]["signature"], "0xdeadbeef");
    }

    #[test]
    fn extract_auth_chain_rejects_mixed_keys() {
        let f = fields(&[
            ("authChain[0][type]", "SIGNER"),
            ("authChain[ABC][type]", "SIGNER"),
            ("authChain[ABC][payload]", "0xabc"),
            ("authChain[ABC][signature]", ""),
        ]);
        let err = extract_auth_chain_from_fields(&f)
            .expect_err("missing payload/signature at index 0 must error");
        assert!(
            err.to_lowercase().contains("missing auth chain element"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn extract_auth_chain_caps_at_max_links() {
        let err = extract_auth_chain_from_fields(&link_fields(MAX_AUTH_CHAIN_LINKS + 1))
            .expect_err("index loop must reject more than the max allowed links");
        assert!(
            err.contains("Auth chain is too long"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_auth_chain_json_caps_at_max_links() {
        let links: Vec<Value> = (0..=MAX_AUTH_CHAIN_LINKS)
            .map(|i| json!({ "type": "SIGNER", "payload": format!("0x{i}"), "signature": "" }))
            .collect();
        let chain = serde_json::to_string(&Value::Array(links)).unwrap();
        let f = fields(&[("authChain", chain.as_str())]);
        let err = extract_auth_chain_from_fields(&f)
            .expect_err("json array longer than the max must be rejected");
        assert!(
            err.contains("Auth chain is too long"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_auth_chain_empty_returns_none() {
        let f = fields(&[("entityId", "Qm123"), ("someOtherField", "x")]);
        assert!(
            extract_auth_chain_from_fields(&f)
                .expect("parser ok")
                .is_none()
        );
    }

    #[test]
    fn extract_auth_chain_from_json_string_works() {
        let f = fields(&[(
            "authChain",
            r#"[{"type":"SIGNER","payload":"0x1","signature":""}]"#,
        )]);
        let arr = extract_auth_chain_from_fields(&f)
            .expect("parser ok")
            .expect("Some(array)");
        let arr = arr.as_array().expect("is array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "SIGNER");
    }

    #[test]
    fn extract_auth_chain_rejects_invalid_json() {
        for bad in ["not-json", r#"{"a":1}"#] {
            assert_eq!(
                extract_auth_chain_from_fields(&fields(&[("authChain", bad)])),
                Err("Invalid auth chain".to_string())
            );
        }
    }

    #[test]
    fn a_chain_is_taken_as_auth_chain_validate_takes_it() {
        let chain_of = |chain: &str| auth_chain_of(&fields(&[("authChain", chain)]));
        let refused = |chain: &str| chain_of(chain).unwrap_err().to_string();
        for bad in [
            "[]",
            "[null]",
            r#"[{"payload":"0x1"}]"#,
            r#"[{"type":"SIGNER"}]"#,
            r#"[{"type":"SIGNER","payload":1}]"#,
            r#"[{"type":"OTHER","payload":"0x1","signature":""}]"#,
            r#"[{"type":"SIGNER","payload":"0x1","signature":"0xsig"}]"#,
            r#"[{"type":"SIGNER","payload":"0x1"},{"type":"ECDSA_SIGNED_ENTITY","payload":"bafy"}]"#,
            r#"[{"type":"SIGNER","payload":"0x1"},{"type":"ECDSA_SIGNED_ENTITY","payload":"bafy","signature":1}]"#,
        ] {
            assert_eq!(refused(bad), "Invalid auth chain", "{bad}");
        }
        for good in [
            r#"[{"type":"SIGNER","payload":"0x1"}]"#,
            r#"[{"type":"SIGNER","payload":"0x1","signature":null}]"#,
            r#"[{"type":"SIGNER","payload":"0x1","signature":""},{"type":"ECDSA_EPHEMERAL","payload":"p","signature":"0xs"},{"type":"ECDSA_SIGNED_ENTITY","payload":"bafy","signature":null}]"#,
            r#"[{"type":"SIGNER","payload":"0x1","signature":""},{"type":"ECDSA_EIP_1654_SIGNED_ENTITY","payload":"bafy","signature":"0xs","extra":1}]"#,
        ] {
            assert!(chain_of(good).is_ok(), "{good}");
        }
        // The legacy pair is no chain, as upstream's handler takes it.
        for form in [
            fields(&[("ethAddress", "0xabc"), ("signature", "0xsig")]),
            fields(&[("entityId", "bafy")]),
        ] {
            assert_eq!(
                auth_chain_of(&form).unwrap_err().to_string(),
                "No auth chain can be derivated"
            );
        }
    }

    #[test]
    fn an_upload_timeout_names_what_arrived_as_upstream_does() {
        assert_eq!(js_fixed(300.0, 3), "300");
        assert_eq!(js_fixed(0.05, 3), "0.05");
        assert_eq!(js_fixed(1.5, 2), "1.5");
        assert_eq!(js_fixed(0.0, 2), "0");
    }

    #[tokio::test]
    async fn a_failure_keeps_its_kind_and_an_undecidable_one_carries_retry_after() {
        use crate::write_deployer::RejectionKind;
        for kind in [
            RejectionKind::Validation,
            RejectionKind::PointerContention,
            RejectionKind::RateLimited,
            RejectionKind::Superseded,
        ] {
            let rejected =
                deployment_failure_response(DeployFailure::Rejected(kind, vec!["no".into()]));
            assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
            assert!(rejected.headers().get(header::RETRY_AFTER).is_none());
            let body = axum::body::to_bytes(rejected.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({ "errors": ["no"] })
            );
        }
        let unavailable =
            deployment_failure_response(DeployFailure::Unavailable(vec!["down".into()]));
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unavailable.headers()[header::RETRY_AFTER], "30");
    }
}
