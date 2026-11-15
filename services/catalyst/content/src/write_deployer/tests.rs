//! Database-backed tests run on the process's scratch database (`db::scratch`,
//! created from `CATALYST_TEST_DATABASE_URL`) inside the real `content`
//! schema, with uuid-prefixed ids and pointers so concurrent tests never
//! meet, and are skipped without the variable.

use super::*;
use crate::auth::AuthLinkType;
use crate::storage::hex_prefix;
use async_trait::async_trait;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const STORED_HASH: &str = "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenosa7776";

async fn test_pool(max_connections: u32) -> Option<PgPool> {
    crate::db::scratch::pool(max_connections).await
}

fn lazy_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
        .unwrap()
}

#[tokio::test]
async fn upload_hash_capacity_survives_request_cancellation() {
    let limit = Arc::new(Semaphore::new(1));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let task_limit = Arc::clone(&limit);
    let request = tokio::spawn(async move {
        bounded_upload_hash(task_limit, move || {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        })
        .await
    });
    started_rx.await.unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(limit.available_permits(), 0);
    let mut waiting = std::pin::pin!(bounded_upload_hash(Arc::clone(&limit), || 42));
    assert!(
        futures::poll!(&mut waiting).is_pending(),
        "the cancelled request's hash still holds the only permit"
    );
    finish_tx.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap(),
        42
    );
    assert_eq!(limit.available_permits(), 1);
}

#[tokio::test]
async fn deployment_files_reuse_both_cid_versions_and_deduplicate_references() {
    let entity_bytes = Bytes::from_static(b"entity");
    let first = Bytes::from_static(b"first content");
    let second = Bytes::from(vec![42; 300_000]);
    let unreferenced = Bytes::from_static(b"unreferenced");
    let uploads = hash_uploads(vec![
        entity_bytes.clone(),
        first.clone(),
        second.clone(),
        unreferenced,
    ])
    .await
    .unwrap();
    let missing = hash_bytes_v1(b"already stored");
    let entity: Entity = serde_json::from_value(serde_json::json!({
        "id": uploads[0].v1,
        "version": "v3",
        "type": "scene",
        "pointers": ["0,0"],
        "timestamp": 1,
        "content": [
            {"file": "first", "hash": uploads[1].v0},
            {"file": "alias", "hash": uploads[1].v0},
            {"file": "second", "hash": uploads[2].v1},
            {"file": "existing", "hash": missing}
        ]
    }))
    .unwrap();
    let files = deployment_files(&entity, &entity_bytes, &uploads);
    assert_eq!(files.len(), 3);
    assert_eq!(files[&entity.id], entity_bytes);
    assert_eq!(files[&uploads[1].v0], first);
    assert_eq!(files[&uploads[2].v1], second);
    assert!(!files.contains_key(&missing));
    assert!(!files.contains_key(&uploads[3].v1));
}

#[tokio::test]
async fn deployment_storage_drains_writes_and_preserves_atomic_files_on_failure() {
    let (deployer, tmp) = deployer_over_temp_storage("parallel-write").await;
    let failed_hash = hash_bytes_v1(b"failed file");
    let failed_path = deployer
        .storage
        .root()
        .join(hex_prefix(&failed_hash))
        .join(&failed_hash);
    tokio::fs::create_dir_all(&failed_path).await.unwrap();
    let mut files = HashMap::from([(failed_hash, Bytes::from_static(b"failed file"))]);
    let expected: Vec<_> = (0..24)
        .map(|index| {
            let bytes = Bytes::from(format!("stored content {index}"));
            (hash_bytes_v1(&bytes), bytes)
        })
        .collect();
    files.extend(expected.iter().cloned());
    let error = store_deployment_files(&deployer.storage, files)
        .await
        .unwrap_err();
    assert!(error.contains("failed to store deployment file"));
    assert!(tokio::fs::metadata(&failed_path).await.unwrap().is_dir());
    for (hash, bytes) in expected {
        assert_eq!(deployer.storage.retrieve(&hash).await.unwrap(), Some(bytes));
    }
    // Every write ran to completion, so the failed one's staging file is already gone.
    let shard = failed_path.parent().unwrap();
    let mut entries = tokio::fs::read_dir(shard).await.unwrap();
    let mut staged = Vec::new();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".tmp") {
            staged.push(name);
        }
    }
    assert!(
        staged.is_empty(),
        "failed concurrent store leaked a staging file: {staged:?}"
    );
    tokio::fs::remove_dir_all(tmp).await.unwrap();
}

struct NoEip1654;

#[async_trait]
impl Eip1654Validator for NoEip1654 {
    async fn validate_signature(
        &self,
        _contract_address: &str,
        _hash: &[u8],
        _signature: &[u8],
    ) -> Result<bool, AuthError> {
        Ok(false)
    }
}

async fn temp_storage(tag: &str) -> (Arc<Storage>, std::path::PathBuf) {
    let tmp = std::env::temp_dir().join(format!(
        "catalyst-write-deployer-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Storage::from_config(&tmp).await.unwrap();
    storage
        .store(STORED_HASH, Bytes::from_static(b"content"))
        .await
        .unwrap();
    (Arc::new(storage), tmp)
}

async fn deployer_over_temp_storage(tag: &str) -> (WriteDeployer, std::path::PathBuf) {
    let (storage, tmp) = temp_storage(tag).await;
    (
        WriteDeployer::with_partial_validation_for_tests(
            lazy_pool(),
            storage,
            Some(Arc::new(NoEip1654)),
        ),
        tmp,
    )
}

#[cfg(unix)]
fn seal_shard(storage: &Storage, hash: &str) -> std::path::PathBuf {
    let shard = storage.root().join(hex_prefix(hash));
    std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o000)).unwrap();
    shard
}

#[cfg(unix)]
fn unseal_shard(shard: &std::path::Path) {
    let _ = std::fs::set_permissions(shard, std::fs::Permissions::from_mode(0o755));
}

#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_shard_faults_the_existence_probe_instead_of_answering_absent() {
    let (deployer, tmp) = deployer_over_temp_storage("exist").await;
    assert_eq!(
        deployer
            .is_content_stored_already(&[STORED_HASH.to_string()])
            .await
            .unwrap()
            .get(STORED_HASH),
        Some(&true)
    );

    let shard = seal_shard(&deployer.storage, STORED_HASH);
    let result = deployer
        .is_content_stored_already(&[STORED_HASH.to_string()])
        .await;
    unseal_shard(&shard);
    let _ = std::fs::remove_dir_all(&tmp);

    let err = result.expect_err("damage this node cannot read is never a depositor's miss");
    assert!(err.contains("I/O error"), "unexpected cause: {err}");
}

#[tokio::test]
async fn an_id_that_cannot_name_content_is_absent_rather_than_a_fault() {
    let (deployer, tmp) = deployer_over_temp_storage("invalid").await;
    let bogus = "../../etc/passwd".to_string();

    let stored = deployer
        .is_content_stored_already(std::slice::from_ref(&bogus))
        .await
        .expect("a provably-invalid id is absence, not damage");
    assert_eq!(stored.get(&bogus), Some(&false));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn auth_chain_parse_rejects_unknown_link_types() {
    let chain: AuthChain = serde_json::from_value(serde_json::json!([
        {"type": "SIGNER", "payload": "0xabc"},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": "bafkrei...", "signature": "0xdeadbeef"}
    ]))
    .expect("known link types must convert");
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1].link_type, AuthLinkType::EcdsaSignedEntity);

    let eip: Result<AuthChain, _> = serde_json::from_value(serde_json::json!([
        {"type": "ECDSA_EIP_1654_SIGNED_ENTITY", "payload": "p", "signature": "0x00"}
    ]));
    assert!(eip.is_ok());

    let bad: Result<AuthChain, _> = serde_json::from_value(serde_json::json!([
        {"type": "TOTALLY_BOGUS", "payload": "p"}
    ]));
    assert!(bad.is_err());
}

#[tokio::test(start_paused = true)]
async fn rate_limiter_ttl_and_size() {
    let p = |s: &str| vec![s.to_string()];

    let mut bucket = TtlBucket::new(Duration::from_millis(50), 1000);
    assert!(!bucket.is_limited(&p("0,0")));
    bucket.record(&p("0,0"));
    assert!(bucket.is_limited(&p("0,0")));
    assert!(!bucket.is_limited(&p("1,1")));
    tokio::time::advance(Duration::from_millis(49)).await;
    assert!(bucket.is_limited(&p("0,0")), "still inside the ttl");
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(
        !bucket.is_limited(&p("0,0")),
        "entry should expire after ttl"
    );

    let mut small = TtlBucket::new(Duration::from_secs(3600), 2);
    small.record(&p("a"));
    small.record(&p("b"));
    small.record(&p("c"));
    assert!(
        small.is_limited(&p("unrelated")),
        "over max_size trips bucket"
    );
}

#[test]
fn a_reservation_holds_its_pointers_until_released() {
    let rl = DeployRateLimiter::with_reference_defaults();
    let ptrs = vec!["0,0".to_string(), "0,1".to_string(), "0,0".to_string()];

    let held = rl.reserve("scene", &ptrs, false).unwrap();
    assert!(
        rl.reserve("scene", &ptrs[1..2], false).is_none(),
        "a held pointer refuses a second reservation"
    );
    held.release();
    assert!(
        !rl.is_rate_limited("scene", &ptrs),
        "a released reservation frees its pointers"
    );

    drop(rl.reserve("scene", &ptrs, false).unwrap());
    assert!(
        rl.is_rate_limited("scene", &ptrs),
        "a reservation that is not released keeps its pointers"
    );

    let profile = vec!["0xabc".to_string()];
    drop(rl.reserve("profile", &profile, true).unwrap());
    assert!(rl.is_unchanged_limited(&profile));
    rl.clear();
    drop(rl.reserve("profile", &profile, true).unwrap());
    rl.buckets
        .lock()
        .get_mut("profile")
        .unwrap()
        .entries
        .clear();
    assert!(
        rl.reserve("profile", &profile, true).is_none(),
        "the unchanged bucket refuses on its own"
    );
    assert!(
        !rl.is_rate_limited("profile", &profile),
        "a refused unchanged reservation releases the type bucket"
    );
}

const DEPLOYER_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// A signed scene entity with no content files: its bytes, id and auth chain.
async fn signed_scene(pointers: &[String], timestamp: i64) -> (Bytes, String, Value) {
    signed_entity_of(serde_json::json!({
        "version": "v3",
        "type": "scene",
        "pointers": pointers,
        "timestamp": timestamp,
        "content": [],
        "metadata": {}
    }))
    .await
}

async fn signed_entity_of(entity: Value) -> (Bytes, String, Value) {
    use alloy::signers::{Signer, local::PrivateKeySigner};
    let bytes = Bytes::from(serde_json::to_vec(&entity).unwrap());
    let id = hash_bytes_v1(&bytes);
    let key: PrivateKeySigner = DEPLOYER_KEY.parse().unwrap();
    let signature = key.sign_message(id.as_bytes()).await.unwrap().to_string();
    let chain = serde_json::json!([
        {"type": "SIGNER", "payload": format!("{:#x}", key.address())},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": id, "signature": signature}
    ]);
    (bytes, id, chain)
}

#[tokio::test]
async fn a_deploy_with_a_bad_or_missing_signature_does_not_hold_its_pointers() {
    let (deployer, tmp) = deployer_over_temp_storage("bad-signature").await;
    let pointers = vec!["3,3".to_string()];
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (bytes, id, chain) = signed_scene(&pointers, now_ms).await;
    let forged = serde_json::json!([
        {"type": "SIGNER", "payload": "0x0000000000000000000000000000000000000001"},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": id, "signature": chain[1]["signature"]}
    ]);
    let unsigned = serde_json::json!([chain[0].clone()]);
    for bad in [forged, unsigned] {
        let err = deployer
            .deploy_entity(vec![bytes.clone()], &id, bad, "LOCAL")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DeployFailure::Rejected(_, errors) if errors[0].starts_with("The signature is invalid. ")),
            "{err:?}"
        );
        assert!(!deployer.is_rate_limited("scene", "3,3"));
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn a_rejected_deploy_with_an_oversized_pointer_list_does_not_fill_the_bucket() {
    let (deployer, tmp) = deployer_over_temp_storage("oversized").await;
    let pointers: Vec<String> = (0..1001).map(|y| format!("0,{y}")).collect();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (bytes, id, _) = signed_scene(&pointers, now_ms).await;
    let unsigned = serde_json::json!([
        {"type": "SIGNER", "payload": "0x0000000000000000000000000000000000000001"}
    ]);
    deployer
        .deploy_entity(vec![bytes], &id, unsigned, "LOCAL")
        .await
        .unwrap_err();
    assert!(
        !deployer.is_rate_limited("scene", "100,100"),
        "an unrelated pointer is still free"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn a_deploy_whose_write_fails_does_not_hold_its_pointers() {
    let (deployer, tmp) = deployer_over_temp_storage("persist-fails").await;
    let pointers = vec!["4,4".to_string()];
    let (bytes, id, chain) = signed_scene(&pointers, chrono::Utc::now().timestamp_millis()).await;
    let err = deployer
        .deploy_entity(vec![bytes], &id, chain, "LOCAL")
        .await
        .unwrap_err();
    assert!(matches!(err, DeployFailure::Unavailable(_)), "{err:?}");
    assert!(!deployer.is_rate_limited("scene", "4,4"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Two concurrent deploys of one entity (a client's retry while the first
/// is still in flight) both answer its stored timestamp; the second waits for
/// the first instead of meeting the pointer it holds.
#[tokio::test]
async fn two_concurrent_deploys_of_one_entity_both_answer_its_stored_timestamp() {
    let Some(pool) = test_pool(4).await else {
        return;
    };
    let (storage, tmp) = temp_storage("same-entity").await;
    let deployer =
        WriteDeployer::with_partial_validation_for_tests(pool, storage, Some(Arc::new(NoEip1654)));
    let pointers = Scope::new().pointers(&["8,8"]);
    let (bytes, id, chain) = signed_scene(&pointers, chrono::Utc::now().timestamp_millis()).await;
    let deploy = || deployer.deploy_entity(vec![bytes.clone()], &id, chain.clone(), "LOCAL");

    let (first, second) = tokio::join!(deploy(), deploy());
    let first = first.expect("the first copy deploys");
    let second = second.expect("the concurrent copy answers the stored timestamp");
    assert_eq!(first, second);
    assert_eq!(deployer.deployed_timestamp(&id).await.unwrap(), Some(first));
    assert!(deployer.is_rate_limited("scene", &pointers[0]));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The first deploy holds its pointer from validation to its write (here a database that
/// accepts the write's connection and never answers), so a concurrent deploy to that pointer
/// is refused with upstream's in-flight text, unqueued; a cancelled deploy lets its pointers
/// go. A connection opened before the claim fails at once, so the newer-entity lookup goes on
/// without it.
#[tokio::test]
async fn of_two_concurrent_deploys_to_one_pointer_the_second_is_refused_as_in_flight() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
    let claimed: Arc<std::sync::OnceLock<Arc<WriteDeployer>>> = Arc::default();
    let silent = {
        let claimed = Arc::clone(&claimed);
        tokio::spawn(async move {
            let mut connected_tx = Some(connected_tx);
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                if claimed
                    .get()
                    .is_some_and(|d| d.holds_pointer("scene", "5,5"))
                {
                    held.push(socket);
                    if let Some(tx) = connected_tx.take() {
                        tx.send(()).unwrap();
                    }
                }
            }
        })
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(60))
        .connect_lazy(&format!("postgres://nobody@{addr}/nowhere"))
        .unwrap();
    let (storage, tmp) = temp_storage("concurrent").await;
    let deployer = Arc::new(WriteDeployer::with_partial_validation_for_tests(
        pool,
        storage,
        Some(Arc::new(NoEip1654)),
    ));
    assert!(claimed.set(Arc::clone(&deployer)).is_ok());
    let pointers = vec!["5,5".to_string()];
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (first_bytes, first_id, first_chain) = signed_scene(&pointers, now_ms).await;
    let (second_bytes, second_id, second_chain) = signed_scene(&pointers, now_ms + 1).await;

    let first = {
        let deployer = Arc::clone(&deployer);
        tokio::spawn(async move {
            deployer
                .deploy_entity(vec![first_bytes], &first_id, first_chain, "LOCAL")
                .await
        })
    };
    connected_rx.await.unwrap();

    let second = tokio::time::timeout(
        Duration::from_secs(10),
        deployer.deploy_entity(vec![second_bytes], &second_id, second_chain, "LOCAL"),
    )
    .await
    .expect("the second deploy is refused, not queued behind the first")
    .unwrap_err();
    assert!(matches!(
        &second,
        DeployFailure::Rejected(RejectionKind::PointerContention, _)
    ));
    assert_eq!(
        second.errors(),
        [
            "The following pointers are currently being deployed: '5,5'. Please try again in a few seconds."
        ],
        "{second:?}"
    );

    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(
        deployer
            .pointers_in_flight
            .lock()
            .values()
            .all(HashSet::is_empty),
        "a cancelled deploy releases its claim"
    );
    silent.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Polls until a deploy's write queues on the shared store lock of `pool`'s database.
async fn queued_on_the_store_lock<F: std::future::Future + Unpin>(pool: &PgPool, deploy: &mut F)
where
    F::Output: std::fmt::Debug,
{
    for _ in 0..60 {
        if let Ok(done) = tokio::time::timeout(Duration::from_millis(50), &mut *deploy).await {
            panic!("the write did not wait for the batch: {done:?}");
        }
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks \
             WHERE locktype = 'advisory' AND mode = 'ShareLock' AND NOT granted \
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
               AND (classid::bigint << 32 | objid::bigint) = $1",
        )
        .bind(crate::storage::gc::STORE_LOCK_KEY)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= 1 {
            return;
        }
    }
    panic!("the write never queued on the store lock");
}

/// A content deploy writes under the shared store lock: while an unlink batch holds the
/// exclusive side the write waits, and it commits once the batch ends. Its own database,
/// so no other test's deploy waits behind this batch.
#[tokio::test]
async fn a_content_deploy_waits_for_an_unlink_batch() {
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let (storage, tmp) = temp_storage("store-lock").await;
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        pool.clone(),
        storage,
        Some(Arc::new(NoEip1654)),
    );
    let pointers = Scope::new().pointers(&["9,9"]);
    let (bytes, id, chain) = signed_scene(&pointers, chrono::Utc::now().timestamp_millis()).await;

    let batch = crate::storage::gc::lock_store_exclusive(&pool)
        .await
        .unwrap();
    let deploy = deployer.deploy_entity(vec![bytes], &id, chain, "LOCAL");
    tokio::pin!(deploy);
    queued_on_the_store_lock(&pool, &mut deploy).await;
    batch.rollback().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), deploy)
        .await
        .expect("the write proceeds once the batch ends")
        .expect("the deploy commits");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A batch that outlasts the write's lock wait answers a retryable 503.
#[tokio::test]
async fn a_content_deploy_behind_a_long_unlink_batch_is_undecided() {
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let (storage, tmp) = temp_storage("store-lock-wait").await;
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        pool.clone(),
        storage,
        Some(Arc::new(NoEip1654)),
    );
    let pointers = Scope::new().pointers(&["9,10"]);
    let (bytes, id, chain) = signed_scene(&pointers, chrono::Utc::now().timestamp_millis()).await;

    let batch = crate::storage::gc::lock_store_exclusive(&pool)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        deployer.deploy_entity(vec![bytes], &id, chain, "LOCAL"),
    )
    .await
    .expect("the wait is bounded")
    .unwrap_err();
    assert!(matches!(&err, DeployFailure::Unavailable(_)), "{err:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(4500),
        "answered after {:?}, before the lock wait ran out",
        started.elapsed()
    );
    assert_eq!(deployer.deployed_timestamp(&id).await.unwrap(), None);
    batch.rollback().await.unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A stored file the entity reuses, removed by a batch the write waited for, answers a
/// retryable 503 instead of committing a reference to a missing file.
#[tokio::test]
async fn a_reused_file_collected_during_the_deploy_is_undecided() {
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let (storage, tmp) = temp_storage("store-lock-gone").await;
    let reused = Bytes::from_static(b"reused bytes");
    let reused_hash = hash_bytes_v1(&reused);
    storage.store(&reused_hash, reused).await.unwrap();
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        pool.clone(),
        storage.clone(),
        Some(Arc::new(NoEip1654)),
    );
    let pointers = Scope::new().pointers(&["9,11"]);
    let (bytes, id, chain) = signed_entity_of(serde_json::json!({
        "version": "v3",
        "type": "scene",
        "pointers": pointers,
        "timestamp": chrono::Utc::now().timestamp_millis(),
        "content": [{"file": "reused.bin", "hash": reused_hash}],
        "metadata": {}
    }))
    .await;

    let batch = crate::storage::gc::lock_store_exclusive(&pool)
        .await
        .unwrap();
    let deploy = deployer.deploy_entity(vec![bytes], &id, chain, "LOCAL");
    tokio::pin!(deploy);
    queued_on_the_store_lock(&pool, &mut deploy).await;
    storage.delete_strict(&reused_hash).await.unwrap();
    batch.commit().await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(10), deploy)
        .await
        .expect("the write proceeds once the batch ends")
        .unwrap_err();
    assert!(
        matches!(&err, DeployFailure::Unavailable(e) if e[0].contains(&reused_hash)),
        "{err:?}"
    );
    assert_eq!(deployer.deployed_timestamp(&id).await.unwrap(), None);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Pointer lists past the per-type bound are refused before any database work.
#[tokio::test]
async fn oversized_pointer_lists_are_refused_early() {
    let (deployer, tmp) = deployer_over_temp_storage("pointer-bound").await;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let parcels: Vec<String> = (0..=MAX_SCENE_POINTERS)
        .map(|i| format!("{},{}", i % 1000, i / 1000))
        .collect();
    let (bytes, id, chain) = signed_scene(&parcels, now_ms).await;
    let err = deployer
        .deploy_entity(vec![bytes], &id, chain, "LOCAL")
        .await
        .unwrap_err();
    assert_eq!(
        err.errors(),
        [format!(
            "A scene may name at most {MAX_SCENE_POINTERS} pointers."
        )]
    );

    let pointers: Vec<String> = (0..=MAX_OTHER_POINTERS)
        .map(|i| format!("0x{i:040x}"))
        .collect();
    let (bytes, id, chain) = signed_entity_of(serde_json::json!({
        "version": "v3",
        "type": "profile",
        "pointers": pointers,
        "timestamp": now_ms,
        "content": [],
        "metadata": {}
    }))
    .await;
    let err = deployer
        .deploy_entity(vec![bytes], &id, chain, "LOCAL")
        .await
        .unwrap_err();
    assert_eq!(
        err.errors(),
        [format!(
            "A profile may name at most {MAX_OTHER_POINTERS} pointers."
        )]
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn has_newer_entity_timestamp_and_tiebreak() {
    assert!(stored_is_newer_or_equal(200, "bafy_a", 100, "bafy_z"));
    assert!(!stored_is_newer_or_equal(50, "bafy_z", 100, "bafy_a"));

    assert!(stored_is_newer_or_equal(100, "bafy_z", 100, "bafy_a"));
    assert!(!stored_is_newer_or_equal(100, "bafy_a", 100, "bafy_z"));

    assert!(!stored_is_newer_or_equal(100, "bafy_m", 100, "bafy_m"));

    assert!(stored_is_newer_or_equal(100, "BAFY_Z", 100, "bafy_a"));
    assert!(!stored_is_newer_or_equal(100, "BAFY_A", 100, "bafy_z"));
}

#[test]
fn newer_and_overwrote_are_consistent_at_the_boundary() {
    let cases = [
        (100i64, "bafy_a", 100i64, "bafy_z"),
        (100, "bafy_z", 100, "bafy_a"),
        (200, "x", 100, "z"),
        (50, "z", 100, "x"),
        (100, "AbC", 100, "abd"),
    ];
    for (sts, sid, its, iid) in cases {
        let newer = stored_is_newer_or_equal(sts, sid, its, iid);
        let overwritten = stored_is_overwritten(sts, sid, its, iid);
        assert!(
            newer ^ overwritten,
            "exactly one of newer/overwritten must hold for ({sts},{sid}) vs ({its},{iid})"
        );
    }
    assert!(!stored_is_newer_or_equal(100, "same", 100, "same"));
    assert!(!stored_is_overwritten(100, "same", 100, "same"));
}

#[test]
fn is_content_unchanged_metadata_compare() {
    let precise: Value = serde_json::from_str(r#"{"v":{"z":9007199254740993,"a":1}}"#).unwrap();
    let reordered: Value = serde_json::from_str(r#"{"a":1,"z":9007199254740993}"#).unwrap();
    let rounded: Value = serde_json::from_str(r#"{"a":1,"z":9007199254740992}"#).unwrap();
    assert!(metadata_unchanged(Some(&precise), Some(&reordered)));
    assert!(!metadata_unchanged(Some(&precise), Some(&rounded)));
    let wrapped = serde_json::json!({ "v": { "name": "alice", "n": 1 } });
    let same = serde_json::json!({ "name": "alice", "n": 1 });
    let diff = serde_json::json!({ "name": "bob", "n": 1 });

    assert!(metadata_unchanged(Some(&wrapped), Some(&same)));
    assert!(!metadata_unchanged(Some(&wrapped), Some(&diff)));

    assert!(!metadata_unchanged(None, Some(&same)));

    let wrapped_null = serde_json::json!({ "v": Value::Null });
    assert!(metadata_unchanged(Some(&wrapped_null), None));
    assert!(!metadata_unchanged(Some(&wrapped_null), Some(&same)));
}

#[test]
fn rate_limiter_per_type_defaults() {
    let rl = DeployRateLimiter::with_reference_defaults();
    let ptrs = vec!["0,0".to_string()];
    assert!(!rl.is_rate_limited("scene", &ptrs));
    rl.record("scene", &ptrs);
    assert!(rl.is_rate_limited("scene", &ptrs));
    assert!(!rl.is_rate_limited("profile", &ptrs));
    assert!(!rl.is_rate_limited("bogus", &ptrs));
}

#[test]
fn the_deploy_function_is_named_by_its_body_and_lives_in_content() {
    let with = deploy_function_name(true);
    let without = deploy_function_name(false);
    assert!(with.starts_with("deploy_entity_") && with.len() == "deploy_entity_".len() + 12);
    assert_ne!(with, without, "provenance changes the body, so the name");
    assert_eq!(
        deploy_function_name(true),
        with,
        "the name is deterministic"
    );
    let sql = deploy_function_sql(true, false);
    assert!(sql.starts_with(&format!("CREATE FUNCTION content.{with}(")));
    assert!(
        deploy_function_sql(true, true)
            .starts_with(&format!("CREATE OR REPLACE FUNCTION content.{with}("))
    );
    assert!(
        sql.contains(r#"ORDER BY k COLLATE "C""#),
        "the locks follow lock_deployment_pointers' byte order"
    );
    assert!(sql.contains("INSERT INTO content.deployments"));
    assert!(sql.contains("INSERT INTO content.local_entities"));
    assert!(!deploy_function_sql(false, false).contains("local_entities"));
    assert!(deploy_function_call(&with).contains(&format!("FROM content.{with}(")));
}

#[tokio::test]
async fn batched_pointer_locks_cover_duplicates_and_release_on_rollback() {
    let Some(pool) = test_pool(2).await else {
        return;
    };
    let prefix = uuid::Uuid::new_v4().to_string();
    let a = format!("{prefix}:a");
    let b = format!("{prefix}:b");
    let mut first = pool.begin().await.unwrap();
    lock_deployment_pointers(&mut first, &[b.clone(), a.clone(), b.clone()])
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE pid = pg_backend_pid() AND locktype = 'advisory'",
    )
    .fetch_one(&mut *first)
    .await
    .unwrap();
    assert_eq!(count, 2);
    let mut second = pool.begin().await.unwrap();
    for key in [&a, &b] {
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext($1))")
            .bind(key)
            .fetch_one(&mut *second)
            .await
            .unwrap();
        assert!(!acquired);
    }
    first.rollback().await.unwrap();
    for key in [&a, &b] {
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext($1))")
            .bind(key)
            .fetch_one(&mut *second)
            .await
            .unwrap();
        assert!(acquired);
    }
    second.rollback().await.unwrap();
    pool.close().await;
}

/// Runs the one-statement write the way `persist` does.
async fn run_persist(
    tx: &mut sqlx::PgConnection,
    provenance: bool,
    entity_id: &str,
    ts_ms: f64,
    pointers: &[String],
    files: &[(&str, &str)],
) -> PersistOutcome {
    let hashes: Vec<String> = files.iter().map(|(h, _)| h.to_string()).collect();
    let keys: Vec<String> = files.iter().map(|(_, k)| k.to_string()).collect();
    let mut query =
        sqlx::query_as::<_, PersistOutcome>(sqlx::AssertSqlSafe(persist_statement(provenance)))
            .bind("0xDeployer")
            .bind("v3")
            .bind("scene")
            .bind(entity_id)
            .bind(serde_json::json!({"v": {"n": entity_id}}))
            .bind(ts_ms)
            .bind(pointers)
            .bind(serde_json::json!([]))
            .bind(&hashes)
            .bind(&keys);
    if provenance {
        query = query.bind("0xdeployer");
    }
    query.fetch_one(&mut *tx).await.unwrap()
}

struct Scope {
    prefix: String,
}

impl Scope {
    fn new() -> Self {
        Self {
            prefix: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    fn id(&self, name: &str) -> String {
        format!("{}-{name}", self.prefix)
    }

    fn pointers(&self, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|n| format!("{}:{n}", self.prefix))
            .collect()
    }

    async fn cleanup(&self, pool: &PgPool) {
        let like = format!("{}%", self.prefix);
        for sql in [
            "DELETE FROM content.content_files WHERE deployment IN (SELECT id FROM content.deployments WHERE entity_id LIKE $1)",
            "DELETE FROM content.active_pointers WHERE pointer LIKE $1",
            "DELETE FROM content.local_entities WHERE entity_id LIKE $1",
            "DELETE FROM content.deployments WHERE entity_id LIKE $1",
        ] {
            sqlx::query(sql).bind(&like).execute(pool).await.unwrap();
        }
    }
}

#[tokio::test]
async fn persist_statement_writes_everything_off_one_insert() {
    let Some(pool) = test_pool(2).await else {
        return;
    };
    let scope = Scope::new();
    let (first_id, stale_id, second_id) =
        (scope.id("first"), scope.id("stale"), scope.id("second"));
    let ids = vec![first_id.clone(), stale_id.clone(), second_id.clone()];
    let p = |names: &[&str]| scope.pointers(names);

    let mut tx = pool.begin().await.unwrap();
    let first = run_persist(
        &mut tx,
        true,
        &first_id,
        1_000.0,
        &p(&["0,0", "1,0"]),
        &[("hA", "a.txt"), ("hB", "b.txt")],
    )
    .await;
    assert!(!first.newer);
    let first_dep = first.dep_id.expect("fresh entity inserts");
    assert_eq!((first.overwrote, first.cleared), (0, 0));

    let again = run_persist(&mut tx, true, &first_id, 1_000.0, &p(&["0,0", "1,0"]), &[]).await;
    assert!(
        again.dep_id.is_none() && !again.newer,
        "conflict is a silent no-op"
    );

    let stale = run_persist(
        &mut tx,
        true,
        &stale_id,
        500.0,
        &p(&["1,0"]),
        &[("hC", "c")],
    )
    .await;
    assert!(
        stale.newer && stale.dep_id.is_none(),
        "a newer entity blocks the insert"
    );

    let second = run_persist(
        &mut tx,
        false,
        &second_id,
        2_000.0,
        &p(&["1,0", "2,0"]),
        &[("hD", "d")],
    )
    .await;
    let second_dep = second.dep_id.expect("newer entity inserts");
    assert_eq!((second.overwrote, second.cleared), (1, 1));

    let rows: Vec<(String, Vec<String>, Option<i32>)> = sqlx::query_as(
        "SELECT entity_id, entity_pointers, deleter_deployment FROM content.deployments
         WHERE entity_id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (first_id.clone(), p(&["0,0", "1,0"]), Some(second_dep)),
            (second_id.clone(), p(&["1,0", "2,0"]), None),
        ]
    );
    let pointers: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT pointer, entity_id, entity_type FROM content.active_pointers
         WHERE pointer = ANY($1) ORDER BY pointer",
    )
    .bind(p(&["0,0", "1,0", "2,0"]))
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        pointers,
        vec![
            (
                p(&["1,0"]).remove(0),
                second_id.clone(),
                Some("scene".into())
            ),
            (
                p(&["2,0"]).remove(0),
                second_id.clone(),
                Some("scene".into())
            ),
        ]
    );
    let files: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT deployment, content_hash, key FROM content.content_files
         WHERE deployment = ANY($1) ORDER BY deployment, key",
    )
    .bind(vec![first_dep, second_dep])
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        files,
        vec![
            (first_dep, "hA".into(), "a.txt".into()),
            (first_dep, "hB".into(), "b.txt".into()),
            (second_dep, "hD".into(), "d".into()),
        ]
    );
    let provenance: Vec<(String, String)> = sqlx::query_as(
        "SELECT entity_id, signer FROM content.local_entities WHERE entity_id = ANY($1) ORDER BY entity_id",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(provenance, vec![(first_id.clone(), "0xdeployer".into())]);
    tx.rollback().await.unwrap();
    pool.close().await;
}

/// The installed function and the raw statement are the same SQL; a deploy through the
/// function must produce the same outcome and rows, and a stale deploy the same rejection.
#[tokio::test]
async fn deploy_function_matches_the_statement() {
    let Some(pool) = test_pool(2).await else {
        return;
    };
    sqlx::query(sqlx::AssertSqlSafe(deploy_function_sql(true, true)))
        .execute(&pool)
        .await
        .unwrap();
    let scope = Scope::new();
    let name = deploy_function_name(true);
    let call = |id: String, ts: f64, pointers: Vec<String>| {
        let pool = pool.clone();
        let call = deploy_function_call(&name);
        async move {
            sqlx::query_as::<_, PersistOutcome>(sqlx::AssertSqlSafe(call))
                .bind("0xDeployer")
                .bind("v3")
                .bind("scene")
                .bind(&id)
                .bind(serde_json::json!({"v": {"n": id}}).to_string())
                .bind(ts)
                .bind(&pointers)
                .bind(serde_json::json!([]))
                .bind(vec!["hA".to_string()])
                .bind(vec!["a.txt".to_string()])
                .bind("0xdeployer")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };

    let first = call(scope.id("first"), 1_000.0, scope.pointers(&["0,0", "1,0"])).await;
    assert_eq!((first.newer, first.overwrote, first.cleared), (false, 0, 0));
    assert!(first.dep_id.is_some());

    let second = call(scope.id("second"), 2_000.0, scope.pointers(&["1,0"])).await;
    assert_eq!(
        (second.newer, second.overwrote, second.cleared),
        (false, 1, 1)
    );

    let stale = call(scope.id("stale"), 1_500.0, scope.pointers(&["1,0"])).await;
    assert!(stale.newer && stale.dep_id.is_none());

    let again = call(scope.id("second"), 2_000.0, scope.pointers(&["1,0"])).await;
    assert!(!again.newer && again.dep_id.is_none());

    let active: Vec<(String, String)> = sqlx::query_as(
        "SELECT pointer, entity_id FROM content.active_pointers WHERE pointer = ANY($1) ORDER BY pointer",
    )
    .bind(scope.pointers(&["0,0", "1,0"]))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        active,
        vec![(scope.pointers(&["1,0"]).remove(0), scope.id("second"))]
    );
    let signer: Option<String> =
        sqlx::query_scalar("SELECT signer FROM content.local_entities WHERE entity_id = $1")
            .bind(scope.id("second"))
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(signer.as_deref(), Some("0xdeployer"));

    scope.cleanup(&pool).await;
    pool.close().await;
}

/// A signed scene through the whole deployer: hashing, entity detection, the auth chain
/// verified against the entity timestamp, files stored, one function call, provenance row.
#[tokio::test]
async fn deploy_entity_persists_a_signed_local_scene_end_to_end() {
    use alloy::signers::{Signer, local::PrivateKeySigner};

    let Some(pool) = test_pool(2).await else {
        return;
    };
    let (storage, tmp) = temp_storage("e2e").await;
    let fresh = || {
        WriteDeployer::with_partial_validation_for_tests(
            pool.clone(),
            storage.clone(),
            Some(Arc::new(NoEip1654)),
        )
    };
    let scope = Scope::new();
    let pointers = scope.pointers(&["7,7"]);
    let model = Bytes::from_static(b"glb bytes");
    let model_hash = hash_bytes_v1(&model);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let entity_json = serde_json::json!({
        "version": "v3",
        "type": "scene",
        "pointers": [pointers[0].to_uppercase()],
        "timestamp": now_ms,
        "content": [
            {"file": "model.glb", "hash": model_hash},
            {"file": "stored.bin", "hash": STORED_HASH}
        ],
        "metadata": {"name": "e2e"}
    });
    let entity_bytes = Bytes::from(serde_json::to_vec(&entity_json).unwrap());
    let entity_id = hash_bytes_v1(&entity_bytes);

    let key: PrivateKeySigner = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        .parse()
        .unwrap();
    let signature = key
        .sign_message(entity_id.as_bytes())
        .await
        .unwrap()
        .to_string();
    let chain = serde_json::json!([
        {"type": "SIGNER", "payload": format!("{:#x}", key.address())},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": entity_id, "signature": signature}
    ]);

    let missing = fresh()
        .deploy_entity(
            vec![entity_bytes.clone()],
            &entity_id,
            chain.clone(),
            "LOCAL",
        )
        .await
        .expect_err("a declared file that is neither uploaded nor stored is rejected");
    assert!(
        matches!(&missing, DeployFailure::Rejected(_, errors) if errors[0].contains(&model_hash)),
        "{missing:?}"
    );

    let forged = fresh()
        .deploy_entity(
            vec![entity_bytes.clone(), model.clone()],
            &entity_id,
            serde_json::json!([
                {"type": "SIGNER", "payload": "0x0000000000000000000000000000000000000001"},
                {"type": "ECDSA_SIGNED_ENTITY", "payload": entity_id, "signature": chain[1]["signature"]}
            ]),
            "LOCAL",
        )
        .await
        .expect_err("a chain whose signature does not recover to its signer is rejected");
    assert!(
        matches!(&forged, DeployFailure::Rejected(_, errors) if errors[0].starts_with("The signature is invalid. ")),
        "{forged:?}"
    );

    let deployer = fresh();
    deployer
        .deploy_entity(
            vec![entity_bytes.clone(), model.clone()],
            &entity_id,
            chain.clone(),
            "LOCAL",
        )
        .await
        .expect("a signed scene with every file at hand deploys");
    assert!(
        deployer.is_rate_limited("scene", &pointers[0]),
        "an accepted deploy holds its pointer"
    );
    deployer.clear_rate_limits();
    deployer
        .deploy_entity(
            vec![entity_bytes.clone(), model.clone()],
            &entity_id,
            chain,
            "LOCAL",
        )
        .await
        .expect("a deploy of an already deployed entity succeeds");
    assert!(
        !deployer.is_rate_limited("scene", &pointers[0]),
        "a deploy that inserted nothing does not hold the pointer"
    );

    let row: (String, String, Vec<String>, Option<Value>) = sqlx::query_as(
        "SELECT deployer_address, entity_type, entity_pointers, entity_metadata
         FROM content.deployments WHERE entity_id = $1",
    )
    .bind(&entity_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, format!("{:#x}", key.address()));
    assert_eq!(row.1, "scene");
    assert_eq!(row.2, pointers, "pointers are stored lowercase");
    assert_eq!(row.3, Some(serde_json::json!({"v": {"name": "e2e"}})));
    let files: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, content_hash FROM content.content_files
         WHERE deployment = (SELECT id FROM content.deployments WHERE entity_id = $1) ORDER BY key",
    )
    .bind(&entity_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        files,
        vec![
            ("model.glb".into(), model_hash.clone()),
            ("stored.bin".into(), STORED_HASH.into())
        ]
    );
    let signer: Option<String> =
        sqlx::query_scalar("SELECT signer FROM content.local_entities WHERE entity_id = $1")
            .bind(&entity_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(signer, Some(format!("{:#x}", key.address())));
    assert_eq!(
        storage.retrieve(&entity_id).await.unwrap(),
        Some(entity_bytes)
    );
    assert_eq!(storage.retrieve(&model_hash).await.unwrap(), Some(model));

    sqlx::query("DELETE FROM content.content_files WHERE deployment = (SELECT id FROM content.deployments WHERE entity_id = $1)")
        .bind(&entity_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.local_entities WHERE entity_id = $1")
        .bind(&entity_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.deployments WHERE entity_id = $1")
        .bind(&entity_id)
        .execute(&pool)
        .await
        .unwrap();
    scope.cleanup(&pool).await;
    let _ = std::fs::remove_dir_all(&tmp);
    pool.close().await;
}

#[tokio::test]
async fn a_local_scene_deploy_without_a_land_rights_source_is_undecidable_never_accepted() {
    let (storage, tmp) = temp_storage("land-source").await;
    // The index cannot be read (no cluster) and no marketplace subgraph is
    // attached: `can_deploy_batch` answers `Unavailable::NoSource`.
    let land = Arc::new(LandAuthzState::new(
        lazy_pool(),
        crate::http::Outbound::new(crate::http::HttpClientCfg::default()).unwrap(),
        crate::land_authz::Settings::for_tests(),
        None,
    ));
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        lazy_pool(),
        storage,
        Some(Arc::new(NoEip1654)),
    )
    .with_land_authz(Some(land));
    let signer = "0x8d6f63e382d73cf53858864f673f39e9ff915a1e";

    let scene: Entity = serde_json::from_value(serde_json::json!({
        "id": "bafyscene", "version": "v3", "type": "scene", "pointers": ["0,0", "0,1"], "timestamp": 1, "content": []
    }))
    .unwrap();
    let err = deployer
        .validate_land_rights(&scene, signer)
        .await
        .unwrap_err();
    assert!(matches!(err, DeployFailure::Unavailable(_)), "{err:?}");
    assert!(
        err.errors()[0] == crate::validator::checker::PARCEL_ACCESS_UNDECIDED,
        "{err:?}"
    );

    let malformed: Entity = serde_json::from_value(serde_json::json!({
        "id": "bafyscene", "version": "v3", "type": "scene", "pointers": ["0,0", "urn:not-a-parcel"], "timestamp": 1, "content": []
    }))
    .unwrap();
    let err = deployer
        .validate_land_rights(&malformed, signer)
        .await
        .unwrap_err();
    assert!(matches!(err, DeployFailure::Rejected(_, _)), "{err:?}");
    assert!(
        err.errors()[0].contains("Invalid pointer: (urn:not-a-parcel)"),
        "{err:?}"
    );

    let profile: Entity = serde_json::from_value(serde_json::json!({
        "id": "bafyprofile", "version": "v3", "type": "profile", "pointers": [signer], "timestamp": 1, "content": []
    }))
    .unwrap();
    deployer
        .validate_land_rights(&profile, signer)
        .await
        .unwrap();

    let without_source = WriteDeployer::with_partial_validation_for_tests(
        lazy_pool(),
        deployer.storage.clone(),
        Some(Arc::new(NoEip1654)),
    );
    without_source
        .validate_land_rights(&scene, signer)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// As upstream's `rate-limiter.ts`: each type takes its own setting, else
/// its built-in window; `DEPLOYMENTS_DEFAULT_RATE_LIMIT_*` (here the operator
/// docs' `60`, read as 60 ms) changes none of them.
#[test]
fn the_default_rate_limit_leaves_every_types_own_window() {
    let config = crate::config::Config::parse_for_tests(&[
        "x",
        "--database-url",
        "postgres:///c",
        "--deployments-default-rate-limit-ttl-ms",
        "60",
        "--deployments-default-rate-limit-max",
        "300",
        "--deployment-rate-limit-ttl-scene-ms",
        "5s",
        "--deployment-rate-limit-max-scene",
        "7",
    ])
    .unwrap();
    let limits = DeployLimits::from_config(&config.content);
    let of = |etype: &str| {
        limits
            .rate_limits
            .iter()
            .find(|(t, ..)| *t == etype)
            .map(|&(_, ttl, max)| (ttl, max))
            .unwrap()
    };
    assert_eq!(of("profile"), (Duration::from_secs(3), 500));
    assert_eq!(of("scene"), (Duration::from_secs(5), 7));
    for &(etype, ttl, max) in REFERENCE_RATE_LIMITS.iter().filter(|(t, ..)| *t != "scene") {
        assert_eq!(of(etype), (ttl, max), "{etype}");
    }
    assert_eq!(limits.rate_limits.len(), REFERENCE_RATE_LIMITS.len());
}

#[tokio::test]
async fn binding_sql_supersession_has_a_typed_rejection() {
    let Some(pool) = test_pool(2).await else {
        return;
    };
    let (storage, tmp) = temp_storage("typed-superseded").await;
    let deployer =
        WriteDeployer::with_partial_validation_for_tests(pool, storage, Some(Arc::new(NoEip1654)));
    let pointers = Scope::new().pointers(&["9,9"]);
    let now = chrono::Utc::now().timestamp_millis();
    let (newer, newer_id, newer_chain) = signed_scene(&pointers, now).await;
    deployer
        .deploy_entity(vec![newer], &newer_id, newer_chain, "LOCAL")
        .await
        .unwrap();
    let (bytes, id, chain) = signed_scene(&pointers, now - 1).await;
    let mut entity: Entity = serde_json::from_slice(&bytes).unwrap();
    entity.id = id;
    let chain = serde_json::from_value(chain).unwrap();
    let uploads = hash_uploads(vec![bytes.clone()]).await.unwrap();
    // Call publication directly to exercise the binding SQL decision after preflight.
    let failure = deployer
        .persist(&entity, &bytes, &uploads, &chain, "LOCAL")
        .await
        .err()
        .unwrap();
    assert!(matches!(
        &failure,
        DeployFailure::Rejected(RejectionKind::Superseded, _)
    ));
    assert_eq!(failure.errors(), &[newer_entity_message(&entity)]);
    let _ = std::fs::remove_dir_all(tmp);
}
