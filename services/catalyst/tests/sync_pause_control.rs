// new in catalyrst-v2 (the old crate had the same SyncControlHandle and the same gap: catalyrst/crates/catalyrst-server/src/sync/sync_orchestrator.rs:894)
//! The console's sync pause (`/admin/api/content/sync/pause`) must stop a
//! RUNNING steady-state pointer-changes stream, not only hold the next pass:
//! the stream polls the peer every `pointer_changes_wait_time_ms`, and before
//! the fix its `should_stop` read the cancellation token alone, so a paused
//! node kept fetching and deploying at the same cadence while
//! `/admin/api/sync` reported `control=paused`. Against an in-process peer
//! whose `/pointer-changes` counts its calls: after the pause at most the
//! poll already in flight lands, several poll intervals pass with no fetch,
//! and the resume restarts the stream from the persisted cursor. Needs
//! CATALYST_TEST_DATABASE_URL (one scratch database).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::Query;
use axum::routing::get;
use axum::{Json, Router};
use catalyst::content::batch_deployer::{BatchDeployer, BatchDeployerConfig};
use catalyst::content::bloom_filter::BloomFilter;
use catalyst::content::failed_deployments_store::LiveFailedDeploymentsStore;
use catalyst::content::sync::orchestrator::{
    SyncControl, SyncDependencies, SyncOrchestratorConfig,
};
use catalyst::content::sync::{
    LiveDeploymentRepository, LiveProcessedSnapshotStore, SyncOrchestrator, SyncState, SyncStatus,
};
use catalyst::content::sync_deployer::{DEFAULT_FLUSH_CONCURRENCY, LiveSyncDeployer};
use catalyst::storage::Storage;
use serde_json::json;

#[path = "support/db.rs"]
mod db;

const POLL_MS: u64 = 100;

/// A peer with nothing to offer: an empty snapshot list and an empty
/// pointer-changes page, every `/pointer-changes` call counted.
async fn spawn_idle_peer() -> (String, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/snapshots", get(|| async { Json(json!([])) }))
        .route("/pointer-changes", {
            let calls = calls.clone();
            get(move |Query(_q): Query<HashMap<String, String>>| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "deltas": [], "pagination": { "moreData": false } }))
                }
            })
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let http = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, http, calls)
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn a_pause_stops_a_running_pointer_changes_stream_at_its_next_poll() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let pool = db.pool.clone();
    let (peer_url, peer_http, calls) = spawn_idle_peer().await;
    let temp = std::env::temp_dir().join(format!("sync-pause-{}", uuid::Uuid::new_v4()));

    let storage = Arc::new(Storage::from_config(&temp).await.unwrap());
    let status = Arc::new(SyncStatus::default());
    let repo = Arc::new(LiveDeploymentRepository::new(pool.clone(), status.clone()));
    let client = reqwest::Client::new();
    let mut bloom = BloomFilter::with_capacity(1_000);
    repo.seed_deployed_entity_ids(&mut bloom).await.unwrap();
    let deployer = Arc::new(BatchDeployer::new(
        BatchDeployerConfig::default(),
        client.clone(),
        storage.clone(),
        pool.clone(),
        Arc::new(LiveSyncDeployer::new(
            pool.clone(),
            DEFAULT_FLUSH_CONCURRENCY,
            catalyst::content::sync_deployer::SyncValidation::AuthChainOnly,
        )),
        Arc::new(LiveFailedDeploymentsStore::new(pool.clone())),
        bloom,
    ));
    let now = chrono::Utc::now().timestamp_millis();
    let orchestrator = SyncOrchestrator::new(
        SyncOrchestratorConfig {
            from_timestamp: now,
            pointer_changes_wait_time_ms: POLL_MS,
            syncing_reconnect_time_ms: 50,
            phased_sync: false,
            ..Default::default()
        },
        SyncDependencies {
            http_client: client,
            storage,
            deployer,
            processed_store: Arc::new(LiveProcessedSnapshotStore::new(pool.clone())),
            deployment_repo: repo.clone(),
            status,
            control: SyncControl::default(),
        },
    );
    let control = orchestrator.control();
    let handle = orchestrator
        .sync_with_servers([peer_url.clone()].into_iter().collect())
        .await
        .unwrap();
    wait_until("the steady state", || {
        orchestrator.state() == SyncState::Syncing
    })
    .await;
    let polls = calls.clone();
    wait_until("three steady-state polls", || {
        polls.load(Ordering::SeqCst) >= 3
    })
    .await;

    control.pause();
    // The poll in flight (at most one) lands; then the stream must be parked.
    tokio::time::sleep(Duration::from_millis(POLL_MS * 3)).await;
    let at_pause = calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(POLL_MS * 8)).await;
    let while_paused = calls.load(Ordering::SeqCst) - at_pause;
    let cursor_while_paused = repo.get_server_sync_cursor(&peer_url).await.unwrap();

    control.resume();
    let polls = calls.clone();
    let resumed_from = at_pause;
    wait_until("polls after the resume", || {
        polls.load(Ordering::SeqCst) >= resumed_from + 3
    })
    .await;

    orchestrator.stop().await;
    drop(handle);
    peer_http.abort();
    db.teardown().await;
    let _ = std::fs::remove_dir_all(temp);

    assert_eq!(
        while_paused, 0,
        "a paused node makes no peer fetch over eight poll intervals"
    );
    assert!(
        cursor_while_paused.is_none_or(|c| c >= now),
        "a cursor the stream confirmed lands before it parks: {cursor_while_paused:?}"
    );
}
