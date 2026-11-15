use super::SnapshotPassVerdict::*;
use super::*;

fn snapshot(hash: &str, end: Timestamp) -> SnapshotMetadata {
    SnapshotMetadata {
        hash: hash.to_string(),
        time_range: TimeRange {
            init_timestamp: 0,
            end_timestamp: end,
        },
        number_of_entities: 1,
        replaced_snapshot_hashes: None,
        generation_timestamp: end,
    }
}

/// Returns the servers the fetch held back.
fn fetch(
    server: &str,
    snapshots: Vec<SnapshotMetadata>,
    discarded: usize,
    genesis: Timestamp,
    last_ts: &mut HashMap<String, Timestamp>,
    by_hash: &mut SnapshotsByHash,
) -> HashSet<String> {
    let mut held = HashSet::new();
    apply_snapshot_fetch(
        server,
        snapshots::SnapshotsFromServer {
            snapshots,
            discarded,
        },
        genesis,
        last_ts,
        by_hash,
        &mut held,
    );
    held
}

#[test]
fn discarded_snapshot_entries_hold_the_server_back() {
    let (mut last_ts, mut by_hash) = (HashMap::new(), HashMap::new());
    let snaps = || vec![snapshot("QmClean", 1_700_000_000_000)];
    let held = fetch(
        "https://peer.test",
        snaps(),
        2,
        0,
        &mut last_ts,
        &mut by_hash,
    );
    assert!(
        held.contains("https://peer.test"),
        "a partially-readable snapshot list must hold the server in bootstrap"
    );
    assert!(by_hash.contains_key("QmClean"));

    let held_clean = fetch("https://ok.test", snaps(), 0, 0, &mut last_ts, &mut by_hash);
    assert!(held_clean.is_empty());
    assert_eq!(last_ts["https://ok.test"], 1_700_000_000_000);
    assert_eq!(by_hash["QmClean"].0.len(), 2);
    assert_eq!(by_hash["QmClean"].1.len(), 2);
}

#[test]
fn empty_snapshot_list_falls_back_to_genesis() {
    let (mut last_ts, mut by_hash) = (HashMap::new(), HashMap::new());
    let held = fetch(
        "https://new.test",
        vec![],
        0,
        42,
        &mut last_ts,
        &mut by_hash,
    );
    assert_eq!(last_ts["https://new.test"], 42);
    assert!(held.is_empty());
}

#[test]
fn failed_pass_always_holds_regardless_of_history() {
    for bits in 0..16u8 {
        let b = |i: u8| bits & (1 << i) != 0;
        assert_eq!(snapshot_pass_verdict(true, b(0), b(1), b(2), b(3)), Failed);
    }
}

#[test]
fn non_marking_slice_remembers_clean_snapshots() {
    assert_eq!(
        snapshot_pass_verdict(false, false, false, false, false),
        CleanUnmarked
    );
    assert_eq!(
        snapshot_pass_verdict(false, false, false, true, false),
        CleanUnmarked
    );
}

#[test]
fn clean_unfiltered_pass_retires_even_previous_failures() {
    assert_eq!(snapshot_pass_verdict(false, true, true, true, true), Retire);
    assert_eq!(
        snapshot_pass_verdict(false, true, true, false, false),
        Retire
    );
}

#[test]
fn filtered_marking_pass_needs_the_complementary_slice_proven() {
    assert_eq!(
        snapshot_pass_verdict(false, true, false, false, true),
        Retire
    );
    assert_eq!(
        snapshot_pass_verdict(false, true, false, false, false),
        Unproven
    );
    assert_eq!(
        snapshot_pass_verdict(false, true, false, true, true),
        Unproven
    );
}

#[test]
fn allowlist_defines_what_a_complete_pass_is() {
    let set = |types: &[&str]| -> HashSet<String> { types.iter().map(|s| s.to_string()).collect() };
    assert!(pass_covers_all_synced_types(None, None));
    assert!(pass_covers_all_synced_types(None, Some(&set(&["scene"]))));
    assert!(!pass_covers_all_synced_types(Some(&set(&["scene"])), None));
    assert!(pass_covers_all_synced_types(
        Some(&set(&["scene"])),
        Some(&set(&["scene"]))
    ));
    assert!(pass_covers_all_synced_types(
        Some(&set(&["scene", "wearable"])),
        Some(&set(&["scene"]))
    ));
    assert!(!pass_covers_all_synced_types(
        Some(&set(&["profile"])),
        Some(&set(&["scene", "profile"]))
    ));
}

#[test]
fn a_server_resumes_from_its_own_cursor_even_when_it_lags_the_global_frontier() {
    assert_eq!(resume_point(Some(1_000), 9_000), 1_000);
    assert_eq!(resume_point(Some(9_500), 9_000), 9_500);
}

#[test]
fn a_server_with_no_cursor_row_falls_back_to_the_global_frontier() {
    assert_eq!(resume_point(None, 9_000), 9_000);
    assert_eq!(
        resume_point(None, 0),
        0,
        "and to the config floor when that is zero"
    );
}

#[test]
fn deployment_report_incomplete_until_all_acknowledged() {
    let report = DeploymentReport::default();
    assert!(report.is_complete(), "an empty run is trivially complete");
    report.record_scheduled();
    report.record_scheduled();
    assert!(!report.is_complete());
    report.record_acknowledged();
    assert!(!report.is_complete());
    report.record_acknowledged();
    assert!(report.is_complete());
}

#[tokio::test]
async fn wait_for_bootstrap_returns_when_the_state_already_left_bootstrapping() {
    let status = Arc::new(SyncStatus::default());
    let handle = SyncHandle {
        bootstrap_done: Arc::new(Notify::new()),
        status: status.clone(),
        task: tokio::spawn(async {}),
    };
    assert!(
        futures::poll!(std::pin::pin!(handle.wait_for_bootstrap())).is_pending(),
        "a Bootstrapping node keeps the waiter waiting"
    );

    status.set_state(SyncState::PartiallySynced {
        ready_types: vec!["scene".into()],
    });
    handle.bootstrap_done.notify_waiters();
    futures::FutureExt::now_or_never(handle.wait_for_bootstrap())
        .expect("a transition that already happened must not be missed");
}

#[tokio::test]
async fn bootstrap_callbacks_require_success_and_keep_parent_cancellation_scope() {
    use crate::batch_deployer::BatchDeployerConfig;
    use crate::bloom_filter::BloomFilter;
    use crate::failed_deployments_store::LiveFailedDeploymentsStore;
    use crate::jobs::Jobs;
    use crate::sync_deployer::{LiveSyncDeployer, SyncValidation};
    use std::sync::atomic::AtomicUsize;

    for (state, cancelled, expected) in [
        (SyncState::Syncing, false, 1),
        (
            SyncState::PartiallySynced {
                ready_types: vec!["scene".into()],
            },
            false,
            1,
        ),
        (SyncState::Failed, false, 0),
        (SyncState::Syncing, true, 0),
    ] {
        let (storage, temp) = crate::sync_test_support::temp_storage("bootstrap-callback").await;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/unused")
            .unwrap();
        let status = Arc::new(SyncStatus::default());
        let client = reqwest::Client::new();
        let deployer = Arc::new(BatchDeployer::new(
            BatchDeployerConfig::default(),
            client.clone(),
            storage.clone(),
            pool.clone(),
            Arc::new(LiveSyncDeployer::new(
                pool.clone(),
                1,
                SyncValidation::AuthChainOnly,
            )),
            Arc::new(LiveFailedDeploymentsStore::new(pool.clone())),
            BloomFilter::with_capacity(1),
        ));
        let orchestrator = Arc::new(SyncOrchestrator::new(
            SyncOrchestratorConfig::default(),
            SyncDependencies {
                http_client: client,
                storage,
                deployer,
                processed_store: Arc::new(LiveProcessedSnapshotStore::new(pool.clone())),
                deployment_repo: Arc::new(LiveDeploymentRepository::new(pool, status.clone())),
                status,
                control: SyncControl::default(),
            },
        ));
        let jobs = Arc::new(Jobs::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let terminated = Arc::new(AtomicUsize::new(0));
        let ended = terminated.clone();
        let registry = jobs.clone();
        let sync = orchestrator.clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        jobs.spawn("bootstrap-test", move |ctx| async move {
            let scope = ctx.clone();
            entered.send(ctx.cancel.clone()).unwrap();
            let (_peers_tx, peers) = tokio::sync::watch::channel(HashSet::new());
            let retry_peers = Arc::new(tokio::sync::RwLock::new(Vec::new()));
            run_sync_job(sync, peers, retry_peers, ctx, || {
                observed.fetch_add(1, SeqCst);
                registry.spawn_scoped(&scope, "after-bootstrap", |child| async move {
                    child.cancel.cancelled().await;
                    ended.fetch_add(1, SeqCst);
                    Ok(())
                });
            })
            .await
        });
        let cancellation = started.await.unwrap();
        assert!(
            crate::shared::test_wait::until(async || jobs
                .report()
                .iter()
                .any(|job| job.name == "bootstrap-test"
                    && job.progress.as_deref() == Some("bootstrapping")))
            .await
        );
        if cancelled {
            cancellation.cancel();
        }
        orchestrator.inner.set_state(state);
        if !cancelled {
            assert!(
                crate::shared::test_wait::until(async || jobs.report().iter().any(|job| job.name
                    == "bootstrap-test"
                    && job
                        .progress
                        .as_deref()
                        .is_some_and(|value| value.starts_with("bootstrapped;"))))
                .await
            );
        }
        assert_eq!(
            jobs.report()
                .iter()
                .filter(|job| job.name == "after-bootstrap")
                .count(),
            expected
        );
        jobs.shutdown(Duration::from_secs(5)).await;
        assert_eq!(calls.load(SeqCst), expected);
        assert_eq!(terminated.load(SeqCst), expected);
        assert!(orchestrator.inner.is_stopped());
        let _ = std::fs::remove_dir_all(temp);
    }
}
