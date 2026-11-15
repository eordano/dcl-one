//! Fan-out for the entities a snapshot or a pointer-changes page announces:
//! skip what is already deployed (bloom filter, then the database), bound the
//! queue, and run each remaining entity through
//! [`super::deploy_remote_entity::deploy_entity_streaming`] in its own task.
//! An entity that fails is recorded in `content.failed_deployments`; one that
//! can be neither deployed nor recorded is counted lost, and the caller must not
//! advance its cursor past it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use super::bloom_filter::BloomFilter;
use super::failed_deployments_store::LiveFailedDeploymentsStore;
use super::sync_deployer::{LiveSyncDeployer, deployed_entity_ids, is_entity_deployed};
use super::sync_types::{
    DeploymentContext, FailedDeployment, SyncDeployment, SyncError, Timestamp,
};
use super::wire_types::TimeRange;
use crate::storage::Storage;

/// Draining (`on_idle`) only proves the queue emptied, not that every entity in it was deployed, so
/// callers create one of these per snapshot or per pointer-changes run and refuse to advance their
/// sync cursor while `acknowledged < scheduled`.
///
/// "Acknowledged" means durably accounted for: deployed, or recorded in `failed_deployments` where
/// the retry loop owns it. An entity that fails BOTH is never acknowledged -- the silent-loss case
/// the sync frontier must not advance past.
#[derive(Debug, Default)]
pub struct DeploymentReport {
    scheduled: std::sync::atomic::AtomicU64,
    acknowledged: std::sync::atomic::AtomicU64,
    lost: std::sync::atomic::AtomicU64,
}

impl DeploymentReport {
    pub fn scheduled(&self) -> u64 {
        self.scheduled.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn acknowledged(&self) -> u64 {
        self.acknowledged.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Entities of THIS report dropped with no durable record anywhere. Attributed per report so a
    /// loss in one stream does not force every concurrent pass to hold back, the way comparing the
    /// deployer's global counter did: during the straggler retry that global check ran concurrently
    /// with live streams, and any loss anywhere marked every snapshot in the pass failed forever.
    pub fn lost(&self) -> u64 {
        self.lost.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Counted on the same code paths that bump the deployers' global `lost` counters, which
    /// remain as process-wide metrics only.
    pub fn record_lost(&self) {
        self.lost.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Counted when an entity is handed to the deployer, before any await: a synchronous
    /// completion acknowledging first would let acknowledged briefly exceed scheduled and
    /// make an incomplete run look complete.
    pub fn record_scheduled(&self) {
        self.scheduled
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn record_acknowledged(&self) {
        self.acknowledged
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Only meaningful after the deployer has drained (`on_idle`), like upstream's post-drain
    /// processed-marker re-check.
    pub fn is_complete(&self) -> bool {
        self.acknowledged() >= self.scheduled()
    }
}

#[derive(Debug, Clone)]
pub struct BatchDeployerConfig {
    /// The old `CONCURRENT_SYNC_DOWNLOADS`.
    pub content_download_concurrency: usize,
    pub ignored_types: HashSet<String>,
    pub profile_max_age_ms: i64,
    pub max_queue_depth: usize,
}

impl Default for BatchDeployerConfig {
    fn default() -> Self {
        Self {
            content_download_concurrency: 200,
            ignored_types: HashSet::new(),
            profile_max_age_ms: 31_536_000_000,
            max_queue_depth: 1000,
        }
    }
}

/// One "Sync progress" line per [`SUMMARY_EVERY`] in place of a line per entity: a bootstrap
/// is hundreds of thousands of deployments, and journald's rate limit dropped the per-entity
/// lines wholesale (22k in one burst), so the per-entity outcome is `debug` and the summary
/// carries the totals, what is in flight and the rate since the previous summary. Whichever
/// completion crosses the boundary writes it, elapsed-gated the way the pointer-changes
/// heartbeat is.
struct SyncProgress {
    synced: AtomicU64,
    failed: AtomicU64,
    clock: parking_lot::Mutex<SummaryClock>,
}

const SUMMARY_EVERY: Duration = Duration::from_secs(60);

/// When a summary is due and the rate it reports; pure, so the gate is testable without
/// waiting a minute.
#[derive(Debug, Clone, Copy)]
struct SummaryClock {
    last_at: Instant,
    last_synced: u64,
}

impl SummaryClock {
    fn new(now: Instant) -> Self {
        Self {
            last_at: now,
            last_synced: 0,
        }
    }

    /// `Some(entities per second since the last summary)` once `every` has elapsed, resetting
    /// the clock; `None` before then.
    fn tick(&mut self, now: Instant, synced_total: u64, every: Duration) -> Option<f64> {
        let elapsed = now.saturating_duration_since(self.last_at);
        if elapsed < every {
            return None;
        }
        let rate = synced_total.saturating_sub(self.last_synced) as f64 / elapsed.as_secs_f64();
        self.last_at = now;
        self.last_synced = synced_total;
        Some(rate)
    }
}

impl SyncProgress {
    fn new() -> Self {
        Self {
            synced: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            clock: parking_lot::Mutex::new(SummaryClock::new(Instant::now())),
        }
    }

    fn maybe_summarize(&self, in_flight: usize, lost: u64, flush_backlog: usize) {
        let Some(mut clock) = self.clock.try_lock() else {
            return;
        };
        let synced_total = self.synced.load(Ordering::Relaxed);
        if let Some(rate) = clock.tick(Instant::now(), synced_total, SUMMARY_EVERY) {
            info!(
                synced_total,
                failed_total = self.failed.load(Ordering::Relaxed),
                lost,
                in_flight,
                flush_backlog,
                entities_per_sec = (rate * 10.0).round() / 10.0,
                "Sync progress"
            );
        }
    }
}

pub struct BatchDeployer {
    config: BatchDeployerConfig,
    http_client: reqwest::Client,
    storage: Arc<Storage>,
    pool: sqlx::PgPool,
    deployer: Arc<LiveSyncDeployer>,
    failed_store: Arc<LiveFailedDeploymentsStore>,

    content_semaphore: Arc<Semaphore>,
    /// One permit per entity from scheduling to its outcome, `queue_depth` of them: taken
    /// before the task is spawned and dropped once the outcome is recorded, so "in flight" is
    /// the permits out and no scheduler can read a stale count and overshoot the depth, which
    /// the atomic's check-then-add let every concurrent caller do at once.
    queue: Arc<Semaphore>,
    queue_depth: usize,
    idle_notify: Arc<tokio::sync::Notify>,
    deployed_bloom: Arc<parking_lot::RwLock<BloomFilter>>,
    servers: Arc<parking_lot::RwLock<Vec<String>>>,
    lost: Arc<AtomicU64>,
    progress: Arc<SyncProgress>,
}

impl BatchDeployer {
    /// `pool` answers the "already deployed" probe; `bloom` is pre-seeded with the entity ids
    /// the database already holds (`LiveDeploymentRepository::seed_deployed_entity_ids`) or empty.
    pub fn new(
        config: BatchDeployerConfig,
        http_client: reqwest::Client,
        storage: Arc<Storage>,
        pool: sqlx::PgPool,
        deployer: Arc<LiveSyncDeployer>,
        failed_store: Arc<LiveFailedDeploymentsStore>,
        bloom: BloomFilter,
    ) -> Self {
        let content_concurrency = config.content_download_concurrency;
        let queue_depth = config.max_queue_depth.max(1);
        BatchDeployer {
            config,
            http_client,
            storage,
            pool,
            deployer,
            failed_store,
            content_semaphore: Arc::new(Semaphore::new(content_concurrency)),
            queue: Arc::new(Semaphore::new(queue_depth)),
            queue_depth,
            idle_notify: Arc::new(tokio::sync::Notify::new()),
            deployed_bloom: Arc::new(parking_lot::RwLock::new(bloom)),
            servers: Arc::new(parking_lot::RwLock::new(Vec::new())),
            lost: Arc::new(AtomicU64::new(0)),
            progress: Arc::new(SyncProgress::new()),
        }
    }

    /// Entities dropped with no durable record anywhere (both writes failed), including those lost
    /// by a failed batch flush inside the underlying deployer. Callers snapshot this before a phase
    /// and compare after draining -- any growth means the sync frontier must not advance, because
    /// nothing will re-deliver those entities.
    pub fn lost_count(&self) -> u64 {
        self.lost.load(Ordering::SeqCst) + self.deployer.lost_count()
    }

    /// Entities scheduled whose outcome is not yet recorded.
    pub fn in_flight(&self) -> usize {
        self.queue_depth - self.queue.available_permits()
    }

    /// Ignored types and profiles older than `profile_max_age_ms` are never deployed.
    fn out_of_scope(&self, entity: &SyncDeployment, now_ms: i64) -> bool {
        self.config.ignored_types.contains(&entity.entity_type)
            || (entity.entity_type == "profile"
                && entity.entity_timestamp < now_ms - self.config.profile_max_age_ms)
    }

    pub async fn schedule_entity_deployment(
        &self,
        entity: SyncDeployment,
        content_servers: &[String],
        report: Option<&Arc<DeploymentReport>>,
    ) -> Result<(), SyncError> {
        if self.out_of_scope(&entity, chrono::Utc::now().timestamp_millis()) {
            return Ok(());
        }
        if self.deployed_bloom.read().maybe_contains(&entity.entity_id)
            && is_entity_deployed(&self.pool, &entity.entity_id, entity.entity_timestamp).await?
        {
            return Ok(());
        }
        self.schedule_undeployed(entity, content_servers, report)
            .await
    }

    /// The records of `records` that [`Self::schedule_entity_deployment`] would not skip, with
    /// one database probe for every bloom hit of the batch instead of a pooled round trip each:
    /// per-record probes from a snapshot pass held most of the pool while the flushes waited.
    /// On Err `records` holds every in-scope record, unprobed.
    pub async fn retain_undeployed(
        &self,
        records: &mut Vec<SyncDeployment>,
    ) -> Result<(), SyncError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        records.retain(|entity| !self.out_of_scope(entity, now_ms));
        let hits: Vec<(&str, Timestamp)> = {
            let bloom = self.deployed_bloom.read();
            records
                .iter()
                .filter(|e| bloom.maybe_contains(&e.entity_id))
                .map(|e| (e.entity_id.as_str(), e.entity_timestamp))
                .collect()
        };
        if hits.is_empty() {
            return Ok(());
        }
        let deployed = deployed_entity_ids(&self.pool, &hits).await?;
        records.retain(|e| !deployed.contains(&e.entity_id));
        Ok(())
    }

    /// Schedules `entity` with no already-deployed probe: the caller ran
    /// [`Self::retain_undeployed`] over it.
    pub async fn schedule_undeployed(
        &self,
        entity: SyncDeployment,
        content_servers: &[String],
        report: Option<&Arc<DeploymentReport>>,
    ) -> Result<(), SyncError> {
        {
            let mut s = self.servers.write();
            for server in content_servers {
                if !s.contains(server) {
                    s.push(server.clone());
                }
            }
        }

        let slot = self
            .queue
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SyncError::Stopped)?;

        let http_client = self.http_client.clone();
        let storage = self.storage.clone();
        let deployer = self.deployer.clone();
        let failed_store = self.failed_store.clone();
        let content_semaphore = self.content_semaphore.clone();
        let queue = self.queue.clone();
        let queue_depth = self.queue_depth;
        let idle_notify = self.idle_notify.clone();
        let deployed_bloom = self.deployed_bloom.clone();
        let lost = self.lost.clone();
        let progress = self.progress.clone();
        let servers: Vec<String> = self.servers.read().clone();
        let report = report.cloned();

        if let Some(report) = &report {
            report.record_scheduled();
        }

        tokio::spawn(async move {
            let result = super::deploy_remote_entity::deploy_entity_streaming(
                super::deploy_remote_entity::ContentSource {
                    client: &http_client,
                    storage,
                    servers: &servers,
                    content_semaphore,
                },
                deployer.as_ref(),
                super::deploy_remote_entity::RemoteDeployment {
                    entity_id: &entity.entity_id,
                    auth_chain: &entity.auth_chain,
                    context: DeploymentContext::Synced,
                    report: report.as_ref(),
                    retry_state: None,
                },
            )
            .await;

            let acknowledged = match result {
                Ok(()) => {
                    deployed_bloom.write().add(&entity.entity_id);
                    progress.synced.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        entity_id = %entity.entity_id,
                        entity_type = %entity.entity_type,
                        "Synced deployment successful"
                    );
                    true
                }
                Err(e) => {
                    progress.failed.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        entity_id = %entity.entity_id,
                        entity_type = %entity.entity_type,
                        error = %e,
                        "Entity deployment failed"
                    );
                    match failed_store
                        .report_failure(FailedDeployment {
                            entity_type: entity.entity_type.clone(),
                            entity_id: entity.entity_id.clone(),
                            reason: e.failure_reason(),
                            auth_chain: entity.auth_chain.clone(),
                            error_description: e.to_string(),
                            failure_timestamp: chrono::Utc::now().timestamp_millis(),
                            snapshot_hash: None,
                            retry_count: 0,
                            next_retry_at: 0,
                        })
                        .await
                    {
                        Ok(()) => true,
                        Err(record_err) => {
                            lost.fetch_add(1, Ordering::SeqCst);
                            if let Some(report) = &report {
                                report.record_lost();
                            }
                            error!(
                                entity_id = %entity.entity_id,
                                error = %record_err,
                                "Failed deployment could not be recorded in failed_deployments; \
                                 holding the sync frontier back"
                            );
                            false
                        }
                    }
                }
            };

            if acknowledged && let Some(report) = &report {
                report.record_acknowledged();
            }

            drop(slot);
            idle_notify.notify_waiters();
            progress.maybe_summarize(
                queue_depth - queue.available_permits(),
                lost.load(Ordering::SeqCst) + deployer.lost_count(),
                deployer.flush_backlog(),
            );
        });

        Ok(())
    }

    /// Waits for every scheduled entity to finish, then flushes the batching deployer.
    pub async fn on_idle(&self) -> Result<(), SyncError> {
        loop {
            let notified = self.idle_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.queue.available_permits() == self.queue_depth {
                self.deployer.flush().await?;
                return Ok(());
            }
            notified.await;
        }
    }

    pub async fn prepare_for_deployments_in(
        &self,
        _time_ranges: &[TimeRange],
    ) -> Result<(), SyncError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::hash_bytes_v1;
    use crate::sync_test_support::{
        signed_chain, signed_chain_behind_contract, spawn_content_server, temp_storage, test_pool,
        unique,
    };

    #[test]
    fn the_summary_is_due_once_per_interval_and_reports_the_rate_since_the_last() {
        let t0 = Instant::now();
        let every = Duration::from_secs(60);
        let mut clock = SummaryClock::new(t0);
        assert_eq!(clock.tick(t0 + Duration::from_secs(30), 10, every), None);
        let rate = clock
            .tick(t0 + Duration::from_secs(60), 120, every)
            .unwrap();
        assert!((rate - 2.0).abs() < 1e-9, "{rate}");
        assert_eq!(clock.tick(t0 + Duration::from_secs(90), 200, every), None);
        let rate = clock
            .tick(t0 + Duration::from_secs(150), 200, every)
            .unwrap();
        assert!((rate - 80.0 / 90.0).abs() < 1e-9, "{rate}");
        assert_eq!(
            clock.tick(t0 + Duration::from_secs(210), 100, every),
            Some(0.0),
            "a counter that went backwards never yields a negative rate"
        );
    }

    #[test]
    fn a_report_is_complete_only_once_everything_scheduled_is_acknowledged() {
        let report = DeploymentReport::default();
        assert!(report.is_complete(), "nothing scheduled, nothing owed");
        report.record_scheduled();
        report.record_scheduled();
        assert!(!report.is_complete());
        report.record_acknowledged();
        report.record_lost();
        assert!(!report.is_complete());
        assert_eq!(
            (report.scheduled(), report.acknowledged(), report.lost()),
            (2, 1, 1)
        );
        report.record_acknowledged();
        assert!(report.is_complete());
    }

    async fn cleanup(pool: &sqlx::PgPool, prefix: &str) {
        let like = format!("{prefix}%");
        for sql in [
            "DELETE FROM content.content_files WHERE deployment IN (SELECT id FROM content.deployments WHERE entity_id LIKE $1)",
            "DELETE FROM content.active_pointers WHERE pointer LIKE $1",
            "DELETE FROM content.deployments WHERE entity_id LIKE $1",
            "DELETE FROM content.failed_deployments WHERE entity_id LIKE $1",
        ] {
            sqlx::query(sql).bind(&like).execute(pool).await.unwrap();
        }
    }

    /// The peer serves one entity file whose id is its real CID. Announced with a chain signed
    /// for something else it becomes a failed_deployments row with the verification reason;
    /// announced again with its own chain it becomes a deployment row, and the transaction that
    /// writes it clears the failure record. The report accounts for both.
    #[tokio::test]
    async fn announced_entities_land_as_rows_or_as_recorded_failures() {
        let Some(pool) = test_pool(3).await else {
            return;
        };
        let prefix = unique();
        let pointer = format!("{prefix}:0,0");
        let body = serde_json::to_vec(&serde_json::json!({
            "version": "v3", "type": "scene", "pointers": [pointer], "timestamp": 1_000,
            "content": [], "metadata": {"p": prefix}
        }))
        .unwrap();
        let entity_id = hash_bytes_v1(&body);
        let (base, server) = spawn_content_server(body, None).await;
        let (storage, tmp) = temp_storage("batch").await;
        let deployer = Arc::new(LiveSyncDeployer::new(
            pool.clone(),
            2,
            crate::sync_deployer::SyncValidation::AuthChainOnly,
        ));
        let batch = BatchDeployer::new(
            BatchDeployerConfig {
                ignored_types: HashSet::from(["outfits".to_string()]),
                ..Default::default()
            },
            reqwest::Client::new(),
            storage,
            pool.clone(),
            deployer,
            Arc::new(LiveFailedDeploymentsStore::new(pool.clone())),
            BloomFilter::with_capacity(100),
        );
        let report = Arc::new(DeploymentReport::default());
        let announce = |id: String, entity_type: String, chain_for: String| async move {
            SyncDeployment {
                entity_id: id,
                entity_type,
                pointers: vec![],
                auth_chain: signed_chain(&chain_for).await,
                entity_timestamp: 1_000,
                local_timestamp: None,
            }
        };

        let forged = format!("{prefix}-forged");
        batch
            .schedule_entity_deployment(
                announce(entity_id.clone(), "scene".into(), forged.clone()).await,
                std::slice::from_ref(&base),
                Some(&report),
            )
            .await
            .unwrap();
        batch
            .schedule_entity_deployment(
                announce(
                    format!("{prefix}-ignored"),
                    "outfits".into(),
                    forged.clone(),
                )
                .await,
                std::slice::from_ref(&base),
                Some(&report),
            )
            .await
            .unwrap();
        batch.on_idle().await.unwrap();
        let recorded = || async {
            sqlx::query_as::<_, (String, String)>(
                "SELECT reason, error_description FROM content.failed_deployments \
                 WHERE entity_id = $1",
            )
            .bind(&entity_id)
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        let failed = recorded().await;
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "Deployment rejected");
        assert!(
            failed[0].1.contains("auth chain verification failed"),
            "{}",
            failed[0].1
        );
        assert_eq!(
            (report.scheduled(), report.acknowledged(), report.lost()),
            (1, 1, 0)
        );

        batch
            .schedule_entity_deployment(
                announce(entity_id.clone(), "scene".into(), entity_id.clone()).await,
                std::slice::from_ref(&base),
                Some(&report),
            )
            .await
            .unwrap();
        batch.on_idle().await.unwrap();

        assert_eq!(batch.in_flight(), 0);
        assert_eq!(
            (report.scheduled(), report.acknowledged(), report.lost()),
            (2, 2, 0)
        );
        assert!(report.is_complete());
        assert_eq!(batch.lost_count(), 0);
        assert!(is_entity_deployed(&pool, &entity_id, 1_000).await.unwrap());
        assert!(recorded().await.is_empty(), "the write cleared the record");

        batch
            .schedule_entity_deployment(
                announce(entity_id.clone(), "scene".into(), entity_id.clone()).await,
                std::slice::from_ref(&base),
                Some(&report),
            )
            .await
            .unwrap();
        assert_eq!(
            report.scheduled(),
            2,
            "a deployed entity is skipped by the bloom+db probe"
        );

        server.abort();
        sqlx::query("DELETE FROM content.failed_deployments WHERE entity_id = $1")
            .bind(&entity_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM content.deployments WHERE entity_id = $1")
            .bind(&entity_id)
            .execute(&pool)
            .await
            .unwrap();
        cleanup(&pool, &prefix).await;
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        pool.close().await;
    }

    /// The address of the sync tests' signing key.
    const TEST_SIGNER: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
    const CONTRACT_SIGNER: &str = "0x0000000000000000000000000000000000000002";

    enum Chain {
        Signed,
        BehindContract,
    }

    struct SyncOutcome {
        deployed: bool,
        active: Option<String>,
        failure: Option<(i32, String)>,
        /// The retry worker would pick the failure up now.
        due: bool,
    }

    /// A contract wallet that refuses today; at a past block it accepts, or cannot answer.
    struct WalletAtBlock {
        answers_at_block: bool,
    }

    #[async_trait::async_trait]
    impl crate::auth::Eip1654Validator for WalletAtBlock {
        async fn validate_signature(
            &self,
            _contract_address: &str,
            _hash: &[u8],
            _signature: &[u8],
        ) -> Result<bool, crate::auth::AuthError> {
            Ok(false)
        }

        async fn validate_signature_at_block(
            &self,
            _contract_address: &str,
            _hash: &[u8],
            _signature: &[u8],
            block: u64,
        ) -> Result<bool, crate::auth::AuthError> {
            use crate::validator::fake_chain::{GENESIS, INTERVAL};
            assert_eq!(block, ((1_700_000_000 - GENESIS) / INTERVAL) as u64);
            if self.answers_at_block {
                Ok(true)
            } else {
                Err(crate::auth::AuthError::Eip1654ValidationFailed(
                    "missing trie node".to_string(),
                ))
            }
        }
    }

    /// Syncs one outfits entity on `owner`'s pointer, signed as `chain` says, through a
    /// deployer with the content validator attached and `eip1654` as its contract-wallet
    /// validator. The chain answers block lookups only.
    async fn sync_outfits(
        owner: &str,
        chain: Chain,
        eip1654: Option<Arc<dyn crate::auth::Eip1654Validator>>,
    ) -> Option<SyncOutcome> {
        let blocks =
            crate::validator::fake_chain::serve(|q, _| panic!("unexpected question {q:?}")).await;
        let entity = serde_json::json!({
            "version": "v3", "type": "outfits", "pointers": [format!("{owner}:outfits")],
            "timestamp": 1_700_000_000_000i64, "content": [],
            "metadata": {"outfits": [], "namesForExtraSlots": [], "tag": unique()}
        });
        sync_one(entity, chain, eip1654, over(&blocks)).await
    }

    /// The one content file an entity of these tests may name, already in the store.
    const MODEL: &[u8] = b"glTF";

    /// Syncs `entity` (its first pointer the one checked), signed as `chain` says, through a
    /// deployer with the content validator over `settings` attached. The peer serves only
    /// the entity file; [`MODEL`] is already stored.
    async fn sync_one(
        entity: serde_json::Value,
        chain: Chain,
        eip1654: Option<Arc<dyn crate::auth::Eip1654Validator>>,
        settings: crate::validator::Settings,
    ) -> Option<SyncOutcome> {
        let pool = test_pool(3).await?;
        let pointer = entity["pointers"][0].as_str().unwrap().to_string();
        let entity_type = entity["type"].as_str().unwrap().to_string();
        let timestamp = entity["timestamp"].as_i64().unwrap();
        let body = serde_json::to_vec(&entity).unwrap();
        let entity_id = hash_bytes_v1(&body);
        let auth_chain = match chain {
            Chain::Signed => signed_chain(&entity_id).await,
            Chain::BehindContract => {
                signed_chain_behind_contract(&entity_id, "2099-01-01T00:00:00.000Z").await
            }
        };
        sqlx::query("DELETE FROM content.active_pointers WHERE pointer = $1")
            .bind(&pointer)
            .execute(&pool)
            .await
            .unwrap();
        let (base, server) = spawn_content_server(body, None).await;
        let (storage, tmp) = temp_storage("validated").await;
        storage
            .store(&hash_bytes_v1(MODEL), MODEL.into())
            .await
            .unwrap();
        let deployer = Arc::new(LiveSyncDeployer::new(
            pool.clone(),
            2,
            crate::sync_deployer::SyncValidation::Content(Arc::new(
                crate::validator::Sources::new(
                    pool.clone(),
                    storage.clone(),
                    crate::http::Outbound::for_tests(),
                    eip1654,
                    settings,
                ),
            )),
        ));
        let batch = BatchDeployer::new(
            BatchDeployerConfig::default(),
            reqwest::Client::new(),
            storage,
            pool.clone(),
            deployer,
            Arc::new(LiveFailedDeploymentsStore::new(pool.clone())),
            BloomFilter::with_capacity(100),
        );
        batch
            .schedule_entity_deployment(
                SyncDeployment {
                    entity_id: entity_id.clone(),
                    entity_type,
                    pointers: vec![],
                    auth_chain,
                    entity_timestamp: timestamp,
                    local_timestamp: None,
                },
                std::slice::from_ref(&base),
                None,
            )
            .await
            .unwrap();
        batch.on_idle().await.unwrap();

        let outcome = SyncOutcome {
            deployed: is_entity_deployed(&pool, &entity_id, 0).await.unwrap(),
            active: sqlx::query_scalar(
                "SELECT entity_id FROM content.active_pointers WHERE pointer = $1",
            )
            .bind(&pointer)
            .fetch_optional(&pool)
            .await
            .unwrap(),
            failure: sqlx::query_as(
                "SELECT retry_count, error_description FROM content.failed_deployments \
                 WHERE entity_id = $1",
            )
            .bind(&entity_id)
            .fetch_optional(&pool)
            .await
            .unwrap(),
            due: LiveFailedDeploymentsStore::new(pool.clone())
                .select_due(chrono::Utc::now().timestamp_millis(), 10, None, 10_000)
                .await
                .unwrap()
                .failures
                .iter()
                .any(|f| f.entity_id == entity_id),
        };
        let active_is_this = outcome.active.as_deref() == Some(entity_id.as_str());

        server.abort();
        for sql in [
            "DELETE FROM content.active_pointers WHERE entity_id = $1",
            "DELETE FROM content.deployments WHERE entity_id = $1",
            "DELETE FROM content.failed_deployments WHERE entity_id = $1",
        ] {
            sqlx::query(sql)
                .bind(&entity_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        pool.close().await;
        Some(SyncOutcome {
            active: active_is_this.then_some(entity_id),
            ..outcome
        })
    }

    #[tokio::test]
    async fn a_synced_entity_refused_by_its_access_rule_is_a_failed_deployment_never_active() {
        let other = format!("0x{}", "ab".repeat(20));
        let Some(outcome) = sync_outfits(&other, Chain::Signed, None).await else {
            return;
        };
        assert!(!outcome.deployed);
        assert_eq!(outcome.active, None);
        assert!(!outcome.due, "a refusal is never retried");
        let (retry_count, description) = outcome.failure.expect("recorded as failed");
        assert_eq!(retry_count, 0);
        assert!(
            description.starts_with("Deployment rejected:")
                && description.contains("You can only alter your own outfits"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn a_valid_synced_entity_is_deployed_and_active() {
        let Some(outcome) = sync_outfits(TEST_SIGNER, Chain::Signed, None).await else {
            return;
        };
        assert!(outcome.deployed);
        assert!(outcome.active.is_some());
        assert!(outcome.failure.is_none());
    }

    #[tokio::test]
    async fn a_contract_wallet_link_without_an_eip1654_validator_is_retried_not_accepted() {
        let Some(outcome) = sync_outfits(CONTRACT_SIGNER, Chain::BehindContract, None).await else {
            return;
        };
        assert!(!outcome.deployed);
        assert_eq!(outcome.active, None);
        let (retry_count, description) = outcome.failure.expect("queued for a retry");
        assert_eq!(retry_count, 0);
        assert!(
            description.starts_with("Validation undecided:"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn a_contract_wallet_valid_at_the_entity_block_is_accepted_though_it_refuses_today() {
        let wallet = Arc::new(WalletAtBlock {
            answers_at_block: true,
        });
        let Some(outcome) =
            sync_outfits(CONTRACT_SIGNER, Chain::BehindContract, Some(wallet)).await
        else {
            return;
        };
        assert!(outcome.deployed);
        assert!(outcome.active.is_some());
        assert!(outcome.failure.is_none());
    }

    #[tokio::test]
    async fn a_contract_wallet_the_rpc_cannot_judge_at_the_entity_block_is_retried() {
        let wallet = Arc::new(WalletAtBlock {
            answers_at_block: false,
        });
        let Some(outcome) =
            sync_outfits(CONTRACT_SIGNER, Chain::BehindContract, Some(wallet)).await
        else {
            return;
        };
        assert!(!outcome.deployed);
        let (_, description) = outcome.failure.expect("queued for a retry");
        assert!(
            description.starts_with("Validation undecided:"),
            "{description}"
        );
    }

    /// Mainnet contracts over `chain` on both layers.
    fn over(chain: &crate::validator::fake_chain::FakeChain) -> crate::validator::Settings {
        crate::validator::Settings {
            eth_network: "mainnet".into(),
            l1_rpc_url: Some(chain.url.clone()),
            l2_rpc_url: Some(chain.url.clone()),
            ..Default::default()
        }
    }

    /// A day ago: well inside the fake chain, well before its head.
    fn a_day_ago() -> i64 {
        chrono::Utc::now().timestamp_millis() - 86_400_000
    }

    /// A scene on one parcel no other test uses.
    fn scene_at(timestamp: i64) -> (serde_json::Value, (i64, i64)) {
        let n = u64::from_str_radix(&unique()[..8], 16).unwrap();
        let parcel = (100 + (n % 50) as i64, -100 - ((n >> 8) % 50) as i64);
        let pointer = format!("{},{}", parcel.0, parcel.1);
        let entity = serde_json::json!({
            "version": "v3", "type": "scene", "pointers": [pointer], "timestamp": timestamp,
            "content": [],
            "metadata": {"main": "bin/index.js", "scene": {"base": pointer, "parcels": [pointer]}, "tag": unique()}
        });
        (entity, parcel)
    }

    /// The LAND the test signer held up to `held_until` (a block), nothing after.
    fn land_held_until(
        parcel: (i64, i64),
        held_until: Option<u64>,
    ) -> impl Fn(&crate::validator::fake_chain::Query, u64) -> crate::validator::fake_chain::Reply
    + Send
    + Sync
    + 'static {
        use crate::validator::fake_chain::{Query, Reply};
        move |q, block| match q {
            Query::Land { sender, x, y, .. } => Reply::Yes(
                sender.to_string().to_lowercase() == TEST_SIGNER
                    && (*x, *y) == parcel
                    && held_until.is_some_and(|b| block <= b),
            ),
            other => panic!("unexpected question {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_synced_scene_is_judged_by_the_land_its_signer_held_at_the_entity_block() {
        use crate::validator::fake_chain::{self, at_deployment};
        let timestamp = a_day_ago();

        let (entity, parcel) = scene_at(timestamp);
        let chain =
            fake_chain::serve(land_held_until(parcel, Some(at_deployment(timestamp)))).await;
        let Some(sold_since) = sync_one(entity, Chain::Signed, None, over(&chain)).await else {
            return;
        };
        assert!(sold_since.deployed, "held then, sold since");
        assert!(sold_since.active.is_some());
        assert!(sold_since.failure.is_none());
        assert!(
            chain
                .calls()
                .iter()
                .filter_map(|c| c.strip_prefix("eth_call@"))
                .all(|b| b.parse::<u64>().unwrap() <= at_deployment(timestamp)),
            "never asked about today: {:?}",
            chain.calls()
        );

        let (entity, parcel) = scene_at(timestamp);
        let chain = fake_chain::serve(land_held_until(parcel, None)).await;
        let never = sync_one(entity, Chain::Signed, None, over(&chain))
            .await
            .unwrap();
        assert!(!never.deployed);
        assert_eq!(never.active, None);
        let (_, description) = never.failure.expect("recorded as failed");
        assert!(
            description.starts_with("Deployment rejected:")
                && description.contains("does not have access to the following parcel"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn the_check_after_the_download_reuses_the_access_the_check_before_it_passed() {
        use crate::validator::fake_chain::{self, at_deployment};
        let Some(pool) = test_pool(1).await else {
            return;
        };
        let timestamp = a_day_ago();
        let (entity, parcel) = scene_at(timestamp);
        let chain =
            fake_chain::serve(land_held_until(parcel, Some(at_deployment(timestamp)))).await;
        let (storage, tmp) = temp_storage("access-passed").await;
        let deployer = LiveSyncDeployer::new(
            pool.clone(),
            1,
            crate::sync_deployer::SyncValidation::Content(Arc::new(
                crate::validator::Sources::new(
                    pool.clone(),
                    storage,
                    crate::http::Outbound::for_tests(),
                    None,
                    over(&chain),
                ),
            )),
        );
        let body = serde_json::to_vec(&entity).unwrap();
        let id = hash_bytes_v1(&body);
        let auth = signed_chain(&id).await;
        let eth_calls = || {
            chain
                .calls()
                .iter()
                .filter(|c| c.starts_with("eth_call"))
                .count()
        };
        let verify = |content| deployer.verify_entity(&body, &id, &auth, content);

        verify(crate::validator::SyncedContent::Pending)
            .await
            .unwrap();
        let asked = eth_calls();
        assert!(asked > 0);
        verify(crate::validator::SyncedContent::Fetched)
            .await
            .unwrap();
        assert_eq!(eth_calls(), asked, "the access already passed");
        verify(crate::validator::SyncedContent::Fetched)
            .await
            .unwrap();
        assert!(eth_calls() > asked, "taken once: a later check asks again");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        pool.close().await;
    }

    #[tokio::test]
    async fn a_synced_collection_item_is_judged_by_the_collection_at_the_entity_block() {
        use crate::validator::fake_chain::{self, Query, Reply, at_deployment};
        let timestamp = a_day_ago();
        let item = |collection: &str| {
            let pointer = format!("urn:decentraland:matic:collections-v2:{collection}:3");
            serde_json::json!({
                "version": "v3", "type": "wearable", "pointers": [pointer], "timestamp": timestamp,
                "content": [{"file": "male/hat.glb", "hash": hash_bytes_v1(MODEL)}],
                "metadata": {
                    "id": pointer, "name": "hat", "description": "",
                    "i18n": [{"code": "en", "text": "Hat"}],
                    "thumbnail": "thumbnail.png", "image": "image.png",
                    "collectionAddress": collection, "rarity": "epic",
                    "data": {
                        "replaces": [], "hides": [], "tags": ["hat"], "category": "hat",
                        "representations": [{
                            "bodyShapes": ["urn:decentraland:off-chain:base-avatars:BaseMale"],
                            "mainFile": "male/hat.glb", "contents": ["male/hat.glb"],
                            "overrideHides": [], "overrideReplaces": []
                        }]
                    },
                    "tag": unique()
                }
            })
        };
        let editor_until = |until: Option<u64>| {
            move |q: &Query, block: u64| match q {
                Query::Wearable { sender, .. } => Reply::Yes(
                    sender.to_string().to_lowercase() == TEST_SIGNER
                        && until.is_some_and(|b| block <= b),
                ),
                other => panic!("unexpected question {other:?}"),
            }
        };

        let collection = format!("0x{}", &unique().repeat(2)[..40]);
        let chain = fake_chain::serve(editor_until(Some(at_deployment(timestamp)))).await;
        let Some(then) = sync_one(item(&collection), Chain::Signed, None, over(&chain)).await
        else {
            return;
        };
        assert!(then.deployed, "an editor then, approved since");
        assert!(then.failure.is_none());

        let collection = format!("0x{}", &unique().repeat(2)[..40]);
        let chain = fake_chain::serve(editor_until(None)).await;
        let never = sync_one(item(&collection), Chain::Signed, None, over(&chain))
            .await
            .unwrap();
        assert!(!never.deployed);
        let (_, description) = never.failure.expect("recorded as failed");
        assert!(
            description.starts_with("Deployment rejected:")
                && description.contains("does not have access to the following item"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn a_synced_entity_a_pruned_rpc_cannot_judge_is_undecided_and_retried() {
        use crate::validator::fake_chain::{self, Reply};
        let (entity, _) = scene_at(a_day_ago());
        let pruned = fake_chain::serve(|_, _| Reply::Fail).await;
        let Some(outcome) = sync_one(entity, Chain::Signed, None, over(&pruned)).await else {
            return;
        };
        assert!(!outcome.deployed);
        let (retry_count, description) = outcome.failure.expect("queued for a retry");
        assert_eq!(retry_count, 0);
        // The RPC's error is logged; the public record carries the fixed sentence.
        assert!(
            description.starts_with("Validation undecided:")
                && description.ends_with(crate::validator::checker::PARCEL_ACCESS_UNDECIDED)
                && !description.contains("missing trie node"),
            "{description}"
        );
        assert!(outcome.due, "the retry worker picks it up");
    }
}
