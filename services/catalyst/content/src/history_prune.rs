//! The content history prune: a sync-only node drops the deployments no
//! pointer is served by any more (superseded by a newer entity over the
//! same pointers, or unpublished), their `content_files` rows and the blobs
//! nothing else names, once they have been out of `active_pointers` for the
//! grace period. `failed_deployments`, `processed_snapshots`, the cursors
//! and `local_entities` are never touched, and a row `local_entities` names
//! (this node's own deployment, tombstoned or not) is never pruned, nor is a
//! row such a deployment superseded.
//!
//! Peer sync does not need the history rows to stay correct. Its only
//! already-known probe is `sync_deployer::is_entity_deployed` (the bloom
//! filter in front of it is seeded from the same table), so a pruned entity
//! a peer offers again is fetched once more and lands as a superseded row:
//! the pointer succession in `sync_deployer::flush_batch` never moves a
//! pointer back to an older entity, and the next pass prunes the row again.
//! A peer's snapshots list only its active entities and the pointer-changes
//! feed resumes from the last durable cursor, so the grace period (hours)
//! covers what a restart can replay (minutes); the cost of a re-offer is a
//! download, never a wrong pointer.
//!
//! An authoritative node (`CONTENT_WRITES_ENABLED`) keeps its history, so
//! the job refuses to run beside the switch, and a pass with the switch off
//! still keeps every row a local deployment superseded.
//!
//! Superseded at: a row has no such column; it is the later of the row's own
//! `local_timestamp` (a late replay lands already superseded) and its
//! `deleter_deployment`'s (the newer entity landing is what superseded it),
//! falling back to the row's own when the deleter row is gone.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::counter;
use serde::Serialize;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::storage::{Storage, gc};

pub const JOB_NAME: &str = "content-history-prune";

/// Pause between two batches of one pass, so serving traffic and the sync
/// flushes get the pool back in between.
pub const DEFAULT_PAUSE_BETWEEN_BATCHES: Duration = Duration::from_millis(100);

/// Upper bound on the random delay before the first pass after a start.
const FIRST_PASS_JITTER: Duration = Duration::from_secs(600);

/// Ids one candidate select covers. A select bounded to a window walks at most
/// this many rows by primary key, so a table whose rows are all active costs a
/// short statement per window instead of one scan to the end of the table
/// (1.19 M rows took 11 s on the deployed host, past sqlx's 1 s slow-statement line).
const ID_WINDOW: i32 = 50_000;

/// Prometheus series over the `metrics` facade (the `content_` prefix),
/// rendered at 0 when the job starts.
pub const DEPLOYMENTS_PRUNED: &str = "content_history_prune_deployments_total";
pub const CONTENT_FILES_PRUNED: &str = "content_history_prune_content_files_total";
pub const BLOBS_UNLINKED: &str = "content_history_prune_blobs_total";
pub const BLOBS_KEPT: &str = "content_history_prune_blobs_kept_total";
pub const BLOB_FAILURES: &str = "content_history_prune_blob_failures_total";
pub const BYTES_FREED: &str = "content_history_prune_bytes_total";
pub const SERIES: [&str; 6] = [
    DEPLOYMENTS_PRUNED,
    CONTENT_FILES_PRUNED,
    BLOBS_UNLINKED,
    BLOBS_KEPT,
    BLOB_FAILURES,
    BYTES_FREED,
];

#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// `CONTENT_HISTORY_PRUNE_ENABLED`.
    pub enabled: bool,
    /// `CONTENT_WRITES_ENABLED`: an authoritative node keeps its history.
    pub content_writes_enabled: bool,
    /// How long a row stays after it left `active_pointers`
    /// (`CONTENT_HISTORY_PRUNE_GRACE_HOURS`).
    pub grace: Duration,
    /// Deployments deleted per transaction (`CONTENT_HISTORY_PRUNE_BATCH`);
    /// a full batch continues within its id window, a short one moves the
    /// pass to the next window.
    pub batch: usize,
    pub pause_between_batches: Duration,
    /// Pause between two passes (`CONTENT_HISTORY_PRUNE_INTERVAL_HOURS`).
    pub interval: Duration,
    /// `GLOBAL_SCENES_URN`: scenes whose files are never unlinked.
    pub global_scenes_urn: Vec<String>,
}

impl PruneOptions {
    pub fn from_config(config: &Config) -> Self {
        Self {
            enabled: config.content.content_history_prune_enabled,
            content_writes_enabled: config.writes.content_writes_enabled,
            grace: Duration::from_secs(
                config
                    .content
                    .content_history_prune_grace_hours
                    .saturating_mul(3600),
            ),
            batch: config.content.content_history_prune_batch.max(1),
            pause_between_batches: DEFAULT_PAUSE_BETWEEN_BATCHES,
            interval: Duration::from_secs(
                config
                    .content
                    .content_history_prune_interval_hours
                    .max(1)
                    .saturating_mul(3600),
            ),
            global_scenes_urn: config
                .worlds_content
                .global_scenes_urn
                .as_deref()
                .map(|r| r.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
        }
    }

    /// Why a pass would not run; `None` when it runs.
    pub fn skip_reason(&self) -> Option<&'static str> {
        if !self.enabled {
            Some("CONTENT_HISTORY_PRUNE_ENABLED=false")
        } else if self.content_writes_enabled {
            Some("CONTENT_WRITES_ENABLED=true: an authoritative node keeps its deployment history")
        } else {
            None
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    /// Why the pass did not run; `None` when it ran.
    pub skipped: Option<String>,
    /// Transactions that deleted at least one row.
    pub batches: u64,
    pub deployments: u64,
    pub content_files: u64,
    /// Distinct blobs unlinked from the store (content files and the
    /// pruned entities' own files).
    pub blobs: u64,
    /// Blobs of pruned rows kept because a row of some domain still names them.
    pub blobs_kept: u64,
    /// Unlink failures, logged; the collector (`storage::gc`) sees the file again.
    pub blob_failures: u64,
    /// Bytes the unlinked blobs held (0 for a blob whose size could not be read).
    pub bytes: u64,
    pub elapsed_ms: u64,
}

/// The candidates of one batch: not the active entity of any pointer, not
/// this node's own nor superseded by it, and out of `active_pointers` for longer than the grace;
/// keyset-paged on id inside one `ID_WINDOW`, rows a concurrent flush holds
/// are left for the next pass. `$1` last id seen, `$2` grace in seconds,
/// `$3` batch, `$4` the window's last id.
const SELECT_CANDIDATES_SQL: &str = "SELECT d.id, d.entity_id \
     FROM content.deployments d \
     LEFT JOIN content.deployments newer ON newer.id = d.deleter_deployment \
     WHERE d.id > $1 AND d.id <= $4 \
       AND NOT EXISTS (SELECT 1 FROM content.active_pointers ap WHERE ap.entity_id = d.entity_id) \
       AND NOT EXISTS (SELECT 1 FROM content.local_entities le WHERE le.entity_id = d.entity_id) \
       AND NOT EXISTS (SELECT 1 FROM content.local_entities le WHERE le.entity_id = newer.entity_id) \
       AND GREATEST(d.local_timestamp, COALESCE(newer.local_timestamp, d.local_timestamp)) \
           < timezone('UTC', now()) - ($2::bigint * interval '1 second') \
     ORDER BY d.id \
     LIMIT $3 \
     FOR UPDATE OF d SKIP LOCKED";

struct Batch {
    /// The largest id the select returned (the next batch starts after it).
    last_id: Option<i32>,
    found: usize,
    deployments: u64,
    content_files: u64,
    /// The pruned rows' content hashes and entity ids, distinct.
    keys: Vec<String>,
}

/// One transaction: the batch's `content_files` rows go (their hashes
/// collected), then the deployment rows.
async fn prune_batch(
    pool: &PgPool,
    after_id: i32,
    until_id: i32,
    grace_secs: i64,
    batch: usize,
) -> anyhow::Result<Batch> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(i32, String)> = sqlx::query_as(SELECT_CANDIDATES_SQL)
        .bind(after_id)
        .bind(grace_secs)
        .bind(i64::try_from(batch).unwrap_or(i64::MAX))
        .bind(until_id)
        .fetch_all(&mut *tx)
        .await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(Batch {
            last_id: None,
            found: 0,
            deployments: 0,
            content_files: 0,
            keys: Vec::new(),
        });
    }
    let ids: Vec<i32> = rows.iter().map(|(id, _)| *id).collect();
    let hashes: Vec<String> = sqlx::query_scalar(
        "DELETE FROM content.content_files WHERE deployment = ANY($1) RETURNING content_hash",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    let deleted = sqlx::query("DELETE FROM content.deployments WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    let content_files = hashes.len() as u64;
    let mut keys: Vec<String> = hashes;
    keys.extend(rows.iter().map(|(_, entity_id)| entity_id.clone()));
    keys.sort();
    keys.dedup();
    Ok(Batch {
        last_id: ids.iter().copied().max(),
        found: rows.len(),
        deployments: deleted,
        content_files,
        keys,
    })
}

/// Unlinks `key`, answering the bytes it held (0 when the size could not be
/// read; a missing file is not an error).
async fn unlink(storage: &Storage, key: &str) -> Result<u64, crate::storage::StorageError> {
    let size = storage
        .file_info(key)
        .await?
        .map(|info| info.size)
        .unwrap_or(0);
    storage.delete_strict(key).await?;
    Ok(size)
}

/// The keys of a pruned batch that no domain names any more are unlinked;
/// the reference check runs right before the unlink, after the rows are gone.
async fn unlink_unreferenced(
    pool: &PgPool,
    storage: &Storage,
    global_scenes_urn: &[String],
    keys: &[String],
    report: &mut PruneReport,
) -> anyhow::Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut lock = gc::lock_store_exclusive(pool).await?;
    let referenced: HashSet<String> =
        gc::referenced_among_on(&mut lock, gc::Pins::new(storage, global_scenes_urn), keys).await?;
    for key in keys {
        if referenced.contains(key) {
            report.blobs_kept += 1;
            continue;
        }
        match unlink(storage, key).await {
            Ok(bytes) => {
                report.blobs += 1;
                report.bytes += bytes;
            }
            Err(e) => {
                report.blob_failures += 1;
                tracing::warn!(key = %key, error = %e, "content history prune: could not unlink a blob");
            }
        }
    }
    lock.commit().await?;
    Ok(())
}

/// One pass: id windows of `ID_WINDOW` from 0 to the table's largest id at
/// the start, each select bounded to its window; a full batch continues
/// inside the window, a short one moves on. The token fires and the report
/// covers what ran. `Ok` with `skipped` set when the options refuse the run.
async fn run(
    pool: &PgPool,
    storage: &Storage,
    opts: &PruneOptions,
    cancel: Option<&CancellationToken>,
) -> anyhow::Result<PruneReport> {
    let started = Instant::now();
    let mut report = PruneReport::default();
    if let Some(reason) = opts.skip_reason() {
        report.skipped = Some(reason.to_string());
        return Ok(report);
    }
    let batch = opts.batch.max(1);
    let grace_secs = i64::try_from(opts.grace.as_secs()).unwrap_or(i64::MAX);
    let max_id: i32 = sqlx::query_scalar("SELECT COALESCE(max(id), 0) FROM content.deployments")
        .fetch_one(pool)
        .await?;
    let mut after_id = 0i32;
    while after_id < max_id {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            tracing::info!(
                after_id,
                "content history prune: pass interrupted by shutdown"
            );
            break;
        }
        let until_id = after_id.saturating_add(ID_WINDOW).min(max_id);
        let pruned = prune_batch(pool, after_id, until_id, grace_secs, batch).await?;
        if pruned.found == 0 {
            after_id = until_id;
            tokio::task::yield_now().await;
            continue;
        }
        let blobs_before = report.blobs;
        report.batches += 1;
        report.deployments += pruned.deployments;
        report.content_files += pruned.content_files;
        unlink_unreferenced(
            pool,
            storage,
            &opts.global_scenes_urn,
            &pruned.keys,
            &mut report,
        )
        .await?;
        counter!(DEPLOYMENTS_PRUNED).increment(pruned.deployments);
        counter!(CONTENT_FILES_PRUNED).increment(pruned.content_files);
        tracing::debug!(
            after_id,
            found = pruned.found,
            deployments = pruned.deployments,
            content_files = pruned.content_files,
            blobs = report.blobs - blobs_before,
            "content history prune: batch"
        );
        // A full batch may leave more in the window: continue after its last id.
        after_id = if pruned.found < batch {
            until_id
        } else {
            pruned.last_id.unwrap_or(until_id)
        };
        tokio::task::yield_now().await;
        if !opts.pause_between_batches.is_zero() {
            tokio::time::sleep(opts.pause_between_batches).await;
        }
    }
    counter!(BLOBS_UNLINKED).increment(report.blobs);
    counter!(BLOBS_KEPT).increment(report.blobs_kept);
    counter!(BLOB_FAILURES).increment(report.blob_failures);
    counter!(BYTES_FREED).increment(report.bytes);
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    tracing::info!(
        deployments = report.deployments,
        content_files = report.content_files,
        blobs = report.blobs,
        blobs_kept = report.blobs_kept,
        blob_failures = report.blob_failures,
        bytes = report.bytes,
        batches = report.batches,
        elapsed_ms = report.elapsed_ms,
        grace_hours = opts.grace.as_secs() / 3600,
        "content history prune: pass finished"
    );
    Ok(report)
}

/// One pass now (the console, the tests); the job calls the same with its token.
pub async fn run_once(
    pool: &PgPool,
    storage: &Storage,
    opts: &PruneOptions,
) -> anyhow::Result<PruneReport> {
    run(pool, storage, opts, None).await
}

/// Spawns the periodic job, or logs once why it does not run (the flag off,
/// an authoritative node) and spawns nothing. Answers whether it runs.
pub fn spawn(jobs: &Jobs, pool: PgPool, storage: Arc<Storage>, opts: PruneOptions) -> bool {
    if let Some(reason) = opts.skip_reason() {
        tracing::info!(reason, "content history prune: not running");
        return false;
    }
    for name in SERIES {
        counter!(name).increment(0);
    }
    tracing::info!(
        grace_hours = opts.grace.as_secs() / 3600,
        interval_hours = opts.interval.as_secs() / 3600,
        batch = opts.batch,
        "content history prune: enabled; superseded deployments and their blobs are dropped after the grace"
    );
    let cfg = PeriodicCfg::every(opts.interval).with_jitter(FIRST_PASS_JITTER);
    jobs.spawn_periodic_leased(JOB_NAME, cfg, move |ctx| {
        let (pool, storage, opts) = (pool.clone(), storage.clone(), opts.clone());
        async move {
            let report = run(&pool, &storage, &opts, Some(&ctx.cancel)).await?;
            ctx.progress(format!(
                "pruned {} deployments, {} content files, {} blobs ({} bytes) in {} ms",
                report.deployments,
                report.content_files,
                report.blobs,
                report.bytes,
                report.elapsed_ms
            ));
            Ok(())
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    /// A pass prunes the whole table, so the per-process scratch database the
    /// other lib tests share is off limits: a database of its own on the
    /// `CATALYST_TEST_DATABASE_URL` cluster, as the collector test does.
    /// `None` (skip) without the variable.
    async fn own_database() -> Option<(PgPool, PgPool, String)> {
        let base_url = std::env::var("CATALYST_TEST_DATABASE_URL").ok()?;
        let opts: PgConnectOptions = base_url.parse().unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.database("postgres"))
            .await
            .unwrap();
        let pool = crate::db::scratch::isolated_pool(2).await?;
        let name = pool.connect_options().get_database().unwrap().to_string();
        Some((pool, admin, name))
    }

    fn options(grace: Duration, batch: usize) -> PruneOptions {
        PruneOptions {
            enabled: true,
            content_writes_enabled: false,
            grace,
            batch,
            pause_between_batches: Duration::ZERO,
            interval: Duration::from_secs(3600),
            global_scenes_urn: Vec::new(),
        }
    }

    /// A stored blob, keyed by its own CID.
    async fn put(storage: &Storage, bytes: &[u8]) -> String {
        let key = crate::auth::hash::hash_bytes_v1(bytes);
        storage
            .store(&key, Bytes::from(bytes.to_vec()))
            .await
            .unwrap();
        key
    }

    /// A deployment row landed `age_secs` ago (entity and local timestamps).
    async fn deployment(
        pool: &PgPool,
        entity_id: &str,
        pointer: &str,
        age_secs: i64,
        deleter: Option<i32>,
    ) -> i32 {
        sqlx::query_scalar(
            "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, \
                 entity_timestamp, entity_pointers, local_timestamp, auth_chain, deleter_deployment) \
             VALUES ('0xAbC', 'v3', 'scene', $1, '{}'::json, \
                 timezone('UTC', now()) - ($3::bigint * interval '1 second'), ARRAY[$2::text], \
                 timezone('UTC', now()) - ($3::bigint * interval '1 second'), '[]', $4) \
             RETURNING id",
        )
        .bind(entity_id)
        .bind(pointer)
        .bind(age_secs)
        .bind(deleter)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn content_file(pool: &PgPool, deployment: i32, hash: &str, key: &str) {
        sqlx::query(
            "INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, $3)",
        )
        .bind(deployment)
        .bind(hash)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn activate(pool: &PgPool, pointer: &str, entity_id: &str) {
        sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'scene')")
            .bind(pointer)
            .bind(entity_id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn entity_exists(pool: &PgPool, entity_id: &str) -> bool {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM content.deployments WHERE entity_id = $1)")
            .bind(entity_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn files_of(pool: &PgPool, deployment: i32) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM content.content_files WHERE deployment = $1")
            .bind(deployment)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// One pointer with a superseded row and its active successor, an
    /// unrelated active row sharing a blob with the superseded one, this
    /// node's own superseded row, a row superseded too recently, a row whose
    /// superseder is too recent, and an orphan no pointer names: the pass
    /// drops the superseded and the orphan with their `content_files` and
    /// exclusive blobs (their entity files too), keeps the shared blob, the
    /// active rows, the local row and the young ones; the switch and the
    /// flag refuse a pass; a second pass finds nothing.
    #[tokio::test]
    async fn superseded_rows_go_with_their_exclusive_blobs_and_everything_active_local_or_young_stays()
     {
        let Some((pool, admin, name)) = own_database().await else {
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "catalyst-history-prune-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let storage = Storage::from_config(&dir).await.unwrap();
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (p, q) = (format!("{tag}:0,0"), format!("{tag}:1,0"));
        let (two_days, one_hour) = (172_800, 3_600);

        let shared = put(&storage, format!("shared {tag}").as_bytes()).await;
        let exclusive = put(&storage, format!("exclusive {tag}").as_bytes()).await;
        let active_file = put(&storage, format!("active {tag}").as_bytes()).await;
        let local_file = put(&storage, format!("local {tag}").as_bytes()).await;
        let young_file = put(&storage, format!("young {tag}").as_bytes()).await;
        let late_file = put(&storage, format!("late {tag}").as_bytes()).await;
        let orphan_file = put(&storage, format!("orphan {tag}").as_bytes()).await;
        let active_entity = put(&storage, format!("entity active {tag}").as_bytes()).await;
        let superseded_entity = put(&storage, format!("entity superseded {tag}").as_bytes()).await;
        let orphan_entity = put(&storage, format!("entity orphan {tag}").as_bytes()).await;
        let unrelated_entity =
            crate::auth::hash::hash_bytes_v1(format!("entity unrelated {tag}").as_bytes());
        let local_entity =
            crate::auth::hash::hash_bytes_v1(format!("entity local {tag}").as_bytes());
        let young_entity =
            crate::auth::hash::hash_bytes_v1(format!("entity young {tag}").as_bytes());
        let late_entity = crate::auth::hash::hash_bytes_v1(format!("entity late {tag}").as_bytes());

        let active = deployment(&pool, &active_entity, &p, two_days, None).await;
        activate(&pool, &p, &active_entity).await;
        content_file(&pool, active, &active_file, "a.glb").await;
        let superseded = deployment(&pool, &superseded_entity, &p, two_days, Some(active)).await;
        content_file(&pool, superseded, &shared, "shared.glb").await;
        content_file(&pool, superseded, &exclusive, "exclusive.glb").await;
        let unrelated = deployment(&pool, &unrelated_entity, &q, two_days, None).await;
        activate(&pool, &q, &unrelated_entity).await;
        content_file(&pool, unrelated, &shared, "shared.glb").await;
        let local = deployment(&pool, &local_entity, &p, two_days, Some(active)).await;
        content_file(&pool, local, &local_file, "l.glb").await;
        sqlx::query("INSERT INTO content.local_entities (entity_id, signer, tombstoned_at) VALUES ($1, '0xAbC', timezone('UTC', now()))")
            .bind(&local_entity)
            .execute(&pool)
            .await
            .unwrap();
        let young = deployment(&pool, &young_entity, &p, one_hour, Some(active)).await;
        content_file(&pool, young, &young_file, "y.glb").await;
        // Landed two days ago, superseded an hour ago (by `young`): its
        // superseded-at is the deleter's landing.
        let late = deployment(&pool, &late_entity, &p, two_days, Some(young)).await;
        content_file(&pool, late, &late_file, "late.glb").await;
        let orphan = deployment(&pool, &orphan_entity, &q, two_days, None).await;
        content_file(&pool, orphan, &orphan_file, "o.glb").await;
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM content.deployments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 7);

        let grace = Duration::from_secs(86_400);
        let refused = run_once(
            &pool,
            &storage,
            &PruneOptions {
                content_writes_enabled: true,
                ..options(grace, 1)
            },
        )
        .await
        .unwrap();
        assert!(
            refused
                .skipped
                .as_deref()
                .unwrap()
                .starts_with("CONTENT_WRITES_ENABLED=true"),
            "{refused:?}"
        );
        let off = run_once(
            &pool,
            &storage,
            &PruneOptions {
                enabled: false,
                ..options(grace, 1)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            off.skipped.as_deref(),
            Some("CONTENT_HISTORY_PRUNE_ENABLED=false")
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM content.deployments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 7, "a refused pass deletes nothing");

        // Batches of one: the keyset walks past the first pruned row to the second.
        let report = run_once(&pool, &storage, &options(grace, 1)).await.unwrap();
        assert_eq!(report.skipped, None);
        assert_eq!(
            (report.batches, report.deployments, report.content_files),
            (2, 2, 3),
            "{report:?}"
        );
        assert_eq!(
            (report.blobs, report.blobs_kept, report.blob_failures),
            (4, 1, 0),
            "{report:?}"
        );
        let freed = [&exclusive, &orphan_file, &superseded_entity, &orphan_entity];
        let expected_bytes: u64 = [
            format!("exclusive {tag}").len(),
            format!("orphan {tag}").len(),
            format!("entity superseded {tag}").len(),
            format!("entity orphan {tag}").len(),
        ]
        .iter()
        .map(|n| *n as u64)
        .sum();
        assert_eq!(report.bytes, expected_bytes);

        assert!(
            !entity_exists(&pool, &superseded_entity).await,
            "the superseded row is gone"
        );
        assert!(
            !entity_exists(&pool, &orphan_entity).await,
            "the orphan row is gone"
        );
        assert_eq!(
            (
                files_of(&pool, superseded).await,
                files_of(&pool, orphan).await
            ),
            (0, 0),
            "their content_files rows are gone"
        );
        for (entity_id, id, why) in [
            (&active_entity, active, "the active entity stays"),
            (
                &unrelated_entity,
                unrelated,
                "the unrelated active entity stays",
            ),
            (
                &local_entity,
                local,
                "this node's own deployment is never pruned",
            ),
            (
                &young_entity,
                young,
                "a row superseded within the grace stays",
            ),
            (
                &late_entity,
                late,
                "a row whose superseder landed within the grace stays",
            ),
        ] {
            assert!(entity_exists(&pool, entity_id).await, "{why}");
            assert_eq!(
                files_of(&pool, id).await,
                1,
                "{why}: its content_files row stays"
            );
        }
        for key in freed {
            assert!(!storage.exist(key).await.unwrap(), "{key} was unlinked");
        }
        for key in [
            &shared,
            &active_file,
            &local_file,
            &young_file,
            &late_file,
            &active_entity,
        ] {
            assert!(storage.exist(key).await.unwrap(), "{key} stays");
        }
        let pointers: i64 = sqlx::query_scalar("SELECT count(*) FROM content.active_pointers")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(pointers, 2, "active_pointers is not touched");

        let again = run_once(&pool, &storage, &options(grace, 1)).await.unwrap();
        assert_eq!(
            (
                again.batches,
                again.deployments,
                again.content_files,
                again.blobs
            ),
            (0, 0, 0, 0),
            "{again:?}"
        );

        pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pass that runs while writes are off (a node that had them on and
    /// turns them on again) keeps the scene a local deployment superseded,
    /// however old; the local scene's unpublish leaves it superseded.
    #[tokio::test]
    async fn the_predecessor_of_a_local_deployment_is_kept() {
        let Some((pool, admin, name)) = own_database().await else {
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "catalyst-history-prune-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let storage = Storage::from_config(&dir).await.unwrap();
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let p = format!("{tag}:0,0");
        let (local_entity, peer_entity) = (format!("bafylocal{tag}"), format!("bafypeer{tag}"));
        let local = deployment(&pool, &local_entity, &p, 172_800, None).await;
        activate(&pool, &p, &local_entity).await;
        sqlx::query("INSERT INTO content.local_entities (entity_id, signer) VALUES ($1, '0xabc')")
            .bind(&local_entity)
            .execute(&pool)
            .await
            .unwrap();
        deployment(&pool, &peer_entity, &p, 172_900, Some(local)).await;

        let report = run_once(&pool, &storage, &options(Duration::from_secs(86_400), 10))
            .await
            .unwrap();
        assert_eq!(
            (report.skipped.as_deref(), report.deployments),
            (None, 0),
            "{report:?}"
        );
        assert!(entity_exists(&pool, &peer_entity).await);
        let out = crate::land_publish::unpublish_local_scene(&pool, &p, &local_entity)
            .await
            .unwrap();
        assert_eq!(out.parcels, vec![p.clone()]);
        let active: Option<String> =
            sqlx::query_scalar("SELECT entity_id FROM content.active_pointers WHERE pointer = $1")
                .bind(&p)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(active, None, "the predecessor is not restored");

        pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
