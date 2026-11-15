// from: catalyrst/crates/catalyrst-db/tests/snapshot_outdated.rs (the outdated probe), catalyrst/crates/catalyrst-db/src/snapshot_generator.rs (the pass the golden pins)
//! The snapshot generator (`content::snapshot_gen`) against its contracts,
//! one scratch database per test (`tests/support/db.rs`; skipped without
//! CATALYST_TEST_DATABASE_URL):
//! * the old outdated probe's inclusive window and strict late-arrival rule;
//! * an empty database writes nothing until its first deployment;
//! * the files and CIDs the OLD generator wrote from `fixtures/snapshots/seed.sql`
//!   (`fixtures/snapshots/old-generator.json`), for a pass and for a later pass
//!   that folds a day into a week;
//! * a pass's reuse, regeneration of an outdated window, replacement, pruning
//!   and the re-key repair, with no blob ever unlinked;
//! * two v2 nodes on two databases and two ports: A syncs from a stub peer,
//!   takes local deployments, generates behind its `Syncing` gate and serves
//!   `/snapshots`; B syncs from A's `/snapshots` + `/pointer-changes` and ends
//!   with A's active entities, generates the same CIDs from them, and on a
//!   fresh bootstrap from A skips every snapshot it generated itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use catalyst::content::snapshot_gen::{self, SnapshotGenerator, snapshot_is_outdated};
use catalyst::content::sync::snapshots::SnapshotScheduling;
use catalyst::content::sync::worker::{self, SyncSettings};
use catalyst::content::wire_types::{SnapshotMetadata, TimeRange};
use catalyst::content::write_deployer::WriteDeployer;
use catalyst::content::{ContentState, Settings as ContentSettings, SyncState, SyncStatus};
use catalyst::jobs::Jobs;
use catalyst::storage::Storage;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::PgPool;

#[path = "support/auth.rs"]
mod auth;
#[path = "support/db.rs"]
mod db;

const SEED: &str = include_str!("fixtures/snapshots/seed.sql");
const GOLDEN: &str = include_str!("fixtures/snapshots/old-generator.json");
const DAY: i64 = 86_400_000;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "catalyst-snapgen-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

async fn content_state(pool: &PgPool, root: &std::path::Path) -> Arc<ContentState> {
    let storage = Arc::new(Storage::from_config(root).await.unwrap());
    Arc::new(ContentState::with_sync(
        pool.clone(),
        ContentSettings::for_tests(),
        storage,
        Arc::new(SyncStatus::default()),
    ))
}

fn settings(tick_ms: u64) -> snapshot_gen::Settings {
    snapshot_gen::Settings {
        interval: Duration::from_secs(3600),
        tick: Duration::from_millis(tick_ms),
    }
}

/// An active deployment with only the columns the probe reads mattering.
async fn insert_deployment(
    pool: &PgPool,
    id: &str,
    entity_ms: i64,
    local_ms: i64,
    deleter: Option<i32>,
) {
    sqlx::query(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata,
             entity_timestamp, entity_pointers, local_timestamp, auth_chain, deleter_deployment)
         VALUES ('0x0', 'v3', 'profile', $1, '{}', to_timestamp($2 / 1000.0) AT TIME ZONE 'UTC', ARRAY[$1],
                 to_timestamp($3 / 1000.0) AT TIME ZONE 'UTC', '[]', $4)",
    )
    .bind(id)
    .bind(entity_ms as f64)
    .bind(local_ms as f64)
    .bind(deleter)
    .execute(pool)
    .await
    .unwrap();
}

/// Every regular file below `dir`.
fn files_below(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// A node with nothing deployed writes no snapshot file and no row, so a
/// freshly stamped host that has synced nothing stays empty for
/// `catalyst-restore --replace-empty` (the VM's restore drill); the first
/// deployment makes the next pass write every window.
#[tokio::test]
async fn an_empty_database_generates_nothing_until_a_deployment_lands() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let root = temp_dir("empty");
    let state = content_state(&db.pool, &root).await;
    let generator = SnapshotGenerator::new(state.clone(), false, settings(1000));
    let end = 1_790_330_400_000;

    let (rows, summary) = generator.generate(end).await.unwrap();
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(summary, snapshot_gen::PassSummary::default());
    assert_eq!(
        files_below(&root),
        Vec::<std::path::PathBuf>::new(),
        "no file in the store"
    );

    insert_deployment(&db.pool, "first-profile", end - DAY, end - DAY, None).await;
    let (rows, summary) = generator.generate(end).await.unwrap();
    assert!(
        summary.windows > 0 && summary.generated == summary.windows,
        "{summary:?}"
    );
    assert_eq!(rows.len(), summary.windows);
    assert_eq!(summary.entities_written, 1);
    assert!(
        !files_below(&root).is_empty(),
        "the windows are in the store"
    );

    db.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// The job, not a direct `generate()`: a node that boots with nothing
/// deployed (serve-unsynced, a node authoring locally) runs empty passes,
/// and the first deployment gets its windows at the next tick, not a whole
/// `SNAPSHOT_GENERATION_INTERVAL_HOURS` later (the 2026-09-25 review: a v2
/// peer naming such a node as SYNC_SOURCE stayed Bootstrapping for 6 h).
#[tokio::test]
async fn the_job_generates_at_the_first_tick_after_a_deployment_lands_on_an_empty_node() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let root = temp_dir("empty-job");
    let state = content_state(&db.pool, &root).await;
    let generator = SnapshotGenerator::new(
        state.clone(),
        false,
        snapshot_gen::Settings {
            interval: Duration::from_secs(6 * 3600),
            tick: Duration::from_millis(100),
        },
    );
    let jobs = Jobs::new();
    generator.spawn_job(&jobs);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while generator.report().last_finished_ms.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the job never ran its first (empty) pass"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        generator.report().last_summary,
        Some(snapshot_gen::PassSummary::default()),
        "the empty pass wrote nothing"
    );

    let now = chrono::Utc::now().timestamp_millis();
    insert_deployment(&db.pool, "first-local-scene", now - DAY, now - DAY, None).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let summary = loop {
        if let Some(summary) = generator.report().last_summary.filter(|s| s.windows > 0) {
            break summary;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no generation within 20 s of the deployment: the empty pass counted as the 6 h generation"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        summary.generated == summary.windows && summary.entities_written == 1,
        "{summary:?}"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM content.snapshots")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows as usize, summary.windows);
    jobs.shutdown(Duration::from_secs(5)).await;
    db.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// The old `outdated_probe_preserves_inclusive_range_and_active_late_arrivals`:
/// the window is inclusive at both ends, only an ACTIVE deployment that landed
/// strictly after the generation time outdates it, and a large set of
/// matching rows still answers (the probe stops at the first).
#[tokio::test]
async fn outdated_probe_preserves_inclusive_range_and_active_late_arrivals() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let window = TimeRange {
        init_timestamp: 1000,
        end_timestamp: 2000,
    };
    let generation = 3000;
    for (i, (entity_ms, local_ms, deleted, expected)) in [
        (999, 3001, false, false),
        (2001, 3001, false, false),
        (1500, 3000, false, false),
        (1500, 3001, true, false),
        (1000, 3001, false, true),
        (2000, 3001, false, true),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query("TRUNCATE content.deployments")
            .execute(&db.pool)
            .await
            .unwrap();
        insert_deployment(
            &db.pool,
            &format!("probe-{i}"),
            entity_ms,
            local_ms,
            deleted.then_some(1),
        )
        .await;
        assert_eq!(
            snapshot_is_outdated(&db.pool, &window, generation)
                .await
                .unwrap(),
            expected,
            "entity {entity_ms} ms, local {local_ms} ms, deleted {deleted}"
        );
    }
    sqlx::query(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata,
             entity_timestamp, entity_pointers, local_timestamp, auth_chain)
         SELECT '0x0', 'v3', 'profile', 'bulk-' || i, '{}', to_timestamp(1.5) AT TIME ZONE 'UTC', ARRAY['bulk-' || i],
                to_timestamp(4) AT TIME ZONE 'UTC', '[]'
         FROM generate_series(1, 10000) AS i",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        snapshot_is_outdated(&db.pool, &window, generation)
            .await
            .unwrap()
    );
    db.teardown().await;
}

struct GoldenWindow {
    range: TimeRange,
    hash: String,
    entities: u64,
    bytes: u64,
    replaced: Vec<String>,
    text: Option<String>,
}

fn golden_pass(index: usize) -> (i64, Vec<GoldenWindow>) {
    let doc: Value = serde_json::from_str(GOLDEN).unwrap();
    let pass = &doc["passes"][index];
    let windows = pass["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| GoldenWindow {
            range: TimeRange {
                init_timestamp: w["initTimestamp"].as_i64().unwrap(),
                end_timestamp: w["endTimestamp"].as_i64().unwrap(),
            },
            hash: w["hash"].as_str().unwrap().to_string(),
            entities: w["numberOfEntities"].as_u64().unwrap(),
            bytes: w["bytes"].as_u64().unwrap(),
            replaced: w["replacedSnapshotHashes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h.as_str().unwrap().to_string())
                .collect(),
            text: w["text"].as_str().map(str::to_string),
        })
        .collect();
    (pass["endMs"].as_i64().unwrap(), windows)
}

async fn assert_matches_golden(
    state: &ContentState,
    rows: &[SnapshotMetadata],
    golden: &[GoldenWindow],
) {
    assert_eq!(rows.len(), golden.len(), "{rows:?}");
    for (row, want) in rows.iter().zip(golden) {
        assert_eq!(row.time_range, want.range);
        assert_eq!(row.hash, want.hash, "window {:?}", want.range);
        assert_eq!(
            row.number_of_entities, want.entities,
            "window {:?}",
            want.range
        );
        assert_eq!(
            row.replaced_snapshot_hashes.as_deref(),
            Some(&want.replaced[..]),
            "window {:?}",
            want.range
        );
        let bytes = state
            .storage
            .retrieve(&row.hash)
            .await
            .unwrap()
            .expect("the file is in the ONE store");
        assert_eq!(bytes.len() as u64, want.bytes, "window {:?}", want.range);
        assert_eq!(
            catalyst::auth::hash_bytes_v1(&bytes),
            row.hash,
            "the bytes hash to their name"
        );
        if let Some(text) = &want.text {
            assert_eq!(
                std::str::from_utf8(&bytes).unwrap(),
                text,
                "window {:?}",
                want.range
            );
        }
        assert!(bytes.starts_with(format!("{}\n", snapshot_gen::SNAPSHOT_HEADER).as_bytes()));
    }
}

/// The v2 generator writes, from the same rows, the files and CIDs the old
/// generator wrote: a 600-profile year (the chunked-DAG CID), a boundary
/// entity in two windows, an overwritten entity left out, a tie ordered by
/// entity id, auth chains re-serialized with sorted keys, the remainder and
/// the pre-genesis row in no file; then a pass a week later that folds the
/// day into a new week and names the day's CID as replaced.
#[tokio::test]
async fn generation_matches_the_old_generator_golden() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    sqlx::raw_sql(SEED).execute(&db.pool).await.unwrap();
    let root = temp_dir("golden");
    let state = content_state(&db.pool, &root).await;
    let generator = SnapshotGenerator::new(state.clone(), false, settings(1000));

    let (end1, golden1) = golden_pass(0);
    let (rows, summary) = generator.generate(end1).await.unwrap();
    assert_eq!(
        (summary.windows, summary.generated, summary.reused),
        (4, 4, 0)
    );
    assert_eq!(summary.entities_written, 602 + 3 + 3 + 1);
    assert_matches_golden(&state, &rows, &golden1).await;

    let (end2, golden2) = golden_pass(1);
    let (rows, summary) = generator.generate(end2).await.unwrap();
    assert_eq!(
        (
            summary.windows,
            summary.generated,
            summary.reused,
            summary.pruned
        ),
        (5, 2, 3, 0)
    );
    assert_matches_golden(&state, &rows, &golden2).await;
    let day = &golden1[3].hash;
    assert!(
        state.storage.exist(day).await.unwrap(),
        "a replaced file stays in the shared store"
    );

    db.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// A second pass over unchanged rows reuses every window; a late arrival
/// regenerates exactly its window with the old CID replaced; a foreign row
/// is pruned; a row whose stored file hashes to another CID is re-keyed
/// (file and row) instead of regenerated; `/snapshots` follows each pass.
#[tokio::test]
async fn a_pass_reuses_regenerates_prunes_and_rekeys() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    sqlx::raw_sql(SEED).execute(&db.pool).await.unwrap();
    let root = temp_dir("lifecycle");
    let state = content_state(&db.pool, &root).await;
    let generator = SnapshotGenerator::new(state.clone(), false, settings(1000));
    let (end, golden) = golden_pass(0);

    let (first, _) = generator.generate(end).await.unwrap();
    let (again, summary) = generator.generate(end).await.unwrap();
    assert_eq!(
        (summary.generated, summary.reused, summary.pruned),
        (0, 4, 0)
    );
    assert_eq!(
        again, first,
        "a reused window keeps its row, generation time included"
    );

    // A late arrival in the week window: active, stamped after the pass.
    let week = golden[2].range;
    insert_deployment(
        &db.pool,
        "bafkreilatearrival",
        week.init_timestamp + DAY,
        chrono::Utc::now().timestamp_millis(),
        None,
    )
    .await;
    let (rows, summary) = generator.generate(end).await.unwrap();
    assert_eq!((summary.generated, summary.reused), (1, 3));
    assert_ne!(rows[2].hash, golden[2].hash);
    assert_eq!(rows[2].number_of_entities, 4);
    assert_eq!(
        rows[2].replaced_snapshot_hashes.as_deref(),
        Some(&[golden[2].hash.clone()][..])
    );
    assert!(
        state.storage.exist(&golden[2].hash).await.unwrap(),
        "the replaced file is not unlinked"
    );
    for i in [0, 1, 3] {
        assert_eq!(rows[i], first[i]);
    }

    // A row no division produces (a window from another genesis) is pruned.
    sqlx::query(
        "INSERT INTO content.snapshots (hash, init_timestamp, end_timestamp, replaced_hashes, number_of_entities, generation_time)
         VALUES ($1, '2020-01-01 01:00', '2020-01-01 02:00', '{}', 0, '2021-01-08')",
    )
    .bind(&golden[3].hash)
    .execute(&db.pool)
    .await
    .unwrap();
    let (rows, summary) = generator.generate(end).await.unwrap();
    assert_eq!((summary.pruned, summary.generated), (1, 0));
    assert_eq!(rows.len(), 4);

    // The 2026-05 repair: the month's row names a CID its bytes do not hash
    // to (stored under that wrong key). A fresh process (nothing verified
    // yet) re-keys the file and the row and reuses the window.
    let month = rows[1].clone();
    let bytes = state.storage.retrieve(&month.hash).await.unwrap().unwrap();
    let wrong = catalyst::auth::hash_bytes_v1(b"a CID from a broken DAG");
    state.storage.store(&wrong, bytes.clone()).await.unwrap();
    sqlx::query("UPDATE content.snapshots SET hash = $1 WHERE hash = $2")
        .bind(&wrong)
        .bind(&month.hash)
        .execute(&db.pool)
        .await
        .unwrap();
    let fresh = SnapshotGenerator::new(state.clone(), false, settings(1000));
    let (rows, summary) = fresh.generate(end).await.unwrap();
    assert_eq!(
        (summary.rekeyed, summary.reused, summary.generated),
        (1, 4, 0)
    );
    assert_eq!(rows[1].hash, month.hash, "the row follows the bytes");
    assert!(
        !state.storage.exist(&wrong).await.unwrap(),
        "the wrong key no longer serves those bytes"
    );
    assert_eq!(
        state.storage.retrieve(&month.hash).await.unwrap().unwrap(),
        bytes
    );

    // An entity of the year window overwritten after the pass: the outdated
    // probe sees active late arrivals only, and the row is younger than a
    // month, so a pass keeps the window; the rebuild (the console's
    // regenerate) rewrites every window, and only the year's CID moves.
    sqlx::query("UPDATE content.deployments SET deleter_deployment = 0 WHERE entity_id = 'bafkreiprofile000001'")
        .execute(&db.pool)
        .await
        .unwrap();
    let (kept, summary) = fresh.generate(end).await.unwrap();
    assert_eq!((summary.reused, summary.generated), (4, 0));
    assert_eq!(kept, rows);
    let (rows, summary) = fresh.regenerate(end).await.unwrap();
    assert_eq!((summary.reused, summary.generated), (0, 4));
    assert_eq!(rows[0].number_of_entities, 601);
    assert_ne!(rows[0].hash, kept[0].hash);
    assert_eq!(
        rows[0].replaced_snapshot_hashes.as_deref(),
        Some(&[kept[0].hash.clone()][..])
    );
    for i in 1..4 {
        assert_eq!(
            rows[i].hash, kept[i].hash,
            "an unchanged window keeps its CID"
        );
        assert_eq!(
            rows[i].replaced_snapshot_hashes.as_deref(),
            Some(&[][..]),
            "and replaces nothing"
        );
    }

    // /snapshots lists what the table holds in division order, oldest window
    // first, as upstream does.
    let listed = get_json(&content_router(state.clone()), "/content/snapshots").await;
    let hashes: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["hash"].as_str().unwrap())
        .collect();
    let want: Vec<&str> = rows.iter().map(|r| r.hash.as_str()).collect();
    assert_eq!(hashes, want);

    db.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// Upstream's inactive-entities check: a window whose row is over a month
/// (four weeks) old and lists more entities than are active now is
/// regenerated with the old hash in `replacedSnapshotHashes`; a month-old
/// row whose entities are all active, or a younger row with an overwritten
/// entity, is reused.
#[tokio::test]
async fn a_month_old_window_listing_an_overwritten_entity_is_regenerated() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    sqlx::raw_sql(SEED).execute(&db.pool).await.unwrap();
    let root = temp_dir("inactive");
    let state = content_state(&db.pool, &root).await;
    let generator = SnapshotGenerator::new(state.clone(), false, settings(1000));
    let (end, golden) = golden_pass(0);
    let year = golden[0].range;
    let (first, _) = generator.generate(end).await.unwrap();
    assert_eq!(first[0].hash, golden[0].hash);

    // The year row as if generated one hour after the seed's local_timestamp
    // (so the outdated probe stays quiet), and a pass four weeks and a day
    // later: the year window is unchanged by the later division.
    let generated = sqlx::query_scalar::<_, chrono::NaiveDateTime>(
        "UPDATE content.snapshots SET generation_time = '2021-01-07 07:00'
         WHERE init_timestamp = to_timestamp($1 / 1000.0) AT TIME ZONE 'UTC'
         RETURNING generation_time",
    )
    .bind(year.init_timestamp as f64)
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .and_utc()
    .timestamp_millis();
    let month = 28 * DAY;
    let year_row =
        |rows: &[SnapshotMetadata]| rows.iter().find(|r| r.time_range == year).cloned().unwrap();

    // Every listed entity still active: the month-old row is reused.
    let (rows, _) = generator.generate(generated + month + DAY).await.unwrap();
    let kept = year_row(&rows);
    assert_eq!(kept.hash, golden[0].hash);
    assert_eq!(kept.generation_timestamp, generated);

    // An entity overwritten, the row younger than a month: still reused.
    sqlx::query("UPDATE content.deployments SET deleter_deployment = 0 WHERE entity_id = 'bafkreiprofile000001'")
        .execute(&db.pool)
        .await
        .unwrap();
    let (rows, _) = generator.generate(generated + month - DAY).await.unwrap();
    assert_eq!(year_row(&rows), kept);

    // Over a month old with fewer active entities than it lists: regenerated.
    let (rows, _) = generator.generate(generated + month + DAY).await.unwrap();
    let rebuilt = year_row(&rows);
    assert_eq!(rebuilt.number_of_entities, golden[0].entities - 1);
    assert_ne!(rebuilt.hash, kept.hash);
    assert_eq!(
        rebuilt.replaced_snapshot_hashes.as_deref(),
        Some(&[kept.hash.clone()][..])
    );
    assert!(rebuilt.generation_timestamp > generated);

    // The new row is younger than a month: the next pass reuses it.
    let (rows, _) = generator.generate(generated + month + DAY).await.unwrap();
    assert_eq!(year_row(&rows), rebuilt);

    db.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

fn content_router(state: Arc<ContentState>) -> Router {
    catalyst::content::build_router(state)
}

const STEP_0026: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/schemas/migrations/0026_content_snapshots_outdated.sql"
));

/// A window's `outdated` mark and whether its generation time is the epoch.
async fn mark_of(pool: &PgPool, hash: &str) -> (bool, bool) {
    sqlx::query_as(
        "SELECT outdated, generation_time = 'epoch'::timestamp FROM content.snapshots WHERE hash = $1",
    )
    .bind(hash)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Step 0026 carries a window an older binary marked by stamping the epoch
/// over its generation time into `outdated`, and leaves the others alone;
/// run twice it changes nothing more.
#[tokio::test]
async fn step_0026_carries_an_epoch_mark_into_outdated() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    sqlx::raw_sql(SEED).execute(&db.pool).await.unwrap();
    let root = temp_dir("step-0026");
    let state = content_state(&db.pool, &root).await;
    let generator = SnapshotGenerator::new(state, false, settings(1000));
    let (end, _) = golden_pass(0);
    let (rows, _) = generator.generate(end).await.unwrap();
    let (marked, kept) = (&rows[2].hash, &rows[1].hash);
    sqlx::query(
        "UPDATE content.snapshots SET generation_time = 'epoch'::timestamp, outdated = false WHERE hash = $1",
    )
    .bind(marked)
    .execute(&db.pool)
    .await
    .unwrap();
    for _ in 0..2 {
        sqlx::raw_sql(STEP_0026).execute(&db.pool).await.unwrap();
        assert_eq!(mark_of(&db.pool, marked).await, (true, true));
        assert_eq!(mark_of(&db.pool, kept).await, (false, false));
    }
    db.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// A window an older binary marked with the epoch, which the new binary
/// reads without the step having flagged it, regenerates at the next pass
/// and comes back with a real generation time and no mark; the others are
/// reused.
#[tokio::test]
async fn an_epoch_mark_from_an_older_binary_regenerates_its_window() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    sqlx::raw_sql(SEED).execute(&db.pool).await.unwrap();
    let root = temp_dir("epoch-mark");
    let state = content_state(&db.pool, &root).await;
    let generator = SnapshotGenerator::new(state, false, settings(1000));
    let (end, _) = golden_pass(0);
    let (first, _) = generator.generate(end).await.unwrap();
    sqlx::query(
        "UPDATE content.snapshots SET generation_time = 'epoch'::timestamp, outdated = false WHERE hash = $1",
    )
    .bind(&first[2].hash)
    .execute(&db.pool)
    .await
    .unwrap();
    let (rows, summary) = generator.generate(end).await.unwrap();
    assert_eq!((summary.generated, summary.reused), (1, 3), "{summary:?}");
    assert!(rows[2].generation_timestamp > 0);
    assert_eq!(mark_of(&db.pool, &rows[2].hash).await, (false, false));
    for i in [0, 1, 3] {
        assert_eq!(rows[i], first[i]);
    }
    db.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

async fn get_json(app: &Router, uri: &str) -> Value {
    use tower::ServiceExt;
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{uri}: {}", resp.status());
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── two nodes ───────────────────────────────────────────────────────────────

fn entity_file(
    kind: &str,
    pointers: &[&str],
    timestamp: i64,
    content: &[(&str, &str)],
) -> (String, Vec<u8>) {
    let bytes = serde_json::to_vec(&json!({
        "version": "v3",
        "type": kind,
        "pointers": pointers,
        "timestamp": timestamp,
        "content": content.iter().map(|(file, hash)| json!({"file": file, "hash": hash})).collect::<Vec<_>>(),
        "metadata": {"name": format!("{kind} at {timestamp}")},
    }))
    .unwrap();
    (catalyst::auth::hash_bytes_v1(&bytes), bytes)
}

/// The stub peer node A syncs from: an empty snapshot list, its entities as
/// pointer changes, and their files.
async fn spawn_stub_peer(now: i64) -> (String, tokio::task::JoinHandle<()>) {
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    let scene_file = b"{\"main\":\"bin/game.js\"}".to_vec();
    let scene_file_hash = catalyst::auth::hash_bytes_v1(&scene_file);
    files.insert(scene_file_hash.clone(), scene_file);
    let entities = [
        entity_file(
            "scene",
            &["-5,-5", "-5,-6"],
            now - 400 * DAY,
            &[("scene.json", &scene_file_hash)],
        ),
        entity_file(
            "outfits",
            &["0x00000000000000000000000000000000000000aa:outfits"],
            now - 100 * DAY,
            &[],
        ),
        entity_file(
            "profile",
            &["0x00000000000000000000000000000000000000bb"],
            now - 50 * DAY,
            &[],
        ),
        entity_file(
            "wearable",
            &["urn:decentraland:ethereum:collections-v1:test:w1"],
            now - 40 * DAY,
            &[],
        ),
        entity_file(
            "profile",
            &["0x00000000000000000000000000000000000000bb"],
            now - 10 * DAY,
            &[],
        ),
        entity_file(
            "emote",
            &["urn:decentraland:matic:collections-v2:0xabc:7"],
            now - 2 * DAY,
            &[],
        ),
    ];
    let mut deltas = Vec::new();
    for (i, (id, bytes)) in entities.iter().enumerate() {
        let entity: Value = serde_json::from_slice(bytes).unwrap();
        deltas.push(json!({
            "entityId": id,
            "entityType": entity["type"],
            "entityTimestamp": entity["timestamp"],
            "localTimestamp": now - 60_000 + i as i64,
            "authChain": auth::signed_chain(id).await,
            "pointers": entity["pointers"],
        }));
        files.insert(id.clone(), bytes.clone());
    }
    let app = Router::new()
        .route("/snapshots", get(|| async { Json(json!([])) }))
        .route(
            "/pointer-changes",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let deltas = deltas.clone();
                async move {
                    let from = query.get("from").and_then(|f| f.parse::<i64>().ok()).unwrap_or(0);
                    let deltas: Vec<Value> = deltas.into_iter().filter(|d| d["localTimestamp"].as_i64().unwrap() >= from).collect();
                    Json(json!({"deltas": deltas, "pagination": {"moreData": false}}))
                }
            }),
        )
        .route(
            "/contents/{id}",
            get(|State(files): State<Arc<HashMap<String, Vec<u8>>>>, Path(id): Path<String>| async move {
                files.get(&id).cloned().map(Bytes::from).ok_or(axum::http::StatusCode::NOT_FOUND)
            }),
        )
        .with_state(Arc::new(files));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

fn sync_settings(peer: &str) -> SyncSettings {
    SyncSettings {
        peers: vec![peer.to_string()],
        dao: None,
        entity_types: None,
        from_timestamp_ms: 0,
        phased_sync: true,
        content_download_concurrency: 8,
        connections_max_idle: 4,
        snapshot_concurrency: 2,
        snapshot_scheduling: SnapshotScheduling {
            nonscene_concurrency: 8,
            scene_concurrency: 1,
        },
        flush_concurrency: 2,
        bloom_expected_elements: 10_000,
        profile_max_age_ms: 31_536_000_000,
        retry_failed_enabled: false,
        max_retries: 3,
        retry_concurrency: 2,
        prune_ttl_days: 7,
    }
}

/// Serves `state`'s content router on a port of its own, recording every
/// request path.
async fn serve_node(
    state: Arc<ContentState>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = content_router(state).layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            record.lock().push(req.uri().path().to_string());
            next.run(req)
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        seen,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

async fn wait_until<F: Fn() -> bool>(what: &str, secs: u64, done: F) {
    tokio::time::timeout(Duration::from_secs(secs), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// The active entities as rows: id, type, pointers, entity timestamp (ms),
/// sorted by id; then the active pointer map; then every active entity's
/// content files.
async fn active_view(
    pool: &PgPool,
) -> (
    Vec<(String, String, Vec<String>, i64)>,
    Vec<(String, String)>,
    Vec<(String, String, String)>,
) {
    let entities = sqlx::query_as(
        "SELECT entity_id, entity_type, entity_pointers, (date_part('epoch', entity_timestamp) * 1000)::bigint
         FROM content.deployments WHERE deleter_deployment IS NULL ORDER BY entity_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let pointers = sqlx::query_as(
        "SELECT pointer::text, entity_id FROM content.active_pointers ORDER BY pointer",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let files = sqlx::query_as(
        "SELECT d.entity_id, cf.key, cf.content_hash
         FROM content.content_files cf JOIN content.deployments d ON d.id = cf.deployment
         WHERE d.deleter_deployment IS NULL ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    (entities, pointers, files)
}

async fn post_active(url: &str, pointers: &[String]) -> Value {
    let body: Value = reqwest::Client::new()
        .post(format!("{url}/content/entities/active"))
        .json(&json!({ "pointers": pointers }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut entities = body.as_array().unwrap().clone();
    entities.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Value::Array(entities)
}

/// A local scene published on A five days ago: its row as the write path
/// leaves it (deployment, active pointers, content files, provenance) and
/// its files in A's store. The write path itself refuses a timestamp that
/// old, so the row is seeded; the scene deployed through it below is "now".
async fn seed_old_local_scene(state: &ContentState, now: i64) -> String {
    let file = b"export function onStart() {}".to_vec();
    let file_hash = catalyst::auth::hash_bytes_v1(&file);
    let (id, bytes) = entity_file(
        "scene",
        &["7,7"],
        now - 5 * DAY,
        &[("bin/game.js", &file_hash)],
    );
    state
        .storage
        .store(&file_hash, Bytes::from(file))
        .await
        .unwrap();
    state
        .storage
        .store(&id, Bytes::from(bytes.clone()))
        .await
        .unwrap();
    let chain = serde_json::to_value(auth::signed_chain(&id).await).unwrap();
    let signer = chain[0]["payload"].as_str().unwrap().to_string();
    let metadata = json!({ "v": serde_json::from_slice::<Value>(&bytes).unwrap()["metadata"] });
    let mut tx = state.pool.begin().await.unwrap();
    let dep: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata,
             entity_timestamp, entity_pointers, local_timestamp, auth_chain)
         VALUES ($1, 'v3', 'scene', $2, $3::json, to_timestamp($4 / 1000.0) AT TIME ZONE 'UTC', ARRAY['7,7'],
                 timezone('UTC', now()), $5::json)
         RETURNING id",
    )
    .bind(&signer)
    .bind(&id)
    .bind(metadata.to_string())
    .bind((now - 5 * DAY) as f64)
    .bind(chain.to_string())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, 'bin/game.js')")
        .bind(dep)
        .bind(&file_hash)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ('7,7', $1, 'scene')")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO content.local_entities (entity_id, signer) VALUES ($1, $2)")
        .bind(&id)
        .bind(&signer)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
async fn a_second_node_syncs_from_the_first_nodes_snapshots_and_pointer_changes() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,catalyst::content=info")
        .with_test_writer()
        .try_init();
    let Some(db_a) = db::scratch_database().await else {
        return;
    };
    let Some(db_b) = db::scratch_database().await else {
        return;
    };
    let now = chrono::Utc::now().timestamp_millis();
    let (root_a, root_b) = (temp_dir("node-a"), temp_dir("node-b"));
    let node_a = content_state(&db_a.pool, &root_a).await;
    let node_b = content_state(&db_b.pool, &root_b).await;

    // Node A: the generator job first, gated on Syncing, then the local
    // deployments and the sync worker against the stub peer.
    let jobs_a = Arc::new(Jobs::new());
    let gen_a = SnapshotGenerator::new(node_a.clone(), true, settings(100));
    gen_a.spawn_job(&jobs_a);
    let old_local = seed_old_local_scene(&node_a, now).await;
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        db_a.pool.clone(),
        node_a.storage.clone(),
        Some(Arc::new(catalyst::auth::PeerValidated)),
    );
    let asset = b"a texture".to_vec();
    let asset_hash = catalyst::auth::hash_bytes_v1(&asset);
    let (new_local, new_local_bytes) = entity_file(
        "scene",
        &["1,1", "1,2"],
        now,
        &[("assets/t.png", &asset_hash)],
    );
    let chain = serde_json::to_value(auth::signed_chain(&new_local).await).unwrap();
    deployer
        .deploy_entity(
            vec![Bytes::from(new_local_bytes), Bytes::from(asset)],
            &new_local,
            chain,
            "LOCAL",
        )
        .await
        .unwrap_or_else(|e| panic!("local deploy on A: {:?}", e.errors()));
    let prior = jobs_a
        .report()
        .iter()
        .find(|job| job.name == snapshot_gen::JOB_NAME)
        .map_or(0, |job| job.passes);
    assert!(
        catalyst::shared::test_wait::until(async || jobs_a
            .report()
            .iter()
            .any(|job| job.name == snapshot_gen::JOB_NAME && job.passes > prior))
        .await
    );
    assert!(
        snapshot_gen::all_snapshots(&db_a.pool)
            .await
            .unwrap()
            .is_empty(),
        "nothing is generated while A bootstraps"
    );
    assert!(!gen_a.report().ready);

    let (peer, peer_task) = spawn_stub_peer(now).await;
    worker::start(
        &sync_settings(&peer),
        db_a.pool.clone(),
        node_a.storage.clone(),
        node_a.sync.clone(),
        reqwest::Client::new(),
        catalyst::content::sync_deployer::SyncValidation::AuthChainOnly,
        &jobs_a,
    )
    .await
    .unwrap();
    wait_until("A to reach Syncing", 30, || {
        node_a.sync.state() == SyncState::Syncing
    })
    .await;
    wait_until("A's first generation pass", 30, || {
        gen_a.report().last_summary.is_some()
    })
    .await;
    let a_rows = snapshot_gen::all_snapshots(&db_a.pool).await.unwrap();
    assert!(!a_rows.is_empty());
    let in_files: u64 = a_rows.iter().map(|r| r.number_of_entities).sum();
    assert_eq!(
        in_files, 6,
        "the four synced active entities in windows, the older profile overwritten, the old local scene; the new local scene is in the remainder"
    );
    let (url_a, seen_a, serve_a) = serve_node(node_a.clone()).await;

    // Node B: syncs from A's content API only.
    let jobs_b = Arc::new(Jobs::new());
    let source = format!("{url_a}/content");
    worker::start(
        &sync_settings(&source),
        db_b.pool.clone(),
        node_b.storage.clone(),
        node_b.sync.clone(),
        reqwest::Client::new(),
        catalyst::content::sync_deployer::SyncValidation::AuthChainOnly,
        &jobs_b,
    )
    .await
    .unwrap();
    wait_until("B to reach Syncing", 60, || {
        node_b.sync.state() == SyncState::Syncing
    })
    .await;
    let expected = active_view(&db_a.pool).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if active_view(&db_b.pool).await == expected || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let failed: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id || ': ' || error_description FROM content.failed_deployments",
    )
    .fetch_all(&db_b.pool)
    .await
    .unwrap();
    assert!(failed.is_empty(), "B failed deployments: {failed:?}");
    assert_eq!(
        active_view(&db_b.pool).await,
        expected,
        "B holds A's active entities, pointers and files"
    );
    assert_eq!(expected.0.len(), 7);
    assert!(
        expected.0.iter().any(|e| e.0 == old_local) && expected.0.iter().any(|e| e.0 == new_local)
    );
    for (_, _, hash) in &expected.2 {
        assert!(
            node_b.storage.exist(hash).await.unwrap(),
            "content file {hash} reached B"
        );
    }
    let processed: Vec<String> = sqlx::query_scalar("SELECT hash FROM content.processed_snapshots")
        .fetch_all(&db_b.pool)
        .await
        .unwrap();
    for row in a_rows.iter().filter(|r| r.number_of_entities > 0) {
        assert!(
            processed.contains(&row.hash),
            "B deployed A's snapshot {} ({} entities)",
            row.hash,
            row.number_of_entities
        );
    }
    let paths = seen_a.lock().clone();
    assert!(paths.iter().any(|p| p == "/content/snapshots"));
    assert!(paths.iter().any(|p| p == "/content/pointer-changes"));

    let (url_b, _, serve_b) = serve_node(node_b.clone()).await;
    let pointers: Vec<String> = expected.1.iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(
        post_active(&url_b, &pointers).await,
        post_active(&url_a, &pointers).await,
        "/entities/active answers alike"
    );

    // Same rows, same files: B's own generation at A's end names A's CIDs.
    let end = chrono::Utc::now().timestamp_millis();
    let gen_b = SnapshotGenerator::new(node_b.clone(), true, settings(100));
    let (a_now, _) = gen_a.generate(end).await.unwrap();
    let (b_now, _) = gen_b.generate(end).await.unwrap();
    let ids = |rows: &[SnapshotMetadata]| {
        rows.iter()
            .map(|r| (r.time_range, r.hash.clone(), r.number_of_entities))
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&b_now), ids(&a_now));

    // A fresh bootstrap of B from A (cursor and processed marks gone) skips
    // every snapshot of A's: B generated each one itself.
    jobs_b.shutdown(Duration::from_secs(10)).await;
    sqlx::raw_sql("DELETE FROM content.server_sync_cursors; DELETE FROM content.processed_snapshots; DELETE FROM content.system_properties")
        .execute(&db_b.pool)
        .await
        .unwrap();
    node_b.sync.set_state(SyncState::Bootstrapping);
    seen_a.lock().clear();
    let jobs_b = Arc::new(Jobs::new());
    worker::start(
        &sync_settings(&source),
        db_b.pool.clone(),
        node_b.storage.clone(),
        node_b.sync.clone(),
        reqwest::Client::new(),
        catalyst::content::sync_deployer::SyncValidation::AuthChainOnly,
        &jobs_b,
    )
    .await
    .unwrap();
    wait_until("B to reach Syncing again", 60, || {
        node_b.sync.state() == SyncState::Syncing
    })
    .await;
    let paths = seen_a.lock().clone();
    assert!(
        paths.iter().any(|p| p == "/content/snapshots"),
        "B read A's list: {paths:?}"
    );
    for row in &a_now {
        assert!(
            !paths.iter().any(|p| p.ends_with(&row.hash)),
            "B downloaded its own snapshot {}: {paths:?}",
            row.hash
        );
    }
    let processed: i64 = sqlx::query_scalar("SELECT count(*) FROM content.processed_snapshots")
        .fetch_one(&db_b.pool)
        .await
        .unwrap();
    assert_eq!(
        processed, 0,
        "an own snapshot is skipped, not marked processed"
    );
    assert_eq!(active_view(&db_b.pool).await, expected);

    jobs_b.shutdown(Duration::from_secs(10)).await;
    jobs_a.shutdown(Duration::from_secs(10)).await;
    for task in [peer_task, serve_a, serve_b] {
        task.abort();
    }
    db_a.teardown().await;
    db_b.teardown().await;
    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}

/// A scene this node unpublished (`DELETE /scenes/{coord}`: tombstoned, its
/// deployment row kept with `deleter_deployment` NULL, the older scene it had
/// overwritten left superseded and the parcel empty) is not history a peer
/// replays: `/pointer-changes` and the snapshot windows leave it and the
/// scenes it superseded out, and the unpublish outdates the windows it
/// touched, so a fresh peer ends with this node's active set (the parcel
/// empty, neither scene anywhere).
#[tokio::test]
async fn an_unpublished_local_scene_is_left_out_of_what_a_peer_replays() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_test_writer()
        .try_init();
    let Some(db_a) = db::scratch_database().await else {
        return;
    };
    let Some(db_b) = db::scratch_database().await else {
        return;
    };
    let now = chrono::Utc::now().timestamp_millis();
    let (root_a, root_b) = (temp_dir("unpub-a"), temp_dir("unpub-b"));
    let node_a = content_state(&db_a.pool, &root_a).await;
    let node_b = content_state(&db_b.pool, &root_b).await;
    node_a.sync.set_state(SyncState::Syncing);

    // A: an old local scene on 7,7, overwritten by a newer local deploy there.
    let old_local = seed_old_local_scene(&node_a, now).await;
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        db_a.pool.clone(),
        node_a.storage.clone(),
        Some(Arc::new(catalyst::auth::PeerValidated)),
    );
    let asset = b"a newer scene".to_vec();
    let asset_hash = catalyst::auth::hash_bytes_v1(&asset);
    let (new_local, new_local_bytes) = entity_file(
        "scene",
        &["7,7"],
        now - 60_000,
        &[("bin/game.js", &asset_hash)],
    );
    let chain = serde_json::to_value(auth::signed_chain(&new_local).await).unwrap();
    deployer
        .deploy_entity(
            vec![Bytes::from(new_local_bytes), Bytes::from(asset)],
            &new_local,
            chain,
            "LOCAL",
        )
        .await
        .unwrap_or_else(|e| panic!("local deploy on A: {:?}", e.errors()));
    let generator = SnapshotGenerator::new(node_a.clone(), false, settings(100));
    let (before, _) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let in_files: u64 = before.iter().map(|r| r.number_of_entities).sum();
    assert_eq!(
        in_files, 0,
        "the old scene is overwritten, the newer one is in today's remainder"
    );

    // The owner unpublishes 7,7: A's parcel is left empty.
    let outcome =
        catalyst::content::land_publish::unpublish_local_scene(&db_a.pool, "7,7", &new_local)
            .await
            .unwrap();
    assert_eq!(outcome.entity_id, new_local);
    assert_eq!(outcome.parcels, vec!["7,7".to_string()]);
    let a_pointer: Option<String> =
        sqlx::query_scalar("SELECT entity_id FROM content.active_pointers WHERE pointer = '7,7'")
            .fetch_optional(&db_a.pool)
            .await
            .unwrap();
    assert_eq!(a_pointer, None);

    // The unpublish outdates the windows it touched: the old scene's is regenerated.
    let (_, summary) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    assert!(summary.generated >= 1, "{summary:?}");

    // A scene on 8,8 B waits for: once it lands, B has replayed A.
    let marker_asset = b"a neighbouring scene".to_vec();
    let marker_hash = catalyst::auth::hash_bytes_v1(&marker_asset);
    let (marker, marker_bytes) = entity_file(
        "scene",
        &["8,8"],
        now - 30_000,
        &[("bin/game.js", &marker_hash)],
    );
    let chain = serde_json::to_value(auth::signed_chain(&marker).await).unwrap();
    deployer
        .deploy_entity(
            vec![Bytes::from(marker_bytes), Bytes::from(marker_asset)],
            &marker,
            chain,
            "LOCAL",
        )
        .await
        .unwrap_or_else(|e| panic!("local deploy on A: {:?}", e.errors()));

    // The next pass holds neither scene of 7,7.
    let (after, _) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let in_files: u64 = after.iter().map(|r| r.number_of_entities).sum();
    assert_eq!(in_files, 0, "the old scene stays superseded");
    let (url_a, _, serve_a) = serve_node(node_a.clone()).await;
    let changes: Value = reqwest::Client::new()
        .get(format!("{url_a}/content/pointer-changes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed: Vec<&str> = changes["deltas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["entityId"].as_str().unwrap())
        .collect();
    assert!(
        listed.contains(&marker.as_str())
            && !listed.contains(&old_local.as_str())
            && !listed.contains(&new_local.as_str()),
        "{listed:?}"
    );

    // B, fresh, syncs from A alone.
    let jobs_b = Arc::new(Jobs::new());
    worker::start(
        &sync_settings(&format!("{url_a}/content")),
        db_b.pool.clone(),
        node_b.storage.clone(),
        node_b.sync.clone(),
        reqwest::Client::new(),
        catalyst::content::sync_deployer::SyncValidation::AuthChainOnly,
        &jobs_b,
    )
    .await
    .unwrap();
    wait_until("B to reach Syncing", 60, || {
        node_b.sync.state() == SyncState::Syncing
    })
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let b_marker = loop {
        let p: Option<String> = sqlx::query_scalar(
            "SELECT entity_id FROM content.active_pointers WHERE pointer = '8,8'",
        )
        .fetch_optional(&db_b.pool)
        .await
        .unwrap();
        if p.is_some() || std::time::Instant::now() > deadline {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(b_marker.as_deref(), Some(marker.as_str()), "B replayed A");
    let b_pointer: Option<String> =
        sqlx::query_scalar("SELECT entity_id FROM content.active_pointers WHERE pointer = '7,7'")
            .fetch_optional(&db_b.pool)
            .await
            .unwrap();
    assert_eq!(b_pointer, None, "B leaves 7,7 empty, as A does");
    let on_b: i64 =
        sqlx::query_scalar("SELECT count(*) FROM content.deployments WHERE entity_id = ANY($1)")
            .bind(vec![new_local.clone(), old_local.clone()])
            .fetch_one(&db_b.pool)
            .await
            .unwrap();
    assert_eq!(on_b, 0, "neither scene of 7,7 reached B");

    jobs_b.shutdown(Duration::from_secs(10)).await;
    serve_a.abort();
    db_a.teardown().await;
    db_b.teardown().await;
    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}

/// A database an earlier unpublish left behind on 7,7: the newer local scene
/// tombstoned, the older one it had superseded restored (deleter cleared, the
/// parcel re-pointed). Returns the database, the store root, the node and the
/// two entity ids (old, new).
async fn restored_scene_database(
    tag: &str,
) -> Option<(
    db::ScratchDb,
    std::path::PathBuf,
    Arc<ContentState>,
    String,
    String,
)> {
    let db_a = db::scratch_database().await?;
    let now = chrono::Utc::now().timestamp_millis();
    let root = temp_dir(tag);
    let node = content_state(&db_a.pool, &root).await;
    node.sync.set_state(SyncState::Syncing);

    let old_local = seed_old_local_scene(&node, now).await;
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        db_a.pool.clone(),
        node.storage.clone(),
        Some(Arc::new(catalyst::auth::PeerValidated)),
    );
    let asset = b"a newer scene".to_vec();
    let asset_hash = catalyst::auth::hash_bytes_v1(&asset);
    let (new_local, new_local_bytes) = entity_file(
        "scene",
        &["7,7"],
        now - 60_000,
        &[("bin/game.js", &asset_hash)],
    );
    let chain = serde_json::to_value(auth::signed_chain(&new_local).await).unwrap();
    deployer
        .deploy_entity(
            vec![Bytes::from(new_local_bytes), Bytes::from(asset)],
            &new_local,
            chain,
            "LOCAL",
        )
        .await
        .unwrap_or_else(|e| panic!("local deploy: {:?}", e.errors()));

    // The earlier unpublish: the newer scene tombstoned, the old one restored.
    for sql in [
        "UPDATE content.local_entities SET tombstoned_at = timezone('UTC', now()) WHERE entity_id = $1",
        "UPDATE content.deployments SET deleter_deployment = NULL WHERE entity_id = $2",
        "UPDATE content.active_pointers SET entity_id = $2 WHERE pointer = '7,7'",
    ] {
        sqlx::query(sql)
            .bind(&new_local)
            .bind(&old_local)
            .execute(&db_a.pool)
            .await
            .unwrap();
    }
    Some((db_a, root, node, old_local, new_local))
}

/// A database an earlier unpublish left behind: the unpublish restored the
/// older scene it had superseded (its deleter cleared, the parcel re-pointed)
/// and the next pass wrote that scene into its window. The deleter repair
/// supersedes it again, empties the parcel and marks the window, so the next
/// pass regenerates it without the scene and `/pointer-changes` agrees.
#[tokio::test]
async fn the_deleter_repair_regenerates_a_window_an_earlier_unpublish_restored() {
    let Some((db_a, root, node, old_local, new_local)) =
        restored_scene_database("repair-window").await
    else {
        return;
    };
    let generator = SnapshotGenerator::new(node.clone(), false, settings(100));
    let (before, _) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let in_files: u64 = before.iter().map(|r| r.number_of_entities).sum();
    assert_eq!(in_files, 1, "the restored scene is in its window");

    catalyst::content::sync::LiveDeploymentRepository::new(
        db_a.pool.clone(),
        Arc::new(SyncStatus::default()),
    )
    .resolve_deleter_deployments()
    .await
    .unwrap();
    let a_pointer: Option<String> =
        sqlx::query_scalar("SELECT entity_id FROM content.active_pointers WHERE pointer = '7,7'")
            .fetch_optional(&db_a.pool)
            .await
            .unwrap();
    assert_eq!(a_pointer, None, "the repair empties the parcel");
    // Upstream never publishes a generation time of 0: the mark is a flag and
    // the window keeps its real generation time until the next pass.
    let window = before
        .iter()
        .find(|r| r.number_of_entities == 1)
        .unwrap()
        .clone();
    let (outdated, generated) = window_mark(&db_a.pool, &window.time_range).await;
    assert!(outdated, "the repair marks the window");
    assert_eq!(generated, window.generation_timestamp);
    assert!(generated > 0);

    let (after, summary) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    assert!(summary.generated >= 1, "{summary:?}");
    let in_files: u64 = after.iter().map(|r| r.number_of_entities).sum();
    assert_eq!(in_files, 0, "the regenerated window leaves the scene out");
    let regenerated = after
        .iter()
        .find(|r| r.time_range == window.time_range)
        .unwrap();
    assert!(regenerated.generation_timestamp >= generated);
    assert_eq!(
        window_mark(&db_a.pool, &window.time_range).await,
        (false, regenerated.generation_timestamp),
        "the regenerated row is unmarked"
    );
    assert!(
        after.iter().all(|r| r.generation_timestamp > 0),
        "{after:?}"
    );

    let (url, _, serve) = serve_node(node.clone()).await;
    let changes: Value = reqwest::Client::new()
        .get(format!("{url}/content/pointer-changes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed: Vec<&str> = changes["deltas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["entityId"].as_str().unwrap())
        .collect();
    assert!(
        !listed.contains(&old_local.as_str()) && !listed.contains(&new_local.as_str()),
        "{listed:?}"
    );

    serve.abort();
    db_a.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// A window row's `outdated` mark and its generation time in milliseconds.
async fn window_mark(pool: &PgPool, window: &TimeRange) -> (bool, i64) {
    let (outdated, generated): (bool, chrono::NaiveDateTime) = sqlx::query_as(
        "SELECT outdated, generation_time FROM content.snapshots
         WHERE init_timestamp = to_timestamp($1 / 1000.0) AT TIME ZONE 'UTC'
           AND end_timestamp = to_timestamp($2 / 1000.0) AT TIME ZONE 'UTC'",
    )
    .bind(window.init_timestamp as f64)
    .bind(window.end_timestamp as f64)
    .fetch_one(pool)
    .await
    .unwrap();
    (outdated, generated.and_utc().timestamp_millis())
}

/// Backends of this database waiting on a lock.
async fn lock_waiters(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn wait_for_lock_waiters(pool: &PgPool, n: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while lock_waiters(pool).await < n {
        assert!(
            std::time::Instant::now() < deadline,
            "{n} lock waiters never showed up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The deleter repair commits while a pass is between reading a window and
/// writing its row: the window's new row still carries the repair's mark, so
/// the next pass regenerates it without the superseded scene. The test holds
/// the window's row so the pass stops right before its write, then lets the
/// repair run into it.
#[tokio::test]
async fn a_window_written_beside_the_deleter_repair_keeps_its_mark() {
    let Some((db_a, root, node, _, _)) = restored_scene_database("repair-race").await else {
        return;
    };
    let generator = SnapshotGenerator::new(node.clone(), false, settings(100));
    let (before, _) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let window = before
        .iter()
        .find(|r| r.number_of_entities == 1)
        .expect("the restored scene is in a window")
        .time_range;

    let mut hold = db_a.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT 1 FROM content.snapshots
         WHERE init_timestamp = to_timestamp($1 / 1000.0) AT TIME ZONE 'UTC'
         FOR UPDATE",
    )
    .bind(window.init_timestamp as f64)
    .execute(&mut *hold)
    .await
    .unwrap();
    let pass = tokio::spawn({
        let generator = generator.clone();
        async move {
            generator
                .regenerate(chrono::Utc::now().timestamp_millis())
                .await
        }
    });
    wait_for_lock_waiters(&db_a.pool, 1).await;
    let repair = tokio::spawn({
        let pool = db_a.pool.clone();
        async move {
            catalyst::content::sync::LiveDeploymentRepository::new(
                pool,
                Arc::new(SyncStatus::default()),
            )
            .resolve_deleter_deployments()
            .await
        }
    });
    wait_for_lock_waiters(&db_a.pool, 2).await;
    hold.rollback().await.unwrap();
    let (written, _) = pass.await.unwrap().unwrap();
    repair.await.unwrap().unwrap();
    let row = written
        .iter()
        .find(|r| r.time_range == window)
        .expect("the window's row");
    assert_eq!(row.number_of_entities, 1, "the pass read before the repair");

    let (outdated, generated) = window_mark(&db_a.pool, &window).await;
    assert!(outdated, "the window keeps the repair's mark");
    assert!(
        generated > 0,
        "the mark leaves the generation time as it was"
    );
    let (after, _) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let in_files: u64 = after.iter().map(|r| r.number_of_entities).sum();
    assert_eq!(in_files, 0, "the next pass leaves the scene out");

    db_a.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// A window the deleter repair marked is regenerated at the job's next tick,
/// not a whole generation interval later.
#[tokio::test]
async fn the_job_regenerates_a_marked_window_at_the_next_tick() {
    let Some((db_a, root, node, _, _)) = restored_scene_database("repair-tick").await else {
        return;
    };
    let generator = SnapshotGenerator::new(
        node.clone(),
        false,
        snapshot_gen::Settings {
            interval: Duration::from_secs(6 * 3600),
            tick: Duration::from_millis(100),
        },
    );
    let jobs = Jobs::new();
    generator.spawn_job(&jobs);
    wait_until("the first pass", 20, || {
        generator
            .report()
            .last_summary
            .is_some_and(|s| s.windows > 0)
    })
    .await;
    let first = generator.report().last_finished_ms;

    catalyst::content::sync::LiveDeploymentRepository::new(
        db_a.pool.clone(),
        Arc::new(SyncStatus::default()),
    )
    .resolve_deleter_deployments()
    .await
    .unwrap();
    wait_until("a pass after the repair", 20, || {
        generator.report().last_finished_ms > first
    })
    .await;
    let summary = generator.report().last_summary.unwrap();
    assert!(summary.generated >= 1, "{summary:?}");
    let in_files: i64 =
        sqlx::query_scalar("SELECT coalesce(sum(number_of_entities), 0) FROM content.snapshots")
            .fetch_one(&db_a.pool)
            .await
            .unwrap();
    assert_eq!(in_files, 0, "the regenerated window leaves the scene out");

    jobs.shutdown(Duration::from_secs(5)).await;
    db_a.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// The deleter repair commits after a pass read a window but before the pass
/// writes its row: the row the pass writes carries the repair's mark, so the
/// next pass regenerates the window without the superseded scene. The test
/// holds the restored scene's row so the repair stops mid-statement while the
/// pass reads.
#[tokio::test]
async fn a_window_read_before_the_deleter_repair_commits_is_written_marked() {
    let Some((db_a, root, node, old_local, _)) = restored_scene_database("repair-read").await
    else {
        return;
    };
    let generator = SnapshotGenerator::new(node.clone(), false, settings(100));
    let (before, _) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let window = before
        .iter()
        .find(|r| r.number_of_entities == 1)
        .expect("the restored scene is in a window")
        .time_range;
    // Only this window is regenerated, so the pass's first row write is its.
    sqlx::query(
        "UPDATE content.snapshots SET outdated = true
         WHERE init_timestamp = to_timestamp($1 / 1000.0) AT TIME ZONE 'UTC'",
    )
    .bind(window.init_timestamp as f64)
    .execute(&db_a.pool)
    .await
    .unwrap();

    let mut hold = db_a.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM content.deployments WHERE entity_id = $1 FOR UPDATE")
        .bind(&old_local)
        .execute(&mut *hold)
        .await
        .unwrap();
    let repair = tokio::spawn({
        let pool = db_a.pool.clone();
        async move {
            catalyst::content::sync::LiveDeploymentRepository::new(
                pool,
                Arc::new(SyncStatus::default()),
            )
            .resolve_deleter_deployments()
            .await
        }
    });
    wait_for_lock_waiters(&db_a.pool, 1).await;
    let pass = tokio::spawn({
        let generator = generator.clone();
        async move {
            generator
                .generate(chrono::Utc::now().timestamp_millis())
                .await
        }
    });
    wait_for_lock_waiters(&db_a.pool, 2).await;
    hold.rollback().await.unwrap();
    repair.await.unwrap().unwrap();
    let (written, _) = pass.await.unwrap().unwrap();
    let row = written
        .iter()
        .find(|r| r.time_range == window)
        .expect("the window's row");
    assert_eq!(row.number_of_entities, 1, "the pass read before the repair");
    let (outdated, generated) = window_mark(&db_a.pool, &window).await;
    assert!(outdated, "the window carries the repair's mark");
    assert_eq!(
        generated, row.generation_timestamp,
        "the real generation time is published"
    );
    assert!(generated > 0);

    let (after, _) = generator
        .generate(chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let in_files: u64 = after.iter().map(|r| r.number_of_entities).sum();
    assert_eq!(in_files, 0, "the next pass leaves the scene out");

    db_a.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// A pass stamps a window with the oldest open transaction's start, which can
/// precede the window when a transaction stays open for days. That stamp is
/// not the deleter repair's mark: the job waits for its interval, and the
/// window is only outdated when a row in it changed since.
#[tokio::test]
async fn a_window_stamped_before_it_began_is_not_rewritten_at_every_tick() {
    let Some((db_a, root, node, _, _)) = restored_scene_database("early-stamp").await else {
        return;
    };
    let generator = SnapshotGenerator::new(
        node.clone(),
        false,
        snapshot_gen::Settings {
            interval: Duration::from_secs(6 * 3600),
            tick: Duration::from_millis(100),
        },
    );
    let jobs = Jobs::new();
    generator.spawn_job(&jobs);
    wait_until("the first pass", 20, || {
        generator
            .report()
            .last_summary
            .is_some_and(|s| s.windows > 0)
    })
    .await;
    let first = generator.report().last_finished_ms;

    let (init, end): (chrono::NaiveDateTime, chrono::NaiveDateTime) = sqlx::query_as(
        "UPDATE content.snapshots SET generation_time = init_timestamp - interval '1 day'
         WHERE hash = (SELECT hash FROM content.snapshots
                       WHERE number_of_entities = 0 ORDER BY init_timestamp DESC LIMIT 1)
         RETURNING init_timestamp, end_timestamp",
    )
    .fetch_one(&db_a.pool)
    .await
    .unwrap();
    let window = TimeRange {
        init_timestamp: init.and_utc().timestamp_millis(),
        end_timestamp: end.and_utc().timestamp_millis(),
    };
    assert!(
        !snapshot_is_outdated(&db_a.pool, &window, window.init_timestamp - 86_400_000)
            .await
            .unwrap(),
        "an unchanged window is current"
    );
    let prior = jobs
        .report()
        .iter()
        .find(|job| job.name == snapshot_gen::JOB_NAME)
        .unwrap()
        .passes;
    assert!(
        catalyst::shared::test_wait::until(async || jobs
            .report()
            .iter()
            .any(|job| job.name == snapshot_gen::JOB_NAME && job.passes >= prior + 2))
        .await
    );
    assert_eq!(
        generator.report().last_finished_ms,
        first,
        "no pass ran before the interval"
    );

    jobs.shutdown(Duration::from_secs(5)).await;
    db_a.teardown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// A pass over a populated database. Not a contract: run by hand with
/// CATALYST_SNAPSHOT_BENCH_DATABASE_URL naming a disposable database (it
/// writes content.snapshots and a temporary store). Prints each pass's wall
/// time, what it wrote and the process RSS high-water mark: pass 1 generates
/// every window, passes 2 and 3 reuse them on the verdicts pass 1 left, and
/// a second generator (a restarted process) reuses them re-reading every
/// file once to check its CID.
#[tokio::test]
#[ignore]
async fn bench_a_pass_over_a_populated_database() {
    let Ok(url) = std::env::var("CATALYST_SNAPSHOT_BENCH_DATABASE_URL") else {
        eprintln!("CATALYST_SNAPSHOT_BENCH_DATABASE_URL unset; skipping");
        return;
    };
    let hwm_kb = || {
        std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("VmHWM:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let root = temp_dir("bench");
    let state = content_state(&pool, &root).await;
    let generator = SnapshotGenerator::new(state.clone(), false, settings(1000));
    let end = std::env::var("CATALYST_SNAPSHOT_BENCH_END_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    eprintln!("start: VmHWM {} kB", hwm_kb());
    let restarted = SnapshotGenerator::new(state, false, settings(1000));
    for pass in 1..=4 {
        let started = std::time::Instant::now();
        let run = if pass == 4 { &restarted } else { &generator };
        let (rows, summary) = run.generate(end).await.unwrap();
        let largest = rows.iter().map(|r| r.number_of_entities).max().unwrap_or(0);
        eprintln!(
            "pass {pass}: {} ms, {summary:?}, largest window {largest} entities, VmHWM {} kB",
            started.elapsed().as_millis(),
            hwm_kb()
        );
    }
    let _ = std::fs::remove_dir_all(root);
}
