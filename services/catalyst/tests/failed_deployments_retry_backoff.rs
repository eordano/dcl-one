// from: catalyrst/crates/catalyrst-server/tests/failed_deployments_retry_backoff.rs
//! Retry-backoff state on content.failed_deployments: the columns only ever move
//! forward, and the give-up DELETE only takes rows that are still at the cap. Both
//! properties live in SQL, so they are asserted against a real postgres
//! (CATALYST_TEST_DATABASE_URL; one scratch database per test).

use catalyst::content::failed_deployments_store::LiveFailedDeploymentsStore;
use catalyst::content::failed_deployments_store::prune_failed_deployments;
use catalyst::content::sync::{AuthChain, AuthLink, AuthLinkType};
use catalyst::content::sync::{FailedDeployment, FailureReason};

#[path = "support/db.rs"]
mod db;

fn auth_chain() -> AuthChain {
    vec![AuthLink {
        link_type: AuthLinkType::SIGNER,
        payload: "0x0000000000000000000000000000000000000001".to_string(),
        signature: None,
    }]
}

const FIRST_FAILURE_MS: i64 = 1_700_000_000_000;

fn failure(entity_id: &str, retry_count: u32, next_retry_at: i64) -> FailedDeployment {
    FailedDeployment {
        entity_type: "scene".to_string(),
        entity_id: entity_id.to_string(),
        reason: FailureReason::DeploymentError,
        auth_chain: auth_chain(),
        error_description: "boom".to_string(),
        failure_timestamp: FIRST_FAILURE_MS,
        snapshot_hash: None,
        retry_count,
        next_retry_at,
    }
}

async fn stored(store: &LiveFailedDeploymentsStore, entity_id: &str) -> FailedDeployment {
    store
        .get_all_failed()
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.entity_id == entity_id)
        .unwrap_or_else(|| panic!("{entity_id} is not in failed_deployments"))
}

#[tokio::test]
async fn a_fresh_failure_is_due_immediately() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    store
        .report_failure(failure("bafyfresh", 0, 0))
        .await
        .unwrap();

    let row = stored(&store, "bafyfresh").await;
    assert_eq!(row.retry_count, 0);
    assert_eq!(row.next_retry_at, 0);
    assert_eq!(row.failure_timestamp, FIRST_FAILURE_MS);

    db.teardown().await;
}

#[tokio::test]
async fn a_plain_re_report_cannot_rewind_the_backoff() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());
    let deadline = 1_800_000_000_000i64;

    store
        .report_failure(failure("bafyback", 0, 0))
        .await
        .unwrap();
    store
        .report_failure(failure("bafyback", 3, deadline))
        .await
        .unwrap();

    let advanced = stored(&store, "bafyback").await;
    assert_eq!(advanced.retry_count, 3);
    assert_eq!(advanced.next_retry_at, deadline);
    assert_eq!(advanced.failure_timestamp, FIRST_FAILURE_MS);

    store
        .report_failure(failure("bafyback", 0, 0))
        .await
        .unwrap();

    let after_sync_report = stored(&store, "bafyback").await;
    assert_eq!(after_sync_report.retry_count, 3);
    assert_eq!(after_sync_report.next_retry_at, deadline);

    db.teardown().await;
}

#[tokio::test]
async fn a_retry_re_report_keeps_the_original_failure_time() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());
    let deadline = 1_800_000_000_000i64;

    store
        .report_failure(failure("bafyfirst", 0, 0))
        .await
        .unwrap();

    let loaded = stored(&store, "bafyfirst").await;
    let deferred = FailedDeployment {
        error_description: "still broken".to_string(),
        retry_count: loaded.retry_count + 1,
        next_retry_at: deadline,
        ..loaded
    };
    store.report_failure(deferred).await.unwrap();

    let after = stored(&store, "bafyfirst").await;
    assert_eq!(after.failure_timestamp, FIRST_FAILURE_MS);
    assert_eq!(after.retry_count, 1);
    assert_eq!(after.next_retry_at, deadline);
    assert_eq!(after.error_description, "still broken");

    db.teardown().await;
}

#[tokio::test]
async fn eviction_only_takes_rows_still_at_the_cap() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    store
        .report_failure(failure("bafyexhausted", 10, 1))
        .await
        .unwrap();
    store
        .report_failure(failure("bafyrefailed", 1, 1))
        .await
        .unwrap();

    let removed = store
        .remove_exhausted(
            &[
                "bafyexhausted".to_string(),
                "bafyrefailed".to_string(),
                "bafyabsent".to_string(),
            ],
            10,
        )
        .await
        .unwrap();

    assert_eq!(removed, vec!["bafyexhausted".to_string()]);
    let left: Vec<String> = store
        .get_all_failed()
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.entity_id)
        .collect();
    assert_eq!(left, vec!["bafyrefailed".to_string()]);

    db.teardown().await;
}

#[tokio::test]
async fn eviction_of_nothing_touches_nothing() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    store
        .report_failure(failure("bafykeep", 10, 1))
        .await
        .unwrap();

    assert!(store.remove_exhausted(&[], 10).await.unwrap().is_empty());
    assert_eq!(store.get_all_failed().await.unwrap().len(), 1);

    db.teardown().await;
}

/// The deployer is asynchronous: a retry that reaches the batch buffer returns Ok while the
/// entity is still only queued, and its row stays until the flush that writes it. When that
/// flush fails, its re-report has to carry the attempt the retry already spent, or the entity
/// never accrues backoff.
#[tokio::test]
async fn a_flush_failure_after_a_retry_keeps_the_spent_attempts() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());
    let deadline = 1_800_000_000_000i64;

    store
        .report_failure(failure("bafyqueued", 3, 1))
        .await
        .unwrap();

    let loaded = stored(&store, "bafyqueued").await;
    let stamped = (loaded.retry_count + 1, deadline);

    store
        .report_failure(FailedDeployment {
            error_description: "batch flush failed: boom".to_string(),
            retry_count: stamped.0,
            next_retry_at: stamped.1,
            ..failure("bafyqueued", 0, 0)
        })
        .await
        .unwrap();

    let after = stored(&store, "bafyqueued").await;
    assert_eq!(after.retry_count, 4);
    assert_eq!(after.next_retry_at, deadline);

    db.teardown().await;
}

#[tokio::test]
async fn a_no_entity_reason_survives_a_retry_re_report() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    store
        .report_failure(FailedDeployment {
            reason: FailureReason::NoEntity,
            ..failure("bafynoentity", 0, 0)
        })
        .await
        .unwrap();

    let loaded = stored(&store, "bafynoentity").await;
    assert_eq!(loaded.reason, FailureReason::NoEntity);

    store
        .report_failure(FailedDeployment {
            error_description: "still missing".to_string(),
            retry_count: loaded.retry_count + 1,
            ..loaded
        })
        .await
        .unwrap();

    let (column,): (String,) =
        sqlx::query_as("SELECT reason FROM content.failed_deployments WHERE entity_id = $1")
            .bind("bafynoentity")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(column, "No entity");
    assert_eq!(
        stored(&store, "bafynoentity").await.reason,
        FailureReason::NoEntity
    );

    db.teardown().await;
}

/// The daily prune (RETRY_FAILED_PRUNE_TTL_DAYS) takes records by their FIRST failure time.
#[tokio::test]
async fn the_prune_takes_only_records_older_than_the_ttl() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());
    let now = chrono::Utc::now().timestamp_millis();
    for (id, failed_at) in [("bafyold", now - 8 * 86_400_000), ("bafyfresh", now)] {
        store
            .report_failure(FailedDeployment {
                failure_timestamp: failed_at,
                ..failure(id, 0, 0)
            })
            .await
            .unwrap();
    }

    assert_eq!(prune_failed_deployments(&db.pool, 7).await.unwrap(), 1);

    let left: Vec<String> = store
        .get_all_failed()
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.entity_id)
        .collect();
    assert_eq!(left, vec!["bafyfresh".to_string()]);

    db.teardown().await;
}

// ── the bounded due query (DATABASE.md follow-up: retries) ─────────────────

use catalyst::content::failed_deployments_store::DueCursor;

const NOW_MS: i64 = 1_800_000_000_000;
const CAP: u32 = 10;

async fn due_ids(
    store: &LiveFailedDeploymentsStore,
    after: Option<&DueCursor>,
    limit: usize,
) -> (Vec<String>, Option<DueCursor>) {
    let batch = store.select_due(NOW_MS, CAP, after, limit).await.unwrap();
    (
        batch.failures.into_iter().map(|f| f.entity_id).collect(),
        batch.cursor,
    )
}

/// The due order is oldest deadline first, entity id as the tie-break; waiting rows, rows at the
/// cap and rows without an auth chain are not part of it.
#[tokio::test]
async fn the_due_page_is_ordered_by_deadline_then_id_and_holds_only_actionable_rows() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    for (id, retry_count, next_retry_at) in [
        ("bafyb-late", 2, NOW_MS - 1_000),
        ("bafya-late", 2, NOW_MS - 1_000),
        ("bafyc-early", 5, NOW_MS - 60_000),
        ("bafyd-now", 0, NOW_MS),
        ("bafye-waiting", 1, NOW_MS + 1),
        ("bafyf-exhausted", CAP, 0),
        ("bafyg-over", CAP + 3, 0),
    ] {
        store
            .report_failure(failure(id, retry_count, next_retry_at))
            .await
            .unwrap();
    }
    store
        .report_failure(FailedDeployment {
            auth_chain: Vec::new(),
            ..failure("bafyh-noauth", 0, 0)
        })
        .await
        .unwrap();

    let (ids, cursor) = due_ids(&store, None, 100).await;
    assert_eq!(
        ids,
        vec!["bafyc-early", "bafya-late", "bafyb-late", "bafyd-now"]
    );
    assert_eq!(
        cursor.as_ref().map(|c| c.entity_id.as_str()),
        Some("bafyd-now")
    );
    assert_eq!(store.count_waiting(NOW_MS).await.unwrap(), 1);

    let (none, no_cursor) = due_ids(&store, cursor.as_ref(), 100).await;
    assert!(none.is_empty());
    assert!(no_cursor.is_none());

    db.teardown().await;
}

/// Paging through one cycle's due set with the cursor visits every due row exactly once, across
/// rows that share a deadline, without ever re-reading a row (the cursor carries the exact
/// timestamp, not its millisecond projection).
#[tokio::test]
async fn keyset_pages_cover_the_due_set_once_including_equal_deadlines() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    let mut expected = Vec::new();
    for i in 0..23 {
        let id = format!("bafy{i:02}");
        // seven deadlines, several rows on each, one of them not on a millisecond boundary
        let deadline = NOW_MS - 10_000 + (i % 7) * 1_000 + if i % 7 == 3 { 1 } else { 0 };
        store
            .report_failure(failure(&id, (i % 4) as u32, deadline))
            .await
            .unwrap();
        expected.push((deadline, id));
    }
    expected.sort();
    let expected: Vec<String> = expected.into_iter().map(|(_, id)| id).collect();

    let mut seen = Vec::new();
    let mut cursor: Option<DueCursor> = None;
    let mut pages = 0;
    loop {
        let (ids, next) = due_ids(&store, cursor.as_ref(), 5).await;
        pages += 1;
        assert!(pages <= 6, "the cursor must advance: {seen:?} then {ids:?}");
        let short = ids.len() < 5;
        seen.extend(ids);
        cursor = next;
        if short {
            break;
        }
    }
    assert_eq!(seen, expected);
    assert_eq!(pages, 5);

    db.teardown().await;
}

/// A row at the cap is shed once it is due; one still inside its last backoff waits for its
/// deadline. Rows under the cap and rows without an auth chain are never evicted here.
#[tokio::test]
async fn the_give_up_pass_takes_due_rows_at_the_cap_only() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    for (id, retry_count, next_retry_at) in [
        ("bafyexhausted", CAP, NOW_MS - 1),
        ("bafyover", CAP + 2, NOW_MS),
        ("bafyexhausted-later", CAP, NOW_MS + 1),
        ("bafyunder", CAP - 1, 0),
    ] {
        store
            .report_failure(failure(id, retry_count, next_retry_at))
            .await
            .unwrap();
    }
    store
        .report_failure(FailedDeployment {
            auth_chain: Vec::new(),
            ..failure("bafynoauth", CAP, 0)
        })
        .await
        .unwrap();

    let evicted = store.remove_exhausted_due(NOW_MS, CAP, 1000).await.unwrap();
    let mut evicted_ids: Vec<(String, u32)> = evicted
        .into_iter()
        .map(|r| (r.entity_id, r.retry_count))
        .collect();
    evicted_ids.sort();
    assert_eq!(
        evicted_ids,
        vec![
            ("bafyexhausted".to_string(), CAP),
            ("bafyover".to_string(), CAP + 2)
        ]
    );

    let mut left: Vec<String> = store
        .get_all_failed()
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.entity_id)
        .collect();
    left.sort();
    assert_eq!(left, vec!["bafyexhausted-later", "bafynoauth", "bafyunder"]);

    // the limit bounds one statement; the worker loops until a short answer
    store
        .report_failure(failure("bafyx1", CAP, 0))
        .await
        .unwrap();
    store
        .report_failure(failure("bafyx2", CAP, 0))
        .await
        .unwrap();
    assert_eq!(
        store
            .remove_exhausted_due(NOW_MS, CAP, 1)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .remove_exhausted_due(NOW_MS, CAP, 1)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .remove_exhausted_due(NOW_MS, CAP, 1)
            .await
            .unwrap()
            .is_empty()
    );

    db.teardown().await;
}

/// Two workers read the same due page. One re-reports a failure (attempt spent, deadline moved
/// past now); the other's attempt "succeeded", which only buffers the entity and leaves the row
/// to the flush that writes it. The fresher failure stands, and the row is no longer due for
/// this cycle's now.
#[tokio::test]
async fn two_workers_over_one_due_page_keep_the_fresher_failure_and_its_backoff() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());
    let peer = LiveFailedDeploymentsStore::new(db.pool.clone());

    store
        .report_failure(failure("bafyraced", 3, NOW_MS - 5_000))
        .await
        .unwrap();

    let (mine, _) = due_ids(&store, None, 10).await;
    let (theirs, _) = due_ids(&peer, None, 10).await;
    assert_eq!(mine, theirs);
    assert_eq!(mine, vec!["bafyraced"]);
    let read = stored(&store, "bafyraced").await;

    let deadline = NOW_MS + 900_000;
    peer.report_failure(FailedDeployment {
        error_description: "peer worker: still broken".to_string(),
        retry_count: read.retry_count + 1,
        next_retry_at: deadline,
        ..read.clone()
    })
    .await
    .unwrap();

    let survivor = stored(&store, "bafyraced").await;
    assert_eq!(survivor.retry_count, 4);
    assert_eq!(survivor.next_retry_at, deadline);
    assert_eq!(survivor.error_description, "peer worker: still broken");

    let (due_now, _) = due_ids(&store, None, 10).await;
    assert!(due_now.is_empty());
    assert_eq!(store.count_waiting(NOW_MS).await.unwrap(), 1);
    let (due_later, _) = {
        let batch = store.select_due(deadline, CAP, None, 10).await.unwrap();
        (
            batch
                .failures
                .into_iter()
                .map(|f| f.entity_id)
                .collect::<Vec<_>>(),
            batch.cursor,
        )
    };
    assert_eq!(due_later, vec!["bafyraced"]);

    db.teardown().await;
}

/// A fresh failure is recorded at the epoch, so after a restart (or any cycle) it sorts ahead of
/// every row with a real deadline and is attempted first; a cap of zero makes it exhausted
/// instead, which the give-up pass takes on the first look.
#[tokio::test]
async fn fresh_failures_lead_the_due_order_and_a_cap_of_zero_sheds_them() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let store = LiveFailedDeploymentsStore::new(db.pool.clone());

    store
        .report_failure(failure("bafyold", 4, NOW_MS - 3_600_000))
        .await
        .unwrap();
    store
        .report_failure(failure("bafyfresh", 0, 0))
        .await
        .unwrap();

    let (ids, _) = due_ids(&store, None, 10).await;
    assert_eq!(ids, vec!["bafyfresh", "bafyold"]);

    let evicted = store.remove_exhausted_due(NOW_MS, 0, 1000).await.unwrap();
    let mut ids: Vec<String> = evicted.into_iter().map(|r| r.entity_id).collect();
    ids.sort();
    assert_eq!(ids, vec!["bafyfresh", "bafyold"]);
    assert!(
        store
            .select_due(NOW_MS, 0, None, 10)
            .await
            .unwrap()
            .failures
            .is_empty()
    );

    db.teardown().await;
}

/// Cycle cost over a populated backlog. Not a contract: run by hand against a database holding
/// the 50,000-row synthetic backlog of metrics/retries-bounded-due.md, with
/// CATALYST_RETRY_BENCH_DATABASE_URL pointing at it. Prints per-phase p50/p95/p99 over 20 cycles
/// for the old cycle shape (load every row, classify in Rust) and the new one (give-up pass,
/// waiting count, due pages), plus the process RSS high-water mark after each.
#[tokio::test]
#[ignore]
async fn bench_retry_cycle_shapes_over_a_populated_backlog() {
    use catalyst::content::sync::retry_failed::{RetryFailedConfig, RetryFailedDeployments};
    use catalyst::content::sync_deployer::LiveSyncDeployer;
    use catalyst::storage::Storage;
    use std::sync::Arc;
    use std::time::Instant;

    let Ok(url) = std::env::var("CATALYST_RETRY_BENCH_DATABASE_URL") else {
        eprintln!("CATALYST_RETRY_BENCH_DATABASE_URL unset; skipping");
        return;
    };
    let shape = std::env::var("CATALYST_RETRY_BENCH_SHAPE").unwrap_or_else(|_| "new".to_string());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap();
    let store = Arc::new(LiveFailedDeploymentsStore::new(pool.clone()));

    fn rss() -> (u64, u64) {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let field = |key: &str| {
            status
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        };
        (field("VmRSS:"), field("VmHWM:"))
    }
    fn percentiles(mut samples: Vec<f64>) -> (f64, f64, f64) {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |q: f64| {
            samples[((samples.len() as f64 * q).ceil() as usize).clamp(1, samples.len()) - 1]
        };
        (at(0.50), at(0.95), at(0.99))
    }

    let (rss_before, hwm_before) = rss();
    let runs = 20;
    let mut samples = Vec::with_capacity(runs);
    let mut summary = String::new();
    let max_retries = 10;
    match shape.as_str() {
        "old" => {
            // the pre-change cycle without peers: load every record, classify, evict at the cap
            for _ in 0..runs {
                let started = Instant::now();
                let failed = store.get_all_failed().await.unwrap();
                let now_ms = chrono::Utc::now().timestamp_millis();
                let (mut due, mut waiting, mut exhausted) = (0u64, 0u64, Vec::new());
                for f in &failed {
                    if f.auth_chain.is_empty() {
                        continue;
                    }
                    if f.retry_count >= max_retries {
                        exhausted.push(f.entity_id.clone());
                    } else if f.next_retry_at > now_ms {
                        waiting += 1;
                    } else {
                        due += 1;
                    }
                }
                // eviction is rolled back so every run sees the same backlog
                let mut tx = pool.begin().await.unwrap();
                let removed: Vec<(String,)> = sqlx::query_as(
                    "DELETE FROM content.failed_deployments WHERE entity_id = ANY($1::text[]) AND retry_count >= $2 RETURNING entity_id",
                )
                .bind(&exhausted)
                .bind(max_retries as i32)
                .fetch_all(&mut *tx)
                .await
                .unwrap();
                tx.rollback().await.unwrap();
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                summary = format!(
                    "rows={} due={due} waiting={waiting} exhausted={} evicted={}",
                    failed.len(),
                    exhausted.len(),
                    removed.len()
                );
            }
        }
        _ => {
            // the new cycle without peers, through the worker itself: give-up pass (committed on
            // the first run, a no-op afterwards), waiting count, first due page, peer guard
            let storage_dir =
                std::env::temp_dir().join(format!("catalyst-retry-bench-{}", std::process::id()));
            let storage = Arc::new(Storage::from_config(&storage_dir).await.unwrap());
            let worker = RetryFailedDeployments::new(
                RetryFailedConfig {
                    max_retries,
                    ..Default::default()
                },
                reqwest::Client::new(),
                storage,
                Arc::new(LiveSyncDeployer::new(
                    pool.clone(),
                    1,
                    catalyst::content::sync_deployer::SyncValidation::AuthChainOnly,
                )),
                store.clone(),
                Arc::new(tokio::sync::RwLock::new(Vec::new())),
            );
            for _ in 0..runs {
                let started = Instant::now();
                let stats = worker.execute_retry_cycle().await.unwrap();
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                summary = format!("{stats:?}");
            }
            let _ = std::fs::remove_dir_all(&storage_dir);
        }
    }
    let (p50, p95, p99) = percentiles(samples.clone());
    let (rss_after, hwm_after) = rss();
    println!(
        "shape={shape} runs={runs} p50={p50:.3}ms p95={p95:.3}ms p99={p99:.3}ms min={:.3}ms max={:.3}ms | {summary} | VmRSS {rss_before}->{rss_after} kB, VmHWM {hwm_before}->{hwm_after} kB",
        samples.iter().cloned().fold(f64::INFINITY, f64::min),
        samples.iter().cloned().fold(0.0, f64::max)
    );
    pool.close().await;
}
