//! Partial (multi-request) publishes: `POST /entities` with `partial=true`,
//! worlds-content-server's `docs/partial-deployment-contract.md`. The signed
//! entity id keys the upload; every batch is authenticated like a publish
//! (the auth chain over the entity id) and serialized per entity in this
//! process (each staging step is atomic across replicas). A batch
//! carrying the manifest passes the staging checks (everything but
//! completeness and size, the permission included unless the upload's own
//! signer resumes it); a batch without it is accepted only from the
//! upload's own signer, whose manifest is read back from the store. Files
//! go to the ONE blob store once admitted; the batch that completes the
//! content runs the full publish checks and the publish transaction, which
//! consumes the upload and records the completion receipt. Answers: `202
//! {missing}` while files are outstanding, `200 {creationTimestamp,
//! message}` on publication, on a replayed completion (the original signer,
//! `COMPLETED_UPLOAD_TTL`) and for any batch of a live entity, 429 with
//! `Retry-After` when a staging budget is full, 400 when it can never fit.
//!
//! Staging state lives in `worlds.pending_scenes` / `pending_scene_files`
//! (no world row until publication); the blob collector keeps every file a
//! pending upload names, and [`delete_expired_uploads`] (the leased
//! `worlds-partial-upload-cleanup` job and `/gc`) reclaims expired ones.

// A refused batch is the answer itself, built where the refusal is decided.
#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use axum::Json;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

use crate::shared::identity::Address;

use super::deploy::{
    self, Check, DeploySuccess, Uploads, deployment_message, err_one, failed, internal,
    parse_entity, raw_strings, read_entity_file,
};
use super::metrics as worlds_metrics;
use super::store::{PARTIAL_UPLOAD_EXPIRED, PartialCompletion};
use super::upload_limits;
use super::upload_spool::SpooledFile;
use super::{CoreState, SharedState};

pub const QUERY_WITHOUT_FIELD_MESSAGE: &str =
    "The 'partial=true' query parameter requires the 'partial=true' form field";
const OWNERSHIP_MESSAGE: &str = "This upload was started by another account.";
const NOT_FOUND_MESSAGE: &str = "Upload no longer exists; resend its manifest.";
const TOO_BIG_MESSAGE: &str = "The deployment is too big.";
pub(super) const NEWER_SCENE_MESSAGE: &str =
    "a newer scene is already deployed on one or more of these parcels.";
const PERSONAL_FIRST_MESSAGE: &str = "Partial uploads to a personal world start once it is published here; publish it in a single request first.";

/// The staging limits (worlds-content-server's `.env.default` names and defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialUploadLimits {
    /// `PENDING_DEPLOYMENT_TTL`: an upload's fixed lifetime from its first batch, ms.
    pub pending_deployment_ttl_ms: u64,
    /// `PARTIAL_UPLOAD_CLEANUP_INTERVAL_MS`.
    pub cleanup_interval_ms: u64,
    /// `MAX_PENDING_DEPLOYMENTS_PER_DEPLOYER`, expired uploads awaiting cleanup included.
    pub max_pending_per_deployer: u64,
    /// `MAX_PENDING_DEPLOYMENTS_PER_WORLD`, live uploads only.
    pub max_pending_per_world: u64,
    /// `PENDING_BYTES_PER_WORLD_FACTOR` times the world's scene size cap.
    pub pending_bytes_per_world_factor: u64,
    /// `MAX_PENDING_BYTES_PER_DEPLOYER`.
    pub max_pending_bytes_per_deployer: u64,
    /// `MAX_PENDING_BYTES_PER_SOURCE`: the live uploads one client source started.
    pub max_pending_bytes_per_source: u64,
    /// `MAX_PENDING_BYTES`.
    pub max_pending_bytes: u64,
    /// `MAX_PARTIAL_UPLOAD_BYTES_PER_MINUTE`.
    pub max_bytes_per_minute: u64,
    /// `COMPLETED_UPLOAD_TTL`, ms.
    pub completed_upload_ttl_ms: u64,
}

impl Default for PartialUploadLimits {
    fn default() -> Self {
        Self {
            pending_deployment_ttl_ms: 3_600_000,
            cleanup_interval_ms: 300_000,
            max_pending_per_deployer: 10,
            max_pending_per_world: 4,
            pending_bytes_per_world_factor: 2,
            max_pending_bytes_per_deployer: 1_073_741_824,
            max_pending_bytes_per_source: 4_294_967_296,
            max_pending_bytes: 53_687_091_200,
            max_bytes_per_minute: 536_870_912,
            completed_upload_ttl_ms: 86_400_000,
        }
    }
}

impl PartialUploadLimits {
    /// Every knob positive; a world-size scene fits the deployer budget, which fits the node's.
    pub fn validate(self, world_size_cap: i64) -> anyhow::Result<Self> {
        for (name, value) in [
            ("PENDING_DEPLOYMENT_TTL", self.pending_deployment_ttl_ms),
            (
                "PARTIAL_UPLOAD_CLEANUP_INTERVAL_MS",
                self.cleanup_interval_ms,
            ),
            (
                "MAX_PENDING_DEPLOYMENTS_PER_DEPLOYER",
                self.max_pending_per_deployer,
            ),
            (
                "MAX_PENDING_DEPLOYMENTS_PER_WORLD",
                self.max_pending_per_world,
            ),
            (
                "PENDING_BYTES_PER_WORLD_FACTOR",
                self.pending_bytes_per_world_factor,
            ),
            (
                "MAX_PARTIAL_UPLOAD_BYTES_PER_MINUTE",
                self.max_bytes_per_minute,
            ),
            ("COMPLETED_UPLOAD_TTL", self.completed_upload_ttl_ms),
        ] {
            if value == 0 {
                anyhow::bail!("{name} must be a positive integer, got {value}");
            }
        }
        if self.max_pending_bytes_per_deployer < world_size_cap.max(0) as u64 {
            anyhow::bail!(
                "MAX_PENDING_BYTES_PER_DEPLOYER ({}) must fit one world-size scene ({world_size_cap} bytes)",
                self.max_pending_bytes_per_deployer
            );
        }
        if self.max_pending_bytes_per_source < self.max_pending_bytes_per_deployer {
            anyhow::bail!(
                "MAX_PENDING_BYTES_PER_SOURCE ({}) must be at least MAX_PENDING_BYTES_PER_DEPLOYER ({})",
                self.max_pending_bytes_per_source,
                self.max_pending_bytes_per_deployer
            );
        }
        if self.max_pending_bytes < self.max_pending_bytes_per_deployer {
            anyhow::bail!(
                "MAX_PENDING_BYTES ({}) must be at least MAX_PENDING_BYTES_PER_DEPLOYER ({})",
                self.max_pending_bytes,
                self.max_pending_bytes_per_deployer
            );
        }
        if i64::try_from(self.max_pending_bytes).is_err() {
            anyhow::bail!("MAX_PENDING_BYTES is too large");
        }
        Ok(self)
    }

    fn ttl_ms(&self) -> i64 {
        self.pending_deployment_ttl_ms as i64
    }
}

/// `MAX_CONCURRENT_UPLOADS` capped to three quarters of `PG_POOL_SIZE` (at least one).
pub fn uploads_within_pool(max_concurrent_uploads: u64, pool_size: u32) -> u64 {
    max_concurrent_uploads.min((u64::from(pool_size) * 3 / 4).max(1))
}

/// The in-process locks serializing the batches of one entity; an entry lives while it is held or awaited.
static ENTITY_LOCKS: LazyLock<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

struct EntityGuard {
    key: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for EntityGuard {
    fn drop(&mut self) {
        let mut locks = ENTITY_LOCKS.lock();
        // The map's and this guard's references only: nobody waits for it.
        if locks
            .get(&self.key)
            .is_some_and(|lock| Arc::strong_count(lock) <= 2)
        {
            locks.remove(&self.key);
        }
    }
}

#[cfg(test)]
tokio::task_local! {
    /// Tests: the replica a request runs on, each with its own in-process locks.
    pub(super) static REPLICA: &'static str;
    /// Pauses this request after upsert returns a row, before reservation starts.
    pub(super) static AFTER_UPSERT: (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
    /// Pauses after reservation reads the owner, before charging and taking its transaction locks.
    pub(super) static AFTER_OWNER_LOOKUP: (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
    /// Signals that this request reached its completing publish.
    pub(super) static PUBLISH_REACHED: Arc<tokio::sync::Notify>;
}

async fn lock_entity(entity_id: &str) -> EntityGuard {
    #[cfg(test)]
    let key = REPLICA
        .try_with(|replica| format!("{replica}/{entity_id}"))
        .unwrap_or_else(|_| entity_id.to_string());
    #[cfg(not(test))]
    let key = entity_id.to_string();
    let lock = ENTITY_LOCKS.lock().entry(key.clone()).or_default().clone();
    EntityGuard {
        key,
        _guard: lock.lock_owned().await,
    }
}

/// `202` of a batch that left files outstanding.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct PartialDeployPending {
    /// The content hashes still to send, in the manifest's order.
    pub missing: Vec<String>,
}

/// One expired-upload cleanup run, as `/gc` reports it under `expiredUploads`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct ExpiredUploadsCleanup {
    /// `success`, `deferred` (publishes kept the store lock busy) or `error` (an upload failed).
    #[cfg_attr(feature = "ts", ts(type = "\"success\" | \"deferred\" | \"error\""))]
    pub outcome: &'static str,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub removed: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub failed: u64,
}

/// Whether the request's query string carries `partial=true`.
pub fn query_declares_partial(uri: &Uri) -> bool {
    uri.query().is_some_and(|q| {
        url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "partial" && v == "true")
    })
}

/// When this process's last cleanup run finished (ms), for `Retry-After` estimates.
static LAST_CLEANUP_FINISHED_MS: AtomicI64 = AtomicI64::new(0);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn naive(ms: i64) -> NaiveDateTime {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .naive_utc()
}

fn millis(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp_millis()
}

fn expired() -> Response {
    err_one(PARTIAL_UPLOAD_EXPIRED)
}

/// The blob collector's fixed references (global scenes) as this node configures them.
fn pins(state: &CoreState) -> catalyst_storage::gc::Pins<'_> {
    catalyst_storage::gc::Pins::new(&state.storage, &state.settings.global_scenes_urn)
}

/// A database failure while staging; the `partial upload` span of [`stage`] names the entity.
fn db_error(error: impl std::fmt::Display) -> Response {
    internal(format!(
        "partial upload: failed to stage the deployment: {error}"
    ))
    .into_response()
}

/// 429 for a staging budget other uploads or traffic filled; `reason` is the metric label.
fn throttled(reason: &'static str, message: String, retry_after: i64) -> Response {
    worlds_metrics::partial_throttled(reason);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
        Json(super::upload_limits::DeployRejection {
            error: "Too Many Requests".to_string(),
            message,
        }),
    )
        .into_response()
}

/// Seconds until the oldest charged upload frees its share: its expiry, else the next cleanup.
fn retry_after(limits: &PartialUploadLimits, oldest: Option<NaiveDateTime>) -> i64 {
    let now = now_ms();
    let until_expiry = oldest.map_or(0, |t| millis(t) + limits.ttl_ms() - now);
    let interval = limits.cleanup_interval_ms as i64;
    let last = LAST_CLEANUP_FINISHED_MS.load(Ordering::Relaxed);
    let until_cleanup = if last == 0 {
        interval
    } else {
        last + interval - now
    };
    let wait = if until_expiry > 0 {
        until_expiry
    } else {
        until_cleanup
    };
    ((wait + 999) / 1000).max(1)
}

async fn lock_key(conn: &mut PgConnection, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct PendingRow {
    deployer: String,
    created_at: NaiveDateTime,
    initialized: bool,
}

impl PendingRow {
    fn expired(&self, limits: &PartialUploadLimits) -> bool {
        now_ms() >= millis(self.created_at) + limits.ttl_ms()
    }
}

const PENDING_COLUMNS: &str = "deployer, created_at, initialized";

/// The live upload of `entity_id` (instance clock), without renewing it.
async fn live_upload(
    pool: &PgPool,
    limits: &PartialUploadLimits,
    entity_id: &str,
) -> Result<Option<PendingRow>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PENDING_COLUMNS} FROM worlds.pending_scenes WHERE entity_id = $1 AND created_at > $2"
    )))
    .bind(entity_id)
    .bind(naive(now_ms() - limits.ttl_ms()))
    .fetch_optional(pool)
    .await
}

/// A file an upload names: its size, whether this node holds it for the upload and whether
/// its bytes count toward the staging budgets.
struct Receipt {
    hash: String,
    size: i64,
    stored: bool,
    charged: bool,
}

/// The answer for a batch of an entity already published: the completion receipt of its
/// original signer, else the live scene for any signer.
async fn replay(
    state: &CoreState,
    entity_id: &str,
    signer: &Address,
) -> Result<Option<Response>, sqlx::Error> {
    let limits = &state.settings.partial_uploads;
    let completed: Option<(String, Vec<String>, i64)> = sqlx::query_as(
        "SELECT world_name, parcels, creation_timestamp FROM worlds.completed_scene_uploads \
         WHERE entity_id = $1 AND deployer = $2 AND completed_at >= $3",
    )
    .bind(entity_id)
    .bind(signer.as_str())
    .bind(naive(now_ms() - limits.completed_upload_ttl_ms as i64))
    .fetch_optional(&state.pool)
    .await?;
    if let Some((world_name, parcels, creation_timestamp)) = completed {
        return Ok(Some(success(
            state,
            creation_timestamp,
            &world_name,
            &parcels,
        )));
    }
    Ok(find_publication(state, entity_id)
        .await?
        .map(|(created_ms, world_name, parcels)| success(state, created_ms, &world_name, &parcels)))
}

/// The scene of `entity_id` published here (not mirrored): its creation time (ms), world and scene parcels.
async fn find_publication(
    state: &CoreState,
    entity_id: &str,
) -> Result<Option<(i64, String, Vec<String>)>, sqlx::Error> {
    let row: Option<(String, Value, Vec<String>, i64)> = sqlx::query_as(
        "SELECT world_name, entity, parcels, (extract(epoch FROM created_at) * 1000)::bigint \
         FROM worlds.world_scenes WHERE entity_id = $1 AND mirror_source IS NULL \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(entity_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.map(|(world_name, entity, parcels, created_ms)| {
        let name = entity
            .pointer("/metadata/worldConfiguration/name")
            .and_then(Value::as_str)
            .map_or(world_name, str::to_string);
        let scene_parcels = raw_strings(entity.pointer("/metadata/scene/parcels"));
        let scene_parcels = if scene_parcels.is_empty() {
            parcels
        } else {
            scene_parcels
        };
        (created_ms, name, scene_parcels)
    }))
}

fn success(
    state: &CoreState,
    creation_timestamp: i64,
    world: &str,
    parcels: &[String],
) -> Response {
    (
        StatusCode::OK,
        Json(DeploySuccess {
            creation_timestamp,
            message: deployment_message(&state.settings.public_url, world, parcels),
        }),
    )
        .into_response()
}

/// One partial batch, authenticated by the caller.
pub(super) async fn stage(
    state: &SharedState,
    headers: &HeaderMap,
    entity_id: &str,
    chain: &Value,
    signer: &Address,
    files: &[SpooledFile],
    now_ms: i64,
) -> Response {
    // Every log line of the batch, the one 500 line of `rejection` included, names the entity.
    let span = tracing::error_span!("partial upload", entity_id = %entity_id);
    match tracing::Instrument::instrument(
        stage_batch(state, headers, entity_id, chain, signer, files, now_ms),
        span,
    )
    .await
    {
        Ok(resp) | Err(resp) => resp,
    }
}

async fn stage_batch(
    state: &SharedState,
    headers: &HeaderMap,
    entity_id: &str,
    chain: &Value,
    signer: &Address,
    files: &[SpooledFile],
    now_ms: i64,
) -> Result<Response, Response> {
    let limits = state.settings.partial_uploads;
    let uploads = Uploads::new(files);

    // Batches of one entity run one at a time in this process; each step below is atomic across replicas.
    let entity_lock = lock_entity(entity_id).await;

    let pending = live_upload(&state.pool, &limits, entity_id)
        .await
        .map_err(db_error)?;
    if let Some(resp) = replay(state, entity_id, signer).await.map_err(db_error)? {
        return Ok(resp);
    }
    let own_upload = pending.as_ref().filter(|p| p.deployer == signer.as_str());

    let entity = match uploads.by_name.get(entity_id) {
        Some(file) => read_entity_file(file).await?,
        None => {
            // Only the upload's own signer resumes without the manifest: anyone can sign an id.
            let stored = match own_upload {
                Some(_) => state.storage.retrieve(entity_id).await.map_err(db_error)?,
                None => None,
            };
            match stored {
                Some(bytes) => parse_entity(&bytes)?,
                None => {
                    return Err(err_one(format!(
                        "The first partial request for an entity must include the entity file \"{entity_id}\"."
                    )));
                }
            }
        }
    };

    // One admission instant, the first batch's arrival, anchors freshness and the lifetime.
    let admitted_ms = pending.as_ref().map_or(now_ms, |p| millis(p.created_at));
    let staged = deploy::validate(
        state,
        &entity,
        signer,
        entity_id,
        &uploads,
        admitted_ms,
        Check::Stage {
            permission: own_upload.is_none(),
        },
    )
    .await
    .map_err(IntoResponse::into_response)?;
    let parcels = staged.parcels.canonical();
    if pending.is_none() {
        if state
            .policy
            .personal_worlds()
            .owner_of(&staged.world)
            .is_some()
        {
            // Staging is outside the personal-world realm cap, so only a world published here stages.
            let hosted: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM worlds.worlds WHERE name = $1 AND mirror_source IS NULL)",
            )
            .bind(staged.world.key())
            .fetch_one(&state.pool)
            .await
            .map_err(db_error)?;
            if !hosted {
                return Err(err_one(PERSONAL_FIRST_MESSAGE));
            }
        }
        deploy::refuse_if_newer_scene(state, &staged.world, &parcels, entity_id, &entity).await?;
    }

    let mut content: Vec<String> = Vec::new();
    for item in entity["content"].as_array().into_iter().flatten() {
        if let Some(hash) = item["hash"].as_str()
            && !content.iter().any(|h| h == hash)
        {
            content.push(hash.to_string());
        }
    }

    // Storage is inventoried once, on the upload's first stored batch.
    let mut receipts: Vec<Receipt> = Vec::new();
    if !pending.as_ref().is_some_and(|p| p.initialized) {
        worlds_metrics::partial_metadata_checks(content.len());
        let mut in_storage = Vec::new();
        for hash in &content {
            if let Some(info) = state.storage.file_info(hash).await.map_err(db_error)? {
                in_storage.push((hash.clone(), info.size as i64));
            }
        }
        let keys: Vec<String> = in_storage.iter().map(|(h, _)| h.clone()).collect();
        // Stored content is free only when published content keeps it.
        let published = catalyst_storage::gc::published_among(&state.pool, pins(state), &keys)
            .await
            .map_err(db_error)?;
        receipts.extend(in_storage.into_iter().map(|(hash, size)| Receipt {
            charged: !published.contains(&hash),
            hash,
            size,
            stored: true,
        }));
    }
    let stored = match pending {
        Some(_) => progress(&state.pool, entity_id).await.map_err(db_error)?,
        None => HashMap::new(),
    };
    // A file already held, before the upload or from an earlier batch, is not stored again.
    let to_store: Vec<&SpooledFile> = uploads
        .order
        .iter()
        .filter(|h| !stored.contains_key(*h) && !receipts.iter().any(|r| &r.hash == *h))
        .filter_map(|h| uploads.by_name.get(h))
        .collect();
    let incoming: i64 = files.iter().map(|f| f.len() as i64).sum();
    receipts.extend(to_store.iter().map(|f| Receipt {
        hash: f.hash.clone(),
        size: f.len() as i64,
        stored: false,
        charged: true,
    }));
    let known: i64 = receipts
        .iter()
        .filter(|r| r.hash != entity_id)
        .map(|r| r.size)
        .sum();
    if known > staged.max_size_bytes {
        return Err(failed(TOO_BIG_MESSAGE).into_response());
    }

    let (upload, created) = match upsert(
        state,
        &limits,
        UpsertInput {
            entity_id,
            world_name: staged.world.key(),
            parcels: &parcels,
            entity: &entity,
            deployer: signer.as_str(),
            admitted_ms,
            resumes: pending.is_some(),
            source: client_source(headers),
        },
    )
    .await?
    {
        Upsert::Row(upload, created) => (upload, created),
        Upsert::Vanished => {
            return replay(state, entity_id, signer)
                .await
                .map_err(db_error)?
                .ok_or_else(expired);
        }
    };
    #[cfg(test)]
    if let Ok((reached, resume)) = AFTER_UPSERT.try_with(Clone::clone) {
        reached.notify_one();
        resume.notified().await;
    }
    match reserve(
        state,
        &limits,
        entity_id,
        &receipts,
        staged.max_size_bytes,
        // Never budget the world below this scene's own cap.
        staged.world_size_cap.max(staged.max_size_bytes),
        incoming,
    )
    .await
    {
        Ok(Reservation::Reserved) => {}
        Ok(Reservation::Vanished) => {
            return replay(state, entity_id, signer)
                .await
                .map_err(db_error)?
                .ok_or_else(|| err_one(NOT_FOUND_MESSAGE));
        }
        Err(resp) => {
            // An upload this batch created and could not admit must not keep holding a slot.
            if created {
                discard_unadmitted(&state.pool, entity_id).await;
            }
            return Err(resp);
        }
    }
    if created {
        worlds_metrics::partial_started();
    }
    // Re-checked right before storing on this instance's clock; cleanup and publication use the database's.
    if upload.expired(&limits) {
        return Err(expired());
    }

    for file in &to_store {
        if let Err(e) = state.storage.store_file(&file.hash, &file.path).await {
            return Err(internal(format!(
                "partial upload: failed to persist deployment content {}: {e}",
                file.hash
            ))
            .into_response());
        }
    }
    let hashes: Vec<String> = to_store.iter().map(|f| f.hash.clone()).collect();
    let Some((batches, progress)) = record_stored(&state.pool, entity_id, &hashes)
        .await
        .map_err(db_error)?
    else {
        // Completed or reclaimed since this batch was admitted.
        return match replay(state, entity_id, signer).await.map_err(db_error)? {
            Some(resp) => Ok(resp),
            None => Err(expired()),
        };
    };
    let missing: Vec<String> = content
        .iter()
        .filter(|h| !progress.contains_key(*h))
        .cloned()
        .collect();
    worlds_metrics::partial_batch(missing.is_empty());
    if !missing.is_empty() {
        return Ok((StatusCode::ACCEPTED, Json(PartialDeployPending { missing })).into_response());
    }

    // The receipts hold every size; the collector keeps a pending upload's files.
    let created_ms = millis(upload.created_at);
    let validated = deploy::validate(
        state,
        &entity,
        signer,
        entity_id,
        &Uploads::new(&[]),
        created_ms,
        Check::Complete(&progress),
    )
    .await
    .map_err(IntoResponse::into_response)?;
    let scene_parcels = raw_strings(entity.pointer("/metadata/scene/parcels"));
    let completion = PartialCompletion {
        ttl_ms: limits.pending_deployment_ttl_ms,
        world_name: &validated.world_name,
        scene_parcels: &scene_parcels,
    };
    #[cfg(test)]
    let _ = PUBLISH_REACHED.try_with(|reached| reached.notify_one());
    let creation_timestamp = match deploy::publish(
        state,
        headers,
        entity_id,
        &entity,
        signer,
        chain,
        &validated,
        Some(&completion),
    )
    .await
    {
        Ok(created) => created,
        Err(resp) => {
            // A publish of the same entity that won the race answers for this one.
            if resp.status() == StatusCode::BAD_REQUEST
                && let Some((created, world, parcels)) =
                    find_publication(state, entity_id).await.map_err(db_error)?
            {
                return Ok(success(state, created, &world, &parcels));
            }
            // A file found stored when the upload began may have been collected since: ask for it again.
            if matches!(
                resp.status(),
                StatusCode::BAD_REQUEST | StatusCode::SERVICE_UNAVAILABLE
            ) {
                // Only for an upload still live: an expired one is answered as such.
                if let Some(missing) = vanished(state, entity_id, &content).await?
                    && !missing.is_empty()
                {
                    return Ok(
                        (StatusCode::ACCEPTED, Json(PartialDeployPending { missing }))
                            .into_response(),
                    );
                }
            }
            return Err(resp);
        }
    };
    worlds_metrics::partial_completed(
        Duration::from_millis((now_ms - created_ms).max(0) as u64),
        batches,
    );
    drop(entity_lock);
    Ok(success(
        state,
        creation_timestamp,
        &validated.world_name,
        &scene_parcels,
    ))
}

struct UpsertInput<'a> {
    entity_id: &'a str,
    world_name: &'a str,
    parcels: &'a [String],
    entity: &'a Value,
    deployer: &'a str,
    admitted_ms: i64,
    /// The caller saw the upload live; gone now, it expired.
    resumes: bool,
    /// The client source key a new upload is charged to.
    source: Option<String>,
}

fn client_source(headers: &HeaderMap) -> Option<String> {
    let ip = crate::shared::client_ip::client_ip(headers);
    (!ip.is_empty()).then(|| crate::shared::client_ip::source_key(&ip))
}

/// The content files the store no longer holds, marked outstanding again; `None` once the
/// upload is expired or gone (database clock).
async fn vanished(
    state: &CoreState,
    entity_id: &str,
    content: &[String],
) -> Result<Option<Vec<String>>, Response> {
    let keys: Vec<&str> = content.iter().map(String::as_str).collect();
    let present = state
        .storage
        .exist_multiple(&keys)
        .await
        .map_err(db_error)?;
    let missing: Vec<String> = content
        .iter()
        .filter(|h| !present.get(h.as_str()).copied().unwrap_or(false))
        .cloned()
        .collect();
    let mut tx = state.pool.begin().await.map_err(db_error)?;
    // Held to commit, so the cleanup cannot reclaim the upload in between.
    let live: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM worlds.pending_scenes WHERE entity_id = $1 \
           AND created_at > timezone('UTC', now()) - make_interval(secs => $2::float8 / 1000) \
         FOR SHARE",
    )
    .bind(entity_id)
    .bind(state.settings.partial_uploads.pending_deployment_ttl_ms as f64)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?;
    if live.is_none() {
        return Ok(None);
    }
    if !missing.is_empty() {
        sqlx::query(
            "UPDATE worlds.pending_scene_files SET stored = false, written = false \
             WHERE entity_id = $1 AND hash = ANY($2::text[])",
        )
        .bind(entity_id)
        .bind(&missing)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    }
    tx.commit().await.map_err(db_error)?;
    Ok(Some(missing))
}

/// Creates or resumes the upload under the per-account and per-world upload caps; whether
/// this call created it.
enum Upsert {
    Row(PendingRow, bool),
    Vanished,
}

async fn upsert(
    state: &CoreState,
    limits: &PartialUploadLimits,
    input: UpsertInput<'_>,
) -> Result<Upsert, Response> {
    let mut tx = state.pool.begin().await.map_err(db_error)?;
    let deployer = input.deployer;
    lock_key(&mut tx, &format!("worlds-partial-deployer:{deployer}"))
        .await
        .map_err(db_error)?;
    let existing: Option<PendingRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PENDING_COLUMNS} FROM worlds.pending_scenes WHERE entity_id = $1"
    )))
    .bind(input.entity_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?;
    if let Some(existing) = existing {
        if existing.expired(limits) {
            return Err(expired());
        }
        // Reservations are charged to the upload's creator, so nobody else adds batches to it.
        if existing.deployer != deployer {
            return Err(err_one(OWNERSHIP_MESSAGE));
        }
        return Ok(Upsert::Row(existing, false));
    }
    if input.resumes {
        return Ok(Upsert::Vanished);
    }
    if now_ms() >= input.admitted_ms + limits.ttl_ms() {
        return Err(expired());
    }
    let (count, oldest): (i64, Option<NaiveDateTime>) = sqlx::query_as(
        "SELECT count(*), min(created_at) FROM worlds.pending_scenes WHERE deployer = $1",
    )
    .bind(deployer)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    let max = limits.max_pending_per_deployer as i64;
    if count >= max {
        return Err(throttled(
            "uploads_per_account",
            format!(
                "Too many partial uploads in progress for this account: {count} of the {max} allowed. Complete an upload or wait for expired uploads to be cleaned up."
            ),
            retry_after(limits, oldest),
        ));
    }
    // Always after the deployer lock, so two deployers of one world cannot both take its last slot.
    lock_key(
        &mut tx,
        &format!("worlds-partial-world:{}", input.world_name),
    )
    .await
    .map_err(db_error)?;
    let (world_count, world_oldest, own_count, own_oldest): (
        i64,
        Option<NaiveDateTime>,
        i64,
        Option<NaiveDateTime>,
    ) = sqlx::query_as(
        "SELECT count(*), min(created_at), count(*) FILTER (WHERE deployer = $2), \
                min(created_at) FILTER (WHERE deployer = $2) \
         FROM worlds.pending_scenes WHERE world_name = $1 AND created_at > $3",
    )
    .bind(input.world_name)
    .bind(deployer)
    .bind(naive(now_ms() - limits.ttl_ms()))
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    let world_max = limits.max_pending_per_world as i64;
    if world_count >= world_max {
        return Err(throttled(
            "uploads_per_world",
            format!(
                "Too many partial uploads in progress for this world: {world_count} of the {world_max} allowed. Complete an upload or wait for one to expire."
            ),
            retry_after(limits, world_oldest),
        ));
    }
    // One account keeps a slot of the world free for its other deployers.
    let own_max = (world_max - 1).max(1);
    if own_count >= own_max {
        return Err(throttled(
            "uploads_per_world",
            format!(
                "Too many partial uploads in progress for this account in this world: {own_count} of the {own_max} allowed. Complete an upload or wait for one to expire."
            ),
            retry_after(limits, own_oldest),
        ));
    }
    let row: PendingRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO worlds.pending_scenes (entity_id, world_name, parcels, entity, deployer, created_at, source) \
         VALUES ($1, $2, $3::text[], $4, $5, $6, $7) RETURNING {PENDING_COLUMNS}"
    )))
    .bind(input.entity_id)
    .bind(input.world_name)
    .bind(input.parcels)
    .bind(input.entity)
    .bind(deployer)
    .bind(naive(input.admitted_ms))
    .bind(&input.source)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    Ok(Upsert::Row(row, true))
}

#[derive(sqlx::FromRow)]
struct BudgetTotals {
    account: i64,
    total: i64,
    own: i64,
    account_oldest: Option<NaiveDateTime>,
    oldest: Option<NaiveDateTime>,
    scene: i64,
    own_content: i64,
    world: i64,
    world_oldest: Option<NaiveDateTime>,
    source: i64,
    source_oldest: Option<NaiveDateTime>,
}

enum Reservation {
    Reserved,
    Vanished,
}

/// Charges the batch's bytes against the per-minute rate, then its receipts against the
/// scene cap and the account, world, source and node staging budgets, atomically.
async fn reserve(
    state: &CoreState,
    limits: &PartialUploadLimits,
    entity_id: &str,
    receipts: &[Receipt],
    max_scene_bytes: i64,
    world_scene_cap: i64,
    incoming: i64,
) -> Result<Reservation, Response> {
    let owner: Option<(String, String, NaiveDateTime, Option<String>)> = sqlx::query_as(
        "SELECT deployer, world_name, created_at, source FROM worlds.pending_scenes WHERE entity_id = $1",
    )
    .bind(entity_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error)?;
    let Some((deployer, world_name, created_at, source)) = owner else {
        return Ok(Reservation::Vanished);
    };
    let live = || now_ms() < millis(created_at) + limits.ttl_ms();
    if !live() {
        return Err(expired());
    }
    #[cfg(test)]
    if let Ok((reached, resume)) = AFTER_OWNER_LOOKUP.try_with(Clone::clone) {
        reached.notify_one();
        resume.notified().await;
    }
    // Committed on its own: a rejected batch was still received, so repeating it cannot escape the rate.
    let (rate_bytes, rate_retry): (i64, i32) = sqlx::query_as(
        "INSERT INTO worlds.partial_upload_rates AS r (deployer, window_started, bytes) \
         VALUES ($1, timezone('UTC', now()), $2) \
         ON CONFLICT (deployer) DO UPDATE SET \
           bytes = CASE WHEN r.window_started < timezone('UTC', now()) - interval '1 minute' \
             THEN EXCLUDED.bytes ELSE r.bytes + EXCLUDED.bytes END, \
           window_started = CASE WHEN r.window_started < timezone('UTC', now()) - interval '1 minute' \
             THEN timezone('UTC', now()) ELSE r.window_started END \
         RETURNING bytes, GREATEST(1, CEIL(EXTRACT(EPOCH FROM \
           window_started + interval '1 minute' - timezone('UTC', now()))))::int",
    )
    .bind(&deployer)
    .bind(incoming)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    let per_minute = limits.max_bytes_per_minute as i64;
    if incoming > per_minute {
        return Err(err_one(format!(
            "This batch is {}, above the partial upload limit of {} per minute. Send smaller batches.",
            upload_limits::format_bytes(incoming.max(0) as u64),
            upload_limits::format_bytes(per_minute.max(0) as u64)
        )));
    }
    if rate_bytes > per_minute {
        return Err(throttled(
            "bytes_per_minute",
            format!(
                "This account sent {} of partial uploads this minute, above the limit of {} per minute. Retry in {rate_retry} s.",
                upload_limits::format_bytes(rate_bytes.max(0) as u64),
                upload_limits::format_bytes(per_minute.max(0) as u64)
            ),
            i64::from(rate_retry),
        ));
    }

    let mut tx = state.pool.begin().await.map_err(db_error)?;
    // The upload's row names its content now: a file still present under the shared lock stays.
    catalyst_storage::gc::lock_store_shared(&mut tx)
        .await
        .map_err(db_error)?;
    let found: Vec<&str> = receipts
        .iter()
        .filter(|r| r.stored)
        .map(|r| r.hash.as_str())
        .collect();
    let present = state
        .storage
        .exist_multiple(&found)
        .await
        .map_err(db_error)?;
    let receipts: Vec<Receipt> = receipts
        .iter()
        .map(|r| Receipt {
            hash: r.hash.clone(),
            size: r.size,
            stored: r.stored && present.get(r.hash.as_str()).copied().unwrap_or(false),
            charged: r.charged,
        })
        .collect();
    // One short node-wide critical section makes the aggregate budgets atomic; no storage I/O under it.
    lock_key(&mut tx, "worlds-partial-budget")
        .await
        .map_err(db_error)?;
    // Publish can consume the upload without the budget lock. Keep its parent
    // through receipt writes; KEY SHARE still permits record_stored's non-key UPDATE.
    let pending: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM worlds.pending_scenes WHERE entity_id = $1 FOR KEY SHARE",
    )
    .bind(entity_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?;
    if pending.is_none() {
        return Ok(Reservation::Vanished);
    }
    if !live() {
        return Err(expired());
    }
    if !receipts.is_empty() {
        let hashes: Vec<&str> = receipts.iter().map(|r| r.hash.as_str()).collect();
        let sizes: Vec<i64> = receipts.iter().map(|r| r.size).collect();
        let stored: Vec<bool> = receipts.iter().map(|r| r.stored).collect();
        let charged: Vec<bool> = receipts.iter().map(|r| r.charged).collect();
        sqlx::query(
            "INSERT INTO worlds.pending_scene_files AS f (entity_id, hash, size, stored, charged) \
             SELECT $1, r.hash, r.size, r.stored, r.charged \
             FROM unnest($2::text[], $3::bigint[], $4::bool[], $5::bool[]) AS r(hash, size, stored, charged) \
             ON CONFLICT (entity_id, hash) DO UPDATE SET size = GREATEST(f.size, EXCLUDED.size), \
               stored = f.stored OR EXCLUDED.stored, charged = f.charged OR EXCLUDED.charged",
        )
        .bind(entity_id)
        .bind(&hashes)
        .bind(&sizes)
        .bind(&stored)
        .bind(&charged)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    }
    sqlx::query(
        "UPDATE worlds.pending_scenes SET reserved_bytes = (SELECT COALESCE(sum(size) FILTER (WHERE charged), 0) \
           FROM worlds.pending_scene_files WHERE entity_id = $1) WHERE entity_id = $1",
    )
    .bind(entity_id)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    let t: BudgetTotals = sqlx::query_as(
        "SELECT COALESCE(sum(reserved_bytes) FILTER (WHERE deployer = $2), 0)::bigint AS account, \
           COALESCE(sum(reserved_bytes), 0)::bigint AS total, \
           COALESCE(sum(reserved_bytes) FILTER (WHERE entity_id = $1), 0)::bigint AS own, \
           min(created_at) FILTER (WHERE deployer = $2 AND reserved_bytes > 0) AS account_oldest, \
           min(created_at) FILTER (WHERE reserved_bytes > 0) AS oldest, \
           (SELECT COALESCE(sum(size), 0)::bigint FROM worlds.pending_scene_files \
              WHERE entity_id = $1 AND hash <> $1) AS scene, \
           (SELECT COALESCE(sum(size), 0)::bigint FROM worlds.pending_scene_files \
              WHERE entity_id = $1 AND hash <> $1 AND charged) AS own_content, \
           (SELECT COALESCE(sum(f.size), 0)::bigint FROM worlds.pending_scene_files f \
              JOIN worlds.pending_scenes p USING (entity_id) \
              WHERE p.world_name = $3 AND p.created_at > $4 AND f.charged AND f.hash <> f.entity_id) AS world, \
           (SELECT min(p.created_at) FROM worlds.pending_scenes p WHERE p.world_name = $3 \
              AND p.created_at > $4 AND EXISTS (SELECT 1 FROM worlds.pending_scene_files f \
                WHERE f.entity_id = p.entity_id AND f.charged AND f.hash <> f.entity_id)) AS world_oldest, \
           COALESCE(sum(reserved_bytes) FILTER (WHERE source = $5 AND created_at > $4), 0)::bigint AS source, \
           min(created_at) FILTER (WHERE source = $5 AND created_at > $4 AND reserved_bytes > 0) AS source_oldest \
         FROM worlds.pending_scenes",
    )
    .bind(entity_id)
    .bind(&deployer)
    .bind(&world_name)
    .bind(naive(now_ms() - limits.ttl_ms()))
    .bind(&source)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    let source_bytes = limits.max_pending_bytes_per_source as i64;
    let account_bytes = limits.max_pending_bytes_per_deployer as i64;
    let world_bytes = world_scene_cap.saturating_mul(limits.pending_bytes_per_world_factor as i64);
    let node_bytes = limits.max_pending_bytes as i64;
    if t.scene > max_scene_bytes {
        return Err(failed(TOO_BIG_MESSAGE).into_response());
    }
    // Over a budget on its own, an upload can never be admitted.
    if t.own > account_bytes {
        return Err(err_one(format!(
            "This upload needs {} of staging, above the per-account partial upload limit of {}. Reduce its size.",
            upload_limits::format_bytes(t.own.max(0) as u64),
            upload_limits::format_bytes(account_bytes.max(0) as u64)
        )));
    }
    if t.own_content > world_bytes {
        return Err(err_one(format!(
            "This upload needs {} of staging, above the per-world partial upload limit of {}. Reduce its size.",
            upload_limits::format_bytes(t.own_content.max(0) as u64),
            upload_limits::format_bytes(world_bytes.max(0) as u64)
        )));
    }
    if t.account > account_bytes {
        return Err(throttled(
            "bytes_per_account",
            format!(
                "This batch would stage {} for this account, above its limit of {}. Complete an upload or wait for expired uploads to be cleaned up.",
                upload_limits::format_bytes(t.account.max(0) as u64),
                upload_limits::format_bytes(account_bytes.max(0) as u64)
            ),
            retry_after(limits, t.account_oldest),
        ));
    }
    if t.world > world_bytes {
        return Err(throttled(
            "bytes_per_world",
            format!(
                "This batch would stage {} for this world, above its limit of {}. Complete an upload or wait for one to expire.",
                upload_limits::format_bytes(t.world.max(0) as u64),
                upload_limits::format_bytes(world_bytes.max(0) as u64)
            ),
            retry_after(limits, t.world_oldest),
        ));
    }
    if source.is_some() && t.source > source_bytes {
        return Err(throttled(
            "bytes_per_source",
            format!(
                "This batch would stage {} for uploads from this source, above its limit of {}. Complete an upload or wait for one to expire.",
                upload_limits::format_bytes(t.source.max(0) as u64),
                upload_limits::format_bytes(source_bytes.max(0) as u64)
            ),
            retry_after(limits, t.source_oldest),
        ));
    }
    if t.total > node_bytes {
        return Err(throttled(
            "bytes_per_server",
            format!(
                "This batch would stage {} on the server, above its limit of {}. Retry later.",
                upload_limits::format_bytes(t.total.max(0) as u64),
                upload_limits::format_bytes(node_bytes.max(0) as u64)
            ),
            retry_after(limits, t.oldest),
        ));
    }
    tx.commit().await.map_err(db_error)?;
    worlds_metrics::partial_reserved_bytes(t.total);
    Ok(Reservation::Reserved)
}

/// Drops a new upload whose first batch was refused before anything was charged to it.
async fn discard_unadmitted(pool: &PgPool, entity_id: &str) {
    if let Err(error) = sqlx::query(
        "DELETE FROM worlds.pending_scenes p WHERE entity_id = $1 AND reserved_bytes = 0 \
         AND NOT EXISTS (SELECT 1 FROM worlds.pending_scene_files f WHERE f.entity_id = p.entity_id)",
    )
    .bind(entity_id)
    .execute(pool)
    .await
    {
        tracing::warn!(%error, entity_id, "partial upload: could not discard an unadmitted upload");
    }
}

/// Marks the batch's files stored by this upload and counts it; the upload's stored batches
/// so far and its stored files with their sizes, `None` once its row is gone.
async fn record_stored(
    pool: &PgPool,
    entity_id: &str,
    hashes: &[String],
) -> Result<Option<(i32, HashMap<String, i64>)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // A publish on another replica deletes the row and, by cascade, its files. The row lock makes
    // that delete wait for this commit: the files read below are never the empty set of a consumed
    // upload, and the delete cannot hold the row while it waits for a file row locked here.
    let pending: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM worlds.pending_scenes WHERE entity_id = $1 FOR KEY SHARE",
    )
    .bind(entity_id)
    .fetch_optional(&mut *tx)
    .await?;
    if pending.is_none() {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE worlds.pending_scene_files SET stored = true, written = true \
         WHERE entity_id = $1 AND hash = ANY($2::text[])",
    )
    .bind(entity_id)
    .bind(hashes)
    .execute(&mut *tx)
    .await?;
    let batches: i32 = sqlx::query_scalar(
        "UPDATE worlds.pending_scenes SET batches = batches + 1, initialized = true \
         WHERE entity_id = $1 RETURNING batches",
    )
    .bind(entity_id)
    .fetch_one(&mut *tx)
    .await?;
    let stored = progress(&mut *tx, entity_id).await?;
    tx.commit().await?;
    Ok(Some((batches, stored)))
}

/// The upload's stored files and their sizes.
async fn progress<'e>(
    db: impl sqlx::PgExecutor<'e>,
    entity_id: &str,
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT hash, size FROM worlds.pending_scene_files WHERE entity_id = $1 AND stored",
    )
    .bind(entity_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Expired uploads fetched per cleanup query.
const CLEANUP_PAGE: i64 = 100;

/// Expired uploads reclaimed under one hold of the exclusive store lock.
const RECLAIM_BATCH: usize = 20;

#[derive(Debug, thiserror::Error)]
enum ReclaimError {
    #[error("publishes kept the store lock busy")]
    Deferred,
    #[error("{0}")]
    Failed(String),
}

/// Reclaims every upload expired (database clock) when the run starts: the files it stored
/// that nothing else names are unlinked under the exclusive store lock, then its row and
/// charges go. Overlapping runs are safe (each upload is reclaimed once, under that lock); a
/// busy store lock defers the rest to the next run.
pub async fn delete_expired_uploads(state: &CoreState) -> ExpiredUploadsCleanup {
    let started = Instant::now();
    let limits = state.settings.partial_uploads;
    let result = run_cleanup(state, &limits).await;
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            tracing::error!(%error, "partial upload cleanup failed");
            ExpiredUploadsCleanup {
                outcome: "error",
                removed: 0,
                failed: 0,
            }
        }
    };
    if let Err(error) = prune_and_report(&state.pool, &limits).await {
        tracing::warn!(%error, "partial upload cleanup: pruning or reporting failed");
    }
    LAST_CLEANUP_FINISHED_MS.store(now_ms(), Ordering::Relaxed);
    worlds_metrics::partial_cleanup_run(report.outcome, started.elapsed());
    tracing::info!(?report, "expired partial uploads cleaned");
    report
}

async fn run_cleanup(
    state: &CoreState,
    limits: &PartialUploadLimits,
) -> anyhow::Result<ExpiredUploadsCleanup> {
    let ttl = limits.pending_deployment_ttl_ms as f64;
    let mut report = ExpiredUploadsCleanup {
        outcome: "success",
        removed: 0,
        failed: 0,
    };
    // The catalogue and global scenes, read once per run outside the store lock to pre-filter.
    let mut fixed: Option<HashSet<String>> = None;
    let mut after: Option<(NaiveDateTime, String)> = None;
    loop {
        let page: Vec<(String, NaiveDateTime)> = sqlx::query_as(
            "SELECT entity_id, created_at FROM worlds.pending_scenes \
             WHERE created_at <= timezone('UTC', now()) - make_interval(secs => $1::float8 / 1000) \
               AND ($2::timestamp IS NULL OR (created_at, entity_id) > ($2::timestamp, $3::text)) \
             ORDER BY created_at, entity_id LIMIT $4",
        )
        .bind(ttl)
        .bind(after.as_ref().map(|(t, _)| *t))
        .bind(after.as_ref().map(|(_, id)| id.clone()))
        .bind(CLEANUP_PAGE)
        .fetch_all(&state.pool)
        .await?;
        let Some(last) = page.last() else { break };
        after = Some((last.1, last.0.clone()));
        let fixed_refs = match &mut fixed {
            Some(fixed) => &*fixed,
            slot @ None => {
                let mut conn = state.pool.acquire().await?;
                slot.insert(catalyst_storage::gc::fixed_references(&mut conn, pins(state)).await?)
            }
        };
        for batch in page.chunks(RECLAIM_BATCH) {
            let ids: Vec<String> = batch.iter().map(|(id, _)| id.clone()).collect();
            match reclaim(state, limits, fixed_refs, &ids).await {
                Ok((removed, failed)) => {
                    for _ in 0..removed {
                        worlds_metrics::partial_expired_reclaimed();
                    }
                    for _ in 0..failed {
                        worlds_metrics::partial_cleanup_failed();
                    }
                    report.removed += removed;
                    report.failed += failed;
                }
                Err(ReclaimError::Deferred) => {
                    tracing::warn!(
                        uploads = ids.len(),
                        "deferred expired upload cleanup: the store lock is busy"
                    );
                    report.outcome = if report.failed > 0 {
                        "error"
                    } else {
                        "deferred"
                    };
                    return Ok(report);
                }
                Err(ReclaimError::Failed(error)) => {
                    report.failed += ids.len() as u64;
                    for _ in &ids {
                        worlds_metrics::partial_cleanup_failed();
                    }
                    tracing::error!(?ids, %error, "failed to reclaim expired uploads");
                }
            }
        }
        if (page.len() as i64) < CLEANUP_PAGE {
            break;
        }
    }
    if report.failed > 0 {
        report.outcome = "error";
    }
    Ok(report)
}

/// Whether the stored file at `key` is at least `floor` old.
async fn aged(state: &CoreState, key: &str, floor: Duration) -> Result<bool, ReclaimError> {
    let path = state
        .storage
        .file_path(key)
        .await
        .map_err(|e| ReclaimError::Failed(e.to_string()))?;
    let Some(path) = path else { return Ok(false) };
    Ok(tokio::fs::metadata(&path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= floor))
}

/// A batch of expired uploads under one store lock: `(reclaimed, failed)`, an upload failing
/// when a file it stored could not be unlinked (left to the collector).
async fn reclaim(
    state: &CoreState,
    limits: &PartialUploadLimits,
    fixed: &HashSet<String>,
    entity_ids: &[String],
) -> Result<(u64, u64), ReclaimError> {
    let failed = |e: &dyn std::fmt::Display| ReclaimError::Failed(e.to_string());
    // Only what each upload stored itself: an inventoried file belongs to whoever stored it.
    let written: Vec<(String, String)> = sqlx::query_as(
        "SELECT entity_id, hash FROM worlds.pending_scene_files \
         WHERE entity_id = ANY($1::text[]) AND written",
    )
    .bind(entity_ids)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| failed(&e))?;
    // The collector's age floor: a younger file may belong to a writer still committing.
    let upload_limits = &state.settings.upload_limits;
    let floor = catalyst_storage::gc::orphan_age_floor(
        upload_limits.multipart_upload_timeout_ms,
        upload_limits.deployment_processing_timeout_ms,
    );
    // Candidates are found before the lock; under it only references, a re-stat and unlinks remain.
    let mut candidates: Vec<(String, String)> = Vec::new();
    for (entity_id, key) in written {
        if !fixed.contains(&key) && aged(state, &key, floor).await? {
            candidates.push((entity_id, key));
        }
    }
    let mut lock = match catalyst_storage::gc::lock_store_exclusive(&state.pool).await {
        Ok(lock) => lock,
        Err(catalyst_storage::gc::GcError::LockTimeout) => return Err(ReclaimError::Deferred),
        Err(e) => return Err(failed(&e)),
    };
    // Deleted first, in the lock's transaction, so the reference check below no longer counts them.
    let gone: Vec<String> = sqlx::query_scalar(
        "DELETE FROM worlds.pending_scenes WHERE entity_id = ANY($1::text[]) \
           AND created_at <= timezone('UTC', now()) - make_interval(secs => $2::float8 / 1000) \
         RETURNING entity_id",
    )
    .bind(entity_ids)
    .bind(limits.pending_deployment_ttl_ms as f64)
    .fetch_all(&mut *lock)
    .await
    .map_err(|e| failed(&e))?;
    let gone: HashSet<String> = gone.into_iter().collect();
    candidates.retain(|(entity_id, _)| gone.contains(entity_id));
    let mut keys: Vec<String> = candidates.iter().map(|(_, k)| k.clone()).collect();
    keys.sort();
    keys.dedup();
    let mut referenced = catalyst_storage::gc::referenced_rows_among_on(&mut lock, &keys)
        .await
        .map_err(|e| failed(&e))?;
    // Re-read under the lock: an import since the run began may name a stored file.
    referenced.extend(
        catalyst_storage::gc::catalogue_references(&mut lock)
            .await
            .map_err(|e| failed(&e))?,
    );
    let mut unlinked: HashMap<String, bool> = HashMap::new();
    let mut broken: HashSet<String> = HashSet::new();
    for (entity_id, key) in &candidates {
        if referenced.contains(key) {
            continue;
        }
        let ok = match unlinked.get(key) {
            Some(ok) => *ok,
            None => {
                // Stored again since the scan, it is young again: keep it.
                let ok = match aged(state, key, floor).await {
                    Ok(false) => true,
                    Ok(true) => match state.storage.delete_strict(key).await {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::warn!(%error, key, "expired upload cleanup: could not unlink a file");
                            false
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, key, "expired upload cleanup: could not stat a file");
                        false
                    }
                };
                unlinked.insert(key.clone(), ok);
                ok
            }
        };
        if !ok {
            broken.insert(entity_id.clone());
        }
    }
    lock.commit().await.map_err(|e| failed(&e))?;
    let failed_count = broken.len() as u64;
    Ok((gone.len() as u64 - failed_count, failed_count))
}

/// Prunes completion receipts and rate windows and refreshes the staging gauges.
async fn prune_and_report(pool: &PgPool, limits: &PartialUploadLimits) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM worlds.completed_scene_uploads WHERE completed_at < $1")
        .bind(naive(now_ms() - limits.completed_upload_ttl_ms as i64))
        .execute(pool)
        .await?;
    sqlx::query(
        "DELETE FROM worlds.partial_upload_rates \
         WHERE window_started < timezone('UTC', now()) - interval '1 minute'",
    )
    .execute(pool)
    .await?;
    let cutoff = naive(now_ms() - limits.ttl_ms());
    let (charged, expired_bytes): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(f.size) FILTER (WHERE f.charged), 0)::bigint, \
                COALESCE(sum(f.size) FILTER (WHERE f.charged AND p.created_at <= $1), 0)::bigint \
         FROM worlds.pending_scene_files f JOIN worlds.pending_scenes p USING (entity_id)",
    )
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    let (live, expired): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > $1), count(*) FILTER (WHERE created_at <= $1) \
         FROM worlds.pending_scenes",
    )
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    worlds_metrics::partial_snapshot(charged, expired_bytes, live, expired);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_flag_is_read_from_the_query_string() {
        let uri = |s: &str| s.parse::<Uri>().unwrap();
        assert!(query_declares_partial(&uri("/entities?partial=true")));
        assert!(query_declares_partial(&uri("/entities?a=1&partial=true")));
        assert!(!query_declares_partial(&uri("/entities?partial=false")));
        assert!(!query_declares_partial(&uri("/entities")));
    }

    #[test]
    fn limits_require_a_world_size_scene_to_fit() {
        let limits = PartialUploadLimits::default();
        assert!(limits.validate(300 * 1024 * 1024).is_ok());
        assert!(
            PartialUploadLimits {
                max_pending_bytes_per_deployer: 1,
                ..limits
            }
            .validate(300 * 1024 * 1024)
            .is_err()
        );
        assert!(
            PartialUploadLimits {
                max_pending_bytes: limits.max_pending_bytes_per_deployer - 1,
                ..limits
            }
            .validate(1)
            .is_err()
        );
        assert!(
            PartialUploadLimits {
                max_pending_per_world: 0,
                ..limits
            }
            .validate(1)
            .is_err()
        );
    }

    #[test]
    fn uploads_leave_a_quarter_of_the_pool() {
        assert_eq!(uploads_within_pool(40, 60), 40);
        assert_eq!(uploads_within_pool(46, 60), 45);
        // The dev and image-smoke pools start with the default.
        assert_eq!(uploads_within_pool(40, 12), 9);
        assert_eq!(uploads_within_pool(40, 8), 6);
        assert_eq!(uploads_within_pool(40, 1), 1);
    }

    #[tokio::test]
    async fn batches_of_one_entity_wait_for_each_other_and_leave_no_entry() {
        let id = format!("entity-lock-{}", uuid::Uuid::new_v4().simple());
        let held = lock_entity(&id).await;
        let waiter = lock_entity(&id);
        tokio::pin!(waiter);
        assert!(
            futures::poll!(&mut waiter).is_pending(),
            "the second batch waits"
        );
        let other = lock_entity("another entity").await;
        drop(other);
        drop(held);
        drop(waiter.await);
        assert!(!ENTITY_LOCKS.lock().contains_key(&id));
    }

    #[test]
    fn a_source_budget_must_fit_one_account() {
        let limits = PartialUploadLimits::default();
        assert!(
            PartialUploadLimits {
                max_pending_bytes_per_source: limits.max_pending_bytes_per_deployer - 1,
                ..limits
            }
            .validate(1)
            .is_err()
        );
    }

    #[test]
    fn retry_after_is_at_least_a_second_and_points_at_the_oldest_expiry() {
        let limits = PartialUploadLimits::default();
        let soon = naive(now_ms() - limits.ttl_ms() + 10_000);
        let wait = retry_after(&limits, Some(soon));
        assert!((9..=11).contains(&wait), "{wait}");
        assert!(retry_after(&limits, Some(naive(0))) >= 1);
    }
}
