//! The write path peer sync drives: entity files that already passed the
//! content-hash check are validated, buffered, and flushed in batches of up
//! to [`BATCH_SIZE`] rows into `content.deployments`, `content.content_files`
//! and `content.active_pointers` under the same pointer locks the local deploy
//! function takes. A flush that fails re-reports every entity it carried to
//! `content.failed_deployments`, where the retry worker owns it.
//!
//! [`LiveSyncDeployer::deploy_entity`] runs the content validator the local
//! write path runs, in its synced form ([`crate::validator::synced`]): no
//! request-recency, rate-limit or newer-entity checks, the auth chain and the
//! access rules judged at the entity timestamp, contract-wallet links asked of
//! the EIP-1654 validator at the entity's block. A refusal comes back as
//! [`SyncError::DeploymentRejected`]; a verdict the node could not reach (a
//! source unset or failing, no EIP-1654 validator, an RPC without that block's
//! state) as
//! [`SyncError::ValidationUndecided`]. The callers record both in
//! `content.failed_deployments`: the retry worker (the sync leader's, or with
//! sync off the read-through's, [`crate::sync::worker::start_failed_deployments`])
//! re-attempts an undecided entity with backoff, outside the retry cap, for up
//! to a week, and never retries a refusal.

use crate::failed_deployments_store::LiveFailedDeploymentsStore;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use super::batch_deployer::DeploymentReport;
use super::deploy_remote_entity::ByteBudget;
use super::sync_types::{
    AuthChain, DeploymentContext, FailedDeployment, FailureReason, SyncError, Timestamp,
};
use super::write_deployer::lock_deployment_pointers;
use crate::auth::verify::ChainVerdict;
use crate::validator::{Sources, SyncedContent, ValidationError, validate_synced};

mod scene_parcels;

struct ParsedEntity {
    deployer_address: String,
    version: String,
    entity_type: String,
    entity_id: String,
    entity_metadata: crate::OrderedJson,
    entity_timestamp: f64,
    entity_pointers: Vec<String>,
    auth_chain: Value,
    content: Vec<(String, String)>,
    report: Option<Arc<DeploymentReport>>,
    /// Retry state the entity must be recorded with if the batch flush that carries it fails.
    /// The deployer is asynchronous -- `deploy_entity` returns once the entity is buffered, so
    /// the caller's attempt is already spent by the time a flush failure re-reports it, and
    /// reporting a fresh 0/0 here would rewind the backoff the retry worker had accrued.
    /// The wait stays relative: the deadline is stamped when the failure is recorded, not when
    /// the attempt was dispatched, so a slow attempt does not eat the wait it just earned.
    retry_count: u32,
    backoff_ms: u64,
    /// The flush-backlog slot this entity occupies from `deploy_entity` until the flush that
    /// carries it is done ([`SyncMemoryLimits::flush_backlog_max`]); dropped with the entity.
    backlog_permit: Option<OwnedSemaphorePermit>,
    /// Marks the id as buffered ([`is_buffered`]) until the entity is dropped.
    buffered: Option<BufferedMark>,
}

/// Entity ids a deployer of this process accepted and has not finished flushing.
static BUFFERED: std::sync::LazyLock<std::sync::Mutex<HashMap<String, usize>>> =
    std::sync::LazyLock::new(Default::default);

struct BufferedMark(String);

impl BufferedMark {
    fn new(entity_id: &str) -> Self {
        let mut ids = BUFFERED.lock().unwrap_or_else(|e| e.into_inner());
        *ids.entry(entity_id.to_string()).or_default() += 1;
        Self(entity_id.to_string())
    }
}

impl Drop for BufferedMark {
    fn drop(&mut self) {
        let mut ids = BUFFERED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = ids.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                ids.remove(&self.0);
            }
        }
    }
}

/// Marks `entity_id` as buffered until the guard drops: from the download that precedes its
/// validation ([`crate::deploy_remote_entity::deploy_entity_streaming`]).
pub(crate) fn hold_buffered(entity_id: &str) -> impl Drop + Send + use<> {
    BufferedMark::new(entity_id)
}

/// Whether any deployer of this process holds `entity_id` between its download and the end of
/// the flush that carries it.
pub fn is_buffered(entity_id: &str) -> bool {
    BUFFERED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(entity_id)
}

fn flush_failure_record(
    entity: &ParsedEntity,
    auth_chain: AuthChain,
    error_description: String,
    retry: (u32, u64),
) -> FailedDeployment {
    let now_ms = chrono::Utc::now().timestamp_millis();
    FailedDeployment {
        entity_type: entity.entity_type.clone(),
        entity_id: entity.entity_id.clone(),
        reason: FailureReason::DeploymentError,
        auth_chain,
        error_description,
        failure_timestamp: now_ms,
        snapshot_hash: None,
        retry_count: retry.0,
        next_retry_at: now_ms + retry.1 as i64,
    }
}

/// Strongest retry state per entity id in a batch. One batch can carry the same entity twice --
/// the sync path keeps re-encountering exactly the entity the retry worker keeps re-attempting --
/// and the single record a failed flush writes has to be the stronger sighting, or the accrued
/// attempts are lost to whichever copy happened to be buffered first.
fn strongest_retry_state(entities: &[ParsedEntity]) -> HashMap<&str, (u32, u64)> {
    let mut strongest: HashMap<&str, (u32, u64)> = HashMap::with_capacity(entities.len());
    for entity in entities {
        let slot = strongest
            .entry(entity.entity_id.as_str())
            .or_insert((entity.retry_count, entity.backoff_ms));
        slot.0 = slot.0.max(entity.retry_count);
        slot.1 = slot.1.max(entity.backoff_ms);
    }
    strongest
}

fn parse_entity_for_deploy(
    entity_data: &[u8],
    entity_id: &str,
    auth_chain: &AuthChain,
) -> Result<ParsedEntity, SyncError> {
    let entity: Value = serde_json::from_slice(entity_data)?;

    let pointers: Vec<String> = entity
        .get("pointers")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default();

    let timestamp = entity
        .get("timestamp")
        .and_then(|t| t.as_f64())
        .unwrap_or(0.0);
    // Stored as the validator reads it (`Scenes` is a scene), so the type-keyed SQL matches.
    let raw_type = entity
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");
    let entity_type = crate::validator::entity_parser::parse_entity_type(raw_type)
        .map_or_else(|| raw_type.to_string(), |t| t.as_str().to_string());
    let version = entity
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::entity::DEFAULT_ENTITY_VERSION)
        .to_string();

    let deployer = auth_chain
        .first()
        .map(|link| link.payload.clone())
        .unwrap_or_default();

    let content: Vec<(String, String)> = entity
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let file = c.get("file").and_then(|f| f.as_str());
                    let hash = c.get("hash").and_then(|h| h.as_str());
                    match (file, hash) {
                        (Some(f), Some(h)) => Some((f.to_string(), h.to_string())),
                        _ => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let auth_chain_json =
        serde_json::to_value(auth_chain).map_err(|e| SyncError::Storage(e.to_string()))?;
    let metadata =
        crate::entity::deployment_metadata(entity_data, crate::entity::MetadataInput::SyncObject)?;

    Ok(ParsedEntity {
        deployer_address: deployer,
        version,
        entity_type,
        entity_id: entity_id.to_string(),
        entity_metadata: metadata,
        entity_timestamp: timestamp,
        entity_pointers: pointers,
        auth_chain: auth_chain_json,
        content,
        report: None,
        retry_count: 0,
        backoff_ms: 0,
        backlog_permit: None,
        buffered: None,
    })
}

/// How a [`LiveSyncDeployer`] judges an entity before it buffers it.
#[derive(Clone)]
pub enum SyncValidation {
    /// The content validator the local write path runs, in its synced form.
    Content(Arc<Sources>),
    /// The auth chain only: ECDSA links recovered, ephemeral expiry against the entity
    /// timestamp, contract-wallet links and unparsed expirations kept as the peer's verdict.
    /// For tests whose fixtures are not valid entities.
    #[cfg(any(test, feature = "test-support"))]
    AuthChainOnly,
}

#[cfg(any(test, feature = "test-support"))]
async fn verify_entity_auth_chain(
    entity_id: &str,
    auth_chain: &AuthChain,
    entity_timestamp_ms: i64,
) -> Result<ChainVerdict, SyncError> {
    crate::auth::verify::verify_peer_auth_chain(auth_chain, entity_id, entity_timestamp_ms)
        .await
        .map_err(|e| {
            SyncError::DeploymentRejected(format!(
                "auth chain verification failed for {entity_id}: {e}"
            ))
        })
}

/// A refused validation as the callers record it: undecided when any error is.
fn validation_failure(entity_id: &str, errors: Vec<ValidationError>) -> SyncError {
    // A contract wallet this node could not check is undecided too, never a refusal.
    let undecided = errors
        .iter()
        .any(|e| e.is_unavailable() || e.is_unverifiable());
    let messages = errors
        .into_iter()
        .map(|e| match e {
            ValidationError::Rejected(m)
            | ValidationError::Unavailable(m)
            | ValidationError::Unverifiable(m) => m,
        })
        .collect::<Vec<_>>()
        .join("; ");
    if undecided {
        SyncError::ValidationUndecided(format!("{entity_id}: {messages}"))
    } else {
        SyncError::DeploymentRejected(format!("{entity_id}: {messages}"))
    }
}

pub const BATCH_SIZE: usize = 500;

/// Content validations a deployer runs at once ([`LiveSyncDeployer::verify_entity`]).
const VALIDATIONS_IN_FLIGHT: usize = 64;
/// How long a `Pending` access pass stands for the `Fetched` check of the same entity.
const ACCESS_PASSED_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const ACCESS_PASSED_MAX: usize = 10_000;
const BATCH_TIMEOUT_MS: u64 = 200;
/// The old `SYNC_FLUSH_CONCURRENCY` default.
pub const DEFAULT_FLUSH_CONCURRENCY: usize = 8;

/// `SYNC_INFLIGHT_BYTES_MAX`: decoded file bodies in memory across every in-flight download.
pub const DEFAULT_INFLIGHT_BYTES_MAX: u64 = 1024 * 1024 * 1024;
/// `SYNC_FLUSH_BACKLOG_MAX`: entities verified and waiting for a database flush.
pub const DEFAULT_FLUSH_BACKLOG_MAX: usize = 4096;

/// What bounds the sync write path's memory: the bytes of file bodies in flight
/// ([`ByteBudget`]) and the entities buffered ahead of the database. Without the second, a
/// database slower than the peers let the flush backlog grow without limit -- every batch a
/// spawned task holding 500 parsed entities, waiting for a flush slot -- and producers never
/// noticed; with it, `deploy_entity` waits for a slot instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncMemoryLimits {
    pub inflight_bytes_max: u64,
    pub flush_backlog_max: usize,
}

impl SyncMemoryLimits {
    pub const DEFAULT: Self = Self {
        inflight_bytes_max: DEFAULT_INFLIGHT_BYTES_MAX,
        flush_backlog_max: DEFAULT_FLUSH_BACKLOG_MAX,
    };
}

impl Default for SyncMemoryLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone)]
pub struct LiveSyncDeployer {
    pool: PgPool,
    batch: Arc<Mutex<Vec<ParsedEntity>>>,

    flush_sem: Arc<Semaphore>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    idle_notify: Arc<tokio::sync::Notify>,
    lost: Arc<std::sync::atomic::AtomicU64>,
    trusted_expirations: Arc<std::sync::atomic::AtomicU64>,
    /// One permit per entity between `deploy_entity` and the end of its flush.
    backlog: Arc<Semaphore>,
    flush_backlog_max: usize,
    inflight_bytes: Arc<ByteBudget>,
    /// How every entity is judged before it is buffered.
    validation: SyncValidation,
    /// Content validations in flight: each asks the chain about twenty times on a cold block
    /// memo, so this bounds the RPC load a bootstrap puts on the providers.
    validations: Arc<Semaphore>,
    /// Entities (with their chain) whose access a `Pending` check passed, for the `Fetched`
    /// check that follows; taken by it, dropped after [`ACCESS_PASSED_TTL`].
    access_passed: Arc<std::sync::Mutex<HashMap<String, std::time::Instant>>>,
}

fn spawn_flush(
    pool: PgPool,
    flush_sem: Arc<Semaphore>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    idle_notify: Arc<tokio::sync::Notify>,
    lost: Arc<std::sync::atomic::AtomicU64>,
    entities: Vec<ParsedEntity>,
) {
    if entities.is_empty() {
        return;
    }
    in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        let _permit = flush_sem.acquire().await;
        if let Err(e) = flush_batch(&pool, &entities).await {
            tracing::error!(error = %e, count = entities.len(), "Batch flush failed");
            let failed_store = LiveFailedDeploymentsStore::new(pool.clone());
            let mut strongest = strongest_retry_state(&entities);
            for entity in &entities {
                let Some(retry) = strongest.remove(entity.entity_id.as_str()) else {
                    continue;
                };
                let auth_chain: AuthChain = match serde_json::from_value(entity.auth_chain.clone())
                {
                    Ok(chain) => chain,
                    Err(conv_err) => {
                        lost.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if let Some(report) = &entity.report {
                            report.record_lost();
                        }
                        tracing::error!(
                            entity_id = %entity.entity_id,
                            error = %conv_err,
                            "Entity dropped by a failed batch flush has an unconvertible auth \
                             chain; counting it as lost instead of recording an un-retryable row"
                        );
                        continue;
                    }
                };
                let failure = flush_failure_record(
                    entity,
                    auth_chain,
                    format!("batch flush failed: {}", e),
                    retry,
                );
                if let Err(record_err) = failed_store.report_failure(failure).await {
                    lost.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(report) = &entity.report {
                        report.record_lost();
                    }
                    tracing::error!(
                        entity_id = %entity.entity_id,
                        error = %record_err,
                        "Entity dropped by a failed batch flush could not be recorded as a \
                         failed deployment; holding the sync frontier back"
                    );
                }
            }
        }
        in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        idle_notify.notify_waiters();
    });
}

impl LiveSyncDeployer {
    /// `flush_concurrency` bounds concurrent batch flushes ([`DEFAULT_FLUSH_CONCURRENCY`]);
    /// the memory limits are the defaults.
    pub fn new(pool: PgPool, flush_concurrency: usize, validation: SyncValidation) -> Self {
        Self::with_limits(
            pool,
            flush_concurrency,
            SyncMemoryLimits::default(),
            validation,
        )
    }

    /// `validation` is how every entity is judged before it is buffered (in `serve`, the
    /// content validator of the local write path, so its caches are shared).
    pub fn with_limits(
        pool: PgPool,
        flush_concurrency: usize,
        limits: SyncMemoryLimits,
        validation: SyncValidation,
    ) -> Self {
        let flush_backlog_max = limits.flush_backlog_max.max(1);
        Self {
            pool,
            batch: Arc::new(Mutex::new(Vec::with_capacity(BATCH_SIZE))),
            flush_sem: Arc::new(Semaphore::new(flush_concurrency.max(1))),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            idle_notify: Arc::new(tokio::sync::Notify::new()),
            lost: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            trusted_expirations: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            backlog: Arc::new(Semaphore::new(flush_backlog_max)),
            flush_backlog_max,
            inflight_bytes: Arc::new(ByteBudget::new(limits.inflight_bytes_max)),
            validation,
            validations: Arc::new(Semaphore::new(VALIDATIONS_IN_FLIGHT)),
            access_passed: Arc::default(),
        }
    }

    fn take_access_passed(&self, key: &str) -> bool {
        let mut passed = self.access_passed.lock().unwrap_or_else(|e| e.into_inner());
        passed
            .remove(key)
            .is_some_and(|at| at.elapsed() < ACCESS_PASSED_TTL)
    }

    fn remember_access_passed(&self, key: String) {
        let mut passed = self.access_passed.lock().unwrap_or_else(|e| e.into_inner());
        if passed.len() >= ACCESS_PASSED_MAX {
            passed.retain(|_, at| at.elapsed() < ACCESS_PASSED_TTL);
            if passed.len() >= ACCESS_PASSED_MAX {
                passed.clear();
            }
        }
        passed.insert(key, std::time::Instant::now());
    }

    /// What [`Self::deploy_entity`] checks before it buffers, without buffering. `Fetched` once
    /// the content files are in the store; `Pending` before (the read-through's check before it
    /// stores anything).
    pub async fn verify_entity(
        &self,
        entity_data: &[u8],
        entity_id: &str,
        auth_chain: &AuthChain,
        content: SyncedContent,
    ) -> Result<ChainVerdict, SyncError> {
        match &self.validation {
            SyncValidation::Content(sources) => {
                let key = format!(
                    "{entity_id}\n{}",
                    serde_json::to_string(auth_chain).unwrap_or_default()
                );
                let access_passed =
                    content == SyncedContent::Fetched && self.take_access_passed(&key);
                let _slot = self
                    .validations
                    .acquire()
                    .await
                    .map_err(|_| SyncError::Stopped)?;
                let verdict = validate_synced(
                    sources,
                    entity_data,
                    entity_id,
                    auth_chain,
                    content,
                    access_passed,
                )
                .await
                .map_err(|errors| validation_failure(entity_id, errors))?;
                if content == SyncedContent::Pending {
                    self.remember_access_passed(key);
                }
                Ok(verdict)
            }
            #[cfg(any(test, feature = "test-support"))]
            SyncValidation::AuthChainOnly => {
                let parsed = parse_entity_for_deploy(entity_data, entity_id, auth_chain)?;
                verify_entity_auth_chain(entity_id, auth_chain, parsed.entity_timestamp as i64)
                    .await
            }
        }
    }

    /// The byte budget every download through this deployer's write path draws on.
    pub fn inflight_bytes(&self) -> &Arc<ByteBudget> {
        &self.inflight_bytes
    }

    /// Entities verified and buffered or in a flush that has not finished.
    pub fn flush_backlog(&self) -> usize {
        self.flush_backlog_max - self.backlog.available_permits()
    }

    /// The batch ticker as a job of the shared lifecycle (`sync-batch-flush`
    /// in `/admin/api/jobs`), scoped to the leased sync job that owns this
    /// deployer (`parent`; it ends with the lease): every [`BATCH_TIMEOUT_MS`]
    /// a partial batch that no arrival completed is handed to a flush.
    /// `deploy_entity` flushes full batches itself, and [`flush`](Self::flush)
    /// drains on demand, so a deployer without the ticker (the tests) only
    /// lacks the timeout.
    pub fn spawn_flush_ticker(&self, jobs: &crate::jobs::Jobs, parent: &crate::jobs::JobContext) {
        let background = self.clone();
        let cfg =
            crate::jobs::PeriodicCfg::every(std::time::Duration::from_millis(BATCH_TIMEOUT_MS))
                .after_first_interval();
        jobs.spawn_periodic_scoped(parent, "sync-batch-flush", cfg, move |_ctx| {
            let background = background.clone();
            async move {
                let mut buf = background.batch.lock().await;
                if !buf.is_empty() {
                    background.trigger_flush(std::mem::take(&mut *buf));
                }
                Ok(())
            }
        });
    }

    /// Counts the batch in flight before returning, so callers take the buffer and call this
    /// under one lock: [`flush`](Self::flush) must never see the buffer empty while a taken
    /// batch is not yet counted, or it returns before that batch is durable.
    fn trigger_flush(&self, entities: Vec<ParsedEntity>) {
        spawn_flush(
            self.pool.clone(),
            self.flush_sem.clone(),
            self.in_flight.clone(),
            self.idle_notify.clone(),
            self.lost.clone(),
            entities,
        );
    }

    /// Entities this deployer dropped without any durable record: the batch flush failed AND
    /// recording them in failed_deployments failed too. Callers compare this before and after a
    /// drain -- any growth means the sync frontier must not advance, because nothing will
    /// re-deliver those entities.
    pub fn lost_count(&self) -> u64 {
        self.lost.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Entities buffered with an ephemeral expiration that parsed as no known date, kept on
    /// the peer's word (each one is also a debug line with its entity id).
    pub fn trusted_expiration_count(&self) -> u64 {
        self.trusted_expirations
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

async fn flush_batch(pool: &PgPool, entities: &[ParsedEntity]) -> Result<(), SyncError> {
    if entities.is_empty() {
        return Ok(());
    }

    let entities: Vec<&ParsedEntity> = {
        let mut seen = std::collections::HashSet::with_capacity(entities.len());
        entities
            .iter()
            .filter(|e| seen.insert(e.entity_id.clone()))
            .collect()
    };

    let count = entities.len();
    let metadatas: Vec<String> = entities
        .iter()
        .map(|e| serde_json::to_string(&e.entity_metadata))
        .collect::<Result<_, _>>()?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;

    {
        let all_pointers: Vec<String> = entities
            .iter()
            .flat_map(|e| e.entity_pointers.iter().cloned())
            .collect();
        lock_deployment_pointers(&mut tx, &all_pointers)
            .await
            .map_err(|e| SyncError::Storage(e.to_string()))?;
    }

    let mut deployer_addrs: Vec<String> = Vec::with_capacity(count);
    let mut versions: Vec<String> = Vec::with_capacity(count);
    let mut entity_types: Vec<String> = Vec::with_capacity(count);
    let mut entity_ids: Vec<String> = Vec::with_capacity(count);
    let mut timestamps: Vec<f64> = Vec::with_capacity(count);
    let mut pointers_json: Vec<Value> = Vec::with_capacity(count);
    let mut auth_chains: Vec<Value> = Vec::with_capacity(count);

    for e in &entities {
        deployer_addrs.push(e.deployer_address.clone());
        versions.push(e.version.clone());
        entity_types.push(e.entity_type.clone());
        entity_ids.push(e.entity_id.clone());
        timestamps.push(e.entity_timestamp);
        pointers_json.push(Value::Array(
            e.entity_pointers
                .iter()
                .map(|p| Value::String(p.clone()))
                .collect(),
        ));
        auth_chains.push(e.auth_chain.clone());
    }

    // Retain JSONB's validation boundary while storing the ordered JSON text.
    // Some downstream readers still require metadata to be representable as JSONB.
    let rows: Vec<(String, i32)> = sqlx::query_as(
        r#"
        INSERT INTO content.deployments
            (deployer_address, version, entity_type, entity_id, entity_metadata,
             entity_timestamp, entity_pointers, local_timestamp, auth_chain)
        SELECT da, v, et, ei, CASE WHEN em::jsonb IS NOT NULL THEN em::json ELSE NULL END,
               to_timestamp(ts / 1000.0),
               ARRAY(SELECT json_array_elements_text(ep::json)),
               now(), ac::json
        FROM unnest(
            $1::text[], $2::text[], $3::text[], $4::text[],
            $5::text[], $6::float8[], $7::jsonb[], $8::jsonb[]
        ) AS t(da, v, et, ei, em, ts, ep, ac)
        ON CONFLICT (entity_id) DO NOTHING
        RETURNING entity_id, id
        "#,
    )
    .bind(&deployer_addrs)
    .bind(&versions)
    .bind(&entity_types)
    .bind(&entity_ids)
    .bind(&metadatas)
    .bind(&timestamps)
    .bind(&pointers_json)
    .bind(&auth_chains)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| SyncError::Storage(e.to_string()))?;

    // Every entity of the batch is in content.deployments once this commits, so its failure
    // record goes in the same transaction: the retry worker never drops one earlier.
    crate::failed_deployments_store::remove_published(&mut tx, &entity_ids).await?;

    if rows.is_empty() {
        tx.commit()
            .await
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        return Ok(());
    }

    let id_map: HashMap<&str, i32> = rows.iter().map(|r| (r.0.as_str(), r.1)).collect();

    let mut cf_deployments: Vec<i32> = Vec::new();
    let mut cf_hashes: Vec<String> = Vec::new();
    let mut cf_keys: Vec<String> = Vec::new();

    let mut ap_dedup: HashMap<String, (String, String, f64)> = HashMap::new();

    for e in entities.iter() {
        let Some(&dep_id) = id_map.get(e.entity_id.as_str()) else {
            continue;
        };

        for (key, hash) in &e.content {
            cf_deployments.push(dep_id);
            cf_hashes.push(hash.clone());
            cf_keys.push(key.clone());
        }

        for ptr in &e.entity_pointers {
            let replace = match ap_dedup.get(ptr) {
                None => true,
                Some((existing_id, _, existing_ts)) => {
                    e.entity_timestamp > *existing_ts
                        || (e.entity_timestamp == *existing_ts
                            && e.entity_id.to_lowercase() > existing_id.to_lowercase())
                }
            };
            if replace {
                ap_dedup.insert(
                    ptr.clone(),
                    (
                        e.entity_id.clone(),
                        e.entity_type.clone(),
                        e.entity_timestamp,
                    ),
                );
            }
        }
    }

    let mut ap_pointers: Vec<String> = Vec::with_capacity(ap_dedup.len());
    let mut ap_entity_ids: Vec<String> = Vec::with_capacity(ap_dedup.len());
    let mut ap_entity_types: Vec<String> = Vec::with_capacity(ap_dedup.len());

    for (ptr, (eid, etype, _)) in ap_dedup {
        ap_pointers.push(ptr);
        ap_entity_ids.push(eid);
        ap_entity_types.push(etype);
    }

    if !cf_deployments.is_empty() {
        sqlx::query(
            r#"
            INSERT INTO content.content_files (deployment, content_hash, key)
            SELECT unnest($1::int[]), unnest($2::text[]), unnest($3::text[])
            "#,
        )
        .bind(&cf_deployments)
        .bind(&cf_hashes)
        .bind(&cf_keys)
        .execute(&mut *tx)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
    }

    let mut ow_ids: Vec<i32> = Vec::with_capacity(rows.len());
    let mut ow_types: Vec<String> = Vec::with_capacity(rows.len());
    let mut ow_eids: Vec<String> = Vec::with_capacity(rows.len());
    let mut ow_ts: Vec<f64> = Vec::with_capacity(rows.len());
    let mut ow_ptrs: Vec<Value> = Vec::with_capacity(rows.len());
    for e in &entities {
        let Some(&dep_id) = id_map.get(e.entity_id.as_str()) else {
            continue;
        };
        ow_ids.push(dep_id);
        ow_types.push(e.entity_type.clone());
        ow_eids.push(e.entity_id.clone());
        ow_ts.push(e.entity_timestamp);
        ow_ptrs.push(Value::Array(
            e.entity_pointers
                .iter()
                .map(|p| Value::String(p.clone()))
                .collect(),
        ));
    }

    sqlx::query(
        r#"
        UPDATE content.deployments AS old
        SET deleter_deployment = hit.new_id
        FROM (
            WITH batch AS (
                SELECT d.new_id, d.etype, d.new_eid, d.new_ts,
                       ARRAY(SELECT json_array_elements_text(d.ptrs_json::json)) AS ptrs
                FROM (
                    SELECT unnest($1::int4[]) AS new_id,
                           unnest($2::text[]) AS etype,
                           unnest($3::text[]) AS new_eid,
                           to_timestamp(unnest($4::float8[]) / 1000.0) AS new_ts,
                           unnest($5::jsonb[]) AS ptrs_json
                ) d
            ),
            -- Pointer overlap alone drives the candidate step. With the type or
            -- timestamp conditions in the same scan the planner ANDs the GIN
            -- pointer index with a btree that covers most of the table
            -- (500k+ entries per batch row: 1-2 s per batch of 15 on a 650k
            -- row node); the GIN scan alone finds the few overlapping rows in
            -- milliseconds and the cheap conditions filter them afterwards.
            cand AS MATERIALIZED (
                SELECT c.id AS old_id, c.entity_type AS old_type,
                       c.entity_timestamp AS old_ts, c.entity_id AS old_eid,
                       c.deleter_deployment AS old_deleter,
                       batch.new_id, batch.etype, batch.new_eid, batch.new_ts
                FROM batch
                JOIN content.deployments c ON c.entity_pointers && batch.ptrs
            )
            -- The nearest newer batch row becomes the deleter unless the
            -- current one is nearer still: the deleter is always the nearest
            -- successor, whatever order the rows arrived in.
            SELECT DISTINCT ON (old_id) old_id, new_id
            FROM cand
            LEFT JOIN content.deployments cur ON cur.id = cand.old_deleter
            WHERE old_type = etype
              AND old_id <> new_id
              AND (old_ts < new_ts OR (old_ts = new_ts AND lower(old_eid) < lower(new_eid)))
              AND (cur.id IS NULL
                   OR new_ts < cur.entity_timestamp
                   OR (new_ts = cur.entity_timestamp
                       AND lower(new_eid) < lower(cur.entity_id)))
            ORDER BY old_id, new_ts, lower(new_eid)
        ) AS hit
        WHERE old.id = hit.old_id
        "#,
    )
    .bind(&ow_ids)
    .bind(&ow_types)
    .bind(&ow_eids)
    .bind(&ow_ts)
    .bind(&ow_ptrs)
    .execute(&mut *tx)
    .await
    .map_err(|e| SyncError::Storage(e.to_string()))?;

    sqlx::query(
        r#"
        UPDATE content.deployments AS n
        SET deleter_deployment = sub.newer_id
        FROM (
            SELECT nr.id,
                   (SELECT c.id FROM (
                        SELECT d2.id, d2.entity_timestamp, d2.entity_id, d2.entity_type
                        FROM content.deployments d2
                        WHERE d2.entity_pointers && nr.entity_pointers
                          AND d2.id <> nr.id
                          AND (d2.entity_timestamp > nr.entity_timestamp
                               OR (d2.entity_timestamp = nr.entity_timestamp
                                   AND lower(d2.entity_id) > lower(nr.entity_id)))
                        OFFSET 0
                    ) c
                    -- An unpublished local scene still succeeds older ones:
                    -- its parcels stay empty.
                    WHERE c.entity_type = nr.entity_type
                    ORDER BY c.entity_timestamp, lower(c.entity_id)
                    LIMIT 1) AS newer_id
            FROM content.deployments nr
            WHERE nr.id = ANY($1)
        ) sub
        WHERE n.id = sub.id
          AND sub.newer_id IS NOT NULL
          AND n.deleter_deployment IS DISTINCT FROM sub.newer_id
        "#,
    )
    .bind(&ow_ids)
    .execute(&mut *tx)
    .await
    .map_err(|e| SyncError::Storage(e.to_string()))?;

    sqlx::query(
        r#"
        DELETE FROM content.active_pointers AS ap
        USING content.deployments d
        WHERE ap.entity_id = d.entity_id
          AND d.deleter_deployment IS NOT NULL
          AND (d.deleter_deployment = ANY($1) OR d.id = ANY($1))
        "#,
    )
    .bind(&ow_ids)
    .execute(&mut *tx)
    .await
    .map_err(|e| SyncError::Storage(e.to_string()))?;

    if !ap_pointers.is_empty() {
        sqlx::query(
            r#"
            INSERT INTO content.active_pointers (pointer, entity_id, entity_type)
            SELECT t.pointer, t.entity_id, t.entity_type
            FROM unnest($1::text[], $2::text[], $3::text[]) AS t(pointer, entity_id, entity_type)
            JOIN content.deployments dep ON dep.entity_id = t.entity_id
            WHERE dep.deleter_deployment IS NULL
            ON CONFLICT (pointer) DO UPDATE
                SET entity_id = EXCLUDED.entity_id, entity_type = EXCLUDED.entity_type
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM content.deployments cur, content.deployments incoming
                    WHERE cur.entity_id = active_pointers.entity_id
                      AND incoming.entity_id = EXCLUDED.entity_id
                      AND (cur.entity_timestamp > incoming.entity_timestamp
                           OR (cur.entity_timestamp = incoming.entity_timestamp
                               AND lower(cur.entity_id) > lower(EXCLUDED.entity_id)))
                )
            "#,
        )
        .bind(&ap_pointers)
        .bind(&ap_entity_ids)
        .bind(&ap_entity_types)
        .execute(&mut *tx)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
    }

    tx.commit()
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;

    tracing::info!(
        count = rows.len(),
        batch_size = count,
        "Batch flush committed"
    );
    Ok(())
}

impl LiveSyncDeployer {
    /// Validates the entity ([`Self::verify_entity`]), then buffers it; returns once it is buffered, so the
    /// write itself is asynchronous (see [`LiveSyncDeployer::flush`]). `retry_state` is the
    /// (attempts spent, backoff) of the retry worker's attempt, kept for the failure record a
    /// failed flush writes.
    pub async fn deploy_entity(
        &self,
        entity_data: &[u8],
        entity_id: &str,
        auth_chain: &AuthChain,
        _context: DeploymentContext,
        report: Option<&Arc<DeploymentReport>>,
        retry_state: Option<(u32, u64)>,
    ) -> Result<(), SyncError> {
        let mut parsed = parse_entity_for_deploy(entity_data, entity_id, auth_chain)?;
        let verdict = self
            .verify_entity(entity_data, entity_id, auth_chain, SyncedContent::Fetched)
            .await?;
        parsed.buffered = Some(BufferedMark::new(entity_id));
        scene_parcels::report_drift(
            &parsed.entity_type,
            entity_id,
            &parsed.entity_pointers,
            parsed.entity_metadata.get("v"),
        );
        if !verdict.trusted_expirations.is_empty() {
            self.trusted_expirations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::debug!(
                entity_id,
                expirations = ?verdict.trusted_expirations,
                "Synced entity's ephemeral expiration parses as no known date; kept as the \
                 peer's verdict"
            );
        }
        parsed.report = report.cloned();
        if let Some((retry_count, backoff_ms)) = retry_state {
            parsed.retry_count = retry_count;
            parsed.backoff_ms = backoff_ms;
        }
        // Taken after verification, so a rejected entity never waits on the database, and
        // before buffering: a full backlog holds the producer here until a flush lands.
        parsed.backlog_permit = Some(
            self.backlog
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| SyncError::Stopped)?,
        );

        let mut buf = self.batch.lock().await;
        buf.push(parsed);
        if buf.len() >= BATCH_SIZE {
            self.trigger_flush(std::mem::take(&mut *buf));
        }

        Ok(())
    }

    /// Flushes the buffer and waits for every in-flight flush to finish.
    pub async fn flush(&self) -> Result<(), SyncError> {
        {
            let mut buf = self.batch.lock().await;
            self.trigger_flush(std::mem::take(&mut *buf));
        }

        loop {
            let notified = self.idle_notify.notified();
            if self.in_flight.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return Ok(());
            }
            notified.await;
        }
    }
}

/// Whether `entity_id` is already deployed at `timestamp_ms` or later: the bloom-guarded
/// skip in the batch deployer. (The old `LiveDeploymentRepository::is_entity_deployed`.)
pub async fn is_entity_deployed(
    pool: &PgPool,
    entity_id: &str,
    timestamp_ms: Timestamp,
) -> Result<bool, SyncError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM content.deployments
                       WHERE entity_id = $1 AND entity_timestamp >= to_timestamp($2 / 1000.0))",
    )
    .bind(entity_id)
    .bind(timestamp_ms as f64)
    .fetch_one(pool)
    .await
    .map_err(|e| SyncError::Storage(e.to_string()))?;
    Ok(exists)
}

/// The ids among `entities` already deployed at their timestamp or later: [`is_entity_deployed`]
/// for a whole batch in one statement.
pub async fn deployed_entity_ids(
    pool: &PgPool,
    entities: &[(&str, Timestamp)],
) -> Result<HashSet<String>, SyncError> {
    let ids: Vec<&str> = entities.iter().map(|(id, _)| *id).collect();
    let timestamps: Vec<f64> = entities.iter().map(|(_, ts)| *ts as f64).collect();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT d.entity_id FROM unnest($1::text[], $2::float8[]) AS r(entity_id, ts)
         JOIN content.deployments d
           ON d.entity_id = r.entity_id AND d.entity_timestamp >= to_timestamp(r.ts / 1000.0)",
    )
    .bind(&ids)
    .bind(&timestamps)
    .fetch_all(pool)
    .await
    .map_err(|e| SyncError::Storage(e.to_string()))?;
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests;
