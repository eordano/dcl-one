// from: catalyrst/crates/catalyrst-server/tests/support/phased_bootstrap.rs
//! A real bootstrap against an in-process peer: the phased hand-off (non-profile slice,
//! then profiles) must neither skip a profile older than the pointer-changes replay window
//! nor publish a per-server cursor before the marking pass proved the snapshots complete;
//! a node interrupted mid-bootstrap must restart from what it persisted, replaying
//! rather than skipping, without duplicating what it already holds; and a peer held back
//! in snapshot bootstrap keeps the node unready until its snapshots are fully deployed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use catalyst::content::batch_deployer::{BatchDeployer, BatchDeployerConfig};
use catalyst::content::bloom_filter::BloomFilter;
use catalyst::content::failed_deployments_store::LiveFailedDeploymentsStore;
use catalyst::content::sync::orchestrator::{
    SyncControl, SyncDependencies, SyncOrchestratorConfig,
};
use catalyst::content::sync::pointer_changes::POINTER_CHANGES_OVERLAP_MS;
use catalyst::content::sync::{
    LiveDeploymentRepository, LiveProcessedSnapshotStore, SyncOrchestrator, SyncState, SyncStatus,
};
use catalyst::content::sync_deployer::{DEFAULT_FLUSH_CONCURRENCY, LiveSyncDeployer};
use catalyst::storage::Storage;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Semaphore;

use super::auth;

/// One peer with a profile (older than the 20-minute replay window) and a scene, an empty
/// snapshot list, and a gate that blocks the SECOND `/snapshots` call (phase 4, the profile
/// slice) until the test lets it through.
struct Peer {
    url: String,
    http: tokio::task::JoinHandle<()>,
    profiles_entered: Arc<Semaphore>,
    profiles_resume: Arc<Semaphore>,
}

async fn spawn_peer(now: i64) -> Peer {
    let mut files = HashMap::new();
    let mut deltas = Vec::new();
    for (kind, timestamp) in [("profile", now - 1_800_000), ("scene", now)] {
        let bytes = serde_json::to_vec(&json!({
            "version": "v3", "type": kind, "timestamp": timestamp,
            "pointers": [format!("{kind}-pointer")], "content": [], "metadata": {}
        }))
        .unwrap();
        let id = catalyst::auth::hash_bytes_v1(&bytes);
        let auth_chain = serde_json::to_value(auth::signed_chain(&id).await).unwrap();
        files.insert(id.clone(), bytes);
        deltas.push(json!({
            "entityId": id, "entityType": kind, "entityTimestamp": timestamp,
            "localTimestamp": timestamp, "authChain": auth_chain,
            "pointers": [format!("{kind}-pointer")]
        }));
    }
    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    let profiles_entered = Arc::new(Semaphore::new(0));
    let profiles_resume = Arc::new(Semaphore::new(0));
    let app = Router::new()
        .route("/snapshots", get({
            let calls = snapshot_calls.clone();
            let entered = profiles_entered.clone();
            let resume = profiles_resume.clone();
            move || {
                let (calls, entered, resume) = (calls.clone(), entered.clone(), resume.clone());
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                        entered.add_permits(1);
                        resume.acquire().await.unwrap().forget();
                    }
                    Json(json!([]))
                }
            }
        }))
        .route("/pointer-changes", get(move |Query(query): Query<HashMap<String, String>>| {
            let deltas = deltas.clone();
            async move {
                let from = query["from"].parse::<i64>().unwrap();
                Json(json!({"deltas": deltas.into_iter().filter(|d| d["localTimestamp"].as_i64().unwrap() >= from).collect::<Vec<_>>(), "pagination": {"moreData": false}}))
            }
        }))
        .route("/contents/{id}", get(|State(files): State<Arc<HashMap<String, Vec<u8>>>>, Path(id): Path<String>| async move {
            files[&id].clone()
        }))
        .with_state(Arc::new(files));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let http = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Peer {
        url,
        http,
        profiles_entered,
        profiles_resume,
    }
}

/// The same peer with a snapshot list that is unreadable (one garbage entry) until `clean`
/// is set, which holds the server in snapshot bootstrap. `snapshot_calls` counts the fetches.
struct HeldPeer {
    url: String,
    http: tokio::task::JoinHandle<()>,
    clean: Arc<AtomicBool>,
    snapshot_calls: Arc<AtomicUsize>,
}

async fn spawn_held_peer(now: i64) -> HeldPeer {
    let mut files = HashMap::new();
    let mut deltas = Vec::new();
    for (kind, timestamp) in [("profile", now - 1_800_000), ("scene", now)] {
        let bytes = serde_json::to_vec(&json!({
            "version": "v3", "type": kind, "timestamp": timestamp,
            "pointers": [format!("{kind}-pointer")], "content": [], "metadata": {}
        }))
        .unwrap();
        let id = catalyst::auth::hash_bytes_v1(&bytes);
        let auth_chain = serde_json::to_value(auth::signed_chain(&id).await).unwrap();
        files.insert(id.clone(), bytes);
        deltas.push(json!({
            "entityId": id, "entityType": kind, "entityTimestamp": timestamp,
            "localTimestamp": timestamp, "authChain": auth_chain,
            "pointers": [format!("{kind}-pointer")]
        }));
    }
    let clean = Arc::new(AtomicBool::new(false));
    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/snapshots", get({
            let (clean, calls) = (clean.clone(), snapshot_calls.clone());
            move || {
                let (clean, calls) = (clean.clone(), calls.clone());
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if clean.load(Ordering::SeqCst) {
                        Json(json!([]))
                    } else {
                        Json(json!([{"hash": "not a cid"}]))
                    }
                }
            }
        }))
        .route("/pointer-changes", get(move |Query(query): Query<HashMap<String, String>>| {
            let deltas = deltas.clone();
            async move {
                let from = query["from"].parse::<i64>().unwrap();
                Json(json!({"deltas": deltas.into_iter().filter(|d| d["localTimestamp"].as_i64().unwrap() >= from).collect::<Vec<_>>(), "pagination": {"moreData": false}}))
            }
        }))
        .route("/contents/{id}", get(|State(files): State<Arc<HashMap<String, Vec<u8>>>>, Path(id): Path<String>| async move {
            files[&id].clone()
        }))
        .with_state(Arc::new(files));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let http = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    HeldPeer {
        url,
        http,
        clean,
        snapshot_calls,
    }
}

/// A node over `pool` and the storage root `temp`, as the integrator wires it: bloom seeded
/// from the deployments the database already holds, one status handle, ONE storage root.
async fn node(
    pool: &PgPool,
    temp: &std::path::Path,
    floor: i64,
) -> (
    SyncOrchestrator,
    Arc<LiveDeploymentRepository>,
    Arc<SyncStatus>,
) {
    node_with(
        pool,
        temp,
        SyncOrchestratorConfig {
            from_timestamp: floor,
            ..Default::default()
        },
    )
    .await
}

async fn node_with(
    pool: &PgPool,
    temp: &std::path::Path,
    config: SyncOrchestratorConfig,
) -> (
    SyncOrchestrator,
    Arc<LiveDeploymentRepository>,
    Arc<SyncStatus>,
) {
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
    let orchestrator = SyncOrchestrator::new(
        config,
        SyncDependencies {
            http_client: client,
            storage,
            deployer,
            processed_store: Arc::new(LiveProcessedSnapshotStore::new(pool.clone())),
            deployment_repo: repo.clone(),
            status: status.clone(),
            control: SyncControl::default(),
        },
    );
    (orchestrator, repo, status)
}

async fn wait_for(orchestrator: &SyncOrchestrator, state: SyncState) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while orchestrator.state() != state {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the node never reached {state:?}"));
}

/// Whether `cursor` is where a finished stream settles when its newest row is `now`: the
/// overlap, plus the poll's span (well under a minute here), behind it.
fn settled(cursor: Option<i64>, now: i64) -> bool {
    let top = now - POINTER_CHANGES_OVERLAP_MS;
    cursor.is_some_and(|c| (top - 60_000..=top).contains(&c))
}

async fn deployed_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT entity_type FROM content.deployments ORDER BY entity_type")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn failures(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT error_description FROM content.failed_deployments")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn phased_catchup_does_not_skip_profiles_older_than_its_replay_window() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("catalyst::content::sync=debug")
        .with_test_writer()
        .try_init();
    let Some(db) = super::db::scratch_database().await else {
        return;
    };
    let pool = db.pool.clone();

    let now = chrono::Utc::now().timestamp_millis();
    let floor = now - 3_600_000;
    let peer = spawn_peer(now).await;
    let temp = std::env::temp_dir().join(format!("phased-bootstrap-{}", uuid::Uuid::new_v4()));
    let (orchestrator, repo, status) = node(&pool, &temp, floor).await;
    repo.advance_server_sync_cursor(&peer.url, floor)
        .await
        .unwrap();
    let handle = orchestrator
        .sync_with_servers([peer.url.clone()].into_iter().collect())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), peer.profiles_entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(
        orchestrator.state(),
        SyncState::PartiallySynced {
            ready_types: ["emote", "outfits", "scene", "store", "wearable"]
                .iter()
                .map(|t| t.to_string())
                .collect()
        },
        "the non-profile slice is served while profiles bootstrap"
    );
    let partial_cursor = repo.get_server_sync_cursor(&peer.url).await.unwrap();
    peer.profiles_resume.add_permits(1);
    wait_for(&orchestrator, SyncState::Syncing).await;
    orchestrator.stop().await;
    drop(handle);
    peer.http.abort();
    let types = deployed_types(&pool).await;
    let final_cursor = repo.get_server_sync_cursor(&peer.url).await.unwrap();
    let failures = failures(&pool).await;
    let heartbeat = status.heartbeat_ms();
    db.teardown().await;
    std::fs::remove_dir_all(temp).unwrap();
    assert!(
        failures.is_empty(),
        "fixture deployments failed: {failures:?}"
    );
    assert_eq!(
        types,
        ["profile", "scene"],
        "a profile outside the 20-minute replay window must survive the phase handoff"
    );
    assert_eq!(
        partial_cursor,
        Some(floor),
        "a partial snapshot pass cannot publish a shared cursor"
    );
    assert!(settled(final_cursor, now), "{final_cursor:?}");
    assert!(
        heartbeat.is_some_and(|hb| hb >= now),
        "the orchestrator's heartbeat reaches the status the routes read"
    );
}

/// Interrupted while the profile slice is still bootstrapping, nothing past the floor was
/// persisted (the non-marking pass deployed rows but published no cursor); a fresh process
/// over the same database and storage root resumes from that floor, replays the window,
/// deploys nothing twice and finishes what the first run started.
#[tokio::test]
async fn an_interrupted_bootstrap_restarts_from_the_persisted_cursor_without_duplicates() {
    let Some(db) = super::db::scratch_database().await else {
        return;
    };
    let pool = db.pool.clone();

    let now = chrono::Utc::now().timestamp_millis();
    let floor = now - 3_600_000;
    let peer = spawn_peer(now).await;
    let temp = std::env::temp_dir().join(format!("resume-bootstrap-{}", uuid::Uuid::new_v4()));

    let (first, repo, _) = node(&pool, &temp, floor).await;
    repo.advance_server_sync_cursor(&peer.url, floor)
        .await
        .unwrap();
    let handle = first
        .sync_with_servers([peer.url.clone()].into_iter().collect())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), peer.profiles_entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    first.stop().await;
    drop(handle);
    drop(first);
    let interrupted_types = deployed_types(&pool).await;
    let interrupted_cursor = repo.get_server_sync_cursor(&peer.url).await.unwrap();
    peer.profiles_resume.add_permits(1);

    let (second, repo, status) = node(&pool, &temp, floor).await;
    let handle = second
        .sync_with_servers([peer.url.clone()].into_iter().collect())
        .await
        .unwrap();
    wait_for(&second, SyncState::Syncing).await;
    second.stop().await;
    drop(handle);
    peer.http.abort();

    let types = deployed_types(&pool).await;
    let final_cursor = repo.get_server_sync_cursor(&peer.url).await.unwrap();
    let frontier = repo.get_sync_frontier().await.unwrap();
    let failures = failures(&pool).await;
    db.teardown().await;
    std::fs::remove_dir_all(temp).unwrap();

    assert_eq!(
        interrupted_types,
        ["profile", "scene"],
        "the first run's non-marking pass had already deployed both entities"
    );
    assert_eq!(
        interrupted_cursor,
        Some(floor),
        "an interrupted bootstrap leaves the cursor where the last proven pass left it"
    );
    assert!(
        failures.is_empty(),
        "replaying already-deployed entities must not fail them: {failures:?}"
    );
    assert_eq!(
        types,
        ["profile", "scene"],
        "the replay after a restart deploys nothing twice"
    );
    assert!(settled(final_cursor, now), "{final_cursor:?}");
    assert!(settled(Some(frontier), now), "{frontier}");
    assert_eq!(status.sync_frontier_ms(), Some(frontier));
}

/// The peer's snapshot list is unreadable, so the node owes it every snapshot it could not
/// read: both phased announcements are withheld and `/about` stays 503 with nothing deployed,
/// until the straggler retry reads a clean list, deploys the catch-up and publishes Syncing.
#[tokio::test]
async fn a_peer_held_back_in_snapshot_bootstrap_keeps_the_node_unready() {
    let Some(db) = super::db::scratch_database().await else {
        return;
    };
    let pool = db.pool.clone();

    let now = chrono::Utc::now().timestamp_millis();
    let floor = now - 3_600_000;
    let peer = spawn_held_peer(now).await;
    let temp = std::env::temp_dir().join(format!("held-bootstrap-{}", uuid::Uuid::new_v4()));
    let (orchestrator, repo, _) = node_with(
        &pool,
        &temp,
        SyncOrchestratorConfig {
            from_timestamp: floor,
            bootstrap_reconnect_time_ms: 50,
            bootstrap_max_reconnect_ms: 50,
            ..Default::default()
        },
    )
    .await;
    let handle = orchestrator
        .sync_with_servers([peer.url.clone()].into_iter().collect())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(15), async {
        while peer.snapshot_calls.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the held peer is re-fetched by both phases and the straggler retry");
    let state_while_held = orchestrator.state();
    let types_while_held = deployed_types(&pool).await;
    let cursor_while_held = repo.get_server_sync_cursor(&peer.url).await.unwrap();

    peer.clean.store(true, Ordering::SeqCst);
    wait_for(&orchestrator, SyncState::Syncing).await;
    orchestrator.stop().await;
    drop(handle);
    peer.http.abort();

    let types = deployed_types(&pool).await;
    let final_cursor = repo.get_server_sync_cursor(&peer.url).await.unwrap();
    let failures = failures(&pool).await;
    db.teardown().await;
    std::fs::remove_dir_all(temp).unwrap();

    assert_eq!(
        state_while_held,
        SyncState::Bootstrapping,
        "a node that owes its only peer's snapshots is not partially synced either"
    );
    assert!(types_while_held.is_empty(), "{types_while_held:?}");
    assert_eq!(
        cursor_while_held, None,
        "a held peer's cursor is never published"
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(types, ["profile", "scene"]);
    assert!(settled(final_cursor, now), "{final_cursor:?}");
}
