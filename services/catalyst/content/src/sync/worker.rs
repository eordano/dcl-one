//! The sync worker as `serve` attaches it: [`SyncSettings`] validates the
//! `SYNC_*` configuration once at startup, [`start`] builds the write path,
//! the orchestrator and the retry/prune loops on the ONE pool, ONE storage
//! root and ONE HTTP client, and registers them with the job registry. The
//! orchestrator owns the Bootstrapping -> Syncing transition from there.
//! With sync off, [`start_failed_deployments`] runs the retry/prune loops for
//! the rows the read-through records.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use tokio::sync::{RwLock, watch};

use crate::batch_deployer::{BatchDeployer, BatchDeployerConfig};
use crate::bloom_filter::BloomFilter;
use crate::config::Config;
use crate::failed_deployments_store::LiveFailedDeploymentsStore;
use crate::jobs::{JobContext, Jobs};
use crate::storage::Storage;
use crate::sync::dao::{self, DaoSource};
use crate::sync::orchestrator::{SyncControl, SyncDependencies, run_sync_job};
use crate::sync::retry_failed::run_prune_loop;
use crate::sync::snapshots::SnapshotScheduling;
use crate::sync::status_mirror;
use crate::sync::{
    LiveDeploymentRepository, LiveProcessedSnapshotStore, RetryFailedConfig,
    RetryFailedDeployments, SyncOrchestrator, SyncOrchestratorConfig, SyncState, SyncStatus,
    names_a_source, parse_entity_types, parse_sync_sources,
};
use crate::sync_deployer::{LiveSyncDeployer, SyncMemoryLimits, SyncValidation};

/// The memory limits `SyncSettings::from_config` validated, read back by [`start`]. They ride
/// beside `SyncSettings` rather than in it: the integration tests build the settings by
/// literal, and settings built by hand run with the defaults.
static MEMORY_LIMITS: parking_lot::Mutex<SyncMemoryLimits> =
    parking_lot::Mutex::new(SyncMemoryLimits::DEFAULT);

pub fn memory_limits() -> SyncMemoryLimits {
    *MEMORY_LIMITS.lock()
}

/// `SYNC_INFLIGHT_BYTES_MAX` and `SYNC_FLUSH_BACKLOG_MAX` as configured; `Err` on a zero, which
/// would stall every download or every producer for good.
pub fn memory_limits_from_config(config: &Config) -> anyhow::Result<SyncMemoryLimits> {
    if config.content.sync_inflight_bytes_max == 0 {
        anyhow::bail!("SYNC_INFLIGHT_BYTES_MAX must be at least 1 (0 would stall every download)");
    }
    if config.content.sync_flush_backlog_max == 0 {
        anyhow::bail!(
            "SYNC_FLUSH_BACKLOG_MAX must be at least 1 (0 would stall the worker forever)"
        );
    }
    Ok(SyncMemoryLimits {
        inflight_bytes_max: config.content.sync_inflight_bytes_max,
        flush_backlog_max: config.content.sync_flush_backlog_max,
    })
}

/// The peer-sync configuration after validation: peers parsed and checked
/// against this node's own URLs, the entity-type subset resolved. Contract-wallet
/// links in synced chains are asked of the validator's EIP-1654 RPC at the
/// entity's block; without `ETH_RPC_URL`, or with an RPC that lacks that block's
/// state, the entity is recorded as undecided and retried with backoff, without
/// counting against `MAX_FAILED_DEPLOYMENT_RETRIES`, for up to a week.
#[derive(Debug, Clone)]
pub struct SyncSettings {
    /// `SYNC_SOURCE`; with [`Self::dao`], the list used until a registry read succeeds.
    pub peers: Vec<String>,
    /// `SYNC_FROM_DAO`: the DAO catalyst list replaces `peers` once read.
    pub dao: Option<DaoSource>,
    pub entity_types: Option<HashSet<String>>,
    pub from_timestamp_ms: i64,
    pub phased_sync: bool,
    pub content_download_concurrency: usize,
    pub connections_max_idle: usize,
    pub snapshot_concurrency: usize,
    pub snapshot_scheduling: SnapshotScheduling,
    pub flush_concurrency: usize,
    pub bloom_expected_elements: usize,
    pub profile_max_age_ms: i64,
    pub retry_failed_enabled: bool,
    pub max_retries: u32,
    pub retry_concurrency: usize,
    pub prune_ttl_days: i64,
}

impl SyncSettings {
    /// `None` when `SYNC_ENABLED` is off; `Err` on any setting that would make
    /// the worker unsafe to start (the old binary panicked on the same ones).
    pub fn from_config(config: &Config) -> anyhow::Result<Option<Self>> {
        if !config.node.sync_enabled {
            return Ok(None);
        }
        if config.node.serve_unsynced {
            anyhow::bail!(
                "SERVE_UNSYNCED and SYNC_ENABLED are exclusive: the sync worker reports \
                 readiness itself, and the override would forge it while bootstrapping"
            );
        }
        let own_urls: Vec<String> = [
            config.node.public_url.clone(),
            config.node.content_url.clone(),
            Some(format!("http://{}", config.node.bind_addr())),
        ]
        .into_iter()
        .flatten()
        .collect();
        let peers = parse_sync_sources(&config.content.sync_source, &own_urls)?;
        let dao = dao_source(config, own_urls)?;
        let entity_types = parse_entity_types(&config.content.sync_entity_types)?;
        for (name, value) in [
            (
                "CONCURRENT_SYNC_DOWNLOADS",
                config.content.concurrent_sync_downloads,
            ),
            (
                "SYNC_SNAPSHOT_CONCURRENCY",
                config.content.sync_snapshot_concurrency,
            ),
            (
                "SYNC_NONSCENE_CONCURRENCY",
                config.content.sync_nonscene_concurrency,
            ),
            (
                "SYNC_SCENE_CONCURRENCY",
                config.content.sync_scene_concurrency,
            ),
            (
                "SYNC_FLUSH_CONCURRENCY",
                config.content.sync_flush_concurrency,
            ),
            (
                "SYNC_RETRY_CONCURRENCY",
                config.content.sync_retry_concurrency,
            ),
        ] {
            if value == 0 {
                anyhow::bail!("{name} must be at least 1 (0 would stall the worker forever)");
            }
        }
        *MEMORY_LIMITS.lock() = memory_limits_from_config(config)?;
        Ok(Some(Self {
            peers,
            dao,
            entity_types,
            from_timestamp_ms: config.content.sync_from_timestamp_ms,
            phased_sync: config.content.phased_sync,
            content_download_concurrency: config.content.concurrent_sync_downloads,
            connections_max_idle: config.content.connections_max_idle,
            snapshot_concurrency: config.content.sync_snapshot_concurrency,
            snapshot_scheduling: SnapshotScheduling {
                nonscene_concurrency: config.content.sync_nonscene_concurrency,
                scene_concurrency: config.content.sync_scene_concurrency,
            },
            flush_concurrency: config.content.sync_flush_concurrency,
            bloom_expected_elements: config.content.bloom_filter_expected_elements,
            profile_max_age_ms: match config.content.sync_profile_max_age_days {
                0 => i64::MAX,
                days => i64::from(days) * 86_400_000,
            },
            retry_failed_enabled: config.content.retry_failed_enabled,
            max_retries: config.content.max_failed_deployment_retries,
            retry_concurrency: config.content.sync_retry_concurrency,
            prune_ttl_days: config.content.retry_failed_prune_ttl_days,
        }))
    }
}

/// `SYNC_FROM_DAO` resolved: `None` when off, or when there is no registry
/// to read (no `ETH_RPC_URL`, an `ETH_NETWORK` without one), which leaves
/// `SYNC_SOURCE` as the only list.
fn dao_source(config: &Config, own_urls: Vec<String>) -> anyhow::Result<Option<DaoSource>> {
    if !config.content.sync_from_dao {
        return Ok(None);
    }
    let Some(rpc_url) = config.content.eth_rpc_url()? else {
        tracing::warn!("SYNC_FROM_DAO=true without ETH_RPC_URL: syncing from SYNC_SOURCE only");
        return Ok(None);
    };
    let Some(registry) = dao::registry_address(&config.node.eth_network) else {
        tracing::warn!(
            eth_network = %config.node.eth_network,
            "SYNC_FROM_DAO=true on a network with no catalyst registry: syncing from SYNC_SOURCE only"
        );
        return Ok(None);
    };
    Ok(Some(DaoSource {
        rpc_url,
        registry,
        own_urls,
    }))
}

/// The peers to start from: the DAO list when it reads, else `SYNC_SOURCE`.
async fn initial_peers(settings: &SyncSettings, client: &reqwest::Client) -> Vec<String> {
    let Some(dao) = &settings.dao else {
        return settings.peers.clone();
    };
    match dao.peers(client).await {
        Ok(peers) => {
            tracing::info!(?peers, "Sync peers read from the DAO catalyst list");
            peers
        }
        Err(e) => {
            tracing::warn!(error = %e, fallback = ?settings.peers, "DAO catalyst list read failed; syncing from SYNC_SOURCE");
            settings.peers.clone()
        }
    }
}

/// Reads the DAO list every [`dao::REFRESH`] and publishes a changed set; a
/// failed read keeps the current one.
fn spawn_dao_refresh(
    jobs: &Arc<Jobs>,
    ctx: &JobContext,
    dao: DaoSource,
    client: reqwest::Client,
    tx: watch::Sender<HashSet<String>>,
) {
    jobs.spawn_scoped(ctx, "sync-dao-peers", move |ctx| async move {
        loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(dao::REFRESH) => {}
            }
            match dao.peers(&client).await {
                Ok(peers) => {
                    let peers: HashSet<String> = peers.into_iter().collect();
                    let changed = tx.send_if_modified(|current| {
                        let changed = *current != peers;
                        if changed {
                            *current = peers.clone();
                        }
                        changed
                    });
                    ctx.progress(if changed {
                        format!("peers changed: {} peers", peers.len())
                    } else {
                        format!("unchanged: {} peers", peers.len())
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "DAO catalyst list refresh failed; keeping the current peers");
                    ctx.error(e);
                }
            }
        }
    });
}

/// `SERVE_UNSYNCED` with no worker attached: the process claims `Syncing` so
/// the content read contracts replay against whatever the database holds.
/// While `SYNC_SOURCE` names a peer, that is an unsynced copy of the peer and
/// deploys keep the bootstrapping refusal, except for the entity types
/// `SERVE_UNSYNCED_DEPLOY_TYPES` names; with no source at all the node is its
/// own authority and takes them.
pub fn serve_unsynced(config: &Config, status: &SyncStatus) {
    let copy = names_a_source(&config.content.sync_source);
    let types: Vec<String> = config
        .node
        .serve_unsynced_deploy_types
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if copy {
        tracing::warn!(
            takes = ?types,
            "SERVE_UNSYNCED with SYNC_SOURCE naming a peer: deploys answer the bootstrapping 503 \
             but for the SERVE_UNSYNCED_DEPLOY_TYPES; an empty SYNC_SOURCE makes this node its own authority"
        );
        status.set_copy_deploy_types(types);
    }
    status.set_authority(true);
    status.set_unsynced_copy(copy);
    status.set_state(SyncState::Syncing);
}

/// What `start` hands back for the operator plane: the orchestrator's
/// pause/resume/force handle (`ops::admin`). Process state only.
pub struct SyncHandles {
    pub control: SyncControl,
}

/// Builds the sync pipeline and registers its jobs: `sync-batch-flush` (the
/// deployer's batch ticker), `sync` (the orchestrator), `retry-failed` when
/// enabled, and `failed-deployments-prune`. Returns once the jobs are spawned; the
/// orchestrator moves `status` out of Bootstrapping on its own.
pub async fn start(
    settings: &SyncSettings,
    pool: PgPool,
    storage: Arc<Storage>,
    status: Arc<SyncStatus>,
    client: reqwest::Client,
    validation: SyncValidation,
    jobs: &Arc<Jobs>,
) -> anyhow::Result<SyncHandles> {
    let control = SyncControl::default();
    let leader = Arc::new(Leader {
        settings: settings.clone(),
        pool,
        storage,
        validation,
        status,
        client,
        control: control.clone(),
        jobs: jobs.clone(),
    });
    jobs.spawn_leased("sync", move |ctx| {
        let leader = leader.clone();
        async move { leader.run(ctx).await }
    });
    Ok(SyncHandles { control })
}

/// Without a sync worker in this process: the deleter repair the sync leader
/// runs at its bootstrap, as job `deleter-repair`, once per database and repair
/// version. It runs under the `sync` lease so it never overlaps a leader's
/// flushes; while another process holds that lease the leader's own bootstrap
/// repair covers the database. Losing the lease rolls the repair back.
fn repair_outcome(result: Result<(), crate::sync::SyncError>) -> anyhow::Result<bool> {
    result
        .map(|()| true)
        .map_err(|e| anyhow::anyhow!("deleter repair: {e}"))
}

/// Cancels the statement the repair's backend runs, so its locks go with the rollback
/// rather than when the statement would have finished.
async fn cancel_backend(pool: &PgPool, pid: i32) {
    if pid == 0 {
        return;
    }
    if let Err(e) = sqlx::query("SELECT pg_cancel_backend($1)")
        .bind(pid)
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, pid, "could not cancel the deleter repair's statement");
    }
}

pub fn spawn_startup_repair(jobs: &Arc<Jobs>, pool: PgPool) {
    let leases = jobs.leases().cloned();
    jobs.spawn_leased("deleter-repair", move |ctx| {
        let (leases, pool) = (leases.clone(), pool.clone());
        async move {
            let repo = LiveDeploymentRepository::new(pool.clone(), Arc::new(SyncStatus::default()));
            if repo.deleter_repair_done().await? {
                ctx.progress("skipped: this database already had this repair");
                return Ok(());
            }
            let guard = match &leases {
                Some(leases) => match leases.try_acquire("sync").await? {
                    Some(guard) => Some(guard),
                    None => {
                        ctx.progress("skipped: a sync leader holds the sync lease");
                        return Ok(());
                    }
                },
                None => None,
            };
            let lost = guard
                .as_ref()
                .map(|g| g.lost().clone())
                .unwrap_or_default();
            let result = if repo.deleter_repair_done().await? {
                Ok(false)
            } else {
                let backend = AtomicI32::new(0);
                let repair = repo.resolve_deleter_deployments_reporting(Some(&backend));
                tokio::pin!(repair);
                // Stopped before the repair future drops, while its connection is still checked out.
                let stopped = tokio::select! {
                    result = &mut repair => repair_outcome(result),
                    _ = lost.cancelled() => Err(anyhow::anyhow!("deleter repair: the sync lease was lost")),
                    _ = ctx.cancel.cancelled() => Err(anyhow::anyhow!("deleter repair: cancelled")),
                };
                if stopped.is_err() {
                    cancel_backend(&pool, backend.load(Ordering::SeqCst)).await;
                }
                stopped
            };
            if let Some(guard) = guard {
                guard.release().await;
            }
            let repaired = result?;
            ctx.progress(if repaired {
                "done"
            } else {
                "skipped: this database already had this repair"
            });
            Ok(())
        }
    });
}

/// Everything the process holding the `sync` lease builds and runs: the
/// deployer and its flush ticker, the bloom filter over the deployed entity
/// ids, the orchestrator, the failed-deployments retry and prune loops and
/// the status publisher. Built afresh each time the lease is taken and ended
/// together when it is lost or the process stops: the helpers are scoped to
/// the leased job ([`Jobs::spawn_scoped`]). While it runs, this process is
/// the [`SyncStatus`] authority; every other process mirrors what the
/// publisher writes ([`status_mirror`]).
struct Leader {
    settings: SyncSettings,
    pool: PgPool,
    storage: Arc<Storage>,
    /// How synced and retried entities are judged.
    validation: SyncValidation,
    status: Arc<SyncStatus>,
    client: reqwest::Client,
    control: SyncControl,
    jobs: Arc<Jobs>,
}

impl Leader {
    async fn run(&self, ctx: JobContext) -> anyhow::Result<()> {
        let _authority = Authority::take(&self.status);
        // A takeover starts where a process restart would: nothing is followed
        // until the orchestrator has caught up with every peer again.
        self.status.set_state(SyncState::Bootstrapping);
        status_mirror::spawn_publisher(&self.jobs, &ctx, self.pool.clone(), self.status.clone());
        let settings = &self.settings;
        let limits = memory_limits();
        let deployer = Arc::new(LiveSyncDeployer::with_limits(
            self.pool.clone(),
            settings.flush_concurrency,
            limits,
            self.validation.clone(),
        ));
        deployer.spawn_flush_ticker(&self.jobs, &ctx);
        tracing::info!(
            inflight_bytes_max = limits.inflight_bytes_max,
            flush_backlog_max = limits.flush_backlog_max,
            content_download_concurrency = settings.content_download_concurrency,
            "Sync memory limits"
        );
        let failed = Arc::new(LiveFailedDeploymentsStore::new(self.pool.clone()));
        let repo = Arc::new(LiveDeploymentRepository::new(
            self.pool.clone(),
            self.status.clone(),
        ));

        let mut bloom = BloomFilter::with_capacity(settings.bloom_expected_elements);
        let seeding = std::time::Instant::now();
        let count = repo
            .seed_deployed_entity_ids(&mut bloom)
            .await
            .context("loading deployed entity ids into the bloom filter")?;
        tracing::info!(
            count,
            elapsed_ms = seeding.elapsed().as_millis() as u64,
            pool_size = self.pool.size(),
            pool_idle = self.pool.num_idle(),
            "Bloom filter populated"
        );

        let batch = Arc::new(BatchDeployer::new(
            BatchDeployerConfig {
                content_download_concurrency: settings.content_download_concurrency,
                profile_max_age_ms: settings.profile_max_age_ms,
                ..Default::default()
            },
            self.client.clone(),
            self.storage.clone(),
            self.pool.clone(),
            deployer.clone(),
            failed.clone(),
            bloom,
        ));
        let orchestrator = Arc::new(SyncOrchestrator::new(
            SyncOrchestratorConfig {
                from_timestamp: settings.from_timestamp_ms,
                entity_types: settings.entity_types.clone(),
                phased_sync: settings.phased_sync,
                snapshot_concurrency: settings.snapshot_concurrency,
                snapshot_scheduling: settings.snapshot_scheduling,
                ..Default::default()
            },
            SyncDependencies {
                http_client: self.client.clone(),
                storage: self.storage.clone(),
                deployer: batch,
                processed_store: Arc::new(LiveProcessedSnapshotStore::new(self.pool.clone())),
                deployment_repo: repo,
                status: self.status.clone(),
                control: self.control.clone(),
            },
        ));
        if let Some(types) = &settings.entity_types {
            tracing::info!(?types, "Sync restricted to an entity-type allowlist");
        }
        if settings.from_timestamp_ms > 0 {
            tracing::warn!(
                from_timestamp_ms = settings.from_timestamp_ms,
                "SYNC_FROM_TIMESTAMP_MS set: bootstrap is bounded, older entities are never fetched"
            );
        }
        let initial = initial_peers(settings, &self.client).await;
        tracing::info!(peers = ?initial, phased_sync = settings.phased_sync, "Sync orchestrator ready");
        let (peers_tx, peers) = watch::channel(initial.iter().cloned().collect::<HashSet<_>>());
        if let Some(dao) = &settings.dao {
            spawn_dao_refresh(&self.jobs, &ctx, dao.clone(), self.client.clone(), peers_tx);
        }
        let retry_peers = Arc::new(RwLock::new(initial));
        let (bootstrapped_tx, bootstrapped) = watch::channel(false);
        spawn_failed_deployment_jobs(
            &self.jobs,
            &ctx,
            &FailedDeploymentSettings {
                retry_failed_enabled: settings.retry_failed_enabled,
                max_retries: settings.max_retries,
                retry_concurrency: settings.retry_concurrency,
                prune_ttl_days: settings.prune_ttl_days,
            },
            FailedDeploymentDeps {
                pool: self.pool.clone(),
                storage: self.storage.clone(),
                client: self.client.clone(),
                deployer,
                failed,
                peers: retry_peers.clone(),
                bootstrapped: Some(bootstrapped),
            },
        );
        run_sync_job(orchestrator, peers, retry_peers, ctx, move || {
            let _ = bootstrapped_tx.send(true);
        })
        .await
    }
}

/// The `RETRY_FAILED_*` settings the retry and prune loops run with.
#[derive(Debug, Clone)]
pub struct FailedDeploymentSettings {
    pub retry_failed_enabled: bool,
    pub max_retries: u32,
    pub retry_concurrency: usize,
    pub prune_ttl_days: i64,
}

impl FailedDeploymentSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            retry_failed_enabled: config.content.retry_failed_enabled,
            max_retries: config.content.max_failed_deployment_retries,
            retry_concurrency: config.content.sync_retry_concurrency,
            prune_ttl_days: config.content.retry_failed_prune_ttl_days,
        }
    }
}

struct FailedDeploymentDeps {
    pool: PgPool,
    storage: Arc<Storage>,
    client: reqwest::Client,
    deployer: Arc<LiveSyncDeployer>,
    failed: Arc<LiveFailedDeploymentsStore>,
    peers: Arc<RwLock<Vec<String>>>,
    /// Set once the sync leader's bootstrap completed; `None` where there is no bootstrap.
    bootstrapped: Option<watch::Receiver<bool>>,
}

/// How long the retry loop waits for the sync leader's bootstrap before it starts anyway.
const RETRY_FAILED_START: Duration = Duration::from_secs(60);
/// The same for the prune loop.
const PRUNE_START: Duration = Duration::from_secs(300);

/// Returns at bootstrap completion or after `delay`, whichever comes first, so a peer that never
/// answers holds the node in `Bootstrapping` without holding the loop; at once with no bootstrap
/// to wait for, and on cancellation, which the loop then sees.
async fn wait_to_start(
    ctx: &JobContext,
    bootstrapped: Option<watch::Receiver<bool>>,
    delay: Duration,
) {
    let Some(mut bootstrapped) = bootstrapped else {
        return;
    };
    tokio::select! {
        _ = ctx.cancel.cancelled() => {}
        _ = tokio::time::timeout(delay, bootstrapped.wait_for(|done| *done)) => {}
    }
}

/// `retry-failed` (when enabled; it also sets [`UNDECIDED_RECORDS`](crate::sync::retry_failed::UNDECIDED_RECORDS)) and
/// `failed-deployments-prune`, scoped to `ctx`; each starts per [`wait_to_start`].
fn spawn_failed_deployment_jobs(
    jobs: &Arc<Jobs>,
    ctx: &JobContext,
    settings: &FailedDeploymentSettings,
    deps: FailedDeploymentDeps,
) {
    if settings.retry_failed_enabled {
        let retry = RetryFailedDeployments::new(
            RetryFailedConfig {
                max_retries: settings.max_retries,
                retry_concurrency: settings.retry_concurrency,
                ..Default::default()
            },
            deps.client,
            deps.storage,
            deps.deployer,
            deps.failed,
            deps.peers,
        );
        let bootstrapped = deps.bootstrapped.clone();
        jobs.spawn_scoped(ctx, "retry-failed", move |ctx| async move {
            wait_to_start(&ctx, bootstrapped, RETRY_FAILED_START).await;
            let trace = ctx.clone();
            retry
                .run(ctx.cancel.clone(), move |outcome| match outcome {
                    Ok(stats) => trace.progress(format!("{stats:?}")),
                    Err(e) => trace.error(e.to_string()),
                })
                .await;
            Ok(())
        });
    }
    let ttl_days = settings.prune_ttl_days;
    let pool = deps.pool;
    let bootstrapped = deps.bootstrapped;
    jobs.spawn_scoped(ctx, "failed-deployments-prune", move |ctx| async move {
        wait_to_start(&ctx, bootstrapped, PRUNE_START).await;
        let trace = ctx.clone();
        run_prune_loop(
            pool,
            ttl_days,
            ctx.cancel.clone(),
            move |outcome| match outcome {
                Ok(pruned) => trace.progress(format!("pruned {pruned}")),
                Err(e) => trace.error(e.to_string()),
            },
        )
        .await;
        Ok(())
    });
}

/// The retry and prune loops of a node that reads through `peers` with no sync worker: the
/// read-through records undecided entities there. A fleet-wide job (`failed-deployments`) with
/// its own deployer and flush ticker; it holds its lease until the process stops.
pub fn start_failed_deployments(
    settings: &FailedDeploymentSettings,
    peers: Vec<String>,
    pool: PgPool,
    storage: Arc<Storage>,
    client: reqwest::Client,
    validation: SyncValidation,
    jobs: &Arc<Jobs>,
) {
    let settings = settings.clone();
    let registry = jobs.clone();
    jobs.spawn_leased("failed-deployments", move |ctx| {
        let settings = settings.clone();
        let jobs = registry.clone();
        let deployer = Arc::new(LiveSyncDeployer::with_limits(
            pool.clone(),
            crate::sync_deployer::DEFAULT_FLUSH_CONCURRENCY,
            memory_limits(),
            validation.clone(),
        ));
        let deps = FailedDeploymentDeps {
            pool: pool.clone(),
            storage: storage.clone(),
            client: client.clone(),
            deployer: deployer.clone(),
            failed: Arc::new(LiveFailedDeploymentsStore::new(pool.clone())),
            peers: Arc::new(RwLock::new(peers.clone())),
            bootstrapped: None,
        };
        async move {
            deployer.spawn_flush_ticker(&jobs, &ctx);
            spawn_failed_deployment_jobs(&jobs, &ctx, &settings, deps);
            ctx.cancel.cancelled().await;
            Ok(())
        }
    });
}

/// Marks this process the sync authority for as long as it is held.
struct Authority(Arc<SyncStatus>);

impl Authority {
    fn take(status: &Arc<SyncStatus>) -> Self {
        status.set_authority(true);
        Self(status.clone())
    }
}

impl Drop for Authority {
    fn drop(&mut self) {
        self.0.set_authority(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str]) -> Config {
        let mut argv = vec!["x", "--database-url", "postgres:///catalyst"];
        argv.extend_from_slice(args);
        Config::parse_for_tests(&argv).unwrap()
    }

    #[test]
    fn disabled_sync_yields_no_settings_and_skips_every_check() {
        let settings = SyncSettings::from_config(&config(&["--sync-source", "garbage"])).unwrap();
        assert!(settings.is_none());
    }

    #[test]
    fn enabled_sync_needs_no_rpc_endpoint() {
        let settings = SyncSettings::from_config(&config(&[
            "--sync-enabled",
            "--sync-source",
            "https://peer.example/content",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(settings.peers, vec!["https://peer.example/content"]);
    }

    #[tokio::test]
    async fn the_dao_list_needs_a_registry_and_falls_back_to_sync_source() {
        let base = [
            "--sync-enabled",
            "--sync-source",
            "https://peer.example/content",
            "--public-url",
            "https://me.example",
        ];
        let settings = SyncSettings::from_config(&config(&base)).unwrap().unwrap();
        assert!(settings.dao.is_none(), "off unless SYNC_FROM_DAO");

        let mut args = base.to_vec();
        args.extend([
            "--sync-from-dao",
            "true",
            "--eth-rpc-url",
            "https://rpc.example/",
        ]);
        let settings = SyncSettings::from_config(&config(&args)).unwrap().unwrap();
        let dao = settings.dao.clone().expect("mainnet has a registry");
        assert_eq!(dao.registry, dao::MAINNET_REGISTRY);
        assert!(dao.own_urls.contains(&"https://me.example".to_string()));

        let mut other = args.clone();
        other.extend(["--eth-network", "holesky"]);
        let none = SyncSettings::from_config(&config(&other)).unwrap().unwrap();
        assert!(none.dao.is_none(), "no registry on that network");

        // A registry read that fails starts from SYNC_SOURCE.
        let unreachable = SyncSettings {
            dao: Some(DaoSource {
                rpc_url: "https://127.0.0.1:9/".to_string(),
                ..dao
            }),
            ..settings
        };
        assert_eq!(
            initial_peers(&unreachable, &reqwest::Client::new()).await,
            vec!["https://peer.example/content"]
        );
    }

    #[test]
    fn enabled_sync_refuses_a_self_referential_or_plaintext_peer() {
        let err = SyncSettings::from_config(&config(&[
            "--sync-enabled",
            "--sync-source",
            "http://127.0.0.1:5210/content",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("itself"), "{err}");

        let err = SyncSettings::from_config(&config(&[
            "--sync-enabled",
            "--sync-source",
            "http://peer.example/content",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("plaintext"), "{err}");
    }

    #[test]
    fn serve_unsynced_cannot_be_combined_with_the_worker() {
        let err = SyncSettings::from_config(&config(&[
            "--sync-enabled",
            "--serve-unsynced",
            "--sync-source",
            "https://peer.example/content",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("exclusive"), "{err}");
    }

    #[test]
    fn a_zero_concurrency_is_refused() {
        let err = SyncSettings::from_config(&config(&[
            "--sync-enabled",
            "--sync-source",
            "https://peer.example/content",
            "--sync-scene-concurrency",
            "0",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("SYNC_SCENE_CONCURRENCY"), "{err}");
    }

    #[test]
    fn memory_limits_follow_the_config_and_refuse_a_zero() {
        let limits = memory_limits_from_config(&config(&[
            "--sync-inflight-bytes-max",
            "123",
            "--sync-flush-backlog-max",
            "7",
        ]))
        .unwrap();
        assert_eq!(
            limits,
            SyncMemoryLimits {
                inflight_bytes_max: 123,
                flush_backlog_max: 7
            }
        );
        assert_eq!(
            memory_limits_from_config(&config(&[])).unwrap(),
            SyncMemoryLimits::DEFAULT
        );

        let err = SyncSettings::from_config(&config(&[
            "--sync-enabled",
            "--sync-source",
            "https://peer.example/content",
            "--sync-flush-backlog-max",
            "0",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("SYNC_FLUSH_BACKLOG_MAX"), "{err}");
        let err =
            memory_limits_from_config(&config(&["--sync-inflight-bytes-max", "0"])).unwrap_err();
        assert!(err.to_string().contains("SYNC_INFLIGHT_BYTES_MAX"), "{err}");
    }

    #[test]
    fn valid_settings_carry_the_parsed_peers_and_types() {
        let settings = SyncSettings::from_config(&config(&[
            "--sync-enabled",
            "--sync-source",
            "https://peer.example/content/, https://other.example/content",
            "--sync-entity-types",
            "store,outfits",
            "--concurrent-sync-downloads",
            "12",
            "--sync-from-timestamp-ms",
            "1788307200000",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            settings.peers,
            vec![
                "https://peer.example/content",
                "https://other.example/content"
            ]
        );
        let types = settings.entity_types.as_ref().unwrap();
        assert_eq!(types.len(), 2);
        assert!(types.contains("store") && types.contains("outfits"));
        assert_eq!(settings.content_download_concurrency, 12);
        assert_eq!(settings.from_timestamp_ms, 1_788_307_200_000);
        assert_eq!(settings.snapshot_scheduling.nonscene_concurrency, 64);
        assert_eq!(settings.connections_max_idle, 25);
    }

    async fn failed_deployment_jobs(retry_failed_enabled: bool, fleet_jobs: bool) -> Vec<String> {
        let (storage, _tmp) = crate::sync_test_support::temp_storage("failed-jobs").await;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
            .unwrap();
        let jobs = Arc::new(Jobs::new().with_fleet_jobs(fleet_jobs));
        start_failed_deployments(
            &FailedDeploymentSettings {
                retry_failed_enabled,
                ..FailedDeploymentSettings::from_config(&config(&[]))
            },
            vec!["https://peer.example/content".to_string()],
            pool,
            storage,
            reqwest::Client::new(),
            SyncValidation::AuthChainOnly,
            &jobs,
        );
        // The leased job settles once it spawned its helpers or stood down.
        let settled = |jobs: &Jobs| {
            jobs.report().iter().any(|job| {
                job.name == "failed-deployments-prune"
                    || job.state == crate::jobs::JobState::StoodDown
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !settled(&jobs) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut names: Vec<String> = jobs
            .report()
            .into_iter()
            .filter(|job| job.state == crate::jobs::JobState::Running)
            .map(|job| job.name)
            .collect();
        names.sort();
        jobs.shutdown(Duration::from_secs(5)).await;
        names
    }

    #[tokio::test]
    async fn without_sync_the_retry_and_prune_loops_run_for_the_read_through() {
        assert_eq!(
            failed_deployment_jobs(true, true).await,
            [
                "failed-deployments",
                "failed-deployments-prune",
                "retry-failed",
                "sync-batch-flush"
            ]
        );
    }

    #[tokio::test]
    async fn without_sync_a_disabled_retry_still_prunes() {
        assert_eq!(
            failed_deployment_jobs(false, true).await,
            [
                "failed-deployments",
                "failed-deployments-prune",
                "sync-batch-flush"
            ]
        );
    }

    #[tokio::test]
    async fn without_sync_a_process_without_fleet_jobs_runs_neither_loop() {
        assert!(failed_deployment_jobs(true, false).await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_loop_starts_at_bootstrap_or_after_its_delay_whichever_comes_first() {
        let jobs = Arc::new(Jobs::new());
        let gate = |bootstrapped: Option<watch::Receiver<bool>>, delay: Duration| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let cancel = jobs.spawn("gate", move |ctx| async move {
                wait_to_start(&ctx, bootstrapped, delay).await;
                let _ = tx.send(());
                Ok(())
            });
            (cancel, rx)
        };
        let start = tokio::time::Instant::now();

        gate(None, PRUNE_START).1.await.unwrap();
        assert_eq!(start.elapsed(), Duration::ZERO, "no bootstrap to wait for");

        let (bootstrapped, rx) = watch::channel(false);
        let (_, started) = gate(Some(rx), PRUNE_START);
        tokio::time::sleep(Duration::from_secs(1)).await;
        bootstrapped.send(true).unwrap();
        started.await.unwrap();
        let at_bootstrap = start.elapsed();
        assert!(
            at_bootstrap >= Duration::from_secs(1) && at_bootstrap < PRUNE_START,
            "started at the bootstrap, after {at_bootstrap:?}"
        );

        let (_held, rx) = watch::channel(false);
        let (_, started) = gate(Some(rx), RETRY_FAILED_START);
        started.await.unwrap();
        let after_delay = start.elapsed() - at_bootstrap;
        assert!(
            after_delay >= RETRY_FAILED_START && after_delay < PRUNE_START,
            "a held bootstrap released the loop after {after_delay:?}"
        );

        let (_held, rx) = watch::channel(false);
        let (cancel, started) = gate(Some(rx), PRUNE_START);
        cancel.cancel();
        started.await.unwrap();
        assert!(
            start.elapsed() - at_bootstrap - after_delay < Duration::from_secs(1),
            "cancellation released the loop"
        );
    }
}
