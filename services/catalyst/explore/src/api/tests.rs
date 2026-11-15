//! In-process contract tests: every route through the gateway router against
//! one mock upstream that stands in for all five sources. Database-backed
//! tests create a scratch database on the cluster `CATALYST_TEST_DATABASE_URL`
//! points at (schema.sql applied, dropped on teardown) and skip without it.

use std::collections::{BTreeSet, HashMap};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State as AxState};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tower::ServiceExt;

use super::*;
use crate::content::{ContentState, SyncState};
use crate::jobs::Jobs;
use crate::storage::Storage;

struct Mock {
    pois: Mutex<Vec<String>>,
    banned: Mutex<Vec<String>>,
    denylist: Mutex<Vec<Value>>,
    flags: Mutex<Value>,
    flags_etag: Mutex<String>,
    /// The `events` application's document (`events.json`), the one
    /// `events::foundation` reads.
    events_flags: Mutex<Value>,
    entities: Mutex<Vec<Value>>,
    profiles: Mutex<Vec<Value>>,
    tiles: Mutex<Value>,
    tiles_etag: Mutex<String>,
    tiles_last_updated: AtomicI64,
    hits: Mutex<HashMap<String, usize>>,
    bodies: Mutex<Vec<Value>>,
}

impl Mock {
    fn hit(&self, what: &str) {
        *self.hits.lock().entry(what.to_string()).or_default() += 1;
    }

    fn hits(&self, what: &str) -> usize {
        self.hits.lock().get(what).copied().unwrap_or(0)
    }
}

fn entity(id: &str, pointers: &[&str]) -> Value {
    json!({
        "id": id, "type": "scene", "timestamp": "1789578092206", "pointers": pointers, "content": [],
        "metadata": {}, "deployer": "0x462a", "status": "complete",
        "bundles": {"assets": {"mac": "complete", "windows": "complete", "webgl": "pending"}},
        "versions": {"assets": {"mac": {"version": "v49", "buildDate": "2026-09-16T17:14:28.886Z"}}}
    })
}

fn tile(x: i32, y: i32, kind: &str, estate: Option<&str>) -> Value {
    let mut t = json!({"id": format!("{x},{y}"), "x": x, "y": y, "type": kind, "top": false, "left": false, "topLeft": false, "updatedAt": 1});
    if let Some(e) = estate {
        t["estateId"] = json!(e);
        t["owner"] = json!("0xowner");
    }
    t
}

fn tiles_doc(stamp: i64) -> Value {
    let mut data = serde_json::Map::new();
    for t in [
        tile(0, 0, "plaza", Some("1164")),
        tile(1, 0, "road", None),
        tile(1, 1, "owned", Some("77")),
        tile(2, 1, "district", None),
    ] {
        data.insert(t["id"].as_str().unwrap().to_string(), t);
    }
    json!({"ok": true, "data": data, "stamp": stamp})
}

fn mock() -> Arc<Mock> {
    Arc::new(Mock {
        pois: Mutex::new(vec!["0,0".into(), "16,83".into(), "-148,131".into()]),
        banned: Mutex::new(vec!["badname".into()]),
        denylist: Mutex::new(vec![
            json!({"wallet": "0xAbC0000000000000000000000000000000000001"}),
            json!("0x0000000000000000000000000000000000000002"),
        ]),
        flags: Mutex::new(
            json!({"flags": {"explorer-alfa-minimum-requirements": true, "explorer-feature-a": false}, "variants": {"explorer-variant": {"name": "v", "enabled": true}}}),
        ),
        flags_etag: Mutex::new("\"flags-1\"".into()),
        events_flags: Mutex::new(json!({
            "flags": {"events-decentraland-foundation-addresses": true},
            "variants": {"events-decentraland-foundation-addresses": {
                "name": "addresses", "enabled": true,
                "payload": {"type": "json", "value": "[\"0xF0000000000000000000000000000000000000F0\"]"}
            }}
        })),
        entities: Mutex::new(vec![
            entity("bafyscene", &["0,0", "0,1"]),
            entity("bafyother", &["5,5"]),
        ]),
        profiles: Mutex::new(vec![
            json!({"timestamp": "1790276949479", "avatars": [{"name": "Ryumi", "ethAddress": "0xC9C29AB98E6BC42015985165A11153F564E9F8C2", "userId": "0xc9c29ab98e6bc42015985165a11153f564e9f8c2"}]}),
        ]),
        tiles: Mutex::new(tiles_doc(1000)),
        tiles_etag: Mutex::new("\"grid-1\"".into()),
        tiles_last_updated: AtomicI64::new(1000),
        hits: Mutex::new(HashMap::new()),
        bodies: Mutex::new(Vec::new()),
    })
}

async fn serve_mock(mock: Arc<Mock>) -> SocketAddr {
    async fn pois(AxState(m): AxState<Arc<Mock>>) -> Json<Value> {
        m.hit("pois");
        Json(json!({"data": *m.pois.lock()}))
    }
    async fn banned(AxState(m): AxState<Arc<Mock>>) -> Json<Value> {
        m.hit("banned-names");
        Json(json!({"data": *m.banned.lock()}))
    }
    async fn denylist(AxState(m): AxState<Arc<Mock>>) -> Json<Value> {
        m.hit("denylist");
        Json(json!({"users": *m.denylist.lock()}))
    }
    async fn flags(AxState(m): AxState<Arc<Mock>>, headers: HeaderMap) -> Response {
        m.hit("flags");
        if headers.get(header::REFERER).and_then(|v| v.to_str().ok())
            != Some("https://decentraland.org")
        {
            return (StatusCode::OK, Json(json!({"flags": {}, "variants": {}}))).into_response();
        }
        let etag = m.flags_etag.lock().clone();
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            == Some(etag.as_str())
        {
            m.hit("flags-304");
            return StatusCode::NOT_MODIFIED.into_response();
        }
        (
            StatusCode::OK,
            [(header::ETAG, etag)],
            Json(m.flags.lock().clone()),
        )
            .into_response()
    }
    async fn events_flags(AxState(m): AxState<Arc<Mock>>, headers: HeaderMap) -> Response {
        m.hit("events-flags");
        if headers.get(header::REFERER).and_then(|v| v.to_str().ok())
            != Some("https://decentraland.org")
        {
            return (StatusCode::OK, Json(json!({"flags": {}, "variants": {}}))).into_response();
        }
        (
            StatusCode::OK,
            [(header::ETAG, "\"events-flags-1\"")],
            Json(m.events_flags.lock().clone()),
        )
            .into_response()
    }
    async fn entities(
        AxState(m): AxState<Arc<Mock>>,
        Query(q): Query<HashMap<String, String>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        m.hit("entities/active");
        m.bodies.lock().push(json!({"body": body, "query": q}));
        let wanted: Vec<String> = body["pointers"]
            .as_array()
            .or(body["ids"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        let docs: Vec<Value> = m
            .entities
            .lock()
            .iter()
            .filter(|e| {
                wanted.contains(&e["id"].as_str().unwrap().to_string())
                    || e["pointers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|p| wanted.contains(&p.as_str().unwrap().to_string()))
            })
            .cloned()
            .collect();
        Json(Value::Array(docs))
    }
    async fn versions(
        AxState(m): AxState<Arc<Mock>>,
        Query(q): Query<HashMap<String, String>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        m.hit("entities/versions");
        m.bodies.lock().push(json!({"body":body,"query":q}));
        Json(json!([]))
    }
    async fn profiles(AxState(m): AxState<Arc<Mock>>, Json(body): Json<Value>) -> Json<Value> {
        m.hit("profiles");
        m.bodies.lock().push(body.clone());
        let wanted: Vec<String> = body["ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        let docs: Vec<Value> = m
            .profiles
            .lock()
            .iter()
            .filter(|p| wanted.contains(&p["avatars"][0]["userId"].as_str().unwrap().to_string()))
            .cloned()
            .collect();
        Json(Value::Array(docs))
    }
    /// Upstream's `getMetadata` projection: no avatars, the address as `pointer`.
    async fn profiles_metadata(
        AxState(m): AxState<Arc<Mock>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        m.hit("profiles/metadata");
        let wanted: Vec<String> = body["ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        let docs: Vec<Value> = m
            .profiles
            .lock()
            .iter()
            .filter(|p| wanted.contains(&p["avatars"][0]["userId"].as_str().unwrap().to_string()))
            .map(|p| json!({"pointer": p["avatars"][0]["userId"], "hasClaimedName": false, "name": p["avatars"][0]["name"], "thumbnailUrl": "https://profile-images.decentraland.org/entities/bafkreiprofile/face.png"}))
            .collect();
        Json(Value::Array(docs))
    }
    async fn tiles_info(AxState(m): AxState<Arc<Mock>>) -> Json<Value> {
        m.hit("tiles/info");
        Json(json!({"lastUpdatedAt": m.tiles_last_updated.load(Ordering::SeqCst)}))
    }
    async fn tiles(AxState(m): AxState<Arc<Mock>>, headers: HeaderMap) -> Response {
        m.hit("tiles");
        let etag = m.tiles_etag.lock().clone();
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            == Some(etag.as_str())
        {
            m.hit("tiles-304");
            return StatusCode::NOT_MODIFIED.into_response();
        }
        (
            StatusCode::OK,
            [(header::ETAG, etag)],
            Json(m.tiles.lock().clone()),
        )
            .into_response()
    }
    async fn ab_object(
        AxState(m): AxState<Arc<Mock>>,
        axum::extract::Path(path): axum::extract::Path<String>,
    ) -> Response {
        m.hit(&format!("ab-cdn:{path}"));
        if path.ends_with("missing_mac") {
            return StatusCode::NOT_FOUND.into_response();
        }
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            format!("bundle:{path}"),
        )
            .into_response()
    }
    let app = Router::new()
        .route("/v49/{*path}", get(ab_object))
        .route("/LOD/{*path}", get(ab_object))
        .route("/pois", post(pois))
        .route("/banned-names", post(banned))
        .route("/denylist.json", get(denylist))
        .route("/explorer.json", get(flags))
        .route("/events.json", get(events_flags))
        .route("/entities/active", post(entities))
        .route("/entities/versions", post(versions))
        .route("/profiles", post(profiles))
        .route("/profiles/metadata", post(profiles_metadata))
        .route("/v2/tiles/info", get(tiles_info))
        .route("/v2/tiles", get(tiles))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());
    addr
}

fn sources_at(addr: SocketAddr) -> Sources {
    let base = format!("http://{addr}");
    Sources {
        lists: base.clone(),
        denylist: format!("{base}/denylist.json"),
        feature_flags: format!("{base}/explorer.json"),
        registry: base.clone(),
        map: base.clone(),
        ab_cdn: base,
    }
}

struct Node {
    state: Shared,
    _tmp: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn content_state(pool: PgPool) -> (Arc<ContentState>, TempDir) {
    let tmp = std::env::temp_dir().join(format!(
        "catalyst-explore-api-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
    (
        Arc::new(ContentState::new(
            pool,
            crate::content::Settings::for_tests(),
            storage,
        )),
        TempDir(tmp),
    )
}

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
        .unwrap()
}

/// A node with no database behind it: everything the routes answer from
/// memory (the realm doc, the registry reader, the embedded flags).
async fn node_without_db(settings: Settings) -> Node {
    let pool = lazy_pool();
    let (content, tmp) = content_state(pool.clone()).await;
    let state = Arc::new(State {
        settings,
        pool,
        http: crate::http::Outbound::for_tests(),
        content,
        denylist: Default::default(),
        flags: feature_flags::Snapshot::default(),
        map: map::MapState::new(),
        ab_cdn: ab_cdn::Cache::new(tmp.0.join("ab-cdn"), u64::MAX).unwrap(),
        satellite: satellite::SatelliteState::new(1 << 20, 64),
        world_manifest: world_manifest::WorldManifestState::default(),
        posters: posters::PosterState::default(),
        opensea: opensea::Runtime::default(),
        auth_api: auth_api::AuthApiState::default(),
        runtime_config: runtime_config::RuntimeConfigState::default(),
        onboarding: Arc::default(),
        registry_denylist: Default::default(),
        eip1654: std::sync::OnceLock::new(),
    });
    Node { state, _tmp: tmp }
}

/// The state alone, its temporary storage leaked for the test's lifetime.
pub(crate) async fn node_without_db_state(settings: Settings) -> Shared {
    let node = node_without_db(settings).await;
    let state = node.state.clone();
    std::mem::forget(node);
    state
}

/// Through `State::new`: the mirrored rows are loaded at construction.
async fn node_with_db(settings: Settings, pool: PgPool) -> Node {
    let (content, tmp) = content_state(pool.clone()).await;
    let state = State::new(settings, pool, crate::http::Outbound::for_tests(), content)
        .await
        .unwrap();
    Node { state, _tmp: tmp }
}

struct ScratchDb {
    pool: PgPool,
    admin: PgPool,
    name: String,
}

async fn scratch_database() -> Option<ScratchDb> {
    let Ok(url) = std::env::var("CATALYST_TEST_DATABASE_URL") else {
        eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
        return None;
    };
    let base: PgConnectOptions = url
        .parse()
        .expect("CATALYST_TEST_DATABASE_URL must be a postgres DSN");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(base.clone().database("postgres"))
        .await
        .unwrap();
    let name = format!("catalyst_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect_with(base.database(&name).options([("TimeZone", "UTC")]))
        .await
        .unwrap();
    crate::db::apply_schema(&pool).await.unwrap();
    Some(ScratchDb { pool, admin, name })
}

impl ScratchDb {
    async fn teardown(self) {
        self.pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
    }
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(path);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

async fn call_json(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, bytes) = call(app, method, path, body).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

/// A live job context (cancelled by `jobs.shutdown`) to run a pass under.
/// A replica that never runs the leased mirror pass still follows the
/// shared denylist tables through its own periodic reload.
#[tokio::test]
async fn every_replica_reloads_the_shared_denylist() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let replica = node_with_db(Settings::for_tests(Sources::defaults()), db.pool.clone()).await;
    let wallet = "0x00000000000000000000000000000000000000fe";
    assert!(!replica.state.denylist.contains(wallet));
    let jobs = Jobs::new();
    denylist::spawn_reload(&replica.state, &jobs, std::time::Duration::from_millis(20));
    denylist::set_local(&db.pool, wallet, true).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !replica.state.denylist.contains(wallet) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the replica reloads the shared overlay");
    denylist::set_local(&db.pool, wallet, false).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while replica.state.denylist.contains(wallet) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("and its removal");
    jobs.shutdown(std::time::Duration::from_secs(2)).await;
    db.teardown().await;
}

#[tokio::test]
async fn denylist_reload_timeout_retains_last_snapshot_and_recovers_after_sql_unblocks() {
    let Some(db) = scratch_database().await else {
        return;
    };
    denylist::replace(
        &db.pool,
        &[denylist::UserEntry {
            wallet: "0xPrevious".into(),
        }],
    )
    .await
    .unwrap();
    let replica = node_with_db(Settings::for_tests(Sources::defaults()), db.pool.clone()).await;
    let previous = replica.state.denylist.list();
    let app = router(replica.state.clone());

    // Commit the next snapshot first. The refresh must preserve the old one
    // when PostgreSQL cannot answer, then load this committed replacement.
    denylist::replace(
        &db.pool,
        &[denylist::UserEntry {
            wallet: "0xReplacement".into(),
        }],
    )
    .await
    .unwrap();
    let mut blocker = db.pool.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("LOCK TABLE explore_api.denylist_users IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();

    let jobs = Jobs::new().with_fleet_jobs(false).with_first_pass_now(true);
    let started = tokio::time::Instant::now();
    // Only shorten the interval between passes; the production five-second
    // pass timeout and real SQL wait remain unchanged.
    denylist::spawn_reload(&replica.state, &jobs, Duration::from_millis(100));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                 WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))",
            )
            .bind(blocker_pid)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the real snapshot query must reach PostgreSQL and wait for the table lock");
    assert_eq!(jobs.report()[0].passes, 0);

    tokio::time::timeout_at(started + Duration::from_secs(8), async {
        while jobs.report()[0].passes == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the five-second reload deadline must finish a pass despite the SQL lock");
    assert!(started.elapsed() >= Duration::from_secs(5));
    let failed = &jobs.report()[0];
    assert!(
        failed
            .last_error
            .as_deref()
            .is_some_and(|error| { error.contains("denylist snapshot reload timed out") }),
        "expected the reload deadline, not a PostgreSQL/pool failure: {failed:?}"
    );
    assert!(Arc::ptr_eq(&previous, &replica.state.denylist.list()));
    assert!(replica.state.denylist.contains("0xprevious"));
    assert!(!replica.state.denylist.contains("0xreplacement"));
    let (status, body) = call_json(&app, "GET", "/config/denylist.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"users": [{"wallet": "0xPrevious"}]}));

    blocker.rollback().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let report = &jobs.report()[0];
            if report.passes >= 2
                && report
                    .progress
                    .as_deref()
                    .is_some_and(|message| message.contains(" ok in "))
                && replica.state.denylist.contains("0xreplacement")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the same job must retry and recover after PostgreSQL unblocks");
    assert!(!replica.state.denylist.contains("0xprevious"));
    let (_, body) = call_json(&app, "GET", "/config/denylist.json", None).await;
    assert_eq!(body, json!({"users": [{"wallet": "0xReplacement"}]}));
    assert!(jobs.shutdown(Duration::from_secs(2)).await.is_empty());
    db.teardown().await;
}

async fn job_ctx() -> (Jobs, crate::jobs::JobContext) {
    let jobs = Jobs::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    jobs.spawn("probe", move |ctx| async move {
        let _ = tx.send(ctx.clone());
        ctx.cancel.cancelled().await;
        Ok(())
    });
    let ctx = rx.await.unwrap();
    (jobs, ctx)
}

fn key_set(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn sources_must_be_public_and_not_this_node() {
    let mut settings = Settings::for_tests(Sources::defaults());
    settings.allow_private_sources = false;
    settings.public_url = "https://gateway.example.org".into();
    settings
        .validate()
        .expect("the decentraland.org defaults validate");

    let private = [
        "http://127.0.0.1:5210",
        "https://localhost/",
        "https://router/",
        "https://169.254.169.254/x",
        "ftp://example.com/",
    ];
    for url in private {
        let mut s = settings.clone();
        s.sources.lists = url.into();
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("EXPLORER_LISTS_SOURCE_URL"), "{url}: {err}");
    }
    let mut s = settings.clone();
    s.sources.map = "https://gateway.example.org/api".into();
    let err = s.validate().unwrap_err().to_string();
    assert!(err.contains("own host gateway.example.org"), "{err}");
    assert!(s.guarded_url("https://api.decentraland.org").is_ok());

    // CONTENT_URL / LAMBDAS_URL hosts count as this node too.
    let mut s = settings.clone();
    s.own_hosts = vec!["content.example.org".into()];
    s.sources.denylist = "https://content.example.org/denylist.json".into();
    let err = s.validate().unwrap_err().to_string();
    assert!(
        err.contains("EXPLORER_DENYLIST_SOURCE_URL")
            && err.contains("own host content.example.org"),
        "{err}"
    );
}

#[test]
fn sources_from_env_strip_trailing_slashes() {
    let d = Sources::defaults();
    assert_eq!(d.all().len(), 6);
    assert!(d.all().iter().all(|(_, u)| !u.ends_with('/')));
}

/// The read-through: one upstream call per object, then the file; the
/// `assets/` family fetched as the CDN lays it out; a 404 remembered;
/// `HEAD` served too.
#[tokio::test]
async fn ab_cdn_reads_through_once_and_serves_the_cached_object_after() {
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    let app = router(node.state.clone());
    let path = "/ab-cdn/v49/bafkreidnionrlsubg63yjdeadaeyk3m5p6gofewq6kfqsju42kfx5cniwu/scene.json";
    let (status, headers, body) = call(&app, "GET", path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-ab-cdn-source"], "upstream");
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        String::from_utf8(body).unwrap(),
        "bundle:bafkreidnionrlsubg63yjdeadaeyk3m5p6gofewq6kfqsju42kfx5cniwu/scene.json"
    );
    let (status, headers, body) = call(&app, "GET", path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-ab-cdn-source"], "store");
    assert_eq!(
        body.len(),
        "bundle:bafkreidnionrlsubg63yjdeadaeyk3m5p6gofewq6kfqsju42kfx5cniwu/scene.json".len()
    );
    assert_eq!(
        mock.hits("ab-cdn:bafkreidnionrlsubg63yjdeadaeyk3m5p6gofewq6kfqsju42kfx5cniwu/scene.json"),
        1
    );
    assert!(
        node.state
            .ab_cdn
            .store
            .path("v49/bafkreidnionrlsubg63yjdeadaeyk3m5p6gofewq6kfqsju42kfx5cniwu/scene.json")
            .unwrap()
            .exists()
    );

    let (status, headers, _) = call(&app, "GET", "/ab-cdn/v49/assets/bafy_mac", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/wasm",
        "a platform-suffixed bundle, as the CDN answers"
    );
    assert_eq!(
        mock.hits("ab-cdn:assets/bafy_mac"),
        1,
        "the assets/ segment is the CDN's layout and stays in the upstream path"
    );

    let (status, _, _) = call(&app, "GET", "/ab-cdn/LOD/0/bafk_missing_mac", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(&app, "GET", "/ab-cdn/LOD/0/bafk_missing_mac", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        mock.hits("ab-cdn:0/bafk_missing_mac"),
        1,
        "an upstream 404 is remembered"
    );

    let (status, headers, body) = call(&app, "HEAD", "/ab-cdn/v49/assets/bafy_mac", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-ab-cdn-source"], "store");
    assert!(body.is_empty());
    assert_eq!(
        call(&app, "GET", "/ab-cdn/content/x", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "GET", "/ab-cdn/v49/../x", None).await.0,
        StatusCode::NOT_FOUND
    );
}

/// With no Registry or asset-bundle CDN configured (an island, or a bare
/// node) neither reader sends a request: the boot entity reads know no
/// converted entity, the other Registry reads are unavailable and every
/// object is this node's own.
#[tokio::test]
async fn no_registry_or_cdn_source_reads_nothing_upstream() {
    let config = crate::config::Config::parse_for_tests(&[
        "x",
        "--database-url",
        "postgres:///catalyst",
        "--abgen-upstream-ab-cdn",
        " ",
    ])
    .unwrap();
    let from_config = Sources::from_config(&config);
    assert_eq!(
        (from_config.registry.as_str(), from_config.ab_cdn.as_str()),
        ("", "")
    );

    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let sources = Sources {
        registry: String::new(),
        ab_cdn: String::new(),
        ..sources_at(addr)
    };
    let settings = Settings::for_tests(sources);
    settings.validate().unwrap();
    let node = node_without_db(settings).await;
    let app = router(node.state.clone());
    for path in ["/entities/active", "/entities/versions"] {
        let (status, body) = call_json(
            &app,
            "POST",
            &format!("/asset-bundle-registry{path}"),
            Some(json!({"pointers": ["0,0"]})),
        )
        .await;
        assert_eq!((status, body), (StatusCode::OK, json!([])), "{path}");
    }
    let (status, _) = call_json(
        &app,
        "GET",
        "/asset-bundle-registry/entities/status/bafy",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _, _) = call(&app, "GET", "/ab-cdn/v49/assets/bafy_mac", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(mock.hits.lock().is_empty(), "{:?}", mock.hits.lock());
}

/// The OpenSea resolver's remote legs follow the RPC relay upstreams: blank
/// ones (an island's) leave it no chain and no CryptoKitties to ask, where
/// it used to fall back to decentraland.org's RPC; set ones keep both.
#[test]
fn opensea_reads_no_chain_or_cryptokitties_without_an_rpc_upstream() {
    let parse = |extra: &[&str]| {
        let mut args = vec!["x", "--database-url", "postgres:///catalyst"];
        args.extend_from_slice(extra);
        let config = crate::config::Config::parse_for_tests(&args).unwrap();
        Settings::from_config(&config, Sources::defaults()).opensea
    };
    let island = parse(&["--rpc-upstream-mainnet", "", "--rpc-upstream-polygon", ""]);
    assert_eq!(
        (
            island.rpc_ethereum.as_str(),
            island.rpc_polygon.as_str(),
            island.cryptokitties_api.as_str()
        ),
        ("", "", "")
    );
    let mirror = parse(&[
        "--rpc-upstream-mainnet",
        "https://eth.example.org/",
        "--rpc-upstream-polygon",
        "https://polygon.example.org/",
        "--opensea-cryptokitties-api",
        "https://kitties.example.org/",
    ]);
    assert_eq!(
        (mirror.rpc_ethereum.as_str(), mirror.rpc_polygon.as_str()),
        ("https://eth.example.org/", "https://polygon.example.org/")
    );
    assert_eq!(mirror.cryptokitties_api, "https://kitties.example.org");
    // An RPC upstream alone names no CryptoKitties API: none is built in.
    let rpc_only = parse(&["--rpc-upstream-mainnet", "https://eth.example.org/"]);
    assert_eq!(rpc_only.cryptokitties_api, "");
}

#[tokio::test]
async fn realm_about_keeps_the_upstream_shape_and_reflects_the_sync_state() {
    let node = node_without_db(Settings::for_tests(Sources::defaults())).await;
    let app = router(node.state.clone());

    let (status, ours) = call_json(&app, "GET", "/realm-provider-ea/main/about", None).await;
    assert_eq!(status, StatusCode::OK);
    let theirs: Value = serde_json::from_str(include_str!("about-upstream.json")).unwrap();
    assert_eq!(key_set(&ours), key_set(&theirs));
    assert_eq!(key_set(&ours["content"]), key_set(&theirs["content"]));
    assert_eq!(key_set(&ours["comms"]), key_set(&theirs["comms"]));
    assert_eq!(key_set(&ours["bff"]), key_set(&theirs["bff"]));
    let mut cfg = key_set(&theirs["configurations"]);
    cfg.extend(["localSceneParcels".to_string(), "skybox".to_string()]);
    assert_eq!(key_set(&ours["configurations"]), cfg);
    assert_eq!(
        key_set(&ours["configurations"]["map"]),
        key_set(&theirs["configurations"]["map"])
    );
    assert_eq!(
        ours["configurations"]["map"]["sizes"],
        theirs["configurations"]["map"]["sizes"]
    );
    assert_eq!(ours["configurations"]["realmName"], "main");
    assert_eq!(ours["content"]["publicUrl"], "http://localhost/content/");
    assert_eq!(ours["content"]["synchronizationStatus"], "Bootstrapping");
    assert_eq!(
        ours["healthy"], false,
        "a bootstrapping node does not claim to accept users"
    );
    assert_eq!(ours["acceptingUsers"], false);

    node.state.content.sync.set_state(SyncState::Syncing);
    for path in ["/realm-provider-ea/main", "/realm-provider-ea/about"] {
        let (_, doc) = call_json(&app, "GET", path, None).await;
        assert_eq!(doc["healthy"], true, "{path}");
        assert_eq!(doc["content"]["synchronizationStatus"], "Syncing");
    }

    let (status, _, body) = call(
        &app,
        "GET",
        "/realm-provider-ea/main/preview-wearables",
        None,
    )
    .await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::NOT_FOUND, b"Not found".as_slice())
    );
    let (status, realms) = call_json(&app, "GET", "/realm-provider-ea/realms", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(realms[0]["serverName"], "main");
    let (_, st) = call_json(&app, "GET", "/realm-provider-ea/status", None).await;
    assert_eq!(
        key_set(&st),
        ["commitHash", "currentTime", "version"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

struct MutableRealmCore {
    status: Mutex<(bool, u64)>,
    probes: std::sync::atomic::AtomicUsize,
}

impl crate::content::about_comms::CommsCore for MutableRealmCore {
    fn core_status(&self) -> (bool, u64) {
        self.probes.fetch_add(1, Ordering::SeqCst);
        *self.status.lock()
    }
}

async fn realm_discovery_cases(with_core: bool) {
    use crate::content::about_comms::{AboutCommsState, Settings as CommsSettings};
    let node = node_without_db(Settings::for_tests(Sources::defaults())).await;
    node.state.content.sync.set_state(SyncState::Syncing);
    let app = router(node.state.clone());
    let attachment = Arc::new(AboutCommsState::new(CommsSettings {
        protocol: "v3".into(),
        fixed_adapter: "offline:offline".into(),
        version: None,
        commit_hash: None,
        v4: None,
        max_users: Some(1),
        offline_when_unreachable: false,
    }));
    assert!(node.state.content.attach_about_comms(attachment.clone()));
    let core = Arc::new(MutableRealmCore {
        status: Mutex::new((true, 42)),
        probes: Default::default(),
    });
    if with_core {
        assert!(attachment.attach_core(core.clone()));
    }
    let cases = if with_core {
        vec![
            (true, 42, true),
            (true, 0, true),
            (false, 42, true),
            (true, 42, false),
        ]
    } else {
        vec![(false, 0, true)]
    };
    for (core_healthy, count, syncing) in cases {
        *core.status.lock() = (core_healthy, count);
        node.state.content.sync.set_state(if syncing {
            SyncState::Syncing
        } else {
            SyncState::Bootstrapping
        });
        for path in [
            "/realm-provider-ea/main/about",
            "/realm-provider-ea/main",
            "/realm-provider-ea/about",
        ] {
            let before = core.probes.load(Ordering::SeqCst);
            let (status, doc) = call_json(&app, "GET", path, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(doc["comms"]["healthy"], core_healthy, "{path}");
            assert_eq!(doc["comms"]["usersCount"], count, "{path}");
            assert_eq!(doc["bff"]["userCount"], count, "{path}");
            assert_eq!(doc["content"]["healthy"], syncing, "{path}");
            assert_eq!(doc["healthy"], syncing && core_healthy, "{path}");
            assert_eq!(doc["acceptingUsers"], syncing && core_healthy, "{path}");
            assert_eq!(doc["comms"]["adapter"], node.state.settings.comms_adapter);
            assert_eq!(
                core.probes.load(Ordering::SeqCst) - before,
                usize::from(with_core)
            );
        }
        let before = core.probes.load(Ordering::SeqCst);
        let (status, realms) = call_json(&app, "GET", "/realm-provider-ea/realms", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(realms.as_array().unwrap().len(), usize::from(core_healthy));
        if core_healthy {
            assert_eq!(realms[0]["usersCount"], count);
        }
        assert_eq!(
            core.probes.load(Ordering::SeqCst) - before,
            usize::from(with_core)
        );
    }
}

#[tokio::test]
async fn realm_about_projects_attached_discovery_without_replacing_content_readiness() {
    realm_discovery_cases(true).await;
}

#[tokio::test]
async fn realm_about_attached_without_core_is_unhealthy() {
    realm_discovery_cases(false).await;
}

#[tokio::test]
async fn feature_flags_serve_the_embedded_full_set_before_the_first_pass() {
    let node = node_without_db(Settings::for_tests(Sources::defaults())).await;
    let app = router(node.state.clone());
    let (status, headers, body) = call(&app, "GET", "/feature-flags/explorer.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "private,no-store,max-age=0");
    let doc: Value = serde_json::from_slice(&body).unwrap();
    feature_flags::validate(&doc).unwrap();
    assert_eq!(
        doc["flags"]["explorer-alfa-minimum-requirements"], true,
        "the referer-gated flag is in the served set"
    );
    let (status, flag) = call_json(
        &app,
        "GET",
        "/feature-flags/flags/explorer-alfa-minimum-requirements",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        flag["name"], "minimum_requirements",
        "a variant of that name answers before the plain flag"
    );
    let plain = doc["flags"]
        .as_object()
        .unwrap()
        .keys()
        .find(|k| doc["variants"].get(k).is_none())
        .unwrap()
        .clone();
    let (status, flag) =
        call_json(&app, "GET", &format!("/feature-flags/flags/{plain}"), None).await;
    assert_eq!((status, &flag), (StatusCode::OK, &doc["flags"][&plain]));
    let (status, missing) = call_json(&app, "GET", "/feature-flags/flags/no-such-flag", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        missing,
        json!({"error": "flag_not_found", "name": "no-such-flag"})
    );
}

#[tokio::test]
async fn registry_gateway_preserves_queries_and_never_caches_native_reads() {
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    let app = router(node.state.clone());
    for operation in ["active", "versions"] {
        let path = format!("/asset-bundle-registry/entities/{operation}?world_name=Foo.dcl.eth");
        for _ in 0..2 {
            let (status, _) =
                call_json(&app, "POST", &path, Some(json!({"pointers":["0,0"]}))).await;
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(mock.hits(&format!("entities/{operation}")), 2);
        assert_eq!(
            mock.bodies.lock().last().unwrap()["query"]["world_name"],
            "Foo.dcl.eth"
        );
    }
    mock.entities.lock().clear();
    let (status, docs) = call_json(
        &app,
        "POST",
        "/asset-bundle-registry/entities/active",
        Some(json!({"pointers":["0,0"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        docs,
        json!([]),
        "native changes must be visible immediately"
    );
}

#[test]
fn registry_backend_is_native_only_on_loopback() {
    use registry::Backend;
    for native in [
        "http://127.0.0.1:5147",
        "https://127.0.0.2:5147",
        "http://[::1]:5147",
        "http://localhost:5147/",
    ] {
        assert!(
            matches!(Backend::parse(native).unwrap(), Backend::Native(_)),
            "{native}"
        );
    }
    // Same-network hosts are not the local runtime: they take the source guard.
    for upstream in [
        "https://asset-bundle-registry.decentraland.org",
        "http://10.0.0.5:5147",
        "http://[fd00::1]:5147",
        "http://catalyst-abgen:5147",
    ] {
        assert!(
            matches!(Backend::parse(upstream).unwrap(), Backend::Upstream(_)),
            "{upstream}"
        );
    }
    for bad in [
        "ftp://127.0.0.1/",
        "http://user:pw@127.0.0.1/",
        "http://127.0.0.1/?token=x",
        "http://127.0.0.1/#x",
        "not a url",
    ] {
        assert!(Backend::parse(bad).is_err(), "{bad}");
    }
}

/// A read queues for a backend slot instead of failing at once, and is
/// refused only when none frees up within the wait.
#[tokio::test]
async fn registry_reads_wait_briefly_for_a_backend_slot() {
    use tokio::sync::Semaphore;
    let permits = Semaphore::new(1);
    let held = permits.acquire().await.unwrap();
    assert!(
        registry::slot(&permits, Duration::from_millis(20))
            .await
            .is_none()
    );
    let waiting = registry::slot(&permits, Duration::from_secs(2));
    let release = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held);
    };
    let (permit, ()) = tokio::join!(waiting, release);
    assert!(permit.is_some());
}

/// Startup validation accepts the local runtime over http or https and a
/// public Registry, and names the variable when it refuses one.
#[test]
fn registry_source_validates_at_startup_and_names_the_variable() {
    let mut settings = Settings::for_tests(Sources::defaults());
    settings.allow_private_sources = false;
    settings.public_url = "https://gateway.example.org".into();
    for ok in [
        "http://127.0.0.1:5147",
        "https://127.0.0.1:5147",
        "http://localhost:5147",
        "https://asset-bundle-registry.decentraland.org",
    ] {
        let mut s = settings.clone();
        s.sources.registry = ok.into();
        s.validate().unwrap_or_else(|e| panic!("{ok}: {e:#}"));
    }
    for bad in [
        "http://10.0.0.5:5147",
        "http://catalyst-abgen:5147",
        "http://user:pw@127.0.0.1:5147",
        "https://gateway.example.org/asset-bundle-registry",
    ] {
        let mut s = settings.clone();
        s.sources.registry = bad.into();
        let err = format!("{:#}", s.validate().unwrap_err());
        assert!(err.contains("EXPLORER_REGISTRY_SOURCE_URL"), "{bad}: {err}");
    }
}

#[tokio::test]
async fn registry_profiles_are_composed_from_the_shared_profile_service() {
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    let app = router(node.state.clone());
    for path in ["/profiles", "/profiles/metadata"] {
        let (status, _, _) = call(
            &app,
            "POST",
            &format!("/asset-bundle-registry{path}"),
            Some(json!({"ids": []})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    assert!(mock.hits.lock().is_empty());
    let (status, st) = call_json(&app, "GET", "/asset-bundle-registry/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        key_set(&st["data"]),
        ["commitHash", "currentTime", "version"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

#[tokio::test]
async fn map_district_documents_are_served_from_the_embedded_upstream_data() {
    let node = node_without_db(Settings::for_tests(sources_at(
        "127.0.0.1:1".parse().unwrap(),
    )))
    .await;
    let app = router(node.state.clone());

    let (status, all) = call_json(&app, "GET", "/api/v2/districts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["ok"], true);
    assert_eq!(all["data"].as_array().unwrap().len(), 42);

    let (status, one) = call_json(
        &app,
        "GET",
        "/api/v2/districts/ea2c3a3e-bbd6-49d9-814c-3b64b37b057a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["data"]["name"], "SciArt Lab");
    assert_eq!(one["data"]["totalParcels"], 208);
    assert_eq!(
        key_set(&one["data"]),
        ["description", "id", "name", "parcels", "totalParcels"]
            .into_iter()
            .map(String::from)
            .collect()
    );

    let (status, headers, body) =
        call(&app, "GET", "/api/v2/districts/no-such-district", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(body, b"Not found");

    let (status, rows) = call_json(
        &app,
        "GET",
        "/api/v2/addresses/0x004398361d0fad60d7fcf5533d94398ee9fc8ddc/contributions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows["data"][0]["districtId"],
        "219ac351-e6ce-4e17-8b84-eb008afddf69"
    );
    let (_, none) = call_json(
        &app,
        "GET",
        "/api/v2/addresses/0x8d6f63e382d73cf53858864f673f39e9ff915a1e/contributions",
        None,
    )
    .await;
    assert_eq!(none, json!({"ok": true, "data": []}));
}

#[tokio::test]
async fn registry_upstream_failure_is_a_502_not_a_short_answer() {
    let node = node_without_db(Settings::for_tests(sources_at(
        "127.0.0.1:1".parse().unwrap(),
    )))
    .await;
    let app = router(node.state.clone());
    let (status, body) = call_json(
        &app,
        "POST",
        "/asset-bundle-registry/entities/active",
        Some(json!({"pointers": ["0,0"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["message"] == "Registry service unavailable", "{body}");
}

#[tokio::test]
async fn lists_mirror_prunes_upstream_deletions_and_keeps_the_local_overlay() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    let app = router(node.state.clone());
    let (jobs, ctx) = job_ctx().await;

    let (status, empty) = call_json(&app, "POST", "/dcl-lists/pois", None).await;
    assert_eq!((status, empty), (StatusCode::OK, json!({"data": []})));

    lists::run_pass(&node.state, &ctx).await.unwrap();
    let (_, pois) = call_json(&app, "POST", "/dcl-lists/pois", None).await;
    assert_eq!(pois, json!({"data": ["-148,131", "0,0", "16,83"]}));
    let (_, banned) = call_json(&app, "POST", "/dcl-lists/banned-names", None).await;
    assert_eq!(banned, json!({"data": ["badname"]}));

    lists::set_local(&db.pool, "pois", "9,9", true)
        .await
        .unwrap();
    lists::set_local(&db.pool, "pois", "0,0", false)
        .await
        .unwrap();
    let (_, pois) = call_json(&app, "POST", "/dcl-lists/pois", None).await;
    assert_eq!(
        pois,
        json!({"data": ["-148,131", "16,83", "9,9"]}),
        "local add and local hide apply over the mirror"
    );

    *mock.pois.lock() = vec!["0,0".into(), "1,1".into(), "-148,131".into()];
    lists::run_pass(&node.state, &ctx).await.unwrap();
    let (_, pois) = call_json(&app, "POST", "/dcl-lists/pois", None).await;
    assert_eq!(
        pois,
        json!({"data": ["-148,131", "1,1", "9,9"]}),
        "16,83 pruned, 1,1 added, the overlay untouched"
    );
    let (local_rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM explore_api.lists_local")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(local_rows, 2);
    lists::clear_local(&db.pool, "pois", "0,0").await.unwrap();
    let (_, pois) = call_json(&app, "POST", "/dcl-lists/pois", None).await;
    assert_eq!(pois["data"].as_array().unwrap().len(), 4);

    let (_, report) = call_json(&app, "GET", "/mirrors", None).await;
    let lists_row = report["mirrors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mirror"] == "lists")
        .unwrap()
        .clone();
    assert_eq!(lists_row["passes"], 2);
    assert_eq!(lists_row["items"], 4);
    assert_eq!(lists_row["complete"], true);
    assert!(lists_row["lastError"].is_null());

    mock.pois.lock().clear();
    let err = lists::run_pass(&node.state, &ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("empty list"), "{err}");
    let (_, pois) = call_json(&app, "POST", "/dcl-lists/pois", None).await;
    assert_eq!(
        pois["data"].as_array().unwrap().len(),
        4,
        "a refused pass leaves the mirror as it was"
    );
    let (_, report) = call_json(&app, "GET", "/mirrors", None).await;
    let lists_row = report["mirrors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mirror"] == "lists")
        .unwrap()
        .clone();
    assert_eq!(
        lists_row["complete"], false,
        "started after the last completion"
    );
    assert_eq!(lists_row["passes"], 2);
    assert!(
        lists_row["lastError"]
            .as_str()
            .unwrap()
            .contains("empty list")
    );

    jobs.shutdown(Duration::from_secs(1)).await;
    db.teardown().await;
}

#[tokio::test]
async fn denylist_mirror_keeps_upstream_casing_and_the_overlay_wins() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    let app = router(node.state.clone());
    let (jobs, ctx) = job_ctx().await;

    let (_, empty) = call_json(&app, "GET", "/config/denylist.json", None).await;
    assert_eq!(empty, json!({"users": []}));
    denylist::run_pass(&node.state, &ctx).await.unwrap();
    let (status, doc) = call_json(&app, "GET", "/config/denylist.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        doc,
        json!({"users": [{"wallet": "0x0000000000000000000000000000000000000002"}, {"wallet": "0xAbC0000000000000000000000000000000000001"}]})
    );
    assert!(
        node.state
            .denylist
            .contains("0xabc0000000000000000000000000000000000001")
    );
    assert!(
        node.state
            .denylist
            .contains(" 0xABC0000000000000000000000000000000000001 ")
    );

    denylist::set_local(
        &db.pool,
        "0xAbC0000000000000000000000000000000000001",
        false,
    )
    .await
    .unwrap();
    denylist::set_local(&db.pool, "0xLocal", true)
        .await
        .unwrap();
    node.state.denylist.reload(&db.pool).await.unwrap();
    let (_, doc) = call_json(&app, "GET", "/config/denylist.json", None).await;
    assert_eq!(
        doc,
        json!({"users": [{"wallet": "0x0000000000000000000000000000000000000002"}, {"wallet": "0xlocal"}]})
    );
    assert!(
        !node
            .state
            .denylist
            .contains("0xabc0000000000000000000000000000000000001")
    );

    *mock.denylist.lock() = vec![json!({"wallet": "0xAbC0000000000000000000000000000000000001"})];
    denylist::run_pass(&node.state, &ctx).await.unwrap();
    let (_, doc) = call_json(&app, "GET", "/config/denylist.json", None).await;
    assert_eq!(
        doc,
        json!({"users": [{"wallet": "0xlocal"}]}),
        "the pruned wallet is gone, the hidden one stays hidden, the local one stays"
    );

    let restarted = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    assert!(
        restarted.state.denylist.contains("0xlocal"),
        "a restart loads the mirrored + local set"
    );
    assert_eq!(
        mock.hits("denylist"),
        2,
        "loading at startup costs no upstream call"
    );

    jobs.shutdown(Duration::from_secs(1)).await;
    db.teardown().await;
}

#[tokio::test]
async fn denylist_refresh_reaches_every_replica_without_mirrors_or_fleet_jobs() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let mut settings = Settings::for_tests(sources_at(addr));
    settings.mirrors_enabled = false;
    settings.ab_cdn_enabled = false;
    let first = node_with_db(settings.clone(), db.pool.clone()).await;
    let second = node_with_db(settings, db.pool.clone()).await;
    let snapshots = [&first.state.denylist, &second.state.denylist];
    assert!(!Arc::ptr_eq(snapshots[0], snapshots[1]));
    // Each replica's social referral readers retain this same Arc.
    let referral_readers = [snapshots[0].clone(), snapshots[1].clone()];
    let first_jobs = Jobs::new().with_fleet_jobs(false);
    let second_jobs = Jobs::new().with_fleet_jobs(false);
    first.state.spawn_jobs(&first_jobs);
    second.state.spawn_jobs(&second_jobs);
    for jobs in [&first_jobs, &second_jobs] {
        assert!(
            jobs.report()
                .iter()
                .any(|j| j.name == "explorer-denylist-reload")
        );
        assert!(jobs.report().iter().all(|j| !j.name.ends_with("-mirror")));
    }

    async fn refresh(jobs: [&Jobs; 2], expected_passes: u64) {
        // Advance only the between-pass sleep, then allow real PostgreSQL IO
        // with normal time so no query budget can auto-expire in paused time.
        tokio::task::yield_now().await;
        tokio::time::pause();
        tokio::time::advance(denylist::RELOAD_INTERVAL + Duration::from_millis(1)).await;
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if jobs.iter().all(|jobs| {
                    jobs.report().iter().any(|job| {
                        job.name == "explorer-denylist-reload"
                            && job.passes >= expected_passes
                            && job.last_error.is_none()
                    })
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("every replica must refresh without a fleet lease");
    }

    // A mirror's transaction and an admin's overlay write are shared facts;
    // neither writer refreshes either process's snapshot in this test.
    denylist::replace(
        &db.pool,
        &[denylist::UserEntry {
            wallet: "0xMirrored".into(),
        }],
    )
    .await
    .unwrap();
    denylist::set_local(&db.pool, "0xLocal", true)
        .await
        .unwrap();
    assert!(referral_readers.iter().all(|s| !s.contains("0xlocal")));
    refresh([&first_jobs, &second_jobs], 1).await;
    for snapshot in &referral_readers {
        assert!(snapshot.contains("0xmirrored"));
        assert!(snapshot.contains("0xlocal"));
    }

    denylist::set_local(&db.pool, "0xMirrored", false)
        .await
        .unwrap();
    denylist::clear_local(&db.pool, "0xLocal").await.unwrap();
    denylist::replace(
        &db.pool,
        &[
            denylist::UserEntry {
                wallet: "0xMirrored".into(),
            },
            denylist::UserEntry {
                wallet: "0xNew".into(),
            },
        ],
    )
    .await
    .unwrap();
    refresh([&first_jobs, &second_jobs], 2).await;
    for snapshot in &referral_readers {
        assert!(!snapshot.contains("0xmirrored"), "local removal wins");
        assert!(
            !snapshot.contains("0xlocal"),
            "cleared local addition disappears"
        );
        assert!(snapshot.contains("0xnew"), "new mirror rows become visible");
    }
    for state in [&first.state, &second.state] {
        let (_, doc) =
            call_json(&router(state.clone()), "GET", "/config/denylist.json", None).await;
        assert_eq!(doc, json!({"users": [{"wallet": "0xNew"}]}));
    }
    assert_eq!(
        mock.hits("denylist"),
        0,
        "refresh only reads the shared database"
    );
    assert!(first_jobs.shutdown(Duration::from_secs(1)).await.is_empty());
    assert!(
        second_jobs
            .shutdown(Duration::from_secs(1))
            .await
            .is_empty()
    );
    db.teardown().await;
}

#[tokio::test]
async fn feature_flags_mirror_sends_the_referer_applies_overrides_and_uses_the_etag() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    let app = router(node.state.clone());
    let (jobs, ctx) = job_ctx().await;

    feature_flags::run_pass(&node.state, &ctx).await.unwrap();
    let (_, doc) = call_json(&app, "GET", "/feature-flags/explorer.json", None).await;
    assert_eq!(
        doc["flags"],
        json!({"explorer-alfa-minimum-requirements": true, "explorer-feature-a": false}),
        "the referer-gated full set"
    );
    let (_, variant) = call_json(&app, "GET", "/feature-flags/flags/explorer-variant", None).await;
    assert_eq!(variant, json!({"name": "v", "enabled": true}));

    feature_flags::set_local_flag(&db.pool, "explorer", "explorer-feature-a", true)
        .await
        .unwrap();
    feature_flags::set_local_flag(&db.pool, "explorer", "explorer-local-only", true)
        .await
        .unwrap();
    node.state.flags.reload(&db.pool, "explorer").await.unwrap();
    let (_, doc) = call_json(&app, "GET", "/feature-flags/explorer.json", None).await;
    assert_eq!(doc["flags"]["explorer-feature-a"], true);
    assert_eq!(doc["flags"]["explorer-local-only"], true);
    let (stored,): (Value,) =
        sqlx::query_as("SELECT document FROM explore_api.feature_flags WHERE app = 'explorer'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        stored["flags"]["explorer-feature-a"], false,
        "the mirror row is upstream's, untouched by the override"
    );

    feature_flags::run_pass(&node.state, &ctx).await.unwrap();
    assert_eq!(
        mock.hits("flags-304"),
        1,
        "the second pass sends the stored etag"
    );
    let (_, doc) = call_json(&app, "GET", "/feature-flags/explorer.json", None).await;
    assert_eq!(
        doc["flags"]["explorer-feature-a"], true,
        "overrides survive a 304 pass"
    );

    mock.flags.lock()["flags"]["explorer-feature-b"] = json!(true);
    *mock.flags_etag.lock() = "\"flags-2\"".into();
    feature_flags::run_pass(&node.state, &ctx).await.unwrap();
    let (_, doc) = call_json(&app, "GET", "/feature-flags/explorer.json", None).await;
    assert_eq!(doc["flags"]["explorer-feature-b"], true);
    let (_, report) = call_json(&app, "GET", "/mirrors", None).await;
    let row = report["mirrors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mirror"] == "feature-flags")
        .unwrap()
        .clone();
    assert_eq!(row["cursor"], "\"flags-2\"");
    assert_eq!(row["passes"], 3);

    feature_flags::clear_local_flag(&db.pool, "explorer", "explorer-feature-a")
        .await
        .unwrap();
    let restarted = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    let doc = restarted.state.flags.document();
    assert_eq!(doc["flags"]["explorer-feature-a"], false);
    assert_eq!(doc["flags"]["explorer-local-only"], true);

    jobs.shutdown(Duration::from_secs(1)).await;
    db.teardown().await;
}

/// Review architecture-1 explore-change-4: the pass also mirrors the
/// `events` document, so `events::foundation` reads a real row.
#[tokio::test]
async fn feature_flags_mirror_stores_the_events_document_for_the_foundation_names() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    let (jobs, ctx) = job_ctx().await;

    feature_flags::run_pass(&node.state, &ctx).await.unwrap();
    assert_eq!(
        mock.hits("events-flags"),
        1,
        "the referer-gated events document"
    );
    let (stored, etag): (Value, Option<String>) =
        sqlx::query_as("SELECT document, etag FROM explore_api.feature_flags WHERE app = 'events'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(stored, *mock.events_flags.lock());
    assert_eq!(etag.as_deref(), Some("\"events-flags-1\""));

    let foundation = crate::events::foundation::Foundation::default();
    foundation.refresh_if_stale(&db.pool).await;
    assert_eq!(
        crate::events::foundation::display_name(
            &foundation.snapshot(),
            Some("0xf0000000000000000000000000000000000000f0"),
            Some("Alice".into())
        ),
        Some(crate::events::foundation::FOUNDATION_NAME.into())
    );
    let (explorer,): (Value,) =
        sqlx::query_as("SELECT document FROM explore_api.feature_flags WHERE app = 'explorer'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        explorer["variants"]
            .get("events-decentraland-foundation-addresses")
            .is_none(),
        "the two documents stay apart"
    );

    jobs.shutdown(Duration::from_secs(1)).await;
    db.teardown().await;
}

#[tokio::test]
async fn dapps_flags_refresh_preserves_the_audience_and_last_good_document() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let reply = Arc::new(Mutex::new((
        StatusCode::OK,
        json!({
            "flags": {"dapps-communities_global_moderators": true},
            "variants": {"dapps-communities_global_moderators": {
                "payload": {"value": "0xABC"}
            }}
        }),
    )));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (reply_route, observed_route) = (reply.clone(), observed.clone());
    let server = Router::new().route(
        "/dapps.json",
        get(move |headers: HeaderMap| {
            let (reply, observed) = (reply_route.clone(), observed_route.clone());
            async move {
                observed.lock().push(headers);
                let (status, body) = reply.lock().clone();
                (status, Json(body))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(axum::serve(listener, server).into_future());
    let node = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    let snapshot = feature_flags::Snapshot::empty();
    let url = format!("http://{addr}/dapps.json");
    let audience = "https://social.example.invalid";
    feature_flags::refresh_app(
        &node.state,
        &snapshot,
        "dapps",
        &url,
        audience,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(observed.lock()[0][header::REFERER], audience);
    assert!(observed.lock()[0].get(header::IF_NONE_MATCH).is_none());
    assert_eq!(
        snapshot.variant_values("dapps", "communities_global_moderators"),
        Some(vec!["0xabc".into()])
    );
    let last_good = snapshot.document();
    for invalid in [
        (StatusCode::SERVICE_UNAVAILABLE, json!({})),
        (
            StatusCode::OK,
            json!({"flags":{"invalid":"true"}, "variants":{}}),
        ),
    ] {
        *reply.lock() = invalid;
        assert!(
            feature_flags::refresh_app(
                &node.state,
                &snapshot,
                "dapps",
                &url,
                audience,
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
        assert_eq!(snapshot.document(), last_good);
        let restarted = feature_flags::Snapshot::empty();
        restarted.reload(&db.pool, "dapps").await.unwrap();
        assert_eq!(restarted.document(), last_good);
    }
    task.abort();
    db.teardown().await;
}

#[tokio::test]
async fn map_grid_mirror_persists_the_grid_resumes_after_restart_and_gates_downloads() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let node = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    let app = router(node.state.clone());
    let (jobs, ctx) = job_ctx().await;

    let (status, _, _) = call(
        &app,
        "GET",
        "/api/v1/map.png?width=64&height=64&center=0,0&size=5",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no grid before the first pass"
    );
    let (status, _) = call_json(&app, "GET", "/api/v2/tiles/info", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    map::run_pass(&node.state, &ctx).await.unwrap();
    assert_eq!((mock.hits("tiles/info"), mock.hits("tiles")), (1, 1));
    let (status, info) = call_json(&app, "GET", "/api/v2/tiles/info", None).await;
    assert_eq!(
        (status, info),
        (StatusCode::OK, json!({"lastUpdatedAt": 1000}))
    );

    let (status, headers, png) = call(
        &app,
        "GET",
        "/api/v1/map.png?width=64&height=64&center=0,0&size=5",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert!(
        headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("max-age=120")
    );
    let etag = headers["etag"].to_str().unwrap().to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v2/map.png?width=64&height=64&center=0,0&size=5")
                .header("if-none-match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    for path in [
        "/api/v1/minimap.png",
        "/api/v1/estatemap.png",
        "/api/v2/parcels/1/1/map.png",
        "/api/v1/estates/77/map.png",
    ] {
        let (status, headers, _) = call(&app, "GET", path, None).await;
        assert_eq!((path, status), (path, StatusCode::OK));
        assert_eq!(headers["content-type"], "image/png");
    }
    let (status, _, _) = call(&app, "GET", "/api/v1/estates/404/map.png", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, tiles) = call_json(
        &app,
        "GET",
        "/api/v2/tiles?x1=0&x2=1&y1=0&y2=1&include=x,y,type",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tiles["ok"], true);
    assert_eq!(
        key_set(&tiles["data"]),
        ["0,0", "1,0", "1,1"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    assert_eq!(
        tiles["data"]["0,0"],
        json!({"x": 0, "y": 0, "type": "plaza"})
    );
    let (_, legacy) = call_json(&app, "GET", "/api/v1/tiles?x1=1&x2=1&y1=1&y2=1", None).await;
    assert_eq!(legacy["data"]["1,1"]["type"], 9);

    let (stored_stamp, count): (i64, i32) =
        sqlx::query_as("SELECT last_updated_at, tile_count FROM explore_api.map_grid WHERE id = 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!((stored_stamp, count), (1000, 4));

    let restarted = node_with_db(Settings::for_tests(sources_at(addr)), db.pool.clone()).await;
    assert!(
        restarted.state.map.is_ready(),
        "the persisted grid is loaded at startup"
    );
    assert_eq!(restarted.state.map.last_updated_at(), 1000);
    assert_eq!(mock.hits("tiles"), 1, "no download at startup");
    let app2 = router(restarted.state.clone());
    let (status, _, _) = call(
        &app2,
        "GET",
        "/api/v1/map.png?width=64&height=64&center=0,0&size=5",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    map::run_pass(&restarted.state, &ctx).await.unwrap();
    assert_eq!(
        mock.hits("tiles"),
        1,
        "an unchanged upstream stamp downloads nothing"
    );

    mock.tiles_last_updated.store(2000, Ordering::SeqCst);
    map::run_pass(&restarted.state, &ctx).await.unwrap();
    assert_eq!(
        mock.hits("tiles"),
        1,
        "a fresh grid is not re-downloaded inside the refresh floor"
    );
    assert_eq!(restarted.state.map.last_updated_at(), 1000);

    let mut eager = Settings::for_tests(sources_at(addr));
    eager.map_min_refresh = Duration::ZERO;
    let eager = node_with_db(eager, db.pool.clone()).await;
    map::run_pass(&eager.state, &ctx).await.unwrap();
    assert_eq!(
        (mock.hits("tiles"), mock.hits("tiles-304")),
        (2, 1),
        "past the floor the stored etag makes it a 304"
    );
    assert_eq!(
        eager.state.map.last_updated_at(),
        2000,
        "the held grid is restamped"
    );
    let (stored_stamp,): (i64,) =
        sqlx::query_as("SELECT last_updated_at FROM explore_api.map_grid WHERE id = 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(stored_stamp, 2000);

    *mock.tiles.lock() = {
        let mut doc = tiles_doc(3000);
        doc["data"]["9,9"] = tile(9, 9, "owned", None);
        doc
    };
    *mock.tiles_etag.lock() = "\"grid-2\"".into();
    mock.tiles_last_updated.store(3000, Ordering::SeqCst);
    map::run_pass(&eager.state, &ctx).await.unwrap();
    assert_eq!(mock.hits("tiles"), 3);
    let app3 = router(eager.state.clone());
    let (_, tiles) = call_json(&app3, "GET", "/api/v2/tiles?include=x", None).await;
    assert_eq!(tiles["data"].as_object().unwrap().len(), 5);
    let (_, report) = call_json(&app3, "GET", "/mirrors", None).await;
    let row = report["mirrors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mirror"] == "map-grid")
        .unwrap()
        .clone();
    assert_eq!(row["cursor"], "3000");
    assert_eq!(row["items"], 5);
    assert_eq!(row["complete"], true);
    assert_eq!(report["mapGridLastUpdatedAt"], 3000);

    jobs.shutdown(Duration::from_secs(1)).await;
    db.teardown().await;
}

/// One bounded run of every mirror pass and reader against the real upstreams
/// (`cargo test --lib live_upstream -- --ignored --nocapture`): one 38 MB grid
/// download and three small documents. Registry uses the local runtime. Prints the `/mirrors`
/// report; the freshness evidence in tests/README.md comes from here.
#[tokio::test]
#[ignore]
async fn live_upstream_passes_complete_and_the_boot_routes_answer() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mut settings = Settings::for_tests(Sources::defaults());
    settings.allow_private_sources = false;
    settings.public_url = "https://v2.example.org".into();
    let node = node_with_db(settings, db.pool.clone()).await;
    let app = router(node.state.clone());
    let (jobs, ctx) = job_ctx().await;

    lists::run_pass(&node.state, &ctx).await.unwrap();
    denylist::run_pass(&node.state, &ctx).await.unwrap();
    feature_flags::run_pass(&node.state, &ctx).await.unwrap();
    map::run_pass(&node.state, &ctx).await.unwrap();

    let (status, pois) = call_json(&app, "POST", "/dcl-lists/pois", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        pois["data"].as_array().unwrap().iter().any(|p| p == "0,0"),
        "Genesis Plaza is a POI"
    );
    let (_, deny) = call_json(&app, "GET", "/config/denylist.json", None).await;
    assert!(deny["users"].as_array().unwrap().len() > 100);
    let (_, flags) = call_json(&app, "GET", "/feature-flags/explorer.json", None).await;
    assert_eq!(
        flags["flags"]["explorer-alfa-minimum-requirements"], true,
        "the referer-gated flag reached the mirror"
    );
    let (status, headers, png) = call(
        &app,
        "GET",
        "/api/v1/map.png?width=64&height=64&center=0,0&size=5",
        None,
    )
    .await;
    assert_eq!(
        (status, headers["content-type"].to_str().unwrap()),
        (StatusCode::OK, "image/png")
    );
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let (_, tiles) = call_json(&app, "GET", "/api/v2/tiles?x1=0&x2=0&y1=0&y2=0", None).await;
    assert_eq!(tiles["data"]["0,0"]["name"], "Genesis Plaza");
    let (_, report) = call_json(&app, "GET", "/mirrors", None).await;
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    println!(
        "pois={} denylist={} flags={} variants={} tiles={}",
        pois["data"].as_array().unwrap().len(),
        deny["users"].as_array().unwrap().len(),
        flags["flags"].as_object().unwrap().len(),
        flags["variants"].as_object().unwrap().len(),
        node.state.map.snapshot().unwrap().tiles.len()
    );
    for m in report["mirrors"].as_array().unwrap() {
        assert_eq!(m["complete"], true, "{m}");
    }

    let restarted = node_with_db(Settings::for_tests(Sources::defaults()), db.pool.clone()).await;
    assert!(restarted.state.map.is_ready());
    assert!(restarted.state.denylist.list().users.len() > 100);
    jobs.shutdown(Duration::from_secs(1)).await;
    db.teardown().await;
}

#[tokio::test]
async fn spawn_jobs_registers_every_explorer_job_and_stops_with_the_registry() {
    // The satellite mirror is optional (no upstream, no job); this node configures it so the
    // registry lists every job the explorer API can run.
    let mut settings = Settings::for_tests(sources_at("127.0.0.1:1".parse().unwrap()));
    settings.satellite_upstream = Some("http://127.0.0.1:1".to_string());
    settings.world_manifest_upstream = "http://127.0.0.1:1/WorldManifest.json".to_string();
    let node = node_without_db(settings).await;
    let jobs = Jobs::new();
    node.state.spawn_jobs(&jobs);
    let names: BTreeSet<String> = jobs.report().into_iter().map(|r| r.name).collect();
    assert_eq!(
        names,
        [
            "ab-cdn-maintenance",
            "auth-api-identity-sweep",
            "explorer-denylist-mirror",
            "explorer-denylist-reload",
            "explorer-flags-mirror",
            "explorer-lists-mirror",
            "explorer-map-mirror",
            "explorer-satellite-mirror",
            "explorer-world-manifest-mirror",
            "opensea-cache-expiry",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    assert!(
        jobs.shutdown(Duration::from_secs(5)).await.is_empty(),
        "every mirror loop yields to shutdown"
    );
}

/// With the mirrors on but no source named (no posture, or an island), no
/// mirror job runs and nothing reaches an upstream: a blank source is none,
/// never a built-in decentraland.org endpoint.
#[tokio::test]
async fn blank_sources_spawn_no_mirror_and_read_no_builtin_upstream() {
    let config = crate::config::Config::parse_for_tests(&[
        "x",
        "--database-url",
        "postgres:///catalyst",
        "--explorer-mirrors-enabled",
        "true",
        "--events-mirror-upstream",
        "true",
    ])
    .unwrap();
    let sources = Sources::from_config(&config);
    assert!(
        sources.all().iter().all(|(_, url)| url.is_empty()),
        "{sources:?}"
    );
    let settings = Settings::from_config(&config, sources);
    assert_eq!(settings.world_manifest_upstream, "");
    assert_eq!(settings.poster_upstream, None);
    assert_eq!(settings.opensea.fallback, None);
    assert!(settings.opensea.ipfs_gateways.is_empty());
    assert_eq!(settings.opensea.cryptokitties_api, "");
    settings.validate().unwrap();

    let settings = Settings::for_tests(Sources::from_config(&config));
    let node = node_without_db(settings).await;
    let jobs = Jobs::new();
    node.state.spawn_jobs(&jobs);
    let names: BTreeSet<String> = jobs.report().into_iter().map(|r| r.name).collect();
    assert!(names.iter().all(|n| !n.ends_with("-mirror")), "{names:?}");
    jobs.shutdown(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn registry_native_routes_preserve_wire_contract_and_replace_untrusted_original_path() {
    use axum::http::{Method, Uri};
    async fn echo(
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> Response {
        (
            StatusCode::ACCEPTED,
            [("cache-control", "no-store"), ("retry-after", "3")],
            Json(json!({
                "method":method.as_str(), "uri":uri.to_string(),
                "body":String::from_utf8(body.to_vec()).unwrap(),
                "authorization":headers.get("authorization").is_some(),
                "identity":headers.get("x-identity-auth-chain-0").unwrap().to_str().unwrap(),
                "timestamp":headers.get("x-identity-timestamp").unwrap().to_str().unwrap(),
                "metadata":headers.get("x-identity-metadata").unwrap().to_str().unwrap(),
                "original":headers.get("x-original-path").unwrap().to_str().unwrap(),
                "contentType":headers.get("content-type").unwrap().to_str().unwrap(),
            })),
        )
            .into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, Router::new().fallback(echo)).into_future());
    let node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    let app = router(node.state);
    for (method, path) in [
        ("POST", "/entities/active"),
        ("POST", "/entities/versions"),
        ("GET", "/entities/status"),
        ("GET", "/entities/status/bafytest"),
        ("GET", "/worlds/Test.dcl.eth/manifest"),
        ("GET", "/denylist"),
        ("GET", "/queues/status"),
    ] {
        let path_and_query = format!("{path}?world_name=A%2Bb.dcl.eth&x=1&x=2");
        let public = format!("/asset-bundle-registry{path_and_query}");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&public)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer operator-test")
                    .header("x-identity-auth-chain-0", "signed-chain")
                    .header("x-identity-timestamp", "1700000000000")
                    .header("x-identity-metadata", "{}")
                    .header("x-original-path", "/forged-path")
                    .body(Body::from(r#"{ "pointers": ["0,0"], "unchanged": true }"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{method} {path}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["retry-after"], "3");
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["method"], method);
        assert_eq!(body["uri"], path_and_query);
        assert_eq!(body["original"], format!("/asset-bundle-registry{path}"));
        assert_eq!(
            body["body"],
            r#"{ "pointers": ["0,0"], "unchanged": true }"#
        );
        assert_eq!(
            body["authorization"], false,
            "the admin bearer never leaves the host"
        );
        assert_eq!(body["identity"], "signed-chain");
        assert_eq!(body["timestamp"], "1700000000000");
        assert_eq!(body["metadata"], "{}");
        assert_eq!(body["contentType"], "application/json");
    }
    let (status, _, _) = call(&app, "POST", "/asset-bundle-registry/not-a-route", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Registry writes stay behind the ops console.
    for (method, path) in [
        ("POST", "/denylist/bafytest"),
        ("DELETE", "/denylist/bafytest"),
        ("POST", "/queues/retry"),
        ("POST", "/queues/pause"),
        ("POST", "/queues/resume"),
        ("POST", "/registry"),
        ("DELETE", "/flush-cache"),
    ] {
        let (status, _, _) =
            call(&app, method, &format!("/asset-bundle-registry{path}"), None).await;
        assert!(
            matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "{method} {path}: {status}"
        );
    }
    // Percent-encoded dot segments must never escape an enumerated route
    // when the outbound URL parser normalizes them.
    for path in [
        "/asset-bundle-registry/entities/status/%2e%2e",
        "/asset-bundle-registry/worlds/%2e%2e/manifest",
    ] {
        let (status, _, _) = call(&app, "GET", path, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    let (status, _, _) = call(
        &app,
        "DELETE",
        "/asset-bundle-registry/entities/active",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    server.abort();
}

/// The Registry gateway is opt-in: `ABGEN_ADMIN_URL` (the ops console's
/// runtime) does not select it, since an abgen without native Registry
/// state cannot answer these routes.
#[test]
fn registry_source_is_only_the_explicit_variable() {
    let config = crate::config::Config::parse_for_tests(&[
        "catalyst",
        "--database-url",
        "postgres:///catalyst",
        "--abgen-admin-url",
        "http://127.0.0.1:5147",
    ])
    .unwrap();
    assert_eq!(Sources::from_config(&config).registry, "");
    let config = crate::config::Config::parse_for_tests(&[
        "catalyst",
        "--database-url",
        "postgres:///catalyst",
        "--abgen-admin-url",
        "http://127.0.0.1:5147",
        "--explorer-registry-source-url",
        "http://localhost:5150/",
    ])
    .unwrap();
    assert_eq!(
        Sources::from_config(&config).registry,
        "http://localhost:5150"
    );
}

/// A public Registry receives the request only: no signed-fetch headers, no
/// asserted original path, no bearer.
#[tokio::test]
async fn registry_upstream_gets_no_identity_or_path_assertion() {
    async fn echo(headers: HeaderMap) -> Json<Value> {
        let names: Vec<String> = headers.keys().map(|k| k.to_string()).collect();
        Json(json!(names))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, Router::new().fallback(echo)).into_future());
    // An IPv4-mapped address is not loopback to the classifier, so this is
    // the upstream path (tests allow private sources).
    let sources = Sources {
        registry: format!("http://[::ffff:127.0.0.1]:{}", addr.port()),
        ..sources_at(addr)
    };
    let node = node_without_db(Settings::for_tests(sources)).await;
    let app = router(node.state);
    let response = app
        .oneshot(
            Request::get("/asset-bundle-registry/entities/status/bafy")
                .header("authorization", "Bearer operator-test")
                .header("x-identity-auth-chain-0", "signed-chain")
                .header("x-original-path", "/forged")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let names: Vec<String> = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    for header in [
        "authorization",
        "x-identity-auth-chain-0",
        "x-original-path",
    ] {
        assert!(!names.iter().any(|n| n == header), "{header}: {names:?}");
    }
    server.abort();
}

#[tokio::test]
async fn registry_native_failures_are_preserved_without_following_redirects() {
    async fn response(Query(q): Query<HashMap<String, String>>) -> Response {
        let status = StatusCode::from_u16(q["status"].parse().unwrap()).unwrap();
        (
            status,
            [
                ("content-type", "application/problem+json"),
                ("location", "http://127.0.0.1:1/never-follow"),
            ],
            r#"{"error":"native failure"}"#,
        )
            .into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server =
        tokio::spawn(axum::serve(listener, Router::new().fallback(response)).into_future());
    let mut node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    // The production pool normally follows redirects; the Registry adapter
    // must explicitly suppress them for this request only.
    Arc::get_mut(&mut node.state).unwrap().http =
        crate::http::Outbound::new(crate::http::HttpClientCfg::serve(4)).unwrap();
    let app = router(node.state);
    for status in [307, 400, 401, 403, 404, 429, 500, 503] {
        let (actual, headers, body) = call(
            &app,
            "POST",
            &format!("/asset-bundle-registry/entities/active?status={status}"),
            Some(json!({"pointers":["0,0"]})),
        )
        .await;
        assert_eq!(actual.as_u16(), status);
        assert_eq!(headers["content-type"], "application/problem+json");
        assert_eq!(body, br#"{"error":"native failure"}"#);
    }
    server.abort();
}

fn abr_capture(name: &str) -> Value {
    let path = format!(
        "{}/services/catalyst/tests/fixtures/conformance/content-lambdas/captures/abr/{name}",
        env!("DCL_ONE_ROOT")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The boot reads apply the Registry's pointer rule before any backend call,
/// with and without a source: `ids` is not read, an absent or empty
/// `pointers` is the captured 400 (keys in the Registry's order), a body
/// that is not JSON or is `null` the http-commons 500, and the pointers are
/// not capped.
#[tokio::test]
async fn registry_boot_reads_take_the_registry_pointer_rule() {
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let empty = Sources {
        registry: String::new(),
        ..sources_at(addr)
    };
    let captured = [
        ("/entities/active", abr_capture("AB-4-empty-pointers.json")),
        (
            "/entities/versions",
            abr_capture("AB-11-missing-pointers.json"),
        ),
    ];
    for sources in [sources_at(addr), empty] {
        let node = node_without_db(Settings::for_tests(sources)).await;
        let app = router(node.state.clone());
        for (path, capture) in &captured {
            let path = format!("/asset-bundle-registry{path}");
            for body in [
                r#"{}"#,
                r#"{"pointers":[]}"#,
                r#"{"pointers":null}"#,
                r#"{"pointers":""}"#,
                r#"{"pointers":{"length":0}}"#,
                r#"{"ids":["bafkreiexample"]}"#,
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::post(&path)
                            .header("content-type", "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path} {body}");
                assert_eq!(response.headers()["content-type"], "application/json");
                let bytes = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                let parsed: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(parsed, capture["response"]["body"], "{path} {body}");
                // The Registry's key order and the captured length.
                assert_eq!(
                    bytes.as_ref(),
                    br#"{"ok":false,"message":"No pointers provided"}"#
                );
                assert_eq!(
                    bytes.len().to_string(),
                    capture["response"]["headers"]["content-length"]
                        .as_str()
                        .unwrap()
                );
            }
            // `null.pointers` throws in the Registry's handler.
            for body in ["", "{", "null"] {
                let response = app
                    .clone()
                    .oneshot(Request::post(&path).body(Body::from(body)).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                let bytes = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(bytes.as_ref(), br#"{"error":"Internal Server Error"}"#);
            }
            let many: Vec<String> = (0..1001).map(|i| format!("{i},0")).collect();
            let response = app
                .clone()
                .oneshot(
                    Request::post(&path)
                        .header("content-type", "application/json")
                        .body(Body::from(json!({ "pointers": many }).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }
    // Only the two admitted reads (one per path, with a source) reached it.
    assert_eq!(mock.hits("entities/active"), 1);
    assert_eq!(mock.hits("entities/versions"), 1);
}

/// `POST /entities/active` drops a backend entity whose id is on this node's
/// content denylist, leaving the other entities' bytes (content and
/// asset-bundle fields) as received.
#[tokio::test]
async fn registry_active_entities_leave_out_the_content_denylist() {
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let entity = |id: &str, pointer: &str| {
        json!({"id": id, "type": "scene", "timestamp": "1791215990965",
            "deployer": "0x0000000000000000000000000000000000000b02",
            "pointers": [pointer], "content": [], "metadata": {},
            "status": "complete",
            "bundles": {"assets": {"mac": "complete", "windows": "complete", "webgl": "pending"}}})
    };
    let kept = entity("bafkreikept", "1,1");
    let listed = entity("bafkreilisted", "2,2");
    *mock.entities.lock() = vec![kept.clone(), listed.clone()];
    let node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    let app = router(node.state.clone());
    let request = Some(json!({"pointers": ["1,1", "2,2"]}));
    let path = "/asset-bundle-registry/entities/active";
    let (status, all) = call_json(&app, "POST", path, request.clone()).await;
    assert_eq!((status, all), (StatusCode::OK, json!([kept, listed])));
    node.state.content.denylist.add("bafkreilisted");
    let (status, _, bytes) = call(&app, "POST", path, request.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, serde_json::to_vec(&json!([kept])).unwrap());
    node.state.content.denylist.add("bafkreikept");
    let (status, rest) = call_json(&app, "POST", path, request).await;
    assert_eq!((status, rest), (StatusCode::OK, json!([])));
}

/// A backend that answers every read with cache validators and a `304` to
/// any conditional request, as an HTTP cache-aware Registry would; its
/// `/entities/active` answer is a fixed two-entity list.
async fn serve_validating_backend() -> SocketAddr {
    async fn answer(uri: axum::http::Uri, headers: HeaderMap) -> Response {
        let validators = [
            ("etag", "\"backend\""),
            ("last-modified", "Tue, 10 Nov 2026 00:00:00 GMT"),
            ("cache-control", "public, max-age=60"),
        ];
        if headers.contains_key("if-none-match") || headers.contains_key("if-modified-since") {
            return (StatusCode::NOT_MODIFIED, validators).into_response();
        }
        let body = if uri.path() == "/entities/active" {
            json!([{"id": "bafkreikept"}, {"id": "bafkreilisted"}])
        } else {
            json!({})
        };
        (validators, Json(body)).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, Router::new().fallback(answer)).into_future());
    addr
}

/// The backend's `ETag` and `Last-Modified` describe its unfiltered answer:
/// `/entities/active` never sends it the client's conditional headers (its
/// `304` would skip the denylist), keeps the validators while the denylist
/// drops nothing and removes them when it does. `Cache-Control` stays, and
/// the other reads still forward the conditional headers.
#[tokio::test]
async fn registry_active_entities_validators_describe_the_filtered_answer() {
    let addr = serve_validating_backend().await;
    let node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    let app = router(node.state.clone());
    let conditional = |request: axum::http::request::Builder| {
        request
            .header("if-none-match", "\"backend\"")
            .header("if-modified-since", "Tue, 10 Nov 2026 00:00:00 GMT")
    };
    let active = |app: Router| async move {
        let response = app
            .oneshot(
                conditional(Request::post("/asset-bundle-registry/entities/active"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"pointers":["1,1","2,2"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        (status, headers, body)
    };

    let (status, h, body) = active(app.clone()).await;
    assert_eq!(status, StatusCode::OK, "no conditional header reached it");
    assert_eq!(
        body,
        json!([{"id": "bafkreikept"}, {"id": "bafkreilisted"}])
    );
    assert_eq!(h["etag"], "\"backend\"");
    assert_eq!(h["last-modified"], "Tue, 10 Nov 2026 00:00:00 GMT");
    assert_eq!(h["cache-control"], "public, max-age=60");

    node.state.content.denylist.add("bafkreilisted");
    let (status, h, body) = active(app.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([{"id": "bafkreikept"}]));
    assert!(h.get("etag").is_none(), "{h:?}");
    assert!(h.get("last-modified").is_none(), "{h:?}");
    assert_eq!(h["cache-control"], "public, max-age=60");

    let response = app
        .oneshot(
            conditional(Request::get("/asset-bundle-registry/entities/status/bafy"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()["etag"], "\"backend\"");
}

/// The boot reads cap their body at 1 MiB, with or without a source and
/// before any backend call: unity-explorer's largest read (every parcel of a
/// world at the coordinate bound) fits, a body just over the cap is a 413.
#[tokio::test]
async fn registry_boot_reads_cap_the_request_body() {
    let mock = mock();
    let addr = serve_mock(mock.clone()).await;
    let empty = Sources {
        registry: String::new(),
        ..sources_at(addr)
    };
    let grid: Vec<String> = (-150..=150)
        .flat_map(|x| (-150..=150).map(move |y| format!("{x},{y}")))
        .collect();
    let world = json!({ "pointers": grid }).to_string();
    assert_eq!(world.len(), 865_088);
    let over = format!(
        r#"{{"pointers":["0,0"],"pad":"{}"}}"#,
        "x".repeat(1024 * 1024)
    );
    for sources in [sources_at(addr), empty] {
        let node = node_without_db(Settings::for_tests(sources)).await;
        let app = router(node.state.clone());
        for path in ["/entities/active", "/entities/versions"] {
            let path = format!("/asset-bundle-registry{path}");
            for (body, expected) in [
                (world.clone(), StatusCode::OK),
                (over.clone(), StatusCode::PAYLOAD_TOO_LARGE),
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::post(&path)
                            .header("content-type", "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected, "{path}");
            }
        }
    }
    // Only the in-limit reads with a source reached the backend.
    assert_eq!(mock.hits("entities/active"), 1);
    assert_eq!(mock.hits("entities/versions"), 1);
}

/// `GET /denylist` at the handler: a source's non-200 answer passes through,
/// a 200 that is not an array is a 502, `HEAD` asks the source with `GET`,
/// and the local half's failed read (no database here) is a 500, with or
/// without a source.
#[tokio::test]
async fn registry_denylist_handler_answers_source_failures_and_local_read_errors() {
    use axum::http::{Method, Uri};
    async fn source(method: Method, uri: Uri) -> Response {
        match (method, uri.query()) {
            (_, Some("case=busy")) => (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "3")],
                Json(json!({"ok":false,"message":"busy"})),
            )
                .into_response(),
            (_, Some("case=object")) => Json(json!({"not":"an array"})).into_response(),
            (_, Some("case=malformed")) => "{".into_response(),
            // Only a GET gets this distinct status, so HEAD must arrive as GET.
            (Method::GET, Some("case=method")) => StatusCode::CONFLICT.into_response(),
            (_, Some("case=method")) => StatusCode::IM_A_TEAPOT.into_response(),
            _ => Json(json!([])).into_response(),
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, Router::new().fallback(source)).into_future());
    let node = node_without_db(Settings::for_tests(sources_at(addr))).await;
    let app = router(node.state.clone());
    let path = "/asset-bundle-registry/denylist";

    let (status, headers, body) = call(&app, "GET", &format!("{path}?case=busy"), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "3");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"ok":false,"message":"busy"})
    );
    let (status, _, _) = call(&app, "GET", &format!("{path}?case=object"), None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let (status, _, _) = call(&app, "GET", &format!("{path}?case=malformed"), None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let (status, _, body) = call(&app, "HEAD", &format!("{path}?case=method"), None).await;
    assert!(body.is_empty());
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "HEAD reaches the source as GET"
    );
    // The source's array is good; the local read has no database.
    let (status, _, body) = call(&app, "GET", path, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"error":"Internal Server Error"})
    );

    let empty = Sources {
        registry: String::new(),
        ..sources_at(addr)
    };
    let node = node_without_db(Settings::for_tests(empty)).await;
    let (status, _, _) = call(&router(node.state.clone()), "GET", path, None).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "without a source the local rows are the answer, so their read failing is a 500"
    );
}

#[tokio::test]
async fn registry_denylist_reads_local_rows_without_a_source_and_reuses_them() {
    let Some(db) = scratch_database().await else {
        return;
    };
    let mut settings = Settings::for_tests(Sources::defaults());
    settings.sources.registry.clear();
    let node = node_with_db(settings, db.pool.clone()).await;
    for id in ["QmCase", "0,0", "profile-pointer"] {
        crate::content::denylist::add(&db.pool, id, "0xoperator")
            .await
            .unwrap();
    }
    let app = router(node.state);
    let path = "/asset-bundle-registry/denylist";
    let (status, _, first) = call(&app, "GET", path, None).await;
    assert_eq!(status, StatusCode::OK);
    let rows: Value = serde_json::from_slice(&first).unwrap();
    let ids: BTreeSet<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["entity_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, BTreeSet::from(["QmCase", "0,0", "profile-pointer"]));
    for row in rows.as_array().unwrap() {
        assert_eq!(row["created_by"], "0xoperator");
        assert!(row["reason"].is_null());
        assert_eq!(row["created_at"], row["updated_at"]);
    }
    // Closing the pool makes another SELECT fail, so every concurrent success
    // must come from this router's previously loaded local rows.
    db.pool.close().await;
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let app = app.clone();
        requests.spawn(async move { call(&app, "GET", path, None).await });
    }
    while let Some(response) = requests.join_next().await {
        let (status, _, body) = response.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, first);
    }
    let (status, _, body) = call(&app, "HEAD", path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    db.teardown().await;
}
