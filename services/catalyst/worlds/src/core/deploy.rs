//! `POST /entities`: a world publish. The multipart form is read under the
//! shared upload budgets, then checked in the old service's order, the
//! first failure answering `{error, message}` with "Deployment failed: …":
//! the auth chain over the entity id (through `crate::auth` with the shared
//! EIP-1654 validator), the entity and scene schemas, the scene type, the
//! file count, the timestamp window, the `.dcl.eth` name, deprecated
//! config, parcel bounds and canonical pointers, the world's parcel cap
//! (`Policy::world_limits`), the DAO deny list, the publish authorized by
//! `worlds::policy::Policy::can_deploy` (the NAME/personal owner replaces
//! every overlapping scene; a deployment grantee exactly the overlapping
//! scenes its grant covers), the minimap, skybox and navmap files, the
//! uploaded files (none extra, none missing) and the scene size cap (the
//! owner's remaining storage allowance, `CoreState::max_scene_size`). The
//! files go to the ONE blob store and the scene row lands in
//! `worlds.world_scenes` in one transaction (`store::deploy_scene`). A
//! `partial=true` batch is staged by [`super::partial`] instead.
//!
//! Why not the content write path's statement: `content.deployments` /
//! `content.active_pointers` key active scenes by parcel, which is Genesis
//! City's authority; every world has its own `0,0`, so world scenes keep
//! the `world_scenes` relation (stage 4) and share the write path's
//! ingredients (hashing, chain verification, the blob store), not its rows.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};

use crate::auth::{AuthChain, AuthError, AuthLinkType, is_eth_address};
use crate::comms::world_identity::{DCL_ETH_SUFFIX, ParcelSet, WorldName};
use crate::content::entity::is_ipfs_v2;
use crate::policy::Deploy;
use crate::shared::identity::{Address, AuthorityNotEstablished};

use super::deploy_form::{self, DeployForm};
use super::deploy_validate::{
    MAX_ENTITY_FILE_SIZE_BYTES, STRING_EXPECTED, canon_pointer_set, entity_file_too_large_error,
    entity_schema_error, extract_auth_chain_from_fields, is_canonical_parcel_set,
    missing_minimap_images, missing_skybox_textures, validate_navmap_thumbnail,
    validate_parcel_in_bounds,
};
use super::metrics as worlds_metrics;
use super::partial;
use super::store::{self, SceneDeployment};
use super::upload_limits::DeployRejection;
use super::upload_spool::{self, SpooledFile};
use super::{SharedState, upload_limits};

/// Payload cap for a deployment: every file chunk and field value, multipart framing excluded.
pub const MAX_UPLOAD_SIZE_BYTES: usize = 350 * 1024 * 1024;

/// Wire-size cap for the whole multipart body; enforced on the Content-Length precheck and as this route's axum body limit.
pub const MAX_UPLOAD_WIRE_SIZE_BYTES: usize = MAX_UPLOAD_SIZE_BYTES + 10 * 1024 * 1024;

const _: () = assert!(MAX_UPLOAD_WIRE_SIZE_BYTES >= MAX_UPLOAD_SIZE_BYTES);

pub(super) const ENTITY_TTL_MS: i64 = 300_000;

/// Reject a deployment dated more than this far in the future: a garbage or clock-skewed
/// timestamp must not let an entity stay deployable/replayable indefinitely. Kept aligned
/// with the Catalyst request TTL forwards guard.
pub(super) const MAX_DEPLOYMENT_FUTURE_SKEW_MS: i64 = 15 * 60 * 1000;

/// `MAX_FILE_COUNT`'s default.
pub const DEFAULT_MAX_FILE_COUNT: usize = 10_000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct DeploySuccess {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub creation_timestamp: i64,
    pub message: String,
}

fn declared_length_exceeds_limit(declared_len: u64) -> bool {
    declared_len > MAX_UPLOAD_WIRE_SIZE_BYTES as u64
}

/// The old service's `{error, message}` answer of a refused deploy; a 500 is
/// `@dcl/http-commons`' bare `{"error":"Internal Server Error"}`, so its message goes to the
/// log instead of the client. This is the one log line of a 500: callers put their context in
/// the message.
pub(super) fn rejection(status: StatusCode, message: impl Into<String>) -> Response {
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        let message: String = message.into();
        tracing::error!(%message, "deploy: internal error");
        return (status, Json(json!({ "error": "Internal Server Error" }))).into_response();
    }
    let error = match status {
        StatusCode::BAD_REQUEST => "Bad request",
        other => other.canonical_reason().unwrap_or("Error"),
    };
    (
        status,
        Json(DeployRejection {
            error: error.to_string(),
            message: message.into(),
        }),
    )
        .into_response()
}

/// A request refused before validation: 400.
pub(super) fn err_one(message: impl Into<String>) -> Response {
    rejection(StatusCode::BAD_REQUEST, message)
}

/// A size or count limit of the multipart body: 413, as the old service.
pub(super) fn payload_too_large(message: impl Into<String>) -> Response {
    upload_limits::too_large(message).into_response()
}

/// A refused publish: its status and message.
pub(super) struct Refusal(pub(super) StatusCode, pub(super) String);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        rejection(self.0, self.1)
    }
}

/// A failed validation, worded as the old service.
pub(super) fn failed(message: impl std::fmt::Display) -> Refusal {
    Refusal(
        StatusCode::BAD_REQUEST,
        format!("Deployment failed: {message}"),
    )
}

pub(super) fn forbidden(message: impl Into<String>) -> Refusal {
    Refusal(StatusCode::FORBIDDEN, message.into())
}

pub(super) fn internal(message: impl Into<String>) -> Refusal {
    Refusal(StatusCode::INTERNAL_SERVER_ERROR, message.into())
}

pub(super) fn unavailable(message: impl Into<String>) -> Refusal {
    Refusal(StatusCode::SERVICE_UNAVAILABLE, message.into())
}

/// Upstream's newer-scene refusal, checked before anything is stored or staged: an overlapping
/// scene with a greater entity timestamp (ties by entity id) answers 400. Shared by the
/// single-request publish and the first partial batch.
pub(super) async fn refuse_if_newer_scene(
    state: &super::CoreState,
    world: &WorldName,
    parcels: &[String],
    entity_id: &str,
    entity: &Value,
) -> Result<(), Response> {
    let timestamp = store::entity_timestamp_value(entity).map_err(IntoResponse::into_response)?;
    let newer = match state.pool.acquire().await {
        Ok(mut conn) => {
            store::newer_scene_deployed(&mut conn, world, parcels, entity_id, timestamp).await
        }
        Err(e) => Err(e),
    };
    match newer {
        Ok(false) => Ok(()),
        Ok(true) => Err(failed(partial::NEWER_SCENE_MESSAGE).into_response()),
        Err(e) => Err(internal(format!("newer-scene check: {e}")).into_response()),
    }
}

pub async fn deploy_entity(State(state): State<SharedState>, request: Request) -> Response {
    // A partial upload's first batch is admitted at its arrival, before the body is read.
    let arrived_ms = chrono::Utc::now().timestamp_millis();
    let (parts, body) = request.into_parts();
    let (uri, headers) = (parts.uri.clone(), parts.headers.clone());
    let limits = &state.settings.upload_limits;
    let declared: Option<u64> = match upload_limits::declared_content_length(&headers) {
        upload_limits::DeclaredContentLength::Invalid => {
            return err_one(upload_limits::INVALID_CONTENT_LENGTH_MESSAGE);
        }
        upload_limits::DeclaredContentLength::Absent => None,
        upload_limits::DeclaredContentLength::Bytes(n) => Some(n),
    };
    if let Some(len) = declared
        && declared_length_exceeds_limit(len)
    {
        return payload_too_large(upload_limits::PAYLOAD_TOO_LARGE_MESSAGE);
    }

    let _source = match upload_limits::admit_source(
        &headers,
        limits,
        worlds_metrics::ENTITIES_ROUTE,
        MAX_UPLOAD_SIZE_BYTES as u64,
    ) {
        Ok(lease) => lease,
        Err(_) => return upload_limits::source_limit_response(),
    };

    let Some(_slot) = upload_limits::try_acquire_upload_slot(limits.max_concurrent_uploads) else {
        worlds_metrics::upload_rejected(worlds_metrics::ENTITIES_ROUTE, "concurrency");
        tracing::warn!(
            active = upload_limits::active_uploads(),
            max = limits.max_concurrent_uploads,
            "POST /entities shed: concurrent-upload cap exceeded"
        );
        return upload_limits::shed_response(upload_limits::BYTES_SHED_MESSAGE);
    };

    if let Some(message) = deploy_form::content_type_error(&headers) {
        worlds_metrics::upload_rejected(worlds_metrics::ENTITIES_ROUTE, "invalid_multipart");
        return err_one(message);
    }
    // The wire bytes received, for the timeout's account of the upload.
    let wire = Arc::new(AtomicU64::new(0));
    let counted = {
        let wire = wire.clone();
        body.into_data_stream().inspect(move |chunk| {
            if let Ok(chunk) = chunk {
                wire.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
        })
    };
    let multipart =
        match Multipart::from_request(Request::from_parts(parts, Body::from_stream(counted)), &())
            .await
        {
            Ok(multipart) => multipart,
            Err(rejection) => return err_one(rejection.body_text()),
        };

    let mut bytes_lease = upload_limits::reserve_in_flight();
    let mut files_lease = upload_limits::reserve_in_flight_files();
    let spool_root = upload_spool::spool_root(&state.storage);
    let receiving = Instant::now();
    let received = |outcome: &'static str, bytes: u64| {
        worlds_metrics::upload_received(
            worlds_metrics::ENTITIES_ROUTE,
            declared.is_some(),
            outcome,
            receiving.elapsed(),
            bytes,
        )
    };

    let form = match tokio::time::timeout(
        Duration::from_millis(limits.multipart_upload_timeout_ms),
        deploy_form::read_deploy_form(
            multipart,
            &mut bytes_lease,
            &mut files_lease,
            limits.max_in_flight_upload_bytes,
            limits.max_in_flight_upload_files,
            &spool_root,
        ),
    )
    .await
    {
        Ok(Ok(form)) => {
            received("completed", bytes_lease.reserved());
            form
        }
        Ok(Err(resp)) => {
            received("rejected", bytes_lease.reserved());
            return *resp;
        }
        Err(_) => {
            received("timeout", bytes_lease.reserved());
            worlds_metrics::upload_rejected(worlds_metrics::ENTITIES_ROUTE, "timeout");
            tracing::warn!(
                timeout_ms = limits.multipart_upload_timeout_ms,
                "POST /entities: multipart upload timed out"
            );
            return upload_limits::timeout_response(&upload_limits::describe_upload_timeout(
                limits.multipart_upload_timeout_ms,
                wire.load(Ordering::Relaxed),
                declared,
            ));
        }
    };

    let is_partial = form.fields.get("partial").is_some_and(|v| v == "true");
    let mode = if is_partial {
        worlds_metrics::PARTIAL_MODE
    } else {
        worlds_metrics::REGULAR_MODE
    };
    // A batch may also be declared with `?partial=true`; it must agree with the form, as upstream.
    if !is_partial && partial::query_declares_partial(&uri) {
        let resp = err_one(partial::QUERY_WITHOUT_FIELD_MESSAGE);
        worlds_metrics::deployment_processed(mode, resp.status(), Duration::ZERO);
        return resp;
    }

    let timeout_ms = limits.deployment_processing_timeout_ms;
    let processing = Instant::now();
    let resp = match tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        deploy_entity_inner(&state, &headers, form, is_partial, arrived_ms),
    )
    .await
    {
        Ok(resp) => resp,
        Err(_) => {
            tracing::warn!(
                timeout_ms,
                "POST /entities: deployment processing timed out"
            );
            upload_limits::timeout_response(&upload_limits::processing_timeout_message(timeout_ms))
        }
    };
    worlds_metrics::deployment_processed(mode, resp.status(), processing.elapsed());
    resp
}

fn present_truthy(v: &Value, key: &str) -> bool {
    match v.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => true,
    }
}

/// The deployment window, both directions. A timestamp dated forward is refused for the
/// same reason a stale one is: the signature covers the entity id, so a deployment the
/// clock has not reached yet would stay replayable until it does.
fn entity_timestamp_error(entity: &Value, now_ms: i64) -> Option<String> {
    let Ok(ts) = store::entity_timestamp(entity) else {
        return Some("The entity is missing a valid timestamp".to_string());
    };
    let ttl = now_ms.saturating_sub(ts);
    if ttl < -MAX_DEPLOYMENT_FUTURE_SKEW_MS {
        return Some(format!(
            "Deployment was created {} secs in the future. Max allowed: {} secs.",
            upload_limits::js_seconds(ttl.unsigned_abs()),
            upload_limits::js_seconds(MAX_DEPLOYMENT_FUTURE_SKEW_MS.unsigned_abs())
        ));
    }
    if ttl > ENTITY_TTL_MS {
        return Some(format!(
            "Deployment was created {} secs ago. Max allowed: {} secs.",
            upload_limits::js_seconds(ttl.unsigned_abs()),
            upload_limits::js_seconds(ENTITY_TTL_MS.unsigned_abs())
        ));
    }
    None
}

/// `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The old `buildSceneDeploymentMessage`: the parcels and the play link.
pub(super) fn deployment_message(base_url: &str, world_name: &str, parcels: &[String]) -> String {
    let world_url = format!("{base_url}/world/{world_name}");
    let position = parcels.first().map_or("0,0", String::as_str);
    format!(
        "Your scene was deployed to World \"{world_name}\" at parcels: {}!\nAccess world: https://play.decentraland.org/?realm={}&position={}",
        parcels.join(", "),
        encode_uri_component(&world_url),
        encode_uri_component(position)
    )
}

pub(super) fn raw_strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The first-seen distinct values, as a JavaScript `Set` keeps them.
fn distinct(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .iter()
        .filter(|v| seen.insert(v.as_str()))
        .cloned()
        .collect()
}

/// `createValidateScenePointers`: `scene.parcels` holds its base, and both
/// it and the pointers are the same unique canonical parcel set.
fn scene_pointers_error(entity: &Value) -> Option<String> {
    const NOT_CANONICAL: &str =
        "Scene pointers and parcels must be unique canonical parcel coordinates.";
    let scene = entity.pointer("/metadata/scene");
    let base = scene.and_then(|s| s.get("base"));
    let parcels = scene
        .and_then(|s| s.get("parcels"))
        .and_then(Value::as_array);
    if let (Some(base), Some(parcels)) = (base, parcels)
        && !parcels.contains(base)
    {
        let listed: Vec<String> = parcels
            .iter()
            .map(|p| p.as_str().map_or_else(|| p.to_string(), str::to_string))
            .collect();
        return Some(format!(
            "The scene base parcel [{}] must be included in the scene parcels [{}].",
            base.as_str()
                .map_or_else(|| base.to_string(), str::to_string),
            listed.join(", ")
        ));
    }
    let canonical = |v: Option<&Vec<Value>>| v.is_some_and(|a| is_canonical_parcel_set(a));
    let base_ok = base
        .and_then(Value::as_str)
        .is_some_and(|b| is_canonical_parcel_set(&[Value::String(b.to_string())]));
    let pointers = entity.get("pointers").and_then(Value::as_array);
    if !base_ok || !canonical(parcels) || !canonical(pointers) {
        return Some(NOT_CANONICAL.to_string());
    }
    let pointers = distinct(&raw_strings(entity.get("pointers")));
    let parcels = distinct(&raw_strings(scene.and_then(|s| s.get("parcels"))));
    let parcel_set: HashSet<&str> = parcels.iter().map(String::as_str).collect();
    let same =
        pointers.len() == parcels.len() && pointers.iter().all(|p| parcel_set.contains(p.as_str()));
    (!same).then(|| {
        format!(
            "The scene pointers [{}] must match the scene parcels [{}].",
            pointers.join(", "),
            parcels.join(", ")
        )
    })
}

/// The deprecated `worldConfiguration` keys, first one found.
fn deprecated_config_error(entity: &Value) -> Option<&'static str> {
    let wc = entity.pointer("/metadata/worldConfiguration")?;
    if present_truthy(wc, "dclName") {
        return Some(
            "`dclName` in scene.json was renamed to `name`. Please update your scene.json accordingly.",
        );
    }
    if present_truthy(wc, "minimapVisible") {
        return Some(
            "`minimapVisible` in scene.json is deprecated in favor of `{ miniMapConfig: { visible } }`. Please update your scene.json accordingly.",
        );
    }
    if present_truthy(wc, "skybox") {
        return Some(
            "`skybox` in scene.json is deprecated in favor of `{ \"skyboxConfig\": { \"fixedTime\": 36000 }}`. Please update your scene.json accordingly.",
        );
    }
    None
}

/// The uploaded files in upload order, keyed by their form name (the form refuses a repeated
/// name). Validation holds every name to the CIDv1 of its bytes before anything is stored.
pub(super) struct Uploads {
    pub(super) order: Vec<String>,
    pub(super) by_name: HashMap<String, SpooledFile>,
}

impl Uploads {
    pub(super) fn new(files: &[SpooledFile]) -> Self {
        // Names are unique: `read_deploy_form` refuses a repeated part name.
        let order = files.iter().map(|blob| blob.name.clone()).collect();
        let by_name = files
            .iter()
            .map(|blob| (blob.name.clone(), blob.clone()))
            .collect();
        Self { order, by_name }
    }

    /// Upstream `validateEntityId`: an uploaded entity file hashes to the entity id.
    fn entity_hash_error(&self, entity_id: &str) -> Option<String> {
        let file = self.by_name.get(entity_id)?;
        (file.hash != entity_id).then(|| {
            format!(
                "Invalid entity hash: expected {} but got {entity_id}",
                file.hash
            )
        })
    }

    /// Upstream `validateUploadedFiles`: each upload is the entity file or referenced by it, is
    /// named by a CIDv1 and hashes to its name.
    fn file_errors(&self, entity_id: &str, referenced: &HashSet<&str>) -> Vec<String> {
        let mut errors = Vec::new();
        for name in &self.order {
            if name != entity_id && !referenced.contains(name.as_str()) {
                errors.push(format!("Extra file detected {name}"));
            }
            if !is_ipfs_v2(name) {
                errors.push(format!("Only CIDv1 are allowed for content files: {name}"));
            }
            if self.by_name[name].hash != *name {
                errors.push(format!(
                    "The hashed file doesn't match the provided content: {name}"
                ));
            }
        }
        errors
    }
}

/// `@dcl/crypto`'s validator a link type is checked by, as its refusals name it.
fn validator_name(link_type: &str) -> &'static str {
    match link_type {
        "ECDSA_EPHEMERAL" => "ECDSA_PERSONAL_EPHEMERAL_VALIDATOR",
        "ECDSA_SIGNED_ENTITY" => "ECDSA_SIGNED_ENTITY_VALIDATOR",
        "ECDSA_EIP_1654_EPHEMERAL" => "ECDSA_EIP_1654_EPHEMERAL_VALIDATOR",
        _ => "EIP_1654_SIGNED_ENTITY_VALIDATOR",
    }
}

/// `@dcl/crypto` 3.7.0 `Authenticator.validateSignature`'s text for a refused chain where the
/// verifier's error identifies it; the verifier's own text otherwise.
fn crypto_message(chain: &AuthChain, entity_id: &str, error: &AuthError) -> String {
    let link = |link_type: &dyn std::fmt::Display, message: String| {
        format!("ERROR. Link type: {link_type}. {message}.")
    };
    let first_of = |types: &[AuthLinkType]| {
        chain
            .iter()
            .find(|l| types.contains(&l.link_type))
            .map(|l| l.link_type)
    };
    match error {
        AuthError::MalformedChain(m) if m == "invalid chain structure" => {
            "ERROR: Malformed authChain".to_string()
        }
        // A lone SIGNER: its address is the final authority.
        AuthError::MalformedChain(_) if chain.len() == 1 => format!(
            "ERROR: Invalid final authority. Expected: {entity_id}. Current {}.",
            chain[0].payload
        ),
        AuthError::FinalAuthorityMismatch { expected, actual } => {
            format!("ERROR: Invalid final authority. Expected: {expected}. Current {actual}.")
        }
        AuthError::SignerMismatch {
            index,
            expected,
            actual,
        } => link(
            &chain[*index].link_type,
            format!("Invalid signer address. Expected: {expected}. Actual: {actual}"),
        ),
        AuthError::MissingSignature { link_type, .. } => link(
            link_type,
            format!(
                "Invalid AuthLink. 'signature' must be present for type '{}'",
                validator_name(link_type)
            ),
        ),
        AuthError::EphemeralExpired {
            expiration_ms,
            now_ms,
        } => match first_of(&[
            AuthLinkType::EcdsaEphemeral,
            AuthLinkType::EcdsaEip1654Ephemeral,
        ]) {
            Some(t) => link(
                &t,
                format!("Ephemeral key expired. Expiration: {expiration_ms}. Test: {now_ms}"),
            ),
            None => error.to_string(),
        },
        // Upstream validates with no provider, so a contract wallet is refused.
        AuthError::Eip1654NotImplemented => match first_of(&[
            AuthLinkType::EcdsaEip1654Ephemeral,
            AuthLinkType::EcdsaEip1654SignedEntity,
        ]) {
            Some(t) => link(&t, "Missing provider".to_string()),
            None => error.to_string(),
        },
        other => other.to_string(),
    }
}

/// The signer of a chain over `entity_id`, or the refusal: a single request answers
/// `Deployment failed: <reason>` (`AuthChain` schema, `Invalid signer`, `@dcl/crypto`), a partial
/// batch `Invalid auth chain.` / `Invalid auth chain: <reason>`, as upstream's
/// `authenticateRequest`.
async fn authenticate(
    state: &SharedState,
    entity_id: &str,
    chain: &Value,
    partial: bool,
) -> Result<Address, Refusal> {
    let schema = |ajv: String| match partial {
        true => Refusal(StatusCode::BAD_REQUEST, "Invalid auth chain.".to_string()),
        false => failed(ajv),
    };
    let refused = |reason: String| match partial {
        true => Refusal(
            StatusCode::BAD_REQUEST,
            format!("Invalid auth chain: {reason}"),
        ),
        false => failed(reason),
    };
    let chain: AuthChain = serde_json::from_value(chain.clone()).map_err(|e| {
        // The indexed fields are strings, so only a link type outside the enum fails here.
        schema(if e.to_string().contains("unknown variant") {
            "must be equal to one of the allowed values".to_string()
        } else {
            format!("The auth chain is malformed: {e}")
        })
    })?;
    let signer = chain
        .first()
        .ok_or_else(|| schema("must NOT have fewer than 1 items".to_string()))?
        .payload
        .clone();
    if !partial && !is_eth_address(&signer) {
        return Err(failed(format!("Invalid signer: {signer}")));
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    match crate::auth::verify_auth_chain_async(
        &chain,
        entity_id,
        Some(now_ms),
        state.eip1654.as_deref(),
    )
    .await
    {
        Ok(()) => Address::parse(&signer).map_err(|_| refused(format!("Invalid signer: {signer}"))),
        Err(AuthError::Eip1654ValidationFailed(e)) => Err(unavailable(format!(
            "The auth chain could not be verified right now: {e}"
        ))),
        Err(e) => Err(refused(crypto_message(&chain, entity_id, &e))),
    }
}

/// What a validated publish carries to the store.
pub(super) struct Validated {
    pub(super) world: WorldName,
    pub(super) world_name: String,
    pub(super) parcels: ParcelSet,
    /// `None` only for a staging check that skipped the permission.
    pub(super) replacement: Option<Deploy>,
    /// The live NAME holder `worlds.owner` is refreshed to; `None` with `replacement`.
    pub(super) name_owner: Option<Address>,
    pub(super) size: i64,
    /// This scene's size cap: the owner's remaining storage allowance, capped per world.
    pub(super) max_size_bytes: i64,
    /// The world's scene size cap, whatever the owner's allowance (the staging budget's unit).
    pub(super) world_size_cap: i64,
    /// The allowance the publish is charged to.
    pub(super) charge: Option<super::quota::QuotaCharge>,
}

/// Which checks [`validate`] runs and where it takes the content sizes from.
pub(super) enum Check<'a> {
    /// A single-request publish: sizes of the uploads, else of the stored files.
    Publish,
    /// A partial batch: no completeness or size check; `permission` unless its own signer resumes.
    Stage { permission: bool },
    /// The batch completing a partial upload: the sizes its receipts recorded.
    Complete(&'a HashMap<String, i64>),
}

/// The publish checks in the old service's order. `anchor_ms` dates the timestamp window:
/// now, or a partial upload's first batch.
pub(super) async fn validate(
    state: &SharedState,
    entity: &Value,
    signer: &Address,
    entity_id: &str,
    uploads: &Uploads,
    anchor_ms: i64,
    check: Check<'_>,
) -> Result<Validated, Refusal> {
    if let Some(e) = entity_schema_error(entity) {
        return Err(failed(e));
    }
    let entity_type = entity["type"].as_str().unwrap_or_default();
    if entity_type != "scene" {
        return Err(failed(format!(
            "Entity type {entity_type} is not supported."
        )));
    }
    let content = entity["content"].as_array().map_or(&[][..], Vec::as_slice);
    let max_files = state.settings.max_file_count;
    if content.len() > max_files {
        return Err(failed(format!(
            "The deployment has too many files. The maximum allowed is {max_files} but the deployment has {}.",
            content.len()
        )));
    }
    let referenced: HashSet<&str> = content
        .iter()
        .filter_map(|item| item["hash"].as_str())
        .collect();
    // Staging checks the entity id before the auth chain and the uploads right after the window,
    // a publish the entity id after the window and the uploads with the missing files.
    let staging = matches!(check, Check::Stage { .. });
    if staging && let Some(e) = uploads.entity_hash_error(entity_id) {
        return Err(failed(e));
    }
    if let Some(e) = entity_timestamp_error(entity, anchor_ms) {
        return Err(failed(e));
    }
    if !staging && let Some(e) = uploads.entity_hash_error(entity_id) {
        return Err(failed(e));
    }
    if staging {
        let errors = uploads.file_errors(entity_id, &referenced);
        if !errors.is_empty() {
            return Err(failed(errors.join(", ")));
        }
    }

    let metadata = entity.get("metadata").unwrap_or(&Value::Null);
    crate::content::validator::validate_scene_metadata(metadata).map_err(failed)?;
    let world_name = metadata
        .pointer("/worldConfiguration/name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            failed("scene.json needs to specify a worldConfiguration section with a valid name inside.")
        })?;
    let world = match WorldName::parse(world_name) {
        Ok(parsed) if parsed.key().ends_with(DCL_ETH_SUFFIX) => parsed,
        Ok(parsed) if parsed.is_ens_world() && state.policy.ens_enabled() => parsed,
        _ if state.policy.ens_enabled() => {
            return Err(failed(format!(
                "Only .dcl.eth and .eth world names are supported for publishing (got \"{world_name}\")"
            )));
        }
        _ => {
            return Err(failed(format!(
                "Only .dcl.eth world names are supported for publishing (got \"{world_name}\")"
            )));
        }
    };
    if let Some(e) = deprecated_config_error(entity) {
        return Err(failed(e));
    }

    let pointers = canon_pointer_set(entity["pointers"].as_array().map_or(&[][..], Vec::as_slice));
    let bounds: Vec<String> = pointers
        .iter()
        .filter_map(|p| validate_parcel_in_bounds(p).err())
        .collect();
    if !bounds.is_empty() {
        return Err(failed(bounds.join(", ")));
    }
    if let Some(e) = scene_pointers_error(entity) {
        return Err(failed(e));
    }
    let parcels = ParcelSet::parse(&pointers).map_err(|e| failed(e.to_string()))?;
    let limits = state
        .policy
        .world_limits(&world)
        .await
        .map_err(|e| internal(format!("Failed to read the world limits of {world}: {e}")))?;
    if pointers.len() > limits.max_parcels as usize {
        return Err(failed(format!(
            "Max allowed scene dimensions is {} parcels.",
            limits.max_parcels
        )));
    }

    if !state
        .names
        .check_name_deny_list(&state.pool, world_name)
        .await
    {
        return Err(failed(format!(
            "Deployment failed: World \"{world_name}\" can not be deployed because the name is in the name deny list managed by Decentraland DAO."
        )));
    }
    let check_permission = !matches!(check, Check::Stage { permission: false });
    let replacement = match check_permission {
        false => None,
        true => match state.policy.can_deploy(&world, signer, &parcels).await {
            Ok(decision) => Some(decision),
            Err(AuthorityNotEstablished::RefusedLacksAuthority { detail }) => {
                tracing::info!(world = %world, signer = %signer.as_str(), "deploy denied: {detail}");
                // A realm-level refusal (personal worlds, no name authority) keeps its own reason.
                return Err(if detail.starts_with("Not authorized:") {
                    failed(detail)
                } else {
                    failed(format!(
                        "Deployment failed: Your wallet has no permission to publish this scene because it does not have permission to deploy under \"{world_name}\". Check scene.json to select a name that either you own or you were given permission to deploy."
                    ))
                });
            }
            Err(undetermined) => {
                tracing::warn!(world = %world, signer = %signer.as_str(), error = %undetermined, "deploy denied: authority undetermined (fail-closed)");
                return Err(forbidden(
                    "Not authorized: could not verify deployment permissions (deploy denied)",
                ));
            }
        },
    };
    match store::is_world_disabled(&state.pool, &world).await {
        Ok(false) => {}
        Ok(true) => return Err(forbidden(store::world_disabled(&world).public_message())),
        Err(e) => {
            return Err(internal(format!(
                "Failed to read whether {world} is disabled: {e:?}"
            )));
        }
    }
    // Every publish refreshes worlds.owner to the live NAME holder.
    let name_owner = match &replacement {
        None => None,
        Some(Deploy::Owner) => Some(signer.clone()),
        Some(Deploy::Scoped(_)) => match state.policy.live_name_owner(&world).await {
            Ok(Some(owner)) => Some(owner),
            Ok(None) => {
                return Err(Refusal(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Cannot deploy scene \"{entity_id}\" to world \"{}\": owner address could not be resolved.",
                        world.display()
                    ),
                ));
            }
            Err(e) => {
                tracing::warn!(world = %world, error = %e, "deploy denied: NAME owner undetermined");
                return Err(unavailable(
                    "Deployment failed: the world's owner could not be resolved right now",
                ));
            }
        },
    };

    let minimap = missing_minimap_images(entity);
    if !minimap.is_empty() {
        return Err(failed(minimap.join(", ")));
    }
    let skybox = missing_skybox_textures(entity);
    if !skybox.is_empty() {
        return Err(failed(skybox.join(", ")));
    }
    let mut thumbnail = Vec::new();
    validate_navmap_thumbnail(entity, &mut thumbnail);
    if let Some(e) = thumbnail.into_iter().next() {
        return Err(failed(e));
    }

    let mut file_errors = match staging {
        true => Vec::new(),
        false => uploads.file_errors(entity_id, &referenced),
    };
    let mut sizes: HashMap<&str, i64> = HashMap::new();
    for item in content.iter().filter(|_| !staging) {
        let (file, hash) = (
            item["file"].as_str().unwrap_or_default(),
            item["hash"].as_str().unwrap_or_default(),
        );
        if sizes.contains_key(hash) {
            continue;
        }
        let known = match &check {
            Check::Complete(known) => Some(known.get(hash).copied()),
            _ => None,
        };
        let size = match (known, uploads.by_name.get(hash)) {
            (Some(known), _) => known,
            (None, Some(blob)) => Some(blob.len() as i64),
            (None, None) => match state.storage.file_info(hash).await {
                Ok(info) => info.map(|i| i.size as i64),
                Err(e) => {
                    tracing::warn!(hash, error = %e, "deploy: could not stat an already-stored file");
                    None
                }
            },
        };
        match size {
            Some(size) => {
                sizes.insert(hash, size);
            }
            None => file_errors.push(format!(
                "The file {hash} ({file}) is neither present in the storage or in the provided entity"
            )),
        }
    }
    if !file_errors.is_empty() {
        return Err(failed(file_errors.join(", ")));
    }
    let size: i64 = sizes.values().copied().fold(0, i64::saturating_add);
    // A staging batch that skipped the permission resolves the owner here; it is refused alike.
    let cap = state
        .scene_cap(&world, &limits, name_owner.as_ref(), Some(pointers.as_slice()))
        .await
        .map_err(|e| {
            tracing::warn!(world = %world, error = ?e, "deploy denied: storage allowance undetermined");
            unavailable("Deployment failed: the storage allowance could not be determined right now")
        })?;
    if size > cap.max {
        return Err(failed(super::quota::too_big(cap.max, size)));
    }

    Ok(Validated {
        world,
        world_name: world_name.to_string(),
        parcels,
        replacement,
        name_owner,
        size,
        max_size_bytes: cap.max,
        world_size_cap: limits.max_size_bytes,
        charge: cap.charge,
    })
}

/// A publish the store refused, or the scene row's creation time (ms).
#[allow(clippy::result_large_err, clippy::too_many_arguments)] // The refusal is the answer itself.
pub(super) async fn publish(
    state: &SharedState,
    headers: &HeaderMap,
    entity_id: &str,
    entity: &Value,
    signer: &Address,
    deployment_auth_chain: &Value,
    validated: &Validated,
    completion: Option<&store::PartialCompletion<'_>>,
) -> Result<i64, Response> {
    let Validated {
        world,
        parcels,
        replacement,
        name_owner,
        size,
        charge,
        ..
    } = validated;
    let (Some(replacement), Some(name_owner)) = (replacement, name_owner) else {
        return Err(internal("Publish without a deploy authorization").into_response());
    };

    let persisted = store::deploy_scene_with(
        &state.pool,
        &state.storage,
        SceneDeployment {
            world,
            name_owner: Some(name_owner),
            entity_id,
            deployer: signer,
            deployment_auth_chain,
            entity,
            parcels,
            size: *size,
            replacement,
            forward_to: state.forwarder.as_deref().map(|f| f.target()),
            quota: charge.as_ref(),
        },
        completion,
    )
    .await;
    let deployed = match persisted {
        Ok(deployed) => deployed,
        Err(e) => {
            let reason = e.public_message();
            if e.is_conflict() {
                tracing::info!(world = %world, entity_id = %entity_id, "deploy conflict: overlapping-scene set changed under a scoped authorization");
                return Err(rejection(StatusCode::CONFLICT, reason));
            }
            if e.status() == 503 {
                tracing::warn!(world = %world, entity_id = %entity_id, reason = %reason, "deploy deferred: stored content changed under it");
                return Err(unavailable(reason).into_response());
            }
            if e.is_forbidden() || e.is_bad_request() {
                tracing::info!(world = %world, entity_id = %entity_id, reason = %reason, "deploy refused");
                return Err(if e.is_forbidden() {
                    forbidden(reason).into_response()
                } else {
                    err_one(reason)
                });
            }
            return Err(internal(format!(
                "Failed to persist deployment {entity_id} to {world}: {e:?}"
            ))
            .into_response());
        }
    };
    // A new holder's publish moved worlds.owner: re-decide the previous one now.
    if let Some(previous) = &deployed.previous_owner {
        state.policy.note_owner(world, Some(name_owner));
        state.revoke_unless_admitted(world, previous).await;
    }

    tracing::info!(
        entity_id = %entity_id,
        signer = %signer.as_str(),
        world = %world,
        authz = match replacement { Deploy::Owner => "name-ownership", Deploy::Scoped(_) => "acl" },
        partial = completion.is_some(),
        content_size = size,
        user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("unknown"),
        forwarded = state.forwarder.is_some(),
        "POST /entities - deployed (validated + authorized + persisted)"
    );
    worlds_metrics::world_deployed(world.key());
    // The publish may have freed space (a smaller scene replaced a larger one).
    let (quota_state, owner) = (state.clone(), name_owner.clone());
    tokio::spawn(async move {
        quota_state
            .unblock_if_under_quota(&owner, chrono::Utc::now().naive_utc())
            .await
    });
    if let Some(forwarder) = &state.forwarder {
        forwarder.kick();
    }
    Ok(deployed.created_at_ms)
}

async fn deploy_entity_inner(
    state: &SharedState,
    headers: &HeaderMap,
    form: DeployForm,
    is_partial: bool,
    arrived_ms: i64,
) -> Response {
    // `_spool` keeps the spooled parts on disk until this deploy has stored them.
    let DeployForm {
        fields,
        files,
        spool: _spool,
    } = form;

    let Some(entity_id) = fields.get("entityId").cloned() else {
        return err_one(STRING_EXPECTED);
    };

    let auth_chain_value = match extract_auth_chain_from_fields(&fields) {
        Ok(v) => v,
        Err(e) => return err_one(e),
    };

    // The signer is established before the entity is read, as the old handler did; every
    // partial batch is authenticated the same way.
    let signer = match authenticate(state, &entity_id, &auth_chain_value, is_partial).await {
        Ok(signer) => signer,
        Err(refusal) => return refusal.into_response(),
    };

    if is_partial {
        return partial::stage(
            state,
            headers,
            &entity_id,
            &auth_chain_value,
            &signer,
            &files,
            arrived_ms,
        )
        .await;
    }

    let uploads = Uploads::new(&files);
    let Some(entity_file) = uploads.by_name.get(&entity_id) else {
        return err_one(format!(
            "Entity file \"{entity_id}\" is missing from the request."
        ));
    };
    let entity = match read_entity_file(entity_file).await {
        Ok(entity) => entity,
        Err(resp) => return resp,
    };

    let validated = match validate(
        state,
        &entity,
        &signer,
        &entity_id,
        &uploads,
        chrono::Utc::now().timestamp_millis(),
        Check::Publish,
    )
    .await
    {
        Ok(v) => v,
        Err(refusal) => return refusal.into_response(),
    };

    // A newer scene on the parcels refuses the publish before any blob is stored; the publish
    // transaction checks it again.
    if let Err(refused) = refuse_if_newer_scene(
        state,
        &validated.world,
        &validated.parcels.canonical(),
        &entity_id,
        &entity,
    )
    .await
    {
        return refused;
    }

    // Every upload is referenced and named by its own hash, as validation checked.
    for blob in uploads.by_name.values() {
        if let Err(e) = state.storage.store_file(&blob.hash, &blob.path).await {
            return internal(format!(
                "Failed to persist deployment content {}: {e}",
                blob.hash
            ))
            .into_response();
        }
    }

    let created_ms = match publish(
        state,
        headers,
        &entity_id,
        &entity,
        &signer,
        &auth_chain_value,
        &validated,
        None,
    )
    .await
    {
        Ok(created_ms) => created_ms,
        Err(resp) => return resp,
    };

    let scene_parcels = raw_strings(entity.pointer("/metadata/scene/parcels"));
    (
        StatusCode::OK,
        Json(DeploySuccess {
            creation_timestamp: created_ms,
            message: deployment_message(
                &state.settings.public_url,
                &validated.world_name,
                &scene_parcels,
            ),
        }),
    )
        .into_response()
}

/// The entity JSON of an uploaded or stored entity file, at most [`MAX_ENTITY_FILE_SIZE_BYTES`].
#[allow(clippy::result_large_err)] // The refusal is the answer itself.
pub(super) async fn read_entity_file(file: &SpooledFile) -> Result<Value, Response> {
    if file.len() > MAX_ENTITY_FILE_SIZE_BYTES {
        return Err(err_one(entity_file_too_large_error()));
    }
    let bytes = file.read().await.map_err(|e| {
        internal(format!("Failed to read the uploaded entity file: {e}")).into_response()
    })?;
    parse_entity(&bytes)
}

/// The entity JSON of entity file bytes.
#[allow(clippy::result_large_err)] // The refusal is the answer itself.
pub(super) fn parse_entity(bytes: &[u8]) -> Result<Value, Response> {
    if bytes.len() > MAX_ENTITY_FILE_SIZE_BYTES {
        return Err(err_one(entity_file_too_large_error()));
    }
    serde_json::from_slice(bytes).map_err(|_| err_one("The entity file is not valid JSON."))
}

#[cfg(test)]
mod tests {
    use super::super::deploy_validate::MAX_SCENE_POINTERS;
    use super::*;
    use serde_json::json;

    const NOW_MS: i64 = 1_700_000_000_000;

    #[test]
    fn upload_precheck_compares_against_the_wire_cap() {
        assert_eq!(MAX_UPLOAD_WIRE_SIZE_BYTES, 360 * 1024 * 1024);
        assert!(!declared_length_exceeds_limit(
            MAX_UPLOAD_SIZE_BYTES as u64 + 1
        ));
        assert!(!declared_length_exceeds_limit(
            MAX_UPLOAD_WIRE_SIZE_BYTES as u64
        ));
        assert!(declared_length_exceeds_limit(
            MAX_UPLOAD_WIRE_SIZE_BYTES as u64 + 1
        ));
        assert!(declared_length_exceeds_limit(u64::MAX));
        assert!(!declared_length_exceeds_limit(0));
        assert!(!declared_length_exceeds_limit(1024));
    }

    #[test]
    fn a_timestamp_inside_the_window_is_accepted_in_both_directions() {
        for ts in [
            NOW_MS,
            NOW_MS - 299_000,
            NOW_MS + MAX_DEPLOYMENT_FUTURE_SKEW_MS,
        ] {
            assert_eq!(
                entity_timestamp_error(&json!({ "timestamp": ts }), NOW_MS),
                None,
                "{ts} is inside the deployment window"
            );
        }
    }

    #[test]
    fn a_timestamp_past_the_forwards_bound_is_refused_in_the_old_words() {
        let error = entity_timestamp_error(
            &json!({ "timestamp": NOW_MS + MAX_DEPLOYMENT_FUTURE_SKEW_MS + 1 }),
            NOW_MS,
        )
        .expect("a deployment the clock has not reached must not be accepted");
        assert_eq!(
            error,
            "Deployment was created 900.001 secs in the future. Max allowed: 900 secs."
        );
    }

    #[test]
    fn a_stale_timestamp_is_refused_in_the_old_words() {
        let error = entity_timestamp_error(&json!({ "timestamp": NOW_MS - 301_000 }), NOW_MS)
            .expect("a deployment older than the TTL must not be accepted");
        assert_eq!(
            error,
            "Deployment was created 301 secs ago. Max allowed: 300 secs."
        );
    }

    #[test]
    fn a_float_timestamp_is_held_to_the_window() {
        let ts = NOW_MS as f64;
        assert_eq!(
            entity_timestamp_error(&json!({ "timestamp": ts }), NOW_MS),
            None
        );
        let stale = (NOW_MS - 301_000) as f64;
        assert_eq!(
            entity_timestamp_error(&json!({ "timestamp": stale }), NOW_MS).as_deref(),
            Some("Deployment was created 301 secs ago. Max allowed: 300 secs.")
        );
    }

    #[test]
    fn a_missing_or_unreadable_timestamp_is_refused() {
        for entity in [json!({}), json!({ "timestamp": "1700000000000" })] {
            assert_eq!(
                entity_timestamp_error(&entity, NOW_MS).as_deref(),
                Some("The entity is missing a valid timestamp")
            );
        }
    }

    #[test]
    fn the_success_message_is_the_old_services() {
        assert_eq!(
            deployment_message(
                "https://peer.example",
                "Boedo.dcl.eth",
                &["1,-2".to_string(), "1,-1".to_string()]
            ),
            "Your scene was deployed to World \"Boedo.dcl.eth\" at parcels: 1,-2, 1,-1!\n\
             Access world: https://play.decentraland.org/?realm=https%3A%2F%2Fpeer.example%2Fworld%2FBoedo.dcl.eth&position=1%2C-2"
        );
        assert!(deployment_message("https://p", "a.dcl.eth", &[]).ends_with("&position=0%2C0"));
    }

    async fn body(resp: Response) -> (u16, Value) {
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn refusals_answer_the_old_envelope_and_body_limits_413() {
        assert_eq!(
            body(failed("x").into_response()).await,
            (
                400,
                json!({ "error": "Bad request", "message": "Deployment failed: x" })
            )
        );
        for limit in [
            upload_limits::PAYLOAD_TOO_LARGE_MESSAGE,
            upload_limits::TOO_MANY_PARTS_MESSAGE,
            upload_limits::TOO_MANY_FIELDS_MESSAGE,
            upload_limits::TOO_MANY_FILES_MESSAGE,
            upload_limits::FILE_TOO_LARGE_MESSAGE,
        ] {
            assert_eq!(
                body(payload_too_large(limit)).await,
                (
                    413,
                    json!({ "error": "Payload Too Large", "message": limit })
                )
            );
        }
        assert_eq!(body(err_one(STRING_EXPECTED)).await.0, 400);
        assert_eq!(
            body(internal("Failed to persist deployment").into_response()).await,
            (500, json!({ "error": "Internal Server Error" }))
        );
        assert_eq!(
            body(rejection(StatusCode::CONFLICT, "c")).await.1["error"],
            "Conflict"
        );
    }

    fn scene(pointers: Value, base: &str, parcels: Value) -> Value {
        json!({ "pointers": pointers, "metadata": { "scene": { "base": base, "parcels": parcels } } })
    }

    #[test]
    fn scene_pointers_are_checked_in_the_old_words() {
        assert_eq!(
            scene_pointers_error(&scene(json!(["0,0"]), "0,0", json!(["0,0"]))),
            None
        );
        assert_eq!(
            scene_pointers_error(&scene(json!(["0,0"]), "1,1", json!(["0,0"]))).as_deref(),
            Some("The scene base parcel [1,1] must be included in the scene parcels [0,0].")
        );
        assert_eq!(
            scene_pointers_error(&scene(json!(["00,0"]), "0,0", json!(["0,0"]))).as_deref(),
            Some("Scene pointers and parcels must be unique canonical parcel coordinates.")
        );
        assert_eq!(
            scene_pointers_error(&scene(json!(["0,1"]), "0,0", json!(["0,0"]))).as_deref(),
            Some("The scene pointers [0,1] must match the scene parcels [0,0].")
        );
    }

    fn entity_with_pointers(pointers: Vec<Value>) -> Value {
        json!({ "version": "v3", "type": "scene", "pointers": pointers, "timestamp": 1, "content": [] })
    }

    #[test]
    fn an_oversized_pointer_list_is_refused_before_the_item_checks() {
        let pointers: Vec<Value> = (0..200_000).map(|i| json!(format!("{i},0"))).collect();
        let started = Instant::now();
        let error = entity_schema_error(&entity_with_pointers(pointers));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            error.as_deref(),
            Some("must NOT have more than 90601 items")
        );
    }

    #[test]
    fn duplicate_pointers_are_found_in_linear_time_and_named_as_ajv_does() {
        let mut pointers: Vec<Value> = (0..MAX_SCENE_POINTERS - 1)
            .map(|i| json!(format!("{i},0")))
            .collect();
        pointers.push(json!("0,0"));
        let started = Instant::now();
        let error = entity_schema_error(&entity_with_pointers(pointers));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            error.as_deref(),
            Some(&*format!(
                "must NOT have duplicate items (items ## {} and 0 are identical)",
                MAX_SCENE_POINTERS - 1
            ))
        );
    }

    #[test]
    fn a_full_grid_scene_is_matched_against_its_parcels_in_linear_time() {
        let grid: Vec<Value> = (-150..=150)
            .flat_map(|x| (-150..=150).map(move |y| json!(format!("{x},{y}"))))
            .collect();
        let mut parcels = grid.clone();
        parcels.reverse();
        let started = Instant::now();
        let error = scene_pointers_error(&scene(json!(grid), "0,0", json!(parcels)));
        // A quadratic match of 90,601 parcels takes minutes; the budget allows a loaded host.
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(error, None);
    }

    #[test]
    fn deprecated_config_reports_the_first_key() {
        let entity = json!({ "metadata": { "worldConfiguration": { "minimapVisible": true, "skybox": 1 } } });
        assert!(
            deprecated_config_error(&entity)
                .unwrap()
                .starts_with("`minimapVisible`")
        );
        assert_eq!(deprecated_config_error(&json!({ "metadata": {} })), None);
    }
}
