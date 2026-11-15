//! A changed peer set (the `SYNC_FROM_DAO` list refresh) reaches the running
//! sync job through `run_sync_job`'s `watch`: after the node is `Syncing`, a
//! new peer is streamed, a dropped peer's stream stops and the retry list
//! follows; during the first bootstrap, dropping a peer that never answers
//! releases the node into `Syncing`. Against in-process peers whose
//! `/pointer-changes` counts its calls. Needs CATALYST_TEST_DATABASE_URL (one
//! scratch database per test).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use catalyst::content::batch_deployer::{BatchDeployer, BatchDeployerConfig};
use catalyst::content::bloom_filter::BloomFilter;
use catalyst::content::failed_deployments_store::LiveFailedDeploymentsStore;
use catalyst::content::sync::orchestrator::{
    SyncControl, SyncDependencies, SyncOrchestratorConfig, run_sync_job,
};
use catalyst::content::sync::{
    LiveDeploymentRepository, LiveProcessedSnapshotStore, SyncOrchestrator, SyncState, SyncStatus,
};
use catalyst::content::sync_deployer::{DEFAULT_FLUSH_CONCURRENCY, LiveSyncDeployer};
use catalyst::jobs::Jobs;
use catalyst::storage::Storage;
use serde_json::json;
use tokio::sync::{RwLock, watch};

#[path = "support/db.rs"]
mod db;

const POLL_MS: u64 = 100;

struct Peer {
    url: String,
    calls: Arc<AtomicUsize>,
    http: tokio::task::JoinHandle<()>,
}

/// A peer with nothing to offer (an empty snapshot list and pointer-changes
/// page), or, when `down`, one whose every answer is a 500.
async fn spawn_peer(down: bool) -> Peer {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = if down {
        Router::new().fallback(|| async { StatusCode::INTERNAL_SERVER_ERROR })
    } else {
        Router::new()
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
            })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let http = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Peer { url, calls, http }
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn orchestrator(pool: &sqlx::PgPool, temp: &std::path::Path) -> Arc<SyncOrchestrator> {
    let storage = Arc::new(Storage::from_config(temp).await.unwrap());
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
    Arc::new(SyncOrchestrator::new(
        SyncOrchestratorConfig {
            from_timestamp: chrono::Utc::now().timestamp_millis(),
            request_max_retries: 1,
            request_retry_wait_ms: 10,
            pointer_changes_wait_time_ms: POLL_MS,
            bootstrap_reconnect_time_ms: 50,
            bootstrap_max_reconnect_ms: 200,
            syncing_reconnect_time_ms: 50,
            phased_sync: false,
            ..Default::default()
        },
        SyncDependencies {
            http_client: client,
            storage,
            deployer,
            processed_store: Arc::new(LiveProcessedSnapshotStore::new(pool.clone())),
            deployment_repo: repo,
            status,
            control: SyncControl::default(),
        },
    ))
}

fn set(urls: &[&str]) -> HashSet<String> {
    urls.iter().map(|u| u.to_string()).collect()
}

#[tokio::test]
async fn a_peer_change_after_the_bootstrap_swaps_the_streams_and_the_retry_list() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let temp = std::env::temp_dir().join(format!("sync-peers-{}", uuid::Uuid::new_v4()));
    let orchestrator = orchestrator(&db.pool, &temp).await;
    let (a, b) = (spawn_peer(false).await, spawn_peer(false).await);

    let (tx, rx) = watch::channel(set(&[&a.url]));
    let retry_peers = Arc::new(RwLock::new(vec![a.url.clone()]));
    let jobs = Jobs::new();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let job = jobs.spawn("sync", {
        let (orchestrator, retry_peers) = (orchestrator.clone(), retry_peers.clone());
        let observed = callbacks.clone();
        move |ctx| {
            run_sync_job(orchestrator, rx, retry_peers, ctx, move || {
                observed.fetch_add(1, Ordering::SeqCst);
            })
        }
    });
    wait_until("the steady state", || {
        orchestrator.state() == SyncState::Syncing
    })
    .await;
    // The bootstrap task has ended by now (every peer streams); the job must
    // still apply a change.
    wait_until("the bootstrap callback", || {
        callbacks.load(Ordering::SeqCst) == 1
    })
    .await;

    tx.send(set(&[&b.url])).unwrap();
    let b_calls = b.calls.clone();
    wait_until("the new peer's stream", || {
        b_calls.load(Ordering::SeqCst) >= 3
    })
    .await;
    let a_after_drop = a.calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(POLL_MS * 6)).await;
    let a_late = a.calls.load(Ordering::SeqCst) - a_after_drop;
    let retry = retry_peers.read().await.clone();
    let state = orchestrator.state();
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);

    job.cancel();
    a.http.abort();
    b.http.abort();
    db.teardown().await;
    let _ = std::fs::remove_dir_all(temp);

    assert!(
        a_late <= 1,
        "the dropped peer's stream stopped ({a_late} late polls)"
    );
    assert_eq!(
        retry,
        vec![b.url.clone()],
        "the retry list follows the peers"
    );
    assert_eq!(
        state,
        SyncState::Syncing,
        "a peer change keeps the node Syncing"
    );
}

#[tokio::test]
async fn dropping_a_dead_peer_during_the_first_bootstrap_releases_the_node() {
    let Some(db) = db::scratch_database().await else {
        return;
    };
    let temp = std::env::temp_dir().join(format!("sync-peers-{}", uuid::Uuid::new_v4()));
    let orchestrator = orchestrator(&db.pool, &temp).await;
    let (good, dead) = (spawn_peer(false).await, spawn_peer(true).await);

    let (tx, rx) = watch::channel(set(&[&good.url, &dead.url]));
    let retry_peers = Arc::new(RwLock::new(Vec::new()));
    let jobs = Jobs::new();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let job = jobs.spawn("sync", {
        let orchestrator = orchestrator.clone();
        let observed = callbacks.clone();
        move |ctx| {
            run_sync_job(orchestrator, rx, retry_peers, ctx, move || {
                observed.fetch_add(1, Ordering::SeqCst);
            })
        }
    });
    let good_calls = good.calls.clone();
    wait_until("the good peer's stream", || {
        good_calls.load(Ordering::SeqCst) >= 2
    })
    .await;
    let held = orchestrator.state();
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);

    tx.send(set(&[&good.url])).unwrap();
    let orch = orchestrator.clone();
    wait_until("the steady state once the dead peer is dropped", || {
        orch.state() == SyncState::Syncing && callbacks.load(Ordering::SeqCst) == 1
    })
    .await;

    job.cancel();
    good.http.abort();
    dead.http.abort();
    db.teardown().await;
    let _ = std::fs::remove_dir_all(temp);

    assert_eq!(
        held,
        SyncState::Bootstrapping,
        "a peer that never answers holds the node in bootstrap"
    );
}
