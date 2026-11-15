//! Router-level contracts for the profile images: the content path over the
//! process's scratch database (`db::scratch`) and a temporary blob store,
//! the origin proxy and the read-through over loopback stubs, the blob-store
//! cache's TTL and budget, the settings checks, and one env-gated live
//! render (`CATALYST_PROFILE_IMAGES_RENDER_LIVE=1`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use bytes::Bytes;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

use super::cache::{ImageCache, ImageKind, ImageSource, sweep_to_budget};
use super::render::{RenderBackend, RenderSettings};
use super::{ProfileImagesState, RawSettings, Settings, SharedState, router};
use crate::http::Outbound;
use crate::storage::Storage;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-face";
const PNG_BODY: &[u8] = b"\x89PNG\r\n\x1a\nfake-body";

/// A distinct, canonical-looking CIDv1 (`ba` + base32 letters, 71 chars),
/// accepted by the route and by the blob store.
fn fake_cid() -> String {
    let m: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .bytes()
        .map(|b| (b'a' + (b % 16)) as char)
        .collect();
    format!("bafkrei{m}{m}")
}

fn temp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "cpi-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

fn lazy_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(300))
        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
        .unwrap()
}

/// The temporary directories one test makes, removed when the test ends,
/// passed or panicked.
#[derive(Default)]
struct Scratch(std::sync::Mutex<Vec<PathBuf>>);

impl Scratch {
    fn temp(&self, label: &str) -> PathBuf {
        let dir = temp(label);
        self.0.lock().unwrap().push(dir.clone());
        dir
    }

    async fn storage(&self, label: &str) -> Arc<Storage> {
        Arc::new(Storage::from_config(&self.temp(label)).await.unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for dir in self.0.get_mut().unwrap().drain(..) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The tests that write `social.profile_images` rows: the budget sweep drops
/// rows across the whole table, so it never runs beside another writer.
static PROFILE_IMAGE_ROWS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn app(pool: PgPool, storage: Arc<Storage>, settings: Settings) -> Router {
    app_state(pool, storage, settings).0
}

fn app_state(pool: PgPool, storage: Arc<Storage>, settings: Settings) -> (Router, SharedState) {
    let state = Arc::new(ProfileImagesState::new(
        pool,
        storage,
        Outbound::for_tests(),
        settings,
    ));
    (router(state.clone()), state)
}

fn count(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.load(Ordering::SeqCst)
}

async fn get_image(
    app: &Router,
    path: &str,
) -> (StatusCode, Option<String>, Option<String>, Bytes) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let h = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (ct, retry_after) = (h("content-type"), h("retry-after"));
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, ct, retry_after, bytes)
}

/// A profile deployment with the given content files; returns its id.
async fn seed_profile(pool: &PgPool, entity: &str, files: &[(&str, &str)]) -> i32 {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, \
             entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
         VALUES ('0xdeployer', 'v3', 'profile', $1, $2, to_timestamp(1700000000), ARRAY['0xdeployer'], \
                 timezone('UTC', now()), '[]'::json) RETURNING id",
    )
    .bind(entity)
    .bind(json!({ "v": { "avatars": [ { "avatar": { "bodyShape": "urn:decentraland:off-chain:base-avatars:BaseMale", "wearables": [] } } ] } }))
    .fetch_one(pool)
    .await
    .unwrap();
    for (key, hash) in files {
        sqlx::query(
            "INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(hash)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

async fn unseed(pool: &PgPool, id: i32) {
    sqlx::query("DELETE FROM content.content_files WHERE deployment = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.deployments WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_invalid_entity_id_is_a_400_before_any_lookup() {
    let scratch = Scratch::default();
    let app = app(
        lazy_pool(),
        scratch.storage("invalid").await,
        Settings::for_tests(scratch.temp("invalid-cache")),
    );
    for path in [
        "/entities/0xabc/face.png",
        "/entities/..%2Fetc/body.png",
        "/entities/entities/face.png",
    ] {
        let (status, _, _, _) = get_image(&app, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
}

/// The primary path: the deployed profile's own snapshot files, keyed by
/// entity id, read from the blob store; a profile without the file, a
/// non-profile entity and an unknown id are 404 when no other source is
/// configured.
#[tokio::test]
async fn snapshots_are_served_from_the_profile_entity_content_files() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let store = scratch.storage("content").await;
    let (entity, face_hash, body_hash) = (fake_cid(), fake_cid(), fake_cid());
    store
        .store(&face_hash, Bytes::from_static(PNG))
        .await
        .unwrap();
    store
        .store(&body_hash, Bytes::from_static(PNG_BODY))
        .await
        .unwrap();
    let id = seed_profile(
        &pool,
        &entity,
        &[("face256.png", &face_hash), ("body.png", &body_hash)],
    )
    .await;
    let bare = fake_cid();
    let bare_id = seed_profile(&pool, &bare, &[]).await;
    let missing_blob = fake_cid();
    let missing_id = seed_profile(&pool, &missing_blob, &[("Face256.PNG", &fake_cid())]).await;

    let (app, state) = app_state(
        pool.clone(),
        store.clone(),
        Settings::for_tests(scratch.temp("content-cache")),
    );

    let (status, ct, _, bytes) = get_image(&app, &format!("/entities/{entity}/face.png")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("image/png"));
    assert_eq!(&bytes[..], PNG);

    let (status, _, _, bytes) = get_image(&app, &format!("/entities/{entity}/body.png")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], PNG_BODY);
    assert_eq!(count(&state.producer.counters.snapshot), 2);

    let (status, _, _, _) = get_image(&app, &format!("/entities/{bare}/face.png")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a profile without snapshot files"
    );
    let (status, _, _, _) = get_image(&app, &format!("/entities/{missing_blob}/face.png")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the file is listed (any case) but the blob is absent"
    );
    let (status, _, _, _) = get_image(&app, &format!("/entities/{}/face.png", fake_cid())).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an entity this node does not hold"
    );

    // A blob the store holds under another entity type is not a profile image.
    let scene = fake_cid();
    let scene_id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_timestamp, \
             entity_pointers, local_timestamp, auth_chain) \
         VALUES ('0xdeployer', 'v3', 'scene', $1, to_timestamp(1700000000), ARRAY['0,0'], timezone('UTC', now()), '[]'::json) \
         RETURNING id",
    )
    .bind(&scene)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, 'face256.png')")
        .bind(scene_id)
        .bind(&face_hash)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _, _, _) = get_image(&app, &format!("/entities/{scene}/face.png")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for id in [id, bare_id, missing_id, scene_id] {
        unseed(&pool, id).await;
    }
}

/// A store that cannot be read is never reported as "no such image".
#[tokio::test]
async fn an_unreadable_store_answers_503_not_404() {
    let scratch = Scratch::default();
    let app = app(
        lazy_pool(),
        scratch.storage("down").await,
        Settings::for_tests(scratch.temp("down-cache")),
    );
    let (status, _, _, _) = get_image(&app, &format!("/entities/{}/face.png", fake_cid())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

async fn spawn_origin(handler: axum::routing::MethodRouter) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/entities/{entity}/{file}", handler);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// A miss on the content path goes to the origin, the answer lands in the
/// blob store under a `social.profile_images` row (the second request is
/// answered from it, the origin not asked again), an origin 404 is a 404
/// (remembered: not asked again) and an origin that keeps failing a 503 with
/// `Retry-After`; an entity this node holds as another type is a 404 the
/// origin is never asked about. The stub fails exactly the two ids it is
/// handed, never a random one.
#[tokio::test]
async fn the_origin_is_the_last_resort_and_its_answers_are_kept_in_the_blob_store() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let (entity, gone, broken) = (fake_cid(), fake_cid(), fake_cid());
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (gone_upstream, broken_upstream) = (gone.clone(), broken.clone());
    let addr = spawn_origin(get(
        move |axum::extract::Path((asked, file)): axum::extract::Path<(String, String)>| {
            let seen = seen.clone();
            let failure = if asked == gone_upstream {
                Some(StatusCode::NOT_FOUND)
            } else if asked == broken_upstream {
                Some(StatusCode::INTERNAL_SERVER_ERROR)
            } else {
                None
            };
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                if let Some(status) = failure {
                    return (status, Bytes::new());
                }
                let body = if file == "face.png" { PNG } else { PNG_BODY };
                (StatusCode::OK, Bytes::from_static(body))
            }
        },
    ))
    .await;
    let cache_dir = scratch.temp("origin-cache");
    let settings = Settings {
        origin_url: Some(format!("http://{addr}")),
        budget: std::time::Duration::from_secs(2),
        ..Settings::for_tests(&cache_dir)
    };
    let store = scratch.storage("origin").await;
    let (app, state) = app_state(pool.clone(), store.clone(), settings);

    let (status, _, _, bytes) = get_image(&app, &format!("/entities/{entity}/face.png")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], PNG);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (cid, size, source): (String, i32, String) = sqlx::query_as(
        "SELECT blob_cid, byte_size, source FROM social.profile_images WHERE entity_id = $1 AND kind = 'face'",
    )
    .bind(&entity)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cid, crate::auth::hash::hash_bytes_v1(PNG));
    assert_eq!((size as usize, source.as_str()), (PNG.len(), "origin"));
    assert_eq!(&store.retrieve(&cid).await.unwrap().unwrap()[..], PNG);

    let (status, _, _, bytes) = get_image(&app, &format!("/entities/{entity}/face.png")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], PNG);
    assert_eq!(count(&state.producer.counters.stored), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the blob store answers the second request"
    );

    let (status, _, _, bytes) = get_image(&app, &format!("/entities/{entity}/body.png")).await;
    assert_eq!((status, &bytes[..]), (StatusCode::OK, PNG_BODY));
    assert_eq!(
        count(&state.producer.counters.origin),
        2,
        "face and body are stored independently"
    );

    let (status, _, _, _) = get_image(&app, &format!("/entities/{gone}/face.png")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let before = calls.load(Ordering::SeqCst);
    let (status, _, _, _) = get_image(&app, &format!("/entities/{gone}/face.png")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(calls.load(Ordering::SeqCst), before, "a 404 is remembered");
    let (status, _, retry_after, _) =
        get_image(&app, &format!("/entities/{broken}/face.png")).await;
    assert_eq!(
        (status, retry_after.as_deref()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("5"))
    );
    assert!(
        calls.load(Ordering::SeqCst) > before + 2,
        "a 5xx is retried within the budget"
    );

    let scene = fake_cid();
    let scene_id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_timestamp, \
             entity_pointers, local_timestamp, auth_chain) \
         VALUES ('0xdeployer', 'v3', 'scene', $1, to_timestamp(1700000000), ARRAY['0,0'], timezone('UTC', now()), '[]'::json) \
         RETURNING id",
    )
    .bind(&scene)
    .fetch_one(&pool)
    .await
    .unwrap();
    let before = calls.load(Ordering::SeqCst);
    let (status, _, _, _) = get_image(&app, &format!("/entities/{scene}/face.png")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "a scene is no profile"
    );
    unseed(&pool, scene_id).await;
}

/// A profile the origin draws is served without filling its entity; one it
/// has no image for is asked of the read-through's sources (`/entities/active`
/// by id), once, for the renderer. A held profile is never asked for.
#[tokio::test]
async fn an_entity_not_held_is_filled_only_when_the_origin_has_no_image() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let asked = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = asked.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = listener.local_addr().unwrap();
    let peer_app = Router::new().route(
        "/content/entities/active",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            seen.lock().unwrap().push(body);
            async { axum::Json(json!([])) }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, peer_app).await.unwrap() });
    let (drawn, undrawn, held) = (fake_cid(), fake_cid(), fake_cid());
    let missing = undrawn.clone();
    let origin = spawn_origin(get(
        move |axum::extract::Path((asked, _)): axum::extract::Path<(String, String)>| {
            let missing = missing.clone();
            async move {
                if asked == missing {
                    (StatusCode::NOT_FOUND, Bytes::new())
                } else {
                    (StatusCode::OK, Bytes::from_static(PNG))
                }
            }
        },
    ))
    .await;

    let store = scratch.storage("read-through").await;
    let read_through = Arc::new(crate::content::read_through::ReadThrough::new(
        pool.clone(),
        store.clone(),
        reqwest::Client::new(),
        vec![format!("http://{peer}/content")],
        None,
        crate::content::read_through::Fills::new(4, std::time::Duration::from_secs(10)),
        crate::content::sync_deployer::SyncValidation::AuthChainOnly,
    ));
    let settings = Settings {
        origin_url: Some(format!("http://{origin}")),
        ..Settings::for_tests(scratch.temp("read-through-cache"))
    };
    let state = Arc::new(
        ProfileImagesState::new(pool.clone(), store, Outbound::for_tests(), settings)
            .with_read_through(Some(read_through)),
    );
    let app = router(state.clone());

    let held_id = seed_profile(&pool, &held, &[]).await;
    let (status, _, _, _) = get_image(&app, &format!("/entities/{drawn}/face.png")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _, _) = get_image(&app, &format!("/entities/{held}/face.png")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(asked.lock().unwrap().is_empty(), "the origin answered");
    let (status, _, _, _) = get_image(&app, &format!("/entities/{undrawn}/face.png")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(*asked.lock().unwrap(), vec![json!({ "ids": [undrawn] })]);
    unseed(&pool, held_id).await;
}

/// A row past the TTL is a miss; with no TTL it never ages.
#[tokio::test]
async fn a_stored_image_past_its_ttl_is_a_miss() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let store = scratch.storage("ttl").await;
    let (aging, forever) = (
        ImageCache::new(pool.clone(), store.clone(), 3600, None),
        ImageCache::new(pool.clone(), store, 0, None),
    );
    let entity = fake_cid();
    let png = Bytes::from_static(PNG_BODY);
    aging
        .put(&entity, ImageKind::Body, ImageSource::Render, &png)
        .await
        .unwrap();
    assert_eq!(aging.get(&entity, ImageKind::Body).await, Some(png.clone()));
    assert_eq!(aging.get(&entity, ImageKind::Face).await, None);
    sqlx::query("UPDATE social.profile_images SET stored_at = stored_at - interval '2 hours' WHERE entity_id = $1")
        .bind(&entity)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(aging.get(&entity, ImageKind::Body).await, None);
    assert_eq!(forever.get(&entity, ImageKind::Body).await, Some(png));
}

/// Over the budget, the sweep keeps the newest rows that fit 90% of it and
/// drops the rest; under it, it drops nothing.
#[tokio::test]
async fn the_sweep_keeps_the_newest_images_within_the_budget() {
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let entities: Vec<String> = (0..10).map(|_| fake_cid()).collect();
    for (i, entity) in entities.iter().enumerate() {
        sqlx::query(
            "INSERT INTO social.profile_images (entity_id, kind, blob_cid, byte_size, source, stored_at) \
             VALUES ($1, 'face', $1, 1000, 'origin', timestamp '2999-01-01' + make_interval(secs => $2))",
        )
        .bind(entity)
        .bind(i as f64)
        .execute(&pool)
        .await
        .unwrap();
    }
    assert!(sweep_to_budget(&pool, 5000).await.unwrap() >= 6);
    let left: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM social.profile_images WHERE entity_id = ANY($1) ORDER BY stored_at",
    )
    .bind(&entities)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        left,
        entities[6..].to_vec(),
        "the four newest fit 4500 bytes"
    );
    assert_eq!(sweep_to_budget(&pool, 5000).await.unwrap(), 0);
    sqlx::query("DELETE FROM social.profile_images WHERE entity_id = ANY($1)")
        .bind(&entities)
        .execute(&pool)
        .await
        .unwrap();
}

#[test]
fn settings_default_to_the_content_path_alone() {
    let s = Settings::parse(RawSettings::default()).unwrap();
    assert_eq!(s.cache_dir, PathBuf::from(super::DEFAULT_CACHE_DIR));
    assert!(s.render.is_none());
    assert!(s.origin_url.is_none());
    assert!(s.cache_max_bytes.is_none());
}

#[test]
fn the_bevy_renderer_is_the_default_and_needs_its_binary() {
    let err = Settings::parse(RawSettings {
        render_enabled: true,
        ..Default::default()
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("PROFILE_IMAGES_AVATAR_RENDER_BIN"),
        "{err}"
    );
    let s = Settings::parse(RawSettings {
        render_enabled: true,
        avatar_render_bin: "/opt/batch-tools/bin/avatar_render".into(),
        // the godot-only switch has no bearing on the bevy renderer
        godot_headless: true,
        cache_dir: "/tmp/pi".into(),
        ..Default::default()
    })
    .unwrap();
    let r: RenderSettings = s.render.unwrap();
    assert_eq!(r.backend, RenderBackend::Bevy);
    assert_eq!(r.bin, "/opt/batch-tools/bin/avatar_render");
    assert_eq!(r.work_root, None);
    assert_eq!(r.workdir_root, "/tmp/pi/.render-tmp");
    let err = Settings::parse(RawSettings {
        render_enabled: true,
        renderer: "unity".into(),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.to_string().contains("PROFILE_IMAGES_RENDERER"), "{err}");
}

#[test]
fn the_godot_renderer_needs_a_binary_and_refuses_headless() {
    let err = Settings::parse(RawSettings {
        render_enabled: true,
        renderer: "godot".into(),
        ..Default::default()
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("PROFILE_IMAGES_GODOT_BIN"),
        "{err}"
    );
    let err = Settings::parse(RawSettings {
        render_enabled: true,
        renderer: "godot".into(),
        godot_bin: "/opt/godot/bin/decentraland.godot.client.x86_64".into(),
        godot_headless: true,
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.to_string().contains("--headless"), "{err}");
    let s = Settings::parse(RawSettings {
        render_enabled: true,
        renderer: "godot".into(),
        godot_bin: "/opt/godot/bin/decentraland.godot.client.x86_64".into(),
        godot_extra_args: "--a  --b".into(),
        cache_dir: "/tmp/pi".into(),
        content_public_url: "https://peer.example.org/content/".into(),
        ..Default::default()
    })
    .unwrap();
    let r: RenderSettings = s.render.unwrap();
    assert_eq!(r.backend, RenderBackend::Godot);
    assert_eq!(r.work_root.as_deref(), Some("/opt/godot"));
    assert_eq!(r.extra_args, vec!["--a", "--b"]);
    assert_eq!(r.workdir_root, "/tmp/pi/.render-tmp");
    assert_eq!(r.timeout_seconds, 120);
    assert_eq!(r.max_concurrent, 1);
    assert_eq!(s.content_public_url, "https://peer.example.org/content");
}

#[test]
fn the_origin_must_be_public() {
    let err = Settings::parse(RawSettings {
        origin_url: "http://127.0.0.1:5152".into(),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.to_string().contains("public"), "{err}");
    let s = Settings::parse(RawSettings {
        origin_url: "https://profile-images.decentraland.org/".into(),
        cache_max_bytes: 10,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        s.origin_url.as_deref(),
        Some("https://profile-images.decentraland.org")
    );
    assert_eq!(s.cache_max_bytes, Some(10));
}

/// One real render, opt-in: the runtime is external (the engine's
/// `avatar_render` needs a GPU adapter; the godot client, with
/// `PROFILE_IMAGES_RENDERER=godot`, also a display). `PROFILE_IMAGES_LIVE_ENTITY` names a profile entity
/// the node at `PROFILE_IMAGES_LIVE_CONTENT_URL` (default
/// peer.decentraland.org/content) serves; its metadata is copied into the
/// scratch database so the renderer's own resolve path runs.
///
/// ```sh
/// CATALYST_PROFILE_IMAGES_RENDER_LIVE=1 PROFILE_IMAGES_AVATAR_RENDER_BIN=... \
///   PROFILE_IMAGES_LIVE_ENTITY=bafkrei... CATALYST_TEST_DATABASE_URL="$DATABASE_URL" \
///   cargo test --lib profile_images::tests::a_live_render -- --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_render_produces_both_images_and_caches_them() {
    let scratch = Scratch::default();
    if std::env::var("CATALYST_PROFILE_IMAGES_RENDER_LIVE")
        .ok()
        .as_deref()
        != Some("1")
    {
        eprintln!("skipping: CATALYST_PROFILE_IMAGES_RENDER_LIVE=1 not set");
        return;
    }
    let Some(pool) = crate::db::scratch::pool(2).await else {
        eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
        return;
    };
    let entity = std::env::var("PROFILE_IMAGES_LIVE_ENTITY").expect("PROFILE_IMAGES_LIVE_ENTITY");
    let content_url = std::env::var("PROFILE_IMAGES_LIVE_CONTENT_URL")
        .unwrap_or_else(|_| "https://peer.decentraland.org/content".to_string());
    let manifest: serde_json::Value = reqwest::get(format!("{content_url}/contents/{entity}"))
        .await
        .expect("fetching the profile entity")
        .json()
        .await
        .expect("profile entity json");
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, \
             entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
         VALUES ('0xlive', 'v3', 'profile', $1, $2, to_timestamp(1700000000), ARRAY['0xlive'], \
                 timezone('UTC', now()), '[]'::json) RETURNING id",
    )
    .bind(&entity)
    .bind(json!({ "v": manifest["metadata"] }))
    .fetch_one(&pool)
    .await
    .unwrap();

    let cache_dir = scratch.temp("live-cache");
    let settings = Settings::parse(RawSettings {
        cache_dir: cache_dir.to_string_lossy().into_owned(),
        render_enabled: true,
        renderer: std::env::var("PROFILE_IMAGES_RENDERER").unwrap_or_default(),
        avatar_render_bin: std::env::var("PROFILE_IMAGES_AVATAR_RENDER_BIN").unwrap_or_default(),
        godot_bin: std::env::var("PROFILE_IMAGES_GODOT_BIN").unwrap_or_default(),
        godot_project: std::env::var("PROFILE_IMAGES_GODOT_PROJECT").unwrap_or_default(),
        godot_display: std::env::var("PROFILE_IMAGES_GODOT_DISPLAY").unwrap_or_default(),
        dclenv: std::env::var("PROFILE_IMAGES_DCLENV").unwrap_or_default(),
        content_public_url: content_url,
        ..Default::default()
    })
    .unwrap();
    let (app, state) = app_state(pool.clone(), scratch.storage("live").await, settings);
    let started = std::time::Instant::now();
    let (status, ct, _, face) = get_image(&app, &format!("/entities/{entity}/face.png")).await;
    eprintln!(
        "render in {:?}: {status} {ct:?} {} bytes",
        started.elapsed(),
        face.len()
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count(&state.producer.counters.render), 1);
    assert!(face.starts_with(b"\x89PNG"));
    let (status, _, _, body) = get_image(&app, &format!("/entities/{entity}/body.png")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        count(&state.producer.counters.stored),
        1,
        "the body came out of the same render"
    );
    assert!(body.len() > face.len() / 4);

    unseed(&pool, id).await;
}

/// An origin stub that answers each request with the next status of
/// `script` (then 200 with the PNG), counting the calls and holding each for
/// `delay`.
async fn scripted_origin(
    script: Vec<(StatusCode, Option<&'static str>)>,
    delay: std::time::Duration,
) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let script = Arc::new(script);
    let addr = spawn_origin(get(move || {
        let (seen, script) = (seen.clone(), script.clone());
        async move {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            let mut resp = axum::response::IntoResponse::into_response(match script.get(n) {
                Some((status, _)) => (*status, Bytes::new()),
                None => (StatusCode::OK, Bytes::from_static(PNG)),
            });
            if let Some((_, Some(after))) = script.get(n) {
                resp.headers_mut()
                    .insert("retry-after", after.parse().unwrap());
            }
            resp
        }
    }))
    .await;
    (addr, calls)
}

fn origin_settings(scratch: &Scratch, addr: std::net::SocketAddr, budget_secs: u64) -> Settings {
    Settings {
        origin_url: Some(format!("http://{addr}")),
        budget: std::time::Duration::from_secs(budget_secs),
        ..Settings::for_tests(scratch.temp("cache"))
    }
}

/// Concurrent first requests for one image share one production: the
/// origin is asked once and every reader gets the image.
#[tokio::test]
async fn concurrent_first_requests_share_one_fetch() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(4).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let (release, held) = tokio::sync::watch::channel(false);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let addr = spawn_origin(get(move || {
        let (seen, mut held) = (seen.clone(), held.clone());
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = held.wait_for(|released| *released).await;
            (StatusCode::OK, Bytes::from_static(PNG))
        }
    }))
    .await;
    let (app, state) = app_state(
        pool,
        scratch.storage("single").await,
        origin_settings(&scratch, addr, 10),
    );
    let entity = fake_cid();
    let path = format!("/entities/{entity}/face.png");
    let counters = &state.producer.counters;
    let readers = futures::future::join_all((0..10).map(|_| get_image(&app, &path)));
    let mut readers = std::pin::pin!(readers);
    // The origin answers once all ten readers are in the production: one
    // that came after the image was stored would read it from the store and
    // share nothing. The readers run only inside this poll, so the check
    // after it sees each of them arrive.
    let answers = std::future::poll_fn(|cx| {
        let polled = readers.as_mut().poll(cx);
        if count(&counters.produced) + count(&counters.joined) == 10 {
            release.send_replace(true);
        }
        polled
    })
    .await;
    for (status, _, _, bytes) in answers {
        assert_eq!((status, &bytes[..]), (StatusCode::OK, PNG));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&state.producer.counters.produced), 1);
    assert_eq!(count(&state.producer.counters.joined), 9);
}

/// A 503 and a 429 with a short `Retry-After` are retried within the budget:
/// the first request answers the image.
#[tokio::test]
async fn transient_origin_failures_are_retried_within_the_budget() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let (addr, calls) = scripted_origin(
        vec![
            (StatusCode::SERVICE_UNAVAILABLE, None),
            (StatusCode::TOO_MANY_REQUESTS, Some("1")),
        ],
        std::time::Duration::ZERO,
    )
    .await;
    let app = app(
        pool,
        scratch.storage("retry").await,
        origin_settings(&scratch, addr, 10),
    );
    let started = std::time::Instant::now();
    let (status, _, _, bytes) =
        get_image(&app, &format!("/entities/{}/face.png", fake_cid())).await;
    assert_eq!((status, &bytes[..]), (StatusCode::OK, PNG));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(1),
        "Retry-After is honoured"
    );
}

/// A source slower than the budget is a 503 with `Retry-After`, never a
/// 404; the production keeps going and stores the image for the next reader.
#[tokio::test]
async fn a_source_slower_than_the_budget_is_a_503_and_still_lands() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let (addr, calls) = scripted_origin(vec![], std::time::Duration::from_millis(1500)).await;
    let (app, state) = app_state(
        pool,
        scratch.storage("slow").await,
        origin_settings(&scratch, addr, 1),
    );
    let path = format!("/entities/{}/face.png", fake_cid());
    let (status, _, retry_after, _) = get_image(&app, &path).await;
    assert_eq!(
        (status, retry_after.as_deref()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("5"))
    );
    assert_eq!(count(&state.producer.counters.unavailable), 1);
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

/// The entity fill is waited for past the read-through's own guard (1 s
/// here): the route's budget is the bound. The origin, which has no image,
/// is what makes the fill worth it.
#[tokio::test]
async fn the_entity_fill_is_waited_for_past_the_read_through_guard() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let (peer, asked, answered) = slow_peer().await;
    let store = scratch.storage("fill-wait").await;
    let settings = Settings {
        origin_url: Some(format!("http://{peer}")),
        budget: std::time::Duration::from_secs(10),
        ..Settings::for_tests(scratch.temp("fill-wait-cache"))
    };
    let app = router(Arc::new(
        ProfileImagesState::new(pool.clone(), store.clone(), Outbound::for_tests(), settings)
            .with_read_through(Some(slow_read_through(pool, store, peer))),
    ));
    let (status, _, _, _) = get_image(&app, &format!("/entities/{}/face.png", fake_cid())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no source knows it");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(
        answered.load(Ordering::SeqCst),
        1,
        "the answer came after the sources did, not at the 1 s guard"
    );
}

/// Without an origin or a renderer no image can come of a fill, so the sync
/// sources are not asked for the entity.
#[tokio::test]
async fn without_an_origin_or_a_renderer_the_sources_are_not_asked() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let (peer, asked, _) = slow_peer().await;
    let store = scratch.storage("no-sources").await;
    let app = router(Arc::new(
        ProfileImagesState::new(
            pool.clone(),
            store.clone(),
            Outbound::for_tests(),
            Settings::for_tests(scratch.temp("no-sources-cache")),
        )
        .with_read_through(Some(slow_read_through(pool, store, peer))),
    ));
    let (status, _, _, _) = get_image(&app, &format!("/entities/{}/face.png", fake_cid())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "no fill without a source to draw from"
    );
}

/// A sync source that knows no entity and answers after 2 s, and an origin
/// with no image on the same address: (address, entity requests, answers).
async fn slow_peer() -> (std::net::SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = listener.local_addr().unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let answered = Arc::new(AtomicUsize::new(0));
    let (ask, done) = (asked.clone(), answered.clone());
    let peer_app = Router::new()
        .route(
            "/content/entities/active",
            axum::routing::post(move || {
                let (ask, done) = (ask.clone(), done.clone());
                async move {
                    ask.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    done.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!([]))
                }
            }),
        )
        .route(
            "/entities/{entity}/face.png",
            axum::routing::get(|| async { StatusCode::NOT_FOUND }),
        );
    tokio::spawn(async move { axum::serve(listener, peer_app).await.unwrap() });
    (peer, asked, answered)
}

fn slow_read_through(
    pool: PgPool,
    store: Arc<Storage>,
    peer: std::net::SocketAddr,
) -> Arc<crate::content::read_through::ReadThrough> {
    Arc::new(crate::content::read_through::ReadThrough::new(
        pool,
        store,
        reqwest::Client::new(),
        vec![format!("http://{peer}/content")],
        None,
        crate::content::read_through::Fills::new(4, std::time::Duration::from_secs(1)),
        crate::content::sync_deployer::SyncValidation::AuthChainOnly,
    ))
}

/// A profile deployed minutes ago may not be drawn upstream yet: its origin
/// 404 is retried; an old profile's 404 is final at once.
#[tokio::test]
async fn a_fresh_profile_waits_for_the_origin_to_draw_it() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let (addr, calls) = scripted_origin(
        vec![(StatusCode::NOT_FOUND, None), (StatusCode::NOT_FOUND, None)],
        std::time::Duration::ZERO,
    )
    .await;
    let fresh_app = app(
        pool.clone(),
        scratch.storage("fresh").await,
        origin_settings(&scratch, addr, 10),
    );
    let fresh = fake_cid();
    let fresh_id = seed_profile(&pool, &fresh, &[]).await;
    sqlx::query(
        "UPDATE content.deployments SET entity_timestamp = timezone('UTC', now()) WHERE id = $1",
    )
    .bind(fresh_id)
    .execute(&pool)
    .await
    .unwrap();
    let (status, _, _, bytes) = get_image(&fresh_app, &format!("/entities/{fresh}/face.png")).await;
    assert_eq!((status, &bytes[..]), (StatusCode::OK, PNG));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    unseed(&pool, fresh_id).await;

    let (addr, calls) = scripted_origin(
        vec![(StatusCode::NOT_FOUND, None)],
        std::time::Duration::ZERO,
    )
    .await;
    let app = app(
        pool.clone(),
        scratch.storage("old").await,
        origin_settings(&scratch, addr, 10),
    );
    let old = fake_cid();
    let old_id = seed_profile(&pool, &old, &[]).await;
    let (status, _, _, _) = get_image(&app, &format!("/entities/{old}/face.png")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    unseed(&pool, old_id).await;
}

/// Warming fetches a listed profile's face ahead of the request, once, and
/// the request finds it stored without asking the origin.
#[tokio::test]
async fn a_warmed_face_is_stored_before_it_is_asked_for() {
    let scratch = Scratch::default();
    let Some(pool) = crate::db::scratch::pool(2).await else {
        return;
    };
    let _rows = PROFILE_IMAGE_ROWS.lock().await;
    let (addr, calls) = scripted_origin(vec![], std::time::Duration::ZERO).await;
    let (app, state) = app_state(
        pool,
        scratch.storage("warm").await,
        origin_settings(&scratch, addr, 10),
    );
    let entity = fake_cid();
    state.warm([entity.clone(), entity.clone(), "not-a-cid".to_string()]);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while count(&state.producer.counters.warmed) < 1 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the warm worker finished");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    state.warm([entity.clone()]);
    let (status, _, _, bytes) = get_image(&app, &format!("/entities/{entity}/face.png")).await;
    assert_eq!((status, &bytes[..]), (StatusCode::OK, PNG));
    assert_eq!(count(&state.producer.counters.stored), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "warmed once, served from the store"
    );
}
