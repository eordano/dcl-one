//! The partial-upload protocol through the content router, on the process's
//! scratch database (each test signs with a fresh wallet on parcels of its
//! own), and the pure quota arithmetic.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy::signers::{Signer as _, local::PrivateKeySigner};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::tests::support::require_test_db;
use crate::write_deployer::WriteDeployer;
use crate::{ContentState, Settings, SharedState, SyncState, build_router};

struct Node {
    state: SharedState,
    tmp: std::path::PathBuf,
}

impl Node {
    /// A node taking local writes over the scratch database, with the test
    /// deployer (the chain and content checks, no chain sources).
    async fn new(tag: &str, limits: Limits) -> Option<Self> {
        Self::with_settings(
            tag,
            Settings {
                partial_uploads: limits,
                ..Settings::for_tests()
            },
        )
        .await
    }

    async fn with_settings(tag: &str, settings: Settings) -> Option<Self> {
        let pool = crate::db::scratch::pool(6).await?;
        let tmp = std::env::temp_dir().join(format!(
            "catalyst-partial-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
        let state = Arc::new(ContentState::new(pool.clone(), settings, storage.clone()));
        state.sync.set_state(SyncState::Syncing);
        let deployer = WriteDeployer::with_partial_validation_for_tests(pool, storage, None)
            .with_denylist(state.denylist.clone());
        assert!(state.attach_writes(true, Arc::new(deployer)));
        Some(Self { state, tmp })
    }

    fn router(&self) -> Router {
        build_router(self.state.clone())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// A scene on fresh parcels with `files` as its content, signed by `signer`.
struct Scene {
    id: String,
    bytes: Bytes,
    chain: Value,
    files: Vec<(String, Bytes)>,
    pointers: Vec<String>,
}

async fn make_scene(signer: &PrivateKeySigner, files: &[&[u8]], entity_type: &str) -> Scene {
    // These fixtures share a database: random parcels can overwrite another
    // test's scene while its upload is still pending. Reserve each parcel once.
    // Keep y in -99..=-90, outside batch_deployer::tests::scene_at's
    // -149..=-100 range in the same scratch database.
    static NEXT_PARCEL: AtomicUsize = AtomicUsize::new(0);
    let parcel = NEXT_PARCEL.fetch_add(1, Ordering::Relaxed);
    assert!(
        parcel < 400,
        "partial-upload fixture parcel range exhausted"
    );
    let x = 100 + (parcel % 40) as i32;
    let y = -90 - (parcel / 40) as i32;
    let pointers = vec![format!("{x},{y}")];
    let files: Vec<(String, Bytes)> = files
        .iter()
        .map(|f| (hash_bytes_v1(f), Bytes::copy_from_slice(f)))
        .collect();
    let entity = json!({
        "version": "v3",
        "type": entity_type,
        "pointers": pointers,
        "timestamp": now_ms(),
        "content": files.iter().enumerate()
            .map(|(i, (hash, _))| json!({"file": format!("f{i}.bin"), "hash": hash}))
            .collect::<Vec<_>>(),
        "metadata": {"scene": {"base": pointers[0], "parcels": pointers}},
    });
    let bytes = Bytes::from(serde_json::to_vec(&entity).unwrap());
    let id = hash_bytes_v1(&bytes);
    let chain = chain_for(signer, &id).await;
    Scene {
        id,
        bytes,
        chain,
        files,
        pointers,
    }
}

/// A scene whose content lists `extra` hashes beside its own `files`
/// (content it does not send: already in the store).
async fn make_scene_listing(signer: &PrivateKeySigner, files: &[&[u8]], extra: &[&str]) -> Scene {
    let mut scene = make_scene(signer, files, "scene").await;
    let mut entity: Value = serde_json::from_slice(&scene.bytes).unwrap();
    let content = entity["content"].as_array_mut().unwrap();
    for (i, hash) in extra.iter().enumerate() {
        content.push(json!({"file": format!("extra{i}.bin"), "hash": hash}));
    }
    scene.bytes = Bytes::from(serde_json::to_vec(&entity).unwrap());
    scene.id = hash_bytes_v1(&scene.bytes);
    scene.chain = chain_for(signer, &scene.id).await;
    scene
}

/// Backdates a stored blob past the collector's age floor.
async fn age_blob(storage: &Storage, hash: &str) {
    let path = storage.file_path(hash).await.unwrap().expect("stored");
    let past = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(past)
        .unwrap();
}

/// Cleanup reclaims every expired upload in the process's shared scratch
/// database but unlinks blobs only in its caller's storage, and each node here
/// has a storage of its own: another test's run would delete this test's
/// expired upload and leave its blobs behind. A test holds this guard from
/// before its first expiry or cleanup run until its last check.
static RECLAIM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Moves an upload past its TTL.
async fn expire(pool: &PgPool, entity_id: &str) {
    sqlx::query(
        "UPDATE content.pending_deployments SET created_at = created_at - interval '2 hours'
         WHERE entity_id = $1",
    )
    .bind(entity_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn pending_rows(pool: &PgPool, entity_id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM content.pending_deployments WHERE entity_id = $1")
        .bind(entity_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn chain_for(signer: &PrivateKeySigner, entity_id: &str) -> Value {
    let signature = signer
        .sign_message(entity_id.as_bytes())
        .await
        .unwrap()
        .to_string();
    json!([
        {"type": "SIGNER", "payload": format!("{:#x}", signer.address()), "signature": ""},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": entity_id, "signature": signature}
    ])
}

/// A batch as the SDK sends it: `entityId`, `authChain`, `partial` (unless
/// `form_partial` is off) and the files named by their hash.
fn batch_request(
    query: &str,
    entity_id: &str,
    chain: &Value,
    files: &[(String, Bytes)],
    form_partial: bool,
) -> Request<Body> {
    let boundary = "catalyst-partial-boundary";
    let mut body: Vec<u8> = Vec::new();
    let mut field = |name: &str, value: &[u8], file: bool| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let disposition = if file {
            format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
        } else {
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n")
        };
        body.extend_from_slice(disposition.as_bytes());
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    };
    if !entity_id.is_empty() {
        field("entityId", entity_id.as_bytes(), false);
    }
    field("authChain", chain.to_string().as_bytes(), false);
    if form_partial {
        field("partial", b"true", false);
    }
    for (name, bytes) in files {
        field(name, bytes, true);
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::post(format!("/content/entities{query}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("x-real-ip", "203.0.113.7")
        // Clients declare their length: an undeclared body takes a
        // source's whole in-flight share.
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

async fn call(router: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn send(
    router: &Router,
    scene: &Scene,
    chain: &Value,
    files: Vec<(String, Bytes)>,
) -> (StatusCode, HeaderMap, Value) {
    call(
        router,
        batch_request("?partial=true", &scene.id, chain, &files, true),
    )
    .await
}

fn entity_file(scene: &Scene) -> (String, Bytes) {
    (scene.id.clone(), scene.bytes.clone())
}

/// ADR-325 end to end: the first batch (entity file and one content file)
/// answers 202 with the rest missing; the batch that completes it publishes
/// the scene (200), which `/entities/active` then serves; the upload's rows
/// are gone, and any later batch is a replay with the same timestamp.
#[tokio::test]
async fn a_scene_uploaded_in_batches_is_published_by_its_last_batch() {
    require_test_db!(Some(node) = Node::new("roundtrip", Limits::default()).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let mut scene = make_scene(&signer, &[b"partial-a", b"partial-b"], "scene").await;
    let raw = String::from_utf8(scene.bytes.to_vec()).unwrap().replacen(
        "\"metadata\":{",
        "\"metadata\":{\"extra\":{\"z\":9007199254740993,\"a\":18446744073709551615},",
        1,
    );
    scene.bytes = Bytes::from(raw);
    scene.id = hash_bytes_v1(&scene.bytes);
    scene.chain = chain_for(&signer, &scene.id).await;
    let (a, b) = (scene.files[0].clone(), scene.files[1].clone());

    let (status, _, body) = send(&router, &scene, &scene.chain, vec![entity_file(&scene), a]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body, json!({ "missing": [b.0.clone()] }));

    // Reconstruct application state from pending receipts and the original stored entity.
    drop(router);
    let state = Arc::new(ContentState::new(
        node.state.pool.clone(),
        node.state.settings.clone(),
        node.state.storage.clone(),
    ));
    state.sync.set_state(SyncState::Syncing);
    let deployer = WriteDeployer::with_partial_validation_for_tests(
        state.pool.clone(),
        state.storage.clone(),
        None,
    )
    .with_denylist(state.denylist.clone());
    assert!(state.attach_writes(true, Arc::new(deployer)));
    let router = build_router(state.clone());

    state.denylist.operator_insert(&b.0);
    let (status, _, _) = send(&router, &scene, &scene.chain, vec![b.clone()]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let committed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM content.deployments WHERE entity_id=$1")
            .bind(&scene.id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        committed, 0,
        "a content refusal at completion cannot publish"
    );
    assert_eq!(pending_rows(&state.pool, &scene.id).await, 1);
    state.denylist.operator_remove(&b.0);

    let (status, _, body) = send(&router, &scene, &scene.chain, vec![b]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created = body["creationTimestamp"].as_i64().expect("a timestamp");
    let metadata: String = sqlx::query_scalar(
        "SELECT entity_metadata::text FROM content.deployments WHERE entity_id=$1",
    )
    .bind(&scene.id)
    .fetch_one(&node.state.pool)
    .await
    .unwrap();
    assert!(
        metadata.starts_with(
            r#"{"v":{"extra":{"z":9007199254740993,"a":18446744073709551615},"scene":"#
        )
    );
    assert_eq!(
        node.state.storage.retrieve(&scene.id).await.unwrap(),
        Some(scene.bytes.clone())
    );

    let req = Request::post("/content/entities/active")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"pointers": scene.pointers}).to_string()))
        .unwrap();
    let (_, _, active) = call(&router, req).await;
    assert_eq!(active[0]["id"], scene.id);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM content.pending_deployments WHERE entity_id = $1")
            .bind(&scene.id)
            .fetch_one(&node.state.pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "a published upload leaves no pending row");

    let (status, _, body) = send(&router, &scene, &scene.chain, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["creationTimestamp"], created,
        "a replay answers the stored timestamp"
    );
}

/// Content already in the store is never listed as missing: a scene whose
/// files are all stored is published by its first batch.
#[tokio::test]
async fn stored_content_is_never_missing() {
    require_test_db!(Some(node) = Node::new("stored", Limits::default()).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"partial-stored"], "scene").await;
    node.state
        .storage
        .store(&scene.files[0].0, scene.files[0].1.clone())
        .await
        .unwrap();
    let (status, _, body) = send(&router, &scene, &scene.chain, vec![entity_file(&scene)]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The refusals a batch gets before it is stored, with upstream's messages.
#[tokio::test]
async fn a_batch_is_refused_with_upstreams_messages() {
    require_test_db!(Some(node) = Node::new("refusals", Limits::default()).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"partial-r1", b"partial-r2"], "scene").await;

    // A file whose field name is not its hash.
    let wrong = ("bafkreiaaaa".to_string(), Bytes::from_static(b"x"));
    let (status, _, body) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), wrong],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0],
        format!(
            "The uploaded file 'bafkreiaaaa' does not match its content hash (computed {}).",
            hash_bytes_v1(b"x")
        )
    );

    // No entity file and no upload yet.
    let (status, _, body) = send(&router, &scene, &scene.chain, vec![scene.files[0].clone()]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0],
        format!(
            "The first partial request for an entity, and any request by another signer, must include the entity file '{}'.",
            scene.id
        )
    );

    // Started by the signer, joined by another account.
    let (status, _, _) = send(&router, &scene, &scene.chain, vec![entity_file(&scene)]).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let stranger = PrivateKeySigner::random();
    let stranger_chain = chain_for(&stranger, &scene.id).await;
    let (status, _, body) = send(&router, &scene, &stranger_chain, vec![entity_file(&scene)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0],
        "This upload was started by another account."
    );
    let (status, _, _) = send(
        &router,
        &scene,
        &stranger_chain,
        vec![scene.files[0].clone()],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "another signer must send the entity file"
    );

    // A bad signature.
    let other = PrivateKeySigner::random();
    let mut forged = chain_for(&other, &scene.id).await;
    forged[0]["payload"] = json!(format!("{:#x}", signer.address()));
    let (status, _, body) = send(&router, &scene, &forged, vec![entity_file(&scene)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["errors"][0]
            .as_str()
            .unwrap()
            .starts_with("The signature is invalid. "),
        "{body}"
    );

    // Only scenes.
    let profile = make_scene(&signer, &[], "profile").await;
    let (status, _, body) = send(
        &router,
        &profile,
        &profile.chain,
        vec![entity_file(&profile)],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0],
        "Partial deployments are only supported for scenes, but the entity type is 'profile'."
    );
}

/// `?partial=true` declares a batch before its body; a form without the
/// `partial` field is then refused rather than deployed past the quota.
#[tokio::test]
async fn the_query_flag_needs_the_form_field() {
    require_test_db!(Some(node) = Node::new("query-flag", Limits::default()).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[], "scene").await;
    let (status, _, body) = call(
        &router,
        batch_request(
            "?partial=true",
            &scene.id,
            &scene.chain,
            &[entity_file(&scene)],
            false,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error": "The 'partial=true' query parameter requires the 'partial=true' form field"})
    );
}

/// The per-account upload cap answers 429 with Retry-After.
#[tokio::test]
async fn an_account_over_its_upload_cap_waits() {
    let limits = Limits {
        max_per_deployer: 1,
        ..Limits::default()
    };
    require_test_db!(Some(node) = Node::new("account-cap", limits).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let first = make_scene(&signer, &[b"cap-a"], "scene").await;
    let (status, _, _) = send(&router, &first, &first.chain, vec![entity_file(&first)]).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let second = make_scene(&signer, &[b"cap-b"], "scene").await;
    let (status, headers, body) =
        send(&router, &second, &second.chain, vec![entity_file(&second)]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body["errors"][0],
        "Too many partial uploads in progress for this account (max 1). Complete an upload or wait for expired uploads to be cleaned up."
    );
    assert!(headers.contains_key(header::RETRY_AFTER));
}

/// An expired upload refuses its next batch; the cleanup job deletes it and
/// unlinks its staged blobs.
#[tokio::test]
async fn an_expired_upload_is_refused_and_reclaimed() {
    require_test_db!(Some(node) = Node::new("expiry", Limits::default()).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"expiry-a", b"expiry-b"], "scene").await;
    let (status, _, _) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), scene.files[0].clone()],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let _reclaim = RECLAIM.lock().await;
    expire(&node.state.pool, &scene.id).await;
    let (status, _, body) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), scene.files[1].clone()],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0],
        "This upload expired. Create a new entity with a fresh timestamp."
    );

    // Younger than the collector's floor, the staged blobs are left to it.
    let storage = &node.state.storage;
    cleanup_expired(&node.state.pool, storage, &Limits::default())
        .await
        .unwrap();
    assert_eq!(pending_rows(&node.state.pool, &scene.id).await, 0);
    assert!(storage.exist(&scene.files[0].0).await.unwrap());
    assert!(storage.exist(&scene.id).await.unwrap());

    // Past it, the next expired upload's blobs go.
    let scene = make_scene(&signer, &[b"expiry-c", b"expiry-d"], "scene").await;
    let (status, _, _) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), scene.files[0].clone()],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    age_blob(storage, &scene.files[0].0).await;
    age_blob(storage, &scene.id).await;
    expire(&node.state.pool, &scene.id).await;
    cleanup_expired(&node.state.pool, storage, &Limits::default())
        .await
        .unwrap();
    assert_eq!(pending_rows(&node.state.pool, &scene.id).await, 0);
    assert!(!storage.exist(&scene.files[0].0).await.unwrap());
    assert!(!storage.exist(&scene.id).await.unwrap());
}

/// Cleanup unlinks only what an upload wrote, and never what the collector
/// keeps: an upload listing a global scene's content (found stored, so not
/// written by it) leaves it alone when it expires, as it leaves a blob some
/// other writer stored; and a receipt that claims the global scene's blob
/// as written is still kept by the collector's fixed references.
#[tokio::test]
async fn cleanup_leaves_blobs_the_upload_did_not_write_and_the_collectors_fixed_set() {
    require_test_db!(Some(node) = Node::new("fixed-set", Limits::default()).await);
    let router = node.router();
    let pool = &node.state.pool;
    let storage = &node.state.storage;
    let global_content = Bytes::from(format!("global-scene-asset-{}", uuid::Uuid::new_v4()));
    let global_hash = hash_bytes_v1(&global_content);
    storage.store(&global_hash, global_content).await.unwrap();
    let global_entity = Bytes::from(
        json!({"type": "scene", "pointers": ["0,0"], "timestamp": 1,
               "content": [{"file": "main.glb", "hash": global_hash}]})
        .to_string(),
    );
    let global_id = hash_bytes_v1(&global_entity);
    storage.store(&global_id, global_entity).await.unwrap();
    let other_content = Bytes::from(format!("another-writers-blob-{}", uuid::Uuid::new_v4()));
    let other_hash = hash_bytes_v1(&other_content);
    storage.store(&other_hash, other_content).await.unwrap();
    let limits = Limits {
        global_scenes_urn: vec![format!("urn:decentraland:entity:{global_id}?=&baseUrl=x")],
        ..Limits::default()
    };

    let signer = PrivateKeySigner::random();
    let scene = make_scene_listing(
        &signer,
        &[b"fixed-own", b"fixed-missing"],
        &[&global_hash, &other_hash],
    )
    .await;
    let (status, _, body) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), scene.files[0].clone()],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let written: Vec<(String, bool)> = sqlx::query_as(
        "SELECT hash, written FROM content.pending_deployment_files WHERE entity_id = $1 ORDER BY hash",
    )
    .bind(&scene.id)
    .fetch_all(pool)
    .await
    .unwrap();
    for (hash, written) in &written {
        let ours = *hash == scene.id || *hash == scene.files[0].0;
        assert_eq!(*written, ours, "{hash}");
    }
    for hash in [
        &global_hash,
        &global_id,
        &other_hash,
        &scene.files[0].0,
        &scene.id,
    ] {
        age_blob(storage, hash).await;
    }
    let _reclaim = RECLAIM.lock().await;
    expire(pool, &scene.id).await;
    cleanup_expired(pool, storage, &limits).await.unwrap();
    assert_eq!(pending_rows(pool, &scene.id).await, 0);
    assert!(
        !storage.exist(&scene.files[0].0).await.unwrap(),
        "its own blob goes"
    );
    for kept in [&global_hash, &global_id, &other_hash] {
        assert!(storage.exist(kept).await.unwrap(), "{kept} is kept");
    }

    // Defence in depth: a receipt claiming the global scene's blob as
    // written, released (live, so no other test's cleanup takes it).
    let scene = make_scene_listing(
        &signer,
        &[b"fixed-own-2", b"fixed-missing-2"],
        &[&global_hash],
    )
    .await;
    let (status, _, _) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), scene.files[0].clone()],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    sqlx::query("UPDATE content.pending_deployment_files SET written = true WHERE entity_id = $1")
        .bind(&scene.id)
        .execute(pool)
        .await
        .unwrap();
    age_blob(storage, &scene.files[0].0).await;
    let lock = EntityLock::try_acquire(pool, &scene.id)
        .await
        .unwrap()
        .expect("free");
    release_upload(pool, storage, &limits, &scene.id).await;
    lock.release().await;
    assert_eq!(pending_rows(pool, &scene.id).await, 0);
    assert!(!storage.exist(&scene.files[0].0).await.unwrap());
    assert!(
        storage.exist(&global_hash).await.unwrap(),
        "the collector's fixed set is kept"
    );
}

/// While a batch holds the entity's lock (committing or publishing the
/// upload, on any replica), cleanup skips the upload even once it expired,
/// and reclaims it on a later run.
#[tokio::test]
async fn cleanup_skips_an_upload_whose_lock_is_held() {
    require_test_db!(Some(node) = Node::new("locked", Limits::default()).await);
    let router = node.router();
    let pool = &node.state.pool;
    let storage = &node.state.storage;
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"locked-a", b"locked-b"], "scene").await;
    let (status, _, _) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), scene.files[0].clone()],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let lock = EntityLock::try_acquire(pool, &scene.id)
        .await
        .unwrap()
        .expect("free");
    let _reclaim = RECLAIM.lock().await;
    expire(pool, &scene.id).await;
    age_blob(storage, &scene.files[0].0).await;
    assert!(
        EntityLock::try_acquire(pool, &scene.id)
            .await
            .unwrap()
            .is_none(),
        "the lock is the database's: another session cannot take it"
    );
    cleanup_expired(pool, storage, &Limits::default())
        .await
        .unwrap();
    assert_eq!(
        pending_rows(pool, &scene.id).await,
        1,
        "a locked upload stays"
    );
    assert!(storage.exist(&scene.files[0].0).await.unwrap());

    lock.release().await;
    cleanup_expired(pool, storage, &Limits::default())
        .await
        .unwrap();
    assert_eq!(pending_rows(pool, &scene.id).await, 0);
    assert!(!storage.exist(&scene.files[0].0).await.unwrap());
}

/// A dropped lock guard (a cancelled request) frees the lock with its
/// session.
#[tokio::test]
async fn a_dropped_entity_lock_is_freed() {
    require_test_db!(Some(node) = Node::new("lock-drop", Limits::default()).await);
    let pool = &node.state.pool;
    let id = format!("lock-drop-{}", uuid::Uuid::new_v4());
    let lock = EntityLock::try_acquire(pool, &id)
        .await
        .unwrap()
        .expect("free");
    drop(lock);
    let again = EntityLock::acquire(pool, &id).await.unwrap();
    assert!(again.is_some(), "the closed session released it");
    again.unwrap().release().await;
}

/// Batches of one upload sent at once are serialized by the entity's lock:
/// the scene is published once, each answer is a 202 naming what the other
/// still had to send or the publication's 200, and nothing stays pending.
#[tokio::test]
async fn concurrent_batches_of_one_upload_publish_it_once() {
    require_test_db!(Some(node) = Node::new("concurrent", Limits::default()).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"conc-a", b"conc-b", b"conc-c"], "scene").await;
    let (a, b, c) = (
        scene.files[0].clone(),
        scene.files[1].clone(),
        scene.files[2].clone(),
    );
    let (status, _, _) = send(&router, &scene, &scene.chain, vec![entity_file(&scene), a]).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (first, second) = tokio::join!(
        send(&router, &scene, &scene.chain, vec![b.clone()]),
        send(&router, &scene, &scene.chain, vec![c.clone()]),
    );
    let answers = [(first, &c.0), (second, &b.0)];
    let mut published = Vec::new();
    for ((status, _, body), others) in &answers {
        match *status {
            StatusCode::OK => published.push(body["creationTimestamp"].as_i64().unwrap()),
            StatusCode::ACCEPTED => assert_eq!(body, &json!({"missing": [others]})),
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert!(!published.is_empty(), "one of them completes the upload");
    published.dedup();
    assert_eq!(published.len(), 1, "one publication");
    assert_eq!(pending_rows(&node.state.pool, &scene.id).await, 0);
}

/// The daily quota on the partial path, as upstream's `withDeploymentQuota`:
/// a 400 before joining an upload counts (a bad chain, a missing
/// `entityId`), creating an upload counts once, a resume and a replay do
/// not; a spent source may only resume its signer's own live upload, and
/// is turned away before its files are read otherwise.
#[tokio::test]
async fn the_daily_quota_counts_partial_batches_as_upstream() {
    let settings = Settings {
        post_entities_daily_quota_max: 3,
        ..Settings::for_tests()
    };
    require_test_db!(Some(node) = Node::with_settings("quota", settings).await);
    let router = node.router();
    let ip = "203.0.113.7";
    let quota = &node.state.deploy_quota;
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"quota-a", b"quota-b"], "scene").await;

    let stranger = PrivateKeySigner::random();
    let forged = chain_for(&stranger, &scene.id).await;
    let mut forged = forged;
    forged[0]["payload"] = json!(format!("{:#x}", signer.address()));
    let (status, _, _) = send(&router, &scene, &forged, vec![entity_file(&scene)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(quota.counted_for(ip), 1, "a refused new batch counts");

    let (status, _, _) = call(
        &router,
        batch_request("?partial=true", "", &scene.chain, &[], true),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(quota.counted_for(ip), 2, "a missing entityId counts");

    let (status, _, _) = send(
        &router,
        &scene,
        &scene.chain,
        vec![entity_file(&scene), scene.files[0].clone()],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        quota.counted_for(ip),
        3,
        "the upload counts once, when created"
    );

    // Spent: another upload is turned away, the signer's own goes on.
    let other = make_scene(&signer, &[b"quota-c"], "scene").await;
    let (status, headers, body) =
        send(&router, &other, &other.chain, vec![entity_file(&other)]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body["errors"][0],
        "The daily deployment quota for this client is exhausted. Only uploads in progress can continue."
    );
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert_eq!(pending_rows(&node.state.pool, &other.id).await, 0);
    let (status, _, body) = send(&router, &scene, &scene.chain, vec![scene.files[1].clone()]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) = send(&router, &scene, &scene.chain, vec![]).await;
    assert_eq!(status, StatusCode::OK, "a replay answers");
    assert_eq!(
        quota.counted_for(ip),
        3,
        "resumes, 429s and replays are not counted"
    );
}

/// An account whose byte-rate window is over its limit is turned away
/// before its next batch's files are read.
#[tokio::test]
async fn an_account_over_its_byte_rate_waits() {
    let limits = Limits {
        bytes_per_minute: 64,
        ..Limits::default()
    };
    require_test_db!(Some(node) = Node::new("byte-rate", limits).await);
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"rate-a"], "scene").await;
    let (status, _, body) = send(&router, &scene, &scene.chain, vec![entity_file(&scene)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0],
        format!(
            "This batch is {} bytes, over the partial upload byte rate limit of 64 bytes per minute. Send it in smaller batches.",
            scene.bytes.len()
        )
    );
    let (status, headers, body) =
        send(&router, &scene, &scene.chain, vec![entity_file(&scene)]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        body["errors"][0]
            .as_str()
            .unwrap()
            .starts_with(&format!(
                "Partial upload byte rate exceeded for this account: {} bytes in the current one-minute window, max 64.",
                scene.bytes.len()
            )),
        "{body}"
    );
    assert!(headers.contains_key(header::RETRY_AFTER));
}

/// Every replica reads the last cleanup run from the database.
#[tokio::test]
async fn the_last_cleanup_run_is_shared() {
    require_test_db!(Some(node) = Node::new("cleanup-at", Limits::default()).await);
    let pool = &node.state.pool;
    let _reclaim = RECLAIM.lock().await;
    let before = now_ms();
    cleanup_expired(pool, &node.state.storage, &Limits::default())
        .await
        .unwrap();
    let last = last_cleanup_at(pool).await.unwrap().expect("recorded");
    assert!(
        last >= before - 60_000 && last <= now_ms(),
        "{last} vs {before}"
    );
}

#[test]
fn ipfs_v2_ids_are_base32_cid_v1() {
    assert!(is_ipfs_v2(&hash_bytes_v1(b"x")));
    assert!(!is_ipfs_v2(
        "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
    ));
    assert!(!is_ipfs_v2("bafkrei"));
}

#[test]
fn the_query_declares_a_batch_only_with_true() {
    assert!(is_declared(Some("partial=true")));
    assert!(is_declared(Some("a=1&partial=true")));
    assert!(!is_declared(Some("partial=1")));
    assert!(!is_declared(None));
}

/// Upstream `checkPointerQuotas`: a full parcel first, then the account's
/// share of one (all but one upload, at least 1).
#[test]
fn pointer_quotas_name_the_full_parcels() {
    let usage = |uploads, deployer_uploads| PointerUsage {
        pointer: "1,1".into(),
        uploads,
        deployer_uploads,
        oldest: 1_000,
        deployer_oldest: Some(2_000),
    };
    let (message, retry) = check_pointer_quotas(&[usage(4, 0)], 4, 1_000, 60_000).unwrap();
    assert_eq!(
        message,
        "Too many partial uploads in progress for parcels 1,1 (max 4 per parcel). Complete an upload or wait for one to expire."
    );
    assert_eq!(retry, 60);
    let (message, retry) = check_pointer_quotas(&[usage(3, 3)], 4, 1_000, 60_000).unwrap();
    assert_eq!(
        message,
        "Too many partial uploads in progress for this account on parcels 1,1 (max 3 per parcel). Complete an upload or wait for one to expire."
    );
    assert_eq!(retry, 61);
    assert!(check_pointer_quotas(&[usage(2, 2)], 4, 1_000, 60_000).is_none());
}

/// Upstream `secondsUntilCleanupFreesQuota`.
#[test]
fn quota_retry_waits_for_the_cleanup_after_the_oldest_upload_expires() {
    // Before any run: from now, the first interval past the expiry.
    assert_eq!(
        seconds_until_cleanup_frees_quota(0, Some(0), 3_600_000, None, 300_000),
        3_600
    );
    // Already expired: the next run.
    assert_eq!(
        seconds_until_cleanup_frees_quota(4_000_000, Some(0), 3_600_000, Some(3_900_000), 300_000),
        200
    );
    assert_eq!(
        seconds_until_cleanup_frees_quota(10, None, 3_600_000, None, 300_000),
        300
    );
}

#[tokio::test]
async fn finalization_retains_rate_limited_uploads_and_releases_superseded_ones() {
    require_test_db!(Some(node) = Node::new("finalize-kinds", Limits::default()).await);
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"finalize-kinds-content"], "scene").await;
    let (status, _, _) = send(
        &node.router(),
        &scene,
        &scene.chain,
        vec![entity_file(&scene)],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (hash, bytes) = &scene.files[0];
    node.state.storage.store(hash, bytes.clone()).await.unwrap();
    let deployer = &node.state.writes.get().unwrap().deployer;
    let mut entity: Entity = serde_json::from_slice(&scene.bytes).unwrap();
    entity.id = scene.id.clone();
    let pending = pending(&node.state.pool, &scene.id).await.unwrap().unwrap();
    let limits = &node.state.settings.partial_uploads;
    let hashes = vec![hash.clone()];
    let mut other: Value = serde_json::from_slice(&scene.bytes).unwrap();
    other["timestamp"] = json!(entity.timestamp - 1);
    let earlier = Bytes::from(serde_json::to_vec(&other).unwrap());
    let earlier_id = hash_bytes_v1(&earlier);
    deployer
        .deploy_entity(
            vec![earlier, bytes.clone()],
            &earlier_id,
            chain_for(&signer, &earlier_id).await,
            "LOCAL",
        )
        .await
        .unwrap();
    let refusal = finalize(
        deployer,
        limits,
        &entity,
        scene.bytes.clone(),
        scene.chain.clone(),
        &hashes,
        pending.created_at,
        pending.created_at + limits.ttl_ms,
    )
    .await
    .err()
    .unwrap();
    let response = refusal.into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response.headers()[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 1
    );
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"errors": [format!("Entity rate limited (entityId={} pointers={}).", scene.id, scene.pointers.join(","))]})
    );
    assert_eq!(pending_rows(&node.state.pool, &scene.id).await, 1);

    deployer.clear_rate_limits();
    other["timestamp"] = json!(entity.timestamp + 1);
    let newer = Bytes::from(serde_json::to_vec(&other).unwrap());
    let newer_id = hash_bytes_v1(&newer);
    deployer
        .deploy_entity(
            vec![newer, bytes.clone()],
            &newer_id,
            chain_for(&signer, &newer_id).await,
            "LOCAL",
        )
        .await
        .unwrap();
    let refusal = finalize(
        deployer,
        limits,
        &entity,
        scene.bytes.clone(),
        scene.chain.clone(),
        &hashes,
        pending.created_at,
        pending.created_at + limits.ttl_ms,
    )
    .await
    .err()
    .unwrap();
    let response = refusal.into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get(header::RETRY_AFTER).is_none());
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"errors": [crate::write_deployer::newer_entity_message(&entity)]})
    );
    assert_eq!(pending_rows(&node.state.pool, &scene.id).await, 0);
}

#[tokio::test]
async fn terminal_pointer_contention_waits_then_retains_the_upload() {
    require_test_db!(Some(node) = Node::new("finalize-contention", Limits::default()).await);
    let signer = PrivateKeySigner::random();
    let scene = make_scene(&signer, &[b"contention-content"], "scene").await;
    let (status, _, _) = send(
        &node.router(),
        &scene,
        &scene.chain,
        vec![entity_file(&scene)],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (hash, bytes) = &scene.files[0];
    node.state.storage.store(hash, bytes.clone()).await.unwrap();
    let deployer = &node.state.writes.get().unwrap().deployer;
    let _claim = deployer.hold_pointers_for_tests("scene", &scene.pointers);
    let mut entity: Entity = serde_json::from_slice(&scene.bytes).unwrap();
    entity.id = scene.id.clone();
    let pending = pending(&node.state.pool, &scene.id).await.unwrap().unwrap();
    let limits = &node.state.settings.partial_uploads;
    let hashes = vec![hash.clone()];
    let started = tokio::time::Instant::now();
    let refusal = tokio::time::timeout(
        Duration::from_secs(10),
        finalize(
            deployer,
            limits,
            &entity,
            scene.bytes.clone(),
            scene.chain.clone(),
            &hashes,
            pending.created_at,
            pending.created_at + limits.ttl_ms,
        ),
    )
    .await
    .expect("contention retries are bounded")
    .err()
    .unwrap();
    assert!(
        started.elapsed() >= FINALIZE_POINTER_CONFLICT_DELAY * FINALIZE_POINTER_CONFLICT_RETRIES
    );
    let response = refusal.into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers()[header::RETRY_AFTER],
        POINTER_CONFLICT_RETRY_AFTER_SECS.to_string()
    );
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"errors": [format!("The following pointers are currently being deployed: '{}'. Please try again in a few seconds.", scene.pointers.join(","))]})
    );
    assert_eq!(pending_rows(&node.state.pool, &scene.id).await, 1);
}
