//! Partial (multi-request) scene uploads, ADR-325: `POST /entities` with the
//! form field `partial=true` (and, recommended, `?partial=true`, which
//! declares the batch before its body is read). Upstream
//! `src/logic/partial-deployments/component.ts`, its repository
//! (`adapters/pending-deployments-repository`) and `docs/PARTIAL_UPLOADS.md`
//! at `decentraland/catalyst` a416565.
//!
//! Each batch carries `entityId`, the auth chain and files keyed by their
//! content hash. The first one must carry the entity file (IPFS v2 scenes
//! only, at most 10 MiB); later ones by the account that started the upload
//! may leave it out and it is read back from the store. A batch is checked
//! without holding anything (hashes, the staging subset of the validator, the
//! newer-entity, rate-limit and TTL checks), then, under the entity's lock,
//! accounted against the quotas, stored, and recorded in
//! `content.pending_deployments` / `content.pending_deployment_files`. While
//! content is missing the answer is `202 {"missing": [...]}`; the batch that
//! completes the upload publishes it through the write deployer, which
//! validates it in full (content already in the store), and answers
//! `200 {"creationTimestamp"}`. A batch for an entity already deployed answers
//! 200 with the stored `local_timestamp`.
//!
//! Quotas, with upstream's messages and `Retry-After` on each 429: uploads
//! per account, live uploads per parcel (and per account on a parcel), staged
//! bytes per account and per server, batch bytes per account per minute, the
//! scene size cap. An upload lives `PENDING_DEPLOYMENT_TTL` from its first
//! batch; the leased `content-partial-uploads-cleanup` job deletes expired
//! uploads and unlinks the staged blobs they wrote themselves (a receipt's
//! `written`) that the collector would: past its age floor, outside its
//! fixed references (asset packs, global scenes) and named by no domain
//! ([`gc::unlink_orphans`], which knows the pending tables). A blob still
//! younger than the floor is left to the collector: a regular deploy may be
//! committing it (the SDK skips what `/available-content` reports).
//!
//! The entity's lock is PostgreSQL's, so it holds across replicas: a
//! session advisory lock on a connection of its own, held from a batch's
//! accounting through its publication (or its release of an overtaken
//! upload). Cleanup takes it without waiting and skips an upload whose lock
//! is held, so it never drops an upload a batch is committing.
//!
//! The quota 429s' Retry-After counts from the last cleanup run, which
//! every replica reads from `content.system_properties`
//! (`partial_uploads_cleanup_at`).
//!
//! The budgets are made atomic by one server-wide advisory lock
//! (`partial-upload-budget`, as upstream's `acquireBudgetLock`): every
//! batch's accounting on every replica runs under it, and sums
//! `reserved_bytes` over all of `content.pending_deployments`. Its cost
//! grows with the live uploads (bounded by the per-account caps) and
//! serializes accounting, not storage or publication; it has not been
//! measured, so raise the upload caps only after measuring it.
//!
//! As upstream, staged blobs go to the one content store, so
//! `/contents/{hash}` serves them before publication (upstream stores a
//! batch with `storeStreamsInBatches` into the served `storage`): an
//! account may hold `MAX_PENDING_BYTES_PER_DEPLOYER` of content for up to
//! `PENDING_DEPLOYMENT_TTL`.
//!
//! Not as upstream: a batch is held in memory like a regular deploy, so it
//! keeps the in-memory upload budget; a declared batch from a source that
//! spent its daily quota, or from an account over its byte rate, is turned
//! away before its files are buffered ([`admit_before_files`]), on the
//! claimed signer and entity (authentication follows). There is no shared
//! content lock against the collector: the receipts are written before the
//! blobs, and the collector reads them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::pool::PoolConnection;

use super::entity::{Entity, EntityType};
use super::write_deployer::{
    DeployFailure, REQUEST_TTL_FORWARDS_MS, RejectionKind, UPLOAD_HASH_JOBS, WriteDeployer,
    bounded_upload_hash,
};
use crate::auth::{AuthChain, hash_bytes_v1};
use crate::config::Config;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::storage::{Storage, gc};

/// The entity file cap on both paths: a resume reads it back from the store.
pub const MAX_ENTITY_FILE_SIZE_BYTES: usize = 10 * 1024 * 1024;

pub const JOB_NAME: &str = "content-partial-uploads-cleanup";

/// A failed publication is retried by resending the last batch.
const INTERNAL_ERROR_RETRY_AFTER_SECS: u64 = 5;
const EXPIRED_UPLOADS_PER_PAGE: i64 = 100;
const STORE_CONCURRENCY: usize = 8;
/// How long a batch waits for its entity's lock (another batch of the
/// upload publishing it) before answering 503.
const ENTITY_LOCK_WAIT: Duration = Duration::from_secs(30);
/// Upstream's waits for pointers another deploy holds when publishing.
const FINALIZE_POINTER_CONFLICT_RETRIES: u32 = 3;
const FINALIZE_POINTER_CONFLICT_DELAY: Duration = Duration::from_millis(300);
const POINTER_CONFLICT_RETRY_AFTER_SECS: u64 = 5;
/// Upstream's `Retry-After` on a busy lock (`SERVICE_UNAVAILABLE_RETRY_AFTER_SECONDS`).
const LOCK_BUSY_RETRY_AFTER_SECS: u64 = 10;
/// The `content.system_properties` key of the last cleanup run's end (ms).
const CLEANUP_AT_KEY: &str = "partial_uploads_cleanup_at";

/// The partial-upload limits (upstream's names and defaults).
#[derive(Clone, Debug)]
pub struct Limits {
    /// `PENDING_DEPLOYMENT_TTL`, ms.
    pub ttl_ms: i64,
    /// `PENDING_DEPLOYMENTS_CLEANUP_INTERVAL`, ms.
    pub cleanup_interval_ms: i64,
    /// `MAX_PENDING_DEPLOYMENTS_PER_DEPLOYER`.
    pub max_per_deployer: i64,
    /// `MAX_PENDING_UPLOADS_PER_POINTER`.
    pub max_per_pointer: i64,
    /// `MAX_PENDING_BYTES_PER_DEPLOYER`.
    pub account_bytes: i64,
    /// `MAX_PENDING_BYTES`.
    pub global_bytes: i64,
    /// `MAX_PARTIAL_UPLOAD_BYTES_PER_MINUTE`.
    pub bytes_per_minute: i64,
    /// The collector's age floor ([`gc::orphan_age_floor`]): cleanup
    /// unlinks no younger blob.
    pub orphan_min_age: Duration,
    /// The global scenes the collector keeps (`GLOBAL_SCENES_URN`).
    pub global_scenes_urn: Vec<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ttl_ms: 60 * 60 * 1000,
            cleanup_interval_ms: 5 * 60 * 1000,
            max_per_deployer: 10,
            max_per_pointer: 4,
            account_bytes: 1024 * 1024 * 1024,
            global_bytes: 50 * 1024 * 1024 * 1024,
            bytes_per_minute: 512 * 1024 * 1024,
            orphan_min_age: gc::MIN_ORPHAN_AGE,
            global_scenes_urn: Vec::new(),
        }
    }
}

impl Limits {
    pub fn from_config(config: &Config) -> Self {
        let c = &config.content;
        let int = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        Self {
            ttl_ms: int(c.pending_deployment_ttl_ms.max(1)),
            cleanup_interval_ms: int(c.pending_deployments_cleanup_interval_ms.max(1000)),
            max_per_deployer: i64::from(c.max_pending_deployments_per_deployer.max(1)),
            max_per_pointer: i64::from(c.max_pending_uploads_per_pointer.max(1)),
            account_bytes: int(c.max_pending_bytes_per_deployer),
            global_bytes: int(c.max_pending_bytes),
            bytes_per_minute: int(c.max_partial_upload_bytes_per_minute),
            orphan_min_age: gc::orphan_age_floor(
                config.worlds.multipart_upload_timeout_ms,
                config.worlds.deployment_processing_timeout_ms,
            ),
            global_scenes_urn: config
                .worlds_content
                .global_scenes_urn
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        }
    }
}

/// Whether the query declares a partial batch before its body (`partial=true`).
pub fn is_declared(query: Option<&str>) -> bool {
    let params = crate::query_params::parse_query_string(query.unwrap_or(""));
    crate::query_params::qs_get_string(&params, "partial").as_deref() == Some("true")
}

/// A batch turned away before its files are buffered, on what its fields
/// claim (the chain is checked later, and a false claim only refuses the
/// claimer's own request): with `quota_exhausted` set, only a replay of a
/// published entity or a resume of the claimed signer's own live upload
/// goes on (upstream's message, checked there after authentication); an
/// account whose byte-rate window is already over its limit waits for the
/// window's end. Counts nothing: neither 429 is counted upstream either.
/// A lookup that fails lets the batch on to the full checks.
pub async fn admit_before_files(
    deployer: &WriteDeployer,
    limits: &Limits,
    entity_id: &str,
    claimed_signer: &str,
    quota_exhausted: Option<u64>,
) -> Option<Response> {
    let pool = deployer.pool();
    let signer = claimed_signer.to_lowercase();
    if let Some(retry_after) = quota_exhausted
        && let Ok(None) = deployer.deployed_timestamp(entity_id).await
    {
        let own_live = pending(pool, entity_id).await.map(|p| {
            p.is_some_and(|p| {
                now_ms() <= p.created_at + limits.ttl_ms && p.deployer_address == signer
            })
        });
        if let Ok(false) = own_live {
            return Some(
                Refusal::throttled(
                    "The daily deployment quota for this client is exhausted. Only uploads in progress can continue.",
                    retry_after,
                )
                .into_response(),
            );
        }
    }
    let window: Option<(i64, f64)> = sqlx::query_as(
        "SELECT bytes,
           (EXTRACT(EPOCH FROM window_started + interval '1 minute' - timezone('UTC', now())) * 1000)::float8
         FROM content.partial_upload_rates
         WHERE deployer_address = $1 AND window_started >= timezone('UTC', now()) - interval '1 minute'",
    )
    .bind(&signer)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let (window_bytes, ends_in_ms) = window?;
    if window_bytes <= limits.bytes_per_minute {
        return None;
    }
    let retry_after = (ends_in_ms / 1000.0).ceil().max(1.0) as u64;
    Some(
        Refusal::throttled(
            format!(
                "Partial upload byte rate exceeded for this account: {window_bytes} bytes in the current one-minute window, max {}. Retry in {retry_after} s.",
                limits.bytes_per_minute
            ),
            retry_after,
        )
        .into_response(),
    )
}

/// One parsed batch.
pub struct Batch {
    pub entity_id: String,
    /// The chain as the form spelled it, handed to the deployer at publication.
    pub auth_chain: Value,
    /// Files by their field name, in form order.
    pub files: Vec<(String, Bytes)>,
    /// When the request arrived (ms): a new upload's lifetime starts here.
    pub requested_at_ms: i64,
}

/// What the daily quota needs to know about a settled batch (upstream
/// `PartialQuotaState`): an upload counts once, when its first batch creates
/// it, and a batch refused with a 400 before joining its upload counts too.
#[derive(Default, Debug)]
pub struct Admission {
    pub admitted: bool,
    pub created: bool,
}

/// A batch's refusal: 400 (`retry_after` unset) or 429 `{errors}`, 503
/// `{errors}`, the 503 `{error}` of a busy entity lock, or a 500.
#[derive(Debug)]
enum Refusal {
    LockBusy(String),
    Invalid {
        errors: Vec<String>,
        retry_after: Option<u64>,
    },
    Unavailable {
        errors: Vec<String>,
        retry_after: u64,
    },
    Internal(String),
}

impl Refusal {
    fn invalid(message: impl Into<String>) -> Self {
        Refusal::Invalid {
            errors: vec![message.into()],
            retry_after: None,
        }
    }

    fn throttled(message: impl Into<String>, retry_after: u64) -> Self {
        Refusal::Invalid {
            errors: vec![message.into()],
            retry_after: Some(retry_after.max(1)),
        }
    }

    fn expired() -> Self {
        Self::invalid("This upload expired. Create a new entity with a fresh timestamp.")
    }

    fn must_include(entity_id: &str) -> Self {
        Self::invalid(format!(
            "The first partial request for an entity, and any request by another signer, must include the entity file '{entity_id}'."
        ))
    }

    fn from_deploy(failure: DeployFailure) -> Self {
        match failure {
            DeployFailure::Rejected(_, errors) => Refusal::Invalid {
                errors,
                retry_after: None,
            },
            DeployFailure::Unavailable(errors) | DeployFailure::Unverifiable(errors) => {
                Refusal::Unavailable {
                    errors,
                    retry_after: super::writes::UNAVAILABLE_RETRY_AFTER_SECS
                        .parse()
                        .unwrap_or(30),
                }
            }
        }
    }

    fn into_response(self) -> Response {
        let (status, errors, retry_after) = match self {
            Refusal::LockBusy(entity_id) => {
                let mut response = super::errors::json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &format!(
                        "The content lock for entity {entity_id} is still busy, please retry shortly."
                    ),
                );
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    HeaderValue::from(LOCK_BUSY_RETRY_AFTER_SECS),
                );
                return response;
            }
            Refusal::Invalid {
                errors,
                retry_after: None,
            } => (StatusCode::BAD_REQUEST, errors, None),
            Refusal::Invalid {
                errors,
                retry_after: Some(secs),
            } => (StatusCode::TOO_MANY_REQUESTS, errors, Some(secs)),
            Refusal::Unavailable {
                errors,
                retry_after,
            } => (StatusCode::SERVICE_UNAVAILABLE, errors, Some(retry_after)),
            Refusal::Internal(detail) => {
                return super::errors::AppError::Internal(detail).into_response();
            }
        };
        let mut response = (status, Json(json!({ "errors": errors }))).into_response();
        if let Some(secs) = retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}

impl From<sqlx::Error> for Refusal {
    fn from(e: sqlx::Error) -> Self {
        Refusal::Internal(format!("partial upload: {e}"))
    }
}

/// A batch's success: published (or already published) at a timestamp, or
/// stored with content still missing.
enum Outcome {
    Published(i64),
    Missing(Vec<String>),
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// When the last cleanup run ended on any replica (ms), for the Retry-After
/// of the quotas only cleanup frees; `None` before any.
async fn last_cleanup_at(executor: impl sqlx::PgExecutor<'_>) -> sqlx::Result<Option<i64>> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM content.system_properties WHERE key = $1")
            .bind(CLEANUP_AT_KEY)
            .fetch_optional(executor)
            .await?;
    Ok(value.and_then(|v| v.parse().ok()))
}

/// Upstream `secondsUntilCleanupFreesQuota`: the first cleanup run after the
/// oldest charged upload expires, runs `interval_ms` apart from the last one
/// (from now before any); at least 1 s.
fn seconds_until_cleanup_frees_quota(
    now: i64,
    oldest_created_at: Option<i64>,
    ttl_ms: i64,
    last_cleanup_at: Option<i64>,
    interval_ms: i64,
) -> u64 {
    let reclaimable_at = oldest_created_at.map_or(now, |oldest| now.max(oldest + ttl_ms));
    let base = last_cleanup_at.unwrap_or(now);
    let runs = ((reclaimable_at - base) as f64 / interval_ms as f64)
        .ceil()
        .max(1.0) as i64;
    (((base + runs * interval_ms - now) as f64 / 1000.0).ceil() as i64).max(1) as u64
}

/// One pointer's live uploads, overall and by the batch's account.
#[derive(sqlx::FromRow, Debug, Clone)]
struct PointerUsage {
    pointer: String,
    uploads: i64,
    deployer_uploads: i64,
    oldest: i64,
    deployer_oldest: Option<i64>,
}

/// Upstream `checkPointerQuotas`: each pointer may be held by at most
/// `max_per_pointer` live uploads, and by all but one of them (at least 1)
/// of one account; Retry-After is when every full pointer's oldest upload in
/// scope expires.
fn check_pointer_quotas(
    usage: &[PointerUsage],
    max_per_pointer: i64,
    now: i64,
    ttl_ms: i64,
) -> Option<(String, u64)> {
    let max_per_deployer = (max_per_pointer - 1).max(1);
    let retry_after = |oldest: &[i64]| {
        let latest = oldest.iter().copied().max().unwrap_or(now);
        (((latest + ttl_ms - now) as f64 / 1000.0).ceil() as i64).max(1) as u64
    };
    let describe = |full: &[&PointerUsage]| {
        let mut text = full
            .iter()
            .take(5)
            .map(|u| u.pointer.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if full.len() > 5 {
            text.push_str(&format!(" and {} more", full.len() - 5));
        }
        text
    };
    let full: Vec<&PointerUsage> = usage
        .iter()
        .filter(|u| u.uploads >= max_per_pointer)
        .collect();
    if !full.is_empty() {
        let oldest: Vec<i64> = full.iter().map(|u| u.oldest).collect();
        return Some((
            format!(
                "Too many partial uploads in progress for parcels {} (max {max_per_pointer} per parcel). Complete an upload or wait for one to expire.",
                describe(&full)
            ),
            retry_after(&oldest),
        ));
    }
    let own_full: Vec<&PointerUsage> = usage
        .iter()
        .filter(|u| u.deployer_uploads >= max_per_deployer)
        .collect();
    if !own_full.is_empty() {
        let oldest: Vec<i64> = own_full
            .iter()
            .map(|u| u.deployer_oldest.unwrap_or(now - ttl_ms))
            .collect();
        return Some((
            format!(
                "Too many partial uploads in progress for this account on parcels {} (max {max_per_deployer} per parcel). Complete an upload or wait for one to expire.",
                describe(&own_full)
            ),
            retry_after(&oldest),
        ));
    }
    None
}

/// Upstream `IPFSv2.validate` (`@dcl/schemas`): a CIDv1 in base32, as the
/// entity files the SDK hashes (`bafk...`/`bafy...`, 59 characters).
fn is_ipfs_v2(id: &str) -> bool {
    id.len() == 59
        && (id.starts_with("bafy") || id.starts_with("bafk"))
        && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The upload row as a batch reads it.
#[derive(sqlx::FromRow, Debug, Clone)]
struct Pending {
    deployer_address: String,
    created_at: i64,
    initialized: bool,
}

const PENDING_COLUMNS: &str = "deployer_address,
    (EXTRACT(EPOCH FROM created_at) * 1000)::bigint AS created_at, initialized";

async fn pending(
    executor: impl sqlx::PgExecutor<'_>,
    entity_id: &str,
) -> sqlx::Result<Option<Pending>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PENDING_COLUMNS} FROM content.pending_deployments WHERE entity_id = $1"
    )))
    .bind(entity_id)
    .fetch_optional(executor)
    .await
}

/// The entity's lock, shared by every replica: a session advisory lock on a
/// pool connection kept for it. Batches of one upload are serialized from
/// accounting through publication; cleanup drops no upload while it is
/// held. Dropped without [`EntityLock::release`] (a cancelled request), the
/// guard closes its connection, which ends the session and so the lock.
struct EntityLock {
    conn: Option<PoolConnection<sqlx::Postgres>>,
    entity_id: String,
}

impl EntityLock {
    /// Takes the lock unless another session holds it.
    async fn try_acquire(pool: &PgPool, entity_id: &str) -> sqlx::Result<Option<Self>> {
        let mut lock = Self {
            conn: Some(pool.acquire().await?),
            entity_id: entity_id.to_string(),
        };
        let conn = lock.conn.as_mut().expect("set above");
        let taken: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_lock(hashtextextended('partial_entity:' || $1, 0))",
        )
        .bind(entity_id)
        .fetch_one(&mut **conn)
        .await?;
        if taken {
            return Ok(Some(lock));
        }
        // Not held: the connection goes back to the pool as it is.
        drop(lock.conn.take());
        Ok(None)
    }

    /// Waits up to [`ENTITY_LOCK_WAIT`], polling without holding a
    /// connection meanwhile, so waiting batches never drain the pool.
    async fn acquire(pool: &PgPool, entity_id: &str) -> sqlx::Result<Option<Self>> {
        let deadline = tokio::time::Instant::now() + ENTITY_LOCK_WAIT;
        let mut pause = Duration::from_millis(20);
        loop {
            if let Some(lock) = Self::try_acquire(pool, entity_id).await? {
                return Ok(Some(lock));
            }
            if tokio::time::Instant::now() + pause > deadline {
                return Ok(None);
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(500));
        }
    }

    async fn release(mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        let unlocked =
            sqlx::query("SELECT pg_advisory_unlock(hashtextextended('partial_entity:' || $1, 0))")
                .bind(&self.entity_id)
                .execute(&mut *conn)
                .await;
        if let Err(e) = unlocked {
            tracing::warn!(entity_id = %self.entity_id, error = %e, "failed to release a partial upload's lock; closing its session");
            conn.close_on_drop();
        }
    }
}

impl Drop for EntityLock {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.as_mut() {
            conn.close_on_drop();
        }
    }
}

/// The partial-upload path of `POST /entities` for an admitted, parsed
/// batch. `quota_exhausted` is the daily quota's Retry-After when the
/// client's source has spent it: only a resume of the signer's own live
/// upload may then proceed. `admission` tells the caller what to count.
pub async fn handle(
    deployer: &WriteDeployer,
    limits: &Limits,
    batch: Batch,
    quota_exhausted: Option<u64>,
    admission: &mut Admission,
) -> Response {
    let entity_id = batch.entity_id.clone();
    match stage(deployer, limits, batch, quota_exhausted, admission).await {
        Ok(Outcome::Published(creation_timestamp)) => {
            tracing::info!(entity_id = %entity_id, "POST /entities - Partial deployment published");
            (
                StatusCode::OK,
                Json(json!({ "creationTimestamp": creation_timestamp })),
            )
                .into_response()
        }
        Ok(Outcome::Missing(missing)) => {
            tracing::info!(entity_id = %entity_id, missing = missing.len(), "POST /entities - Partial deployment staged");
            (StatusCode::ACCEPTED, Json(json!({ "missing": missing }))).into_response()
        }
        Err(refusal) => {
            tracing::warn!(entity_id = %entity_id, ?refusal, "POST /entities - Partial deployment refused");
            refusal.into_response()
        }
    }
}

async fn stage(
    deployer: &WriteDeployer,
    limits: &Limits,
    batch: Batch,
    quota_exhausted: Option<u64>,
    admission: &mut Admission,
) -> Result<Outcome, Refusal> {
    let pool = deployer.pool();
    let entity_id = batch.entity_id.as_str();
    // Deployments are idempotent: a replay gets the original timestamp
    // without re-authentication (its chain may have expired since).
    if let Some(ts) = deployer.deployed_timestamp(entity_id).await? {
        return Ok(Outcome::Published(ts));
    }
    let auth_chain: AuthChain = serde_json::from_value(batch.auth_chain.clone())
        .map_err(|e| Refusal::invalid(format!("invalid auth chain: {e}")))?;
    let deployer_address = auth_chain
        .first()
        .map(|link| link.payload.to_lowercase())
        .unwrap_or_default();

    if let Some(retry_after) = quota_exhausted {
        let own_live = pending(pool, entity_id).await?.is_some_and(|p| {
            now_ms() <= p.created_at + limits.ttl_ms && p.deployer_address == deployer_address
        });
        if !own_live {
            return Err(Refusal::throttled(
                "The daily deployment quota for this client is exhausted. Only uploads in progress can continue.",
                retry_after,
            ));
        }
    }

    // Authenticated before anything is stored: at the entity's timestamp when
    // the batch carries a readable entity file, else now (a resume, whose
    // read-back needs a chain valid now).
    let uploaded_entity = batch
        .files
        .iter()
        .rev()
        .find(|(name, _)| name == entity_id)
        .map(|(_, bytes)| bytes.clone());
    let signed_at = uploaded_entity
        .as_ref()
        .filter(|bytes| bytes.len() <= MAX_ENTITY_FILE_SIZE_BYTES)
        .filter(|bytes| hash_bytes_v1(bytes) == entity_id)
        .and_then(|bytes| serde_json::from_slice::<Entity>(bytes).ok())
        .map_or_else(now_ms, |entity| entity.timestamp);
    deployer
        .check_signature(entity_id, &auth_chain, signed_at)
        .await
        .map_err(Refusal::from_deploy)?;

    let prepared = prepare(
        deployer,
        limits,
        &batch,
        &auth_chain,
        &deployer_address,
        uploaded_entity,
    )
    .await?;
    let Some(lock) = EntityLock::acquire(pool, entity_id).await? else {
        // A concurrent batch may have held it while publishing the entity.
        if let Some(ts) = deployer.deployed_timestamp(entity_id).await? {
            return Ok(Outcome::Published(ts));
        }
        return Err(Refusal::LockBusy(entity_id.to_string()));
    };
    let outcome = commit(deployer, limits, &batch, prepared, admission).await;
    lock.release().await;
    outcome
}

/// A batch whose request-only checks passed.
struct Prepared {
    entity: Entity,
    entity_bytes: Bytes,
    files: HashMap<String, Bytes>,
    deployer_address: String,
    content_hashes: Vec<String>,
    max_scene_bytes: i64,
    seen: Option<Pending>,
    /// A newer deployment overtook this live upload, which can never be
    /// published: the commit releases it under the entity's lock.
    overtaken: bool,
}

async fn prepare(
    deployer: &WriteDeployer,
    limits: &Limits,
    batch: &Batch,
    auth_chain: &AuthChain,
    deployer_address: &str,
    uploaded_entity: Option<Bytes>,
) -> Result<Prepared, Refusal> {
    let pool = deployer.pool();
    let entity_id = batch.entity_id.as_str();
    if !is_ipfs_v2(entity_id) {
        return Err(Refusal::invalid(format!(
            "The entity id '{entity_id}' is not a valid IPFS v2 hash. Partial deployments require IPFS v2 entities."
        )));
    }

    // The field names are load-bearing here: every file must hash to its key.
    let named: Vec<(String, Bytes)> = batch.files.clone();
    let hashed = bounded_upload_hash(Arc::clone(&UPLOAD_HASH_JOBS), move || {
        named
            .into_iter()
            .map(|(key, bytes)| {
                let computed = hash_bytes_v1(&bytes);
                (key, computed, bytes)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| Refusal::Unavailable {
        errors: vec![e],
        retry_after: INTERNAL_ERROR_RETRY_AFTER_SECS,
    })?;
    let mut files: HashMap<String, Bytes> = HashMap::with_capacity(hashed.len());
    for (key, computed, bytes) in hashed {
        if key != computed {
            return Err(Refusal::invalid(format!(
                "The uploaded file '{key}' does not match its content hash (computed {computed})."
            )));
        }
        files.insert(key, bytes);
    }

    let entity_bytes = match uploaded_entity {
        Some(bytes) => {
            if bytes.len() > MAX_ENTITY_FILE_SIZE_BYTES {
                return Err(Refusal::invalid(format!(
                    "The entity file '{entity_id}' is too large ({} bytes, max {MAX_ENTITY_FILE_SIZE_BYTES}).",
                    bytes.len()
                )));
            }
            bytes
        }
        None => {
            read_back_entity_file(deployer, limits, entity_id, auth_chain, deployer_address).await?
        }
    };

    let mut entity: Entity = serde_json::from_slice(&entity_bytes).map_err(|e| {
        tracing::info!(entity_id, error = %e, "partial upload refused: unparsable entity file");
        Refusal::invalid("There was a problem parsing the entity")
    })?;
    entity.id = entity_id.to_string();
    for pointer in &mut entity.pointers {
        *pointer = pointer.to_lowercase();
    }
    if entity.entity_type != EntityType::Scene {
        return Err(Refusal::invalid(format!(
            "Partial deployments are only supported for scenes, but the entity type is '{}'.",
            entity.entity_type
        )));
    }
    if entity.pointers.is_empty() {
        return Err(Refusal::invalid("The entity does not have any pointer."));
    }

    let seen = pending(pool, entity_id).await?;
    if seen
        .as_ref()
        .is_some_and(|p| now_ms() > p.created_at + limits.ttl_ms)
    {
        return Err(Refusal::expired());
    }
    // A resume by the upload's creator skips the access step: creating the
    // upload passed it, and publication runs it again.
    let resume_by_creator = seen
        .as_ref()
        .is_some_and(|p| p.deployer_address == deployer_address);
    // The staging checks read which files came, not their bytes.
    let staging_files: HashMap<String, Bytes> = files
        .keys()
        .map(|key| {
            let bytes = if key == entity_id {
                entity_bytes.clone()
            } else {
                Bytes::new()
            };
            (key.clone(), bytes)
        })
        .chain(std::iter::once((
            entity_id.to_string(),
            entity_bytes.clone(),
        )))
        .collect();
    deployer
        .validate_staging(&entity, &staging_files, auth_chain, !resume_by_creator)
        .await
        .map_err(|failure| match failure {
            DeployFailure::Rejected(_, errors) if errors.is_empty() => {
                Refusal::invalid("The staging validation was not successful.")
            }
            other => Refusal::from_deploy(other),
        })?;

    let mut seen_hashes = HashSet::new();
    let content_hashes: Vec<String> = entity
        .content
        .iter()
        .map(|c| c.hash.clone())
        .filter(|h| seen_hashes.insert(h.clone()))
        .collect();
    let max_scene_bytes = super::validator::types::max_size_mb(EntityType::Scene).unwrap_or(15)
        as i64
        * 1024
        * 1024
        * entity.pointers.len() as i64;

    // A fast refusal for new uploads only; publication enforces the order.
    if seen.is_none() && deployer.has_newer_deployment(&entity).await? {
        return Err(newer_entity(&entity));
    }
    if let Some(retry_after) =
        deployer.rate_limit_retry_after(entity.entity_type.as_str(), &entity.pointers)
    {
        // The deployment holding the limit may have overtaken this upload, which never clears.
        if seen.is_some() && deployer.has_newer_deployment(&entity).await? {
            return Ok(Prepared {
                entity,
                entity_bytes,
                files,
                deployer_address: deployer_address.to_string(),
                content_hashes,
                max_scene_bytes,
                seen,
                overtaken: true,
            });
        }
        return Err(Refusal::throttled(
            format!(
                "Entity rate limited (entityId={} pointers={}).",
                entity.id,
                entity.pointers.join(",")
            ),
            retry_after,
        ));
    }
    // Freshness and lifetime are measured from the upload's first request.
    let ttl_anchor = seen
        .as_ref()
        .map_or(batch.requested_at_ms, |p| p.created_at);
    if ttl_anchor - entity.timestamp > deployer.request_ttl_backwards_ms() {
        return Err(Refusal::invalid(format!(
            "The request is not recent enough, please submit it again with a new timestamp (entityId={}).",
            entity.id
        )));
    }
    if now_ms() - entity.timestamp < -REQUEST_TTL_FORWARDS_MS {
        return Err(Refusal::invalid(format!(
            "The request is too far in the future, please submit it again with a new timestamp (entityId={}).",
            entity.id
        )));
    }

    Ok(Prepared {
        entity,
        entity_bytes,
        files,
        deployer_address: deployer_address.to_string(),
        content_hashes,
        max_scene_bytes,
        seen,
        overtaken: false,
    })
}

fn newer_entity(entity: &Entity) -> Refusal {
    Refusal::invalid(super::write_deployer::newer_entity_message(entity))
}

/// A resume's entity file, read back from the store; only for a chain valid
/// now and a live upload this signer started, since the read is outside the
/// multipart limits.
async fn read_back_entity_file(
    deployer: &WriteDeployer,
    limits: &Limits,
    entity_id: &str,
    auth_chain: &AuthChain,
    deployer_address: &str,
) -> Result<Bytes, Refusal> {
    if deployer
        .check_signature(entity_id, auth_chain, now_ms())
        .await
        .is_err()
    {
        return Err(Refusal::must_include(entity_id));
    }
    let live_own = pending(deployer.pool(), entity_id).await?.is_some_and(|p| {
        now_ms() <= p.created_at + limits.ttl_ms && p.deployer_address == deployer_address
    });
    if !live_own {
        return Err(Refusal::must_include(entity_id));
    }
    let stored = deployer
        .storage()
        .retrieve(entity_id)
        .await
        .map_err(|e| store_unreadable(entity_id, &e))?
        .ok_or_else(|| Refusal::must_include(entity_id))?;
    if stored.len() > MAX_ENTITY_FILE_SIZE_BYTES {
        return Err(Refusal::invalid(format!(
            "The stored entity file is too large (over {MAX_ENTITY_FILE_SIZE_BYTES} bytes)."
        )));
    }
    if hash_bytes_v1(&stored) != entity_id {
        tracing::warn!(
            entity_id,
            "the stored entity file of a partial upload does not match its id"
        );
        // Unrecorded, so the entity file the client resends replaces the copy.
        mark_missing(deployer.pool(), entity_id, &[entity_id.to_string()]).await?;
        return Err(Refusal::must_include(entity_id));
    }
    Ok(stored)
}

async fn mark_missing(pool: &PgPool, entity_id: &str, hashes: &[String]) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE content.pending_deployment_files SET stored = false
         WHERE entity_id = $1 AND hash = ANY($2)",
    )
    .bind(entity_id)
    .bind(hashes)
    .execute(pool)
    .await?;
    Ok(())
}

/// A file an upload knows, as accounted; `written` when this upload stores
/// it (set before the write, so a write cut short still counts).
struct Receipt {
    hash: String,
    size: i64,
    stored: bool,
    charged: bool,
    written: bool,
}

/// Runs under the entity's lock: accounting, storage and, once nothing is
/// missing, publication; or the release of an overtaken upload.
async fn commit(
    deployer: &WriteDeployer,
    limits: &Limits,
    batch: &Batch,
    prepared: Prepared,
    admission: &mut Admission,
) -> Result<Outcome, Refusal> {
    let pool = deployer.pool();
    let storage = deployer.storage();
    let Prepared {
        entity,
        entity_bytes,
        files,
        deployer_address,
        content_hashes,
        max_scene_bytes,
        seen,
        overtaken,
    } = prepared;
    let entity_id = entity.id.as_str();
    // Another batch may have published the entity meanwhile.
    if let Some(ts) = deployer.deployed_timestamp(entity_id).await? {
        return Ok(Outcome::Published(ts));
    }
    if overtaken {
        release_upload(pool, storage, limits, entity_id).await;
        return Err(newer_entity(&entity));
    }
    let current = pending(pool, entity_id).await?;
    if seen.is_some() && current.is_none() {
        return Err(Refusal::expired());
    }
    let mut receipts: HashMap<String, Receipt> = HashMap::new();
    let mut already_stored: HashSet<String> = match &current {
        Some(_) => stored_files(pool, entity_id).await?.into_keys().collect(),
        None => HashSet::new(),
    };
    // One inventory of the content already in the store per upload; later
    // batches rely on the receipts.
    if !current.as_ref().is_some_and(|p| p.initialized) {
        let mut sizes: Vec<(String, i64)> = Vec::new();
        for hash in &content_hashes {
            let info = storage
                .file_info(hash)
                .await
                .map_err(|e| store_unreadable(entity_id, &e))?;
            if let Some(info) = info {
                sizes.push((hash.clone(), info.size as i64));
            }
        }
        // Only published content is free; content just pending uploads hold
        // is charged as if received.
        let keys: Vec<String> = sizes.iter().map(|(h, _)| h.clone()).collect();
        let published: HashSet<String> = sqlx::query_scalar(
            "SELECT DISTINCT content_hash FROM content.content_files WHERE content_hash = ANY($1)",
        )
        .bind(&keys)
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
        for (hash, size) in sizes {
            receipts.insert(
                hash.clone(),
                Receipt {
                    charged: !published.contains(&hash),
                    hash: hash.clone(),
                    size,
                    stored: true,
                    written: false,
                },
            );
            already_stored.insert(hash);
        }
    }
    // Every received byte counts toward the rate; stored files are not rewritten.
    let mut incoming_bytes: i64 = 0;
    let mut to_store: Vec<(String, Bytes)> = Vec::new();
    for (hash, bytes) in &files {
        incoming_bytes += bytes.len() as i64;
        if already_stored.contains(hash) {
            continue;
        }
        receipts.insert(
            hash.clone(),
            Receipt {
                hash: hash.clone(),
                size: bytes.len() as i64,
                stored: false,
                charged: true,
                written: true,
            },
        );
        to_store.push((hash.clone(), bytes.clone()));
    }
    let known_scene_bytes: i64 = receipts
        .values()
        .filter(|r| r.hash != entity_id)
        .map(|r| r.size)
        .sum();
    if known_scene_bytes > max_scene_bytes {
        return Err(Refusal::invalid(
            "Deployment failed: The deployment is too big.",
        ));
    }

    let created_at = create_upload(
        pool,
        limits,
        &entity,
        &content_hashes,
        &deployer_address,
        seen.is_some() || current.is_some(),
        batch.requested_at_ms,
    )
    .await?;
    // A resume joins its admitted upload even when over budget; a new upload
    // is admitted once reserved.
    if current.is_some() {
        admission.admitted = true;
    }
    let expires_at = created_at + limits.ttl_ms;
    let receipts: Vec<Receipt> = receipts.into_values().collect();
    if let Err(refusal) = reserve(
        pool,
        limits,
        entity_id,
        &deployer_address,
        &receipts,
        max_scene_bytes,
        incoming_bytes,
        expires_at,
    )
    .await
    {
        // A first batch that is not admitted must not keep its upload holding a slot.
        if current.is_none() {
            let _ = sqlx::query(
                "DELETE FROM content.pending_deployments WHERE entity_id = $1 AND reserved_bytes = 0
                   AND NOT EXISTS (SELECT 1 FROM content.pending_deployment_files WHERE entity_id = $1)",
            )
            .bind(entity_id)
            .execute(pool)
            .await;
        }
        return Err(refusal);
    }
    if current.is_none() {
        admission.admitted = true;
        admission.created = true;
    }

    if now_ms() > expires_at {
        return Err(Refusal::expired());
    }
    store_files(storage, to_store.clone()).await?;
    // A write that outlived the upload stays unrecorded; cleanup reclaims it.
    let mut tx = pool.begin().await?;
    let live = sqlx::query(
        "UPDATE content.pending_deployments SET initialized = true, batches = batches + 1
         WHERE entity_id = $1 AND created_at >= to_timestamp($2 / 1000.0)",
    )
    .bind(entity_id)
    .bind((now_ms() - limits.ttl_ms) as f64)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if !live {
        return Err(Refusal::expired());
    }
    let stored_keys: Vec<String> = to_store.iter().map(|(h, _)| h.clone()).collect();
    sqlx::query(
        "UPDATE content.pending_deployment_files SET stored = true
         WHERE entity_id = $1 AND hash = ANY($2)",
    )
    .bind(entity_id)
    .bind(&stored_keys)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let progress = stored_files(pool, entity_id).await?;
    let missing: Vec<String> = content_hashes
        .iter()
        .filter(|h| !progress.contains_key(*h))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return incomplete_until(missing, expires_at);
    }

    // One full check at completion, then publication.
    let present = storage
        .exist_multiple(&content_hashes)
        .await
        .map_err(|e| store_unreadable(entity_id, &e))?;
    let now_missing: Vec<String> = content_hashes
        .iter()
        .filter(|h| !present.get(*h).copied().unwrap_or(false))
        .cloned()
        .collect();
    if !now_missing.is_empty() {
        mark_missing(pool, entity_id, &now_missing).await?;
        return incomplete_until(now_missing, expires_at);
    }
    finalize(
        deployer,
        limits,
        &entity,
        entity_bytes,
        batch.auth_chain.clone(),
        &content_hashes,
        created_at,
        expires_at,
    )
    .await
}

/// A resumable answer tells the client to keep uploading, so only while the
/// upload is live.
fn incomplete_until(missing: Vec<String>, expires_at: i64) -> Result<Outcome, Refusal> {
    if now_ms() > expires_at {
        return Err(Refusal::expired());
    }
    Ok(Outcome::Missing(missing))
}

async fn stored_files(pool: &PgPool, entity_id: &str) -> sqlx::Result<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT hash, size FROM content.pending_deployment_files WHERE entity_id = $1 AND stored",
    )
    .bind(entity_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// A content store this node could not read: upstream's fixed sentence,
/// the cause logged.
fn store_unreadable(entity_id: &str, e: &dyn std::fmt::Display) -> Refusal {
    tracing::error!(entity_id, error = %e, "failed to read the content store for a partial upload");
    Refusal::Unavailable {
        errors: vec![super::write_deployer::DEPLOY_ERROR_MESSAGE.to_string()],
        retry_after: INTERNAL_ERROR_RETRY_AFTER_SECS,
    }
}

/// A store failure is logged; the client sees upstream's fixed sentence.
async fn store_files(storage: &Storage, files: Vec<(String, Bytes)>) -> Result<(), Refusal> {
    let mut writes = stream::iter(files.into_iter().map(|(hash, bytes)| async move {
        storage.store(&hash, bytes).await.map_err(|e| {
            tracing::error!(hash, error = %e, "failed to store a partial upload's content file");
        })
    }))
    .buffer_unordered(STORE_CONCURRENCY);
    while let Some(result) = writes.next().await {
        result.map_err(|()| Refusal::Unavailable {
            errors: vec![super::write_deployer::DEPLOY_ERROR_MESSAGE.to_string()],
            retry_after: INTERNAL_ERROR_RETRY_AFTER_SECS,
        })?;
    }
    Ok(())
}

/// Creates the upload unless it exists, and returns when it was created
/// (ms). One seen earlier in this request must still exist: re-creating it
/// would restart its lifetime.
async fn create_upload(
    pool: &PgPool,
    limits: &Limits,
    entity: &Entity,
    content_hashes: &[String],
    deployer_address: &str,
    seen: bool,
    requested_at: i64,
) -> Result<i64, Refusal> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("pending_deployer:{deployer_address}"))
        .execute(&mut *tx)
        .await?;
    match pending(&mut *tx, &entity.id).await? {
        None if seen => return Err(Refusal::expired()),
        Some(existing) => {
            if now_ms() > existing.created_at + limits.ttl_ms {
                return Err(Refusal::expired());
            }
            // Reservations are charged to the creator; nobody else adds to it.
            if existing.deployer_address != deployer_address {
                return Err(Refusal::invalid(
                    "This upload was started by another account.",
                ));
            }
            return Ok(existing.created_at);
        }
        None => {}
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM content.pending_deployments WHERE deployer_address = $1",
    )
    .bind(deployer_address)
    .fetch_one(&mut *tx)
    .await?;
    if count >= limits.max_per_deployer {
        return Err(quota_exceeded(
            &mut tx,
            limits,
            format!(
                "Too many partial uploads in progress for this account (max {}). Complete an upload or wait for expired uploads to be cleaned up.",
                limits.max_per_deployer
            ),
            Some(deployer_address),
        )
        .await?);
    }
    // Bounds what any set of parcels can stage, however many accounts deploy there.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(k) FROM (
           SELECT DISTINCT hashtextextended('pending_pointer:' || p, 0) AS k
           FROM unnest($1::text[]) AS p ORDER BY k) AS keys",
    )
    .bind(&entity.pointers)
    .execute(&mut *tx)
    .await?;
    let usage: Vec<PointerUsage> = sqlx::query_as(
        "SELECT p.pointer,
                count(*) AS uploads,
                count(*) FILTER (WHERE d.deployer_address = $2) AS deployer_uploads,
                (EXTRACT(EPOCH FROM min(d.created_at)) * 1000)::bigint AS oldest,
                (EXTRACT(EPOCH FROM min(d.created_at) FILTER (WHERE d.deployer_address = $2)) * 1000)::bigint
                    AS deployer_oldest
         FROM content.pending_deployments d CROSS JOIN LATERAL unnest(d.pointers) AS p(pointer)
         WHERE d.pointers && $1 AND p.pointer = ANY($1)
           AND d.created_at >= to_timestamp($3 / 1000.0)
         GROUP BY p.pointer",
    )
    .bind(&entity.pointers)
    .bind(deployer_address)
    .bind((now_ms() - limits.ttl_ms) as f64)
    .fetch_all(&mut *tx)
    .await?;
    if let Some((message, retry_after)) =
        check_pointer_quotas(&usage, limits.max_per_pointer, now_ms(), limits.ttl_ms)
    {
        return Err(Refusal::throttled(message, retry_after));
    }
    // A delayed first batch may arrive past its deadline.
    if now_ms() > requested_at + limits.ttl_ms {
        return Err(Refusal::expired());
    }
    sqlx::query(
        "INSERT INTO content.pending_deployments
            (entity_id, entity_type, pointers, content_hashes, deployer_address, created_at)
         VALUES ($1, $2, $3, $4, $5, to_timestamp($6 / 1000.0))",
    )
    .bind(&entity.id)
    .bind(entity.entity_type.as_str())
    .bind(&entity.pointers)
    .bind(content_hashes)
    .bind(deployer_address)
    .bind(requested_at as f64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(requested_at)
}

/// A 429 for a quota only cleanup frees: Retry-After is the first cleanup
/// run after the oldest upload in scope expires.
async fn quota_exceeded(
    tx: &mut sqlx::PgConnection,
    limits: &Limits,
    message: String,
    deployer_address: Option<&str>,
) -> sqlx::Result<Refusal> {
    let oldest: Option<i64> = sqlx::query_scalar(
        "SELECT (EXTRACT(EPOCH FROM min(created_at)) * 1000)::bigint
         FROM content.pending_deployments WHERE $1::text IS NULL OR deployer_address = $1",
    )
    .bind(deployer_address)
    .fetch_one(&mut *tx)
    .await?;
    let last = last_cleanup_at(&mut *tx).await?;
    Ok(Refusal::throttled(
        message,
        seconds_until_cleanup_frees_quota(
            now_ms(),
            oldest,
            limits.ttl_ms,
            last,
            limits.cleanup_interval_ms,
        ),
    ))
}

#[derive(sqlx::FromRow)]
struct Totals {
    upload: i64,
    account: i64,
    total: i64,
    scene: i64,
}

/// Charges a batch: its bytes against the account's per-minute rate
/// (committed on its own, so a repeated rejected batch still counts), then
/// its receipts against the scene cap and the account and server budgets,
/// under one global lock.
#[allow(clippy::too_many_arguments)]
async fn reserve(
    pool: &PgPool,
    limits: &Limits,
    entity_id: &str,
    deployer_address: &str,
    receipts: &[Receipt],
    max_scene_bytes: i64,
    incoming_bytes: i64,
    expires_at: i64,
) -> Result<(), Refusal> {
    if pending(pool, entity_id).await?.is_none() {
        return Err(Refusal::invalid(
            "Upload no longer exists; resend its manifest.",
        ));
    }
    if now_ms() > expires_at {
        return Err(Refusal::expired());
    }
    let (window_bytes, ends_in_ms): (i64, f64) = sqlx::query_as(
        "INSERT INTO content.partial_upload_rates (deployer_address, window_started, bytes)
         VALUES ($1, timezone('UTC', now()), $2)
         ON CONFLICT (deployer_address) DO UPDATE SET
           bytes = CASE WHEN partial_upload_rates.window_started < timezone('UTC', now()) - interval '1 minute'
             THEN EXCLUDED.bytes ELSE partial_upload_rates.bytes + EXCLUDED.bytes END,
           window_started = CASE WHEN partial_upload_rates.window_started < timezone('UTC', now()) - interval '1 minute'
             THEN timezone('UTC', now()) ELSE partial_upload_rates.window_started END
         RETURNING bytes,
           (EXTRACT(EPOCH FROM window_started + interval '1 minute' - timezone('UTC', now())) * 1000)::float8",
    )
    .bind(deployer_address)
    .bind(incoming_bytes)
    .fetch_one(pool)
    .await?;
    // No window can ever admit this batch.
    if incoming_bytes > limits.bytes_per_minute {
        return Err(Refusal::invalid(format!(
            "This batch is {incoming_bytes} bytes, over the partial upload byte rate limit of {} bytes per minute. Send it in smaller batches.",
            limits.bytes_per_minute
        )));
    }
    if window_bytes > limits.bytes_per_minute {
        let retry_after = (ends_in_ms / 1000.0).ceil().max(1.0) as u64;
        return Err(Refusal::throttled(
            format!(
                "Partial upload byte rate exceeded for this account: {window_bytes} bytes in the current one-minute window, max {}. Retry in {retry_after} s.",
                limits.bytes_per_minute
            ),
            retry_after,
        ));
    }

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('partial-upload-budget', 0))")
        .execute(&mut *tx)
        .await?;
    if now_ms() > expires_at {
        return Err(Refusal::expired());
    }
    let hashes: Vec<&str> = receipts.iter().map(|r| r.hash.as_str()).collect();
    let sizes: Vec<i64> = receipts.iter().map(|r| r.size).collect();
    let stored: Vec<bool> = receipts.iter().map(|r| r.stored).collect();
    let charged: Vec<bool> = receipts.iter().map(|r| r.charged).collect();
    let written: Vec<bool> = receipts.iter().map(|r| r.written).collect();
    sqlx::query(
        "INSERT INTO content.pending_deployment_files (entity_id, hash, size, stored, charged, written)
         SELECT $1, r.hash, r.size, r.stored, r.charged, r.written
         FROM unnest($2::text[], $3::bigint[], $4::bool[], $5::bool[], $6::bool[])
           AS r(hash, size, stored, charged, written)
         ON CONFLICT (entity_id, hash) DO UPDATE SET
           size = GREATEST(pending_deployment_files.size, EXCLUDED.size),
           stored = pending_deployment_files.stored OR EXCLUDED.stored,
           charged = pending_deployment_files.charged OR EXCLUDED.charged,
           written = pending_deployment_files.written OR EXCLUDED.written",
    )
    .bind(entity_id)
    .bind(&hashes)
    .bind(&sizes)
    .bind(&stored)
    .bind(&charged)
    .bind(&written)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE content.pending_deployments SET reserved_bytes = (
           SELECT COALESCE(sum(size), 0) FROM content.pending_deployment_files
           WHERE entity_id = $1 AND charged)
         WHERE entity_id = $1",
    )
    .bind(entity_id)
    .execute(&mut *tx)
    .await?;
    let totals: Totals = sqlx::query_as(
        "SELECT
           COALESCE(sum(reserved_bytes) FILTER (WHERE entity_id = $1), 0)::bigint AS upload,
           COALESCE(sum(reserved_bytes) FILTER (WHERE deployer_address = $2), 0)::bigint AS account,
           COALESCE(sum(reserved_bytes), 0)::bigint AS total,
           (SELECT COALESCE(sum(size), 0)::bigint FROM content.pending_deployment_files
              WHERE entity_id = $1 AND hash <> $1) AS scene
         FROM content.pending_deployments",
    )
    .bind(entity_id)
    .bind(deployer_address)
    .fetch_one(&mut *tx)
    .await?;
    if totals.scene > max_scene_bytes {
        return Err(Refusal::invalid(
            "Deployment failed: The deployment is too big.",
        ));
    }
    // An upload larger than a budget on its own never fits.
    let (upload_budget, limit) = if limits.account_bytes < limits.global_bytes {
        (limits.account_bytes, "per-account")
    } else {
        (limits.global_bytes, "server")
    };
    if totals.upload > upload_budget {
        return Err(Refusal::invalid(format!(
            "This upload needs {} bytes of partial upload storage, over the {limit} budget of {upload_budget} bytes.",
            totals.upload
        )));
    }
    if totals.account > limits.account_bytes {
        return Err(quota_exceeded(
            &mut tx,
            limits,
            format!(
                "Partial upload storage budget exceeded for this account: {} bytes staged with this batch, max {}. Complete uploads or wait for cleanup.",
                totals.account, limits.account_bytes
            ),
            Some(deployer_address),
        )
        .await?);
    }
    if totals.total > limits.global_bytes {
        return Err(quota_exceeded(
            &mut tx,
            limits,
            "Partial upload storage on this server is full. Retry later.".to_string(),
            None,
        )
        .await?);
    }
    tx.commit().await?;
    Ok(())
}

/// Publishes a complete upload through the write deployer (full validation,
/// content read from the store), its request TTL measured from the upload's
/// first batch.
#[allow(clippy::too_many_arguments)]
async fn finalize(
    deployer: &WriteDeployer,
    limits: &Limits,
    entity: &Entity,
    entity_bytes: Bytes,
    auth_chain: Value,
    content_hashes: &[String],
    created_at: i64,
    expires_at: i64,
) -> Result<Outcome, Refusal> {
    if now_ms() > expires_at {
        return Err(Refusal::expired());
    }
    let pool = deployer.pool();
    let entity_id = entity.id.as_str();
    // Another deploy of this process holding the pointers is waited out a
    // few times, as upstream's finalize does, then answered 429.
    let mut attempt = 0;
    let result = loop {
        let result = deployer
            .deploy_entity_anchored(
                vec![entity_bytes.clone()],
                entity_id,
                auth_chain.clone(),
                "LOCAL",
                Some(created_at),
            )
            .await;
        match &result {
            Err(DeployFailure::Rejected(RejectionKind::PointerContention, _))
                if attempt < FINALIZE_POINTER_CONFLICT_RETRIES =>
            {
                attempt += 1;
                tokio::time::sleep(FINALIZE_POINTER_CONFLICT_DELAY).await;
                if let Some(ts) = deployer.deployed_timestamp(entity_id).await? {
                    delete_pending(pool, entity_id).await;
                    return Ok(Outcome::Published(ts));
                }
            }
            _ => break result,
        }
    };
    let failure = match result {
        Ok(_) => {
            delete_pending(pool, entity_id).await;
            let ts = deployer
                .deployed_timestamp(entity_id)
                .await?
                .unwrap_or_else(now_ms);
            return Ok(Outcome::Published(ts));
        }
        Err(failure) => failure,
    };
    if let Some(ts) = deployer.deployed_timestamp(entity_id).await? {
        delete_pending(pool, entity_id).await;
        return Ok(Outcome::Published(ts));
    }
    // Content went missing while the pipeline ran: the client re-uploads it.
    if let Ok(present) = deployer.storage().exist_multiple(content_hashes).await {
        let missing: Vec<String> = content_hashes
            .iter()
            .filter(|h| !present.get(*h).copied().unwrap_or(false))
            .cloned()
            .collect();
        if !missing.is_empty() {
            mark_missing(pool, entity_id, &missing).await?;
            return incomplete_until(missing, expires_at);
        }
    }
    match failure {
        // A server failure says nothing about the entity: the upload stays.
        DeployFailure::Unavailable(errors) => Err(Refusal::Unavailable {
            errors,
            retry_after: INTERNAL_ERROR_RETRY_AFTER_SECS,
        }),
        DeployFailure::Rejected(RejectionKind::RateLimited, errors) => {
            let retry_after = deployer
                .rate_limit_retry_after(entity.entity_type.as_str(), &entity.pointers)
                .unwrap_or(1);
            Err(Refusal::Invalid {
                errors,
                retry_after: Some(retry_after),
            })
        }
        DeployFailure::Rejected(RejectionKind::PointerContention, errors) => {
            Err(Refusal::Invalid {
                errors,
                retry_after: Some(POINTER_CONFLICT_RETRY_AFTER_SECS),
            })
        }
        // Only an overtaken upload is released: other refusals may clear.
        DeployFailure::Rejected(RejectionKind::Superseded, errors) => {
            release_upload(pool, deployer.storage(), limits, entity_id).await;
            Err(Refusal::Invalid {
                errors,
                retry_after: None,
            })
        }
        other => Err(Refusal::from_deploy(other)),
    }
}

/// Deletes a published upload's row (its receipts by cascade); a failure
/// leaves it to the cleanup job.
async fn delete_pending(pool: &PgPool, entity_id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM content.pending_deployments WHERE entity_id = $1")
        .bind(entity_id)
        .execute(pool)
        .await
    {
        tracing::warn!(entity_id, error = %e, "failed to delete a published partial upload; cleanup will reclaim it");
    }
}

/// Drops an upload that can never be published, with its accounting, and
/// unlinks the staged blobs it wrote that the collector would. Runs under
/// the entity's lock.
async fn release_upload(pool: &PgPool, storage: &Storage, limits: &Limits, entity_id: &str) {
    let released = async {
        let Some(keys) = take_upload(pool, entity_id, None).await? else {
            return anyhow::Ok(());
        };
        tracing::info!(
            entity_id,
            "released a partial upload that can never be published"
        );
        unlink_orphans(pool, storage, limits, &keys).await?;
        anyhow::Ok(())
    };
    if let Err(e) = released.await {
        tracing::warn!(entity_id, error = %e, "failed to release a partial upload; cleanup or the collector will reclaim it");
    }
}

/// Deletes an upload (when `expired_before` is set, only if created before
/// it) and returns the keys it wrote itself. The caller holds the entity's
/// lock, so no batch is committing it.
async fn take_upload(
    pool: &PgPool,
    entity_id: &str,
    expired_before: Option<i64>,
) -> sqlx::Result<Option<Vec<String>>> {
    let mut tx = pool.begin().await?;
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT hash FROM content.pending_deployment_files WHERE entity_id = $1 AND written",
    )
    .bind(entity_id)
    .fetch_all(&mut *tx)
    .await?;
    let deleted = sqlx::query(
        "DELETE FROM content.pending_deployments WHERE entity_id = $1
           AND ($2::float8 IS NULL OR created_at < to_timestamp($2 / 1000.0))",
    )
    .bind(entity_id)
    .bind(expired_before.map(|ms| ms as f64))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((deleted > 0).then_some(keys))
}

/// Unlinks the keys the collector would: aged past its floor, outside its
/// fixed references and named by no domain (checked right before the
/// unlink). Answers how many went.
async fn unlink_orphans(
    pool: &PgPool,
    storage: &Storage,
    limits: &Limits,
    keys: &[String],
) -> anyhow::Result<usize> {
    Ok(gc::unlink_orphans(
        pool,
        storage,
        &limits.global_scenes_urn,
        keys,
        limits.orphan_min_age,
    )
    .await?)
}

/// One cleanup run: every expired upload whose lock is free (keyset pages
/// by `created_at, entity_id`) is deleted and its orphaned staged blobs
/// unlinked; then the elapsed rate windows go, and the run's end is
/// recorded for every replica. Answers the uploads removed.
pub async fn cleanup_expired(
    pool: &PgPool,
    storage: &Storage,
    limits: &Limits,
) -> anyhow::Result<usize> {
    let result = reclaim_expired(pool, storage, limits).await;
    sqlx::query(
        "INSERT INTO content.system_properties (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(CLEANUP_AT_KEY)
    .bind(now_ms().to_string())
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM content.partial_upload_rates
         WHERE window_started < timezone('UTC', now()) - interval '1 minute'",
    )
    .execute(pool)
    .await?;
    result
}

async fn reclaim_expired(
    pool: &PgPool,
    storage: &Storage,
    limits: &Limits,
) -> anyhow::Result<usize> {
    let cutoff = now_ms() - limits.ttl_ms;
    let mut removed = 0;
    let mut failed = 0;
    let mut after: Option<(f64, String)> = None;
    loop {
        let page: Vec<(f64, String)> = sqlx::query_as(
            "SELECT (EXTRACT(EPOCH FROM created_at) * 1000)::float8, entity_id
             FROM content.pending_deployments
             WHERE created_at < to_timestamp($1 / 1000.0)
               AND ($2::float8 IS NULL OR (created_at, entity_id) > (to_timestamp($2 / 1000.0)::timestamp, $3))
             ORDER BY created_at, entity_id LIMIT $4",
        )
        .bind(cutoff as f64)
        .bind(after.as_ref().map(|(ts, _)| *ts))
        .bind(after.as_ref().map(|(_, id)| id.clone()).unwrap_or_default())
        .bind(EXPIRED_UPLOADS_PER_PAGE)
        .fetch_all(pool)
        .await?;
        for (_, entity_id) in &page {
            // A batch holding the lock may be publishing the upload: the next run retries it.
            let Some(lock) = EntityLock::try_acquire(pool, entity_id).await? else {
                continue;
            };
            let reclaimed = async {
                if let Some(keys) = take_upload(pool, entity_id, Some(cutoff)).await? {
                    unlink_orphans(pool, storage, limits, &keys).await?;
                }
                anyhow::Ok(())
            }
            .await;
            lock.release().await;
            match reclaimed {
                Ok(()) => removed += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(entity_id, error = %e, "failed to reclaim an expired partial upload");
                }
            }
        }
        if (page.len() as i64) < EXPIRED_UPLOADS_PER_PAGE {
            break;
        }
        after = page.last().cloned();
    }
    if failed > 0 {
        anyhow::bail!("{failed} expired partial upload(s) could not be reclaimed; {removed} were");
    }
    Ok(removed)
}

/// The leased cleanup job, on a node taking local writes.
pub fn spawn(jobs: &Jobs, pool: PgPool, storage: Arc<Storage>, limits: Limits) {
    let interval = Duration::from_millis(limits.cleanup_interval_ms as u64);
    jobs.spawn_periodic_leased(JOB_NAME, PeriodicCfg::every(interval), move |ctx| {
        let (pool, storage, limits) = (pool.clone(), storage.clone(), limits.clone());
        async move {
            let removed = cleanup_expired(&pool, &storage, &limits).await?;
            ctx.progress(format!("{removed} expired partial uploads reclaimed"));
            Ok(())
        }
    });
}

#[cfg(test)]
mod tests;
