//! Router-level contract tests. `tests/fixtures/` is the acceptance set: every file
//! is replayed against the in-process router with the fixture semantics of
//! `crate::testing::conformance` (the format, the leaf volatility scrub and
//! the masked-leaf diff), the ones `tests/conformance.rs` applies against a
//! live host. Tests that need rows set
//! `CATALYST_TEST_DATABASE_URL` (a `catalyst` database with schema.sql
//! applied, e.g. the lab one) and are skipped without it.

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::SyncState;
use crate::testing::conformance::diff::compare_json;
use crate::testing::conformance::fixture::{
    Fixture, RecordedRequest, VOLATILE_HEADERS, apply_volatile_paths, media_type,
};
use crate::testing::conformance::volatility::Volatility;
use support::{TestNode, require_test_db};

pub(crate) mod support {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use bytes::Bytes;
    use sqlx::postgres::PgPoolOptions;

    use crate::storage::{Storage, hex_prefix};
    use crate::{ContentState, Settings, SharedState, build_router};

    pub const TEST_CID: &str = "bafkreie4eisvkzyjuqrcendydk6vikqs2vco5lmib4nlzsxtjzofiqy2pa";

    pub struct TestNode {
        pub state: SharedState,
        storage: Arc<Storage>,
        tmp: PathBuf,
    }

    impl TestNode {
        /// A node over a lazy pool that never connects: every route that
        /// does not touch Postgres works, the others answer 500.
        pub async fn without_db(tag: &str) -> Self {
            Self::build(tag, None, Settings::for_tests()).await
        }

        pub async fn without_db_settings(tag: &str, settings: Settings) -> Self {
            Self::build(tag, None, settings).await
        }

        /// A node over `pool` (a stalling one, say) with `settings`.
        pub async fn with_pool_settings(tag: &str, pool: sqlx::PgPool, settings: Settings) -> Self {
            Self::build(tag, Some(pool), settings).await
        }

        /// `None` (skip) without CATALYST_TEST_DATABASE_URL; otherwise over
        /// the process's scratch database (`db::scratch`).
        pub async fn with_db(tag: &str) -> Option<Self> {
            let pool = crate::db::scratch::pool(4).await?;
            Some(Self::build(tag, Some(pool), Settings::for_tests()).await)
        }

        async fn build(tag: &str, pool: Option<sqlx::PgPool>, settings: Settings) -> Self {
            let pool = pool.unwrap_or_else(|| {
                PgPoolOptions::new()
                    .acquire_timeout(std::time::Duration::from_millis(500))
                    .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
                    .unwrap()
            });
            let tmp =
                std::env::temp_dir().join(format!("catalyst-content-{tag}-{}", std::process::id()));
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
            let state = Arc::new(ContentState::new(pool, settings, storage.clone()));
            Self {
                state,
                storage,
                tmp,
            }
        }

        /// The router as `serve` mounts it, with upstream's routing under `/content/`.
        pub fn router(&self) -> axum::Router {
            crate::routing::upstream_routing(build_router(self.state.clone()))
        }

        pub async fn store(&self, hash: &str, data: Bytes) {
            self.storage.store(hash, data).await.unwrap();
        }

        /// Wipes the shard directory behind `hash`: the storage then reports
        /// damage on every read instead of a miss.
        pub async fn destroy_shard(&self, hash: &str) {
            let shard: &Path = &self.storage.root().join(hex_prefix(hash));
            tokio::fs::remove_dir_all(shard).await.unwrap();
        }
    }

    impl Drop for TestNode {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    /// Skips (instead of failing) the calling test when `$expr` doesn't match `$pat` —
    /// every test needing `CATALYST_TEST_DATABASE_URL` guards on it.
    macro_rules! require_test_db {
        ($pat:pat = $expr:expr) => {
            let $pat = $expr else {
                eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
                return;
            };
        };
    }
    pub(crate) use require_test_db;
}

/// Fixtures whose answer comes out of Postgres (an empty one): skipped
/// without a database.
const DB_FIXTURES: &[&str] = &[
    "active-entities-200-nonexistent-id",
    "active-entities-200-nonexistent-pointer",
    "audit-200-scenes-canonicalized",
    "audit-404-outfits-accepted",
    "audit-404-unknown-entity",
    "contents-active-entities-404-unknown-hash",
    "entities-200-outfits-accepted",
];

/// The replay fixtures are the two edge-case directories; `tests/fixtures`
/// also holds the explorer-boot corpus, which is not a request/response pair.
fn fixture_files() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("DCL_ONE_ROOT")).join("services/catalyst/tests/fixtures");
    let mut out = Vec::new();
    for dir in ["content", "cors"].map(|d| root.join(d)) {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                out.push(path);
            }
        }
    }
    out.sort();
    assert!(out.len() >= 29, "fixture set shrank: {}", out.len());
    out
}

fn fixture_request(fx: &RecordedRequest) -> Request<Body> {
    let mut uri = fx.path.clone();
    let query: Vec<String> = fx
        .query
        .iter()
        .flat_map(|(k, vs)| {
            vs.iter()
                .map(move |v| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        })
        .collect();
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(&query.join("&"));
    }
    let mut builder = Request::builder().method(fx.method.as_str()).uri(uri);
    for (k, v) in &fx.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    match &fx.body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// The old replay binary's rules over the in-process router: status, the
/// non-volatile headers the fixture lists (`content-type` by media type), the
/// JSON body after both sides' leaf volatility through the shared differ
/// (`section` is the file stem, as the live replay names it), or the exact bytes.
async fn replay_one(router: Router, section: &str, fx: &Fixture) -> Vec<String> {
    let mut notes = Vec::new();
    let resp = router.oneshot(fixture_request(&fx.request)).await.unwrap();

    if resp.status().as_u16() != fx.response.status {
        notes.push(format!(
            "status: fixture={} candidate={}",
            fx.response.status,
            resp.status()
        ));
    }
    for (raw_key, expected) in &fx.response.headers {
        let key = raw_key.to_ascii_lowercase();
        if VOLATILE_HEADERS.contains(&key.as_str()) {
            continue;
        }
        let Some(actual) = resp
            .headers()
            .get(key.as_str())
            .and_then(|v| v.to_str().ok())
        else {
            notes.push(format!(
                "header {raw_key}: fixture={expected:?} candidate=<missing>"
            ));
            continue;
        };
        let (exp_cmp, act_cmp) = if key == "content-type" {
            (media_type(expected), media_type(actual))
        } else {
            (expected.trim().to_string(), actual.trim().to_string())
        };
        if exp_cmp != act_cmp {
            notes.push(format!(
                "header {raw_key}: fixture={expected:?} candidate={actual:?}"
            ));
        }
    }

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    if let Some(expected_b64) = &fx.response.body_bytes_b64 {
        let expected = base64::engine::general_purpose::STANDARD
            .decode(expected_b64)
            .unwrap();
        if expected.as_slice() != bytes.as_ref() {
            notes.push(format!(
                "body bytes differ: fixture={:?} candidate={:?}",
                String::from_utf8_lossy(&expected),
                String::from_utf8_lossy(&bytes)
            ));
        }
    } else if let Some(expected_json) = &fx.response.body_json {
        let mut baseline = expected_json.clone();
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut candidate) => {
                apply_volatile_paths(&mut baseline, &fx.volatile_paths);
                apply_volatile_paths(&mut candidate, &fx.volatile_paths);
                let diffs = compare_json(
                    section,
                    "fixture",
                    &baseline,
                    &candidate,
                    &Volatility::default(),
                );
                if !diffs.is_empty() {
                    notes.push(format!("body diff: {} difference(s)", diffs.len()));
                    notes.extend(diffs.iter().map(|d| format!("  {d}")));
                }
            }
            Err(e) => notes.push(format!(
                "candidate body is not valid JSON: {e}: {:?}",
                String::from_utf8_lossy(&bytes)
            )),
        }
    }
    notes
}

#[tokio::test]
async fn edge_case_fixtures_replay_exactly() {
    let node = match TestNode::with_db("fixtures").await {
        Some(node) => node,
        None => TestNode::without_db("fixtures").await,
    };
    let has_db = std::env::var("CATALYST_TEST_DATABASE_URL").is_ok();
    // A node that has caught up reports Syncing; the status fixture was captured on one.
    node.state.sync.set_state(SyncState::Syncing);
    // The write fixture is a node that takes deploys (upstream outside `READ_ONLY`).
    node.state.attach_writes(
        true,
        std::sync::Arc::new(
            super::write_deployer::WriteDeployer::with_partial_validation_for_tests(
                node.state.pool.clone(),
                node.state.storage.clone(),
                None,
            ),
        ),
    );

    let mut failures = Vec::new();
    let mut replayed = 0;
    for path in fixture_files() {
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        if !has_db && DB_FIXTURES.contains(&stem.as_str()) {
            eprintln!("skipping {stem}: CATALYST_TEST_DATABASE_URL not set");
            continue;
        }
        let fx: Fixture = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let notes = replay_one(node.router(), &stem, &fx).await;
        replayed += 1;
        if !notes.is_empty() {
            failures.push(format!("{stem}:\n    {}", notes.join("\n    ")));
        }
    }
    assert!(replayed > 0);
    assert!(
        failures.is_empty(),
        "{} fixture(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

async fn get_json(router: Router, uri: &str) -> (StatusCode, Value) {
    let resp = router
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn post_json(router: Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// With the check on a busy host only loses `acceptingUsers`: `/about`
/// still answers 200 and `healthy` follows the sync state.
#[tokio::test]
async fn about_with_the_resource_check_keeps_healthy() {
    let mut settings = crate::Settings::for_tests();
    settings.about_resource_check = true;
    let node = TestNode::without_db_settings("about-check", settings).await;
    node.state.sync.set_state(SyncState::Syncing);
    node.state.sync.beat();
    let (status, body) = get_json(node.router(), "/about").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["healthy"], true);
    assert!(body["acceptingUsers"].is_boolean(), "{body}");
}

#[tokio::test]
async fn about_and_status_report_the_sync_state_honestly() {
    let node = TestNode::without_db("about").await;
    let (status, body) = get_json(node.router(), "/about").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["healthy"], false);
    assert_eq!(body["content"]["synchronizationStatus"], "Bootstrapping");
    assert_eq!(body["content"]["version"], "test");
    assert!(
        body["content"].get("commitHash").is_none(),
        "no made-up commit hash"
    );
    assert_eq!(body["configurations"]["networkId"], 1);
    assert_eq!(body["bff"]["protocolVersion"], "1.0_0");
    assert_eq!(body["acceptingUsers"], false);

    let (status, body) = get_json(node.router(), "/content/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["synchronizationStatus"]["synchronizationState"],
        "Bootstrapping"
    );
    assert_eq!(body["commitHash"], "Unknown");
    assert_eq!(body["synchronizationStatus"]["lastSyncWithDAO"], 0);
    assert!(body["synchronizationStatus"].get("up").is_none());

    node.state.sync.set_state(SyncState::Syncing);
    node.state.sync.beat();
    node.state.sync.set_sync_frontier_ms(42);
    let (status, body) = get_json(node.router(), "/about").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["healthy"], true);
    let (lambdas_status, lambdas_body) = get_json(node.router(), "/lambdas/about").await;
    assert_eq!(
        (lambdas_status, lambdas_body),
        (status, body),
        "/lambdas/about is the root /about"
    );
    let (_, body) = get_json(node.router(), "/status").await;
    assert_eq!(body["synchronizationStatus"]["up"], true);
    assert_eq!(body["synchronizationStatus"]["syncFrontier"], 42);
    let heartbeat = node.state.sync.heartbeat_ms().unwrap();
    assert_eq!(body["synchronizationStatus"]["lastSyncWithDAO"], heartbeat);

    // Upstream answers 200 whatever the sync state.
    node.state.sync.set_state(SyncState::Failed);
    let (status, body) = get_json(node.router(), "/content/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["synchronizationStatus"]["synchronizationState"],
        "Failed"
    );
}

struct FakeCore(bool, u64);
impl super::about_comms::CommsCore for FakeCore {
    fn core_status(&self) -> (bool, u64) {
        (self.0, self.1)
    }
}

fn about_comms_state(
    max_users: Option<u64>,
    commit_hash: Option<&str>,
) -> std::sync::Arc<super::about_comms::AboutCommsState> {
    let settings = super::about_comms::Settings::resolve(super::about_comms::RawSettings {
        protocol: "v3",
        fixed_adapter: Some("archipelago:wss://realm.example/ws"),
        version: None,
        commit_hash,
        max_users,
        offline_when_unreachable: true,
        v4_control_url: None,
        v4_control_audience: None,
        v4_pulse_native_endpoint: None,
        v4_pulse_webtransport_url: None,
        v4_pulse_certificate_hash: None,
        v4_pulse_audience: None,
        v4_island_refresh_url: None,
    })
    .unwrap();
    std::sync::Arc::new(super::about_comms::AboutCommsState::new(settings))
}

struct ParcelCore;
impl super::about_comms::CommsCore for ParcelCore {
    fn core_status(&self) -> (bool, u64) {
        (true, 3)
    }
    fn parcel_counts(&self) -> Vec<((i32, i32), u32)> {
        vec![((10, -2), 2), ((0, 0), 1)]
    }
}

#[tokio::test]
async fn explore_realms_lists_this_node_with_its_live_peers() {
    let node = TestNode::without_db("explore-realms").await;
    let (status, body) = get_json(node.router(), "/lambdas/explore/realms").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([{
            "serverName": "http://localhost",
            "url": "http://localhost",
            "usersCount": 0,
            "maxUsers": 100,
            "userParcels": []
        }])
    );

    let about = about_comms_state(Some(50), None);
    assert!(node.state.attach_about_comms(about.clone()));
    assert!(about.attach_core(std::sync::Arc::new(ParcelCore)));
    let (status, body) = get_json(node.router(), "/lambdas/explore/realms").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["usersCount"], 3);
    assert_eq!(body[0]["maxUsers"], 50);
    assert_eq!(body[0]["userParcels"], json!([[0, 0], [10, -2], [10, -2]]));
}

#[tokio::test]
async fn scenes_listing_is_empty_like_v1() {
    let node = TestNode::without_db("scenes-listing").await;
    let (status, body) = get_json(node.router(), "/scenes").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"scenes": [], "total": 0}));
}

#[tokio::test]
async fn about_carries_the_comms_block_and_its_health_gates_the_realm() {
    let node = TestNode::without_db("about-comms").await;
    node.state.sync.set_state(SyncState::Syncing);
    let (status, body) = get_json(node.router(), "/about").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("comms").is_none(),
        "no COMMS_FIXED_ADAPTER: no block"
    );

    let about = about_comms_state(Some(5), Some("abc"));
    assert!(node.state.attach_about_comms(about.clone()));
    let (status, body) = get_json(node.router(), "/about").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no core attached yet: the connector would not answer"
    );
    assert_eq!(body["comms"]["healthy"], false);
    assert_eq!(body["content"]["healthy"], true);
    assert_eq!(body["healthy"], false);

    assert!(about.attach_core(std::sync::Arc::new(FakeCore(true, 4))));
    let (status, body) = get_json(node.router(), "/about").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["comms"],
        json!({
            "healthy": true,
            "protocol": "v3",
            "commitHash": "abc",
            "usersCount": 4,
            "adapter": "fixed-adapter:archipelago:wss://realm.example/ws",
            "fixedAdapter": "archipelago:wss://realm.example/ws"
        })
    );
    assert_eq!(body["acceptingUsers"], true);

    let full = TestNode::without_db("about-comms-full").await;
    full.state.sync.set_state(SyncState::Syncing);
    let about = about_comms_state(Some(4), None);
    about.attach_core(std::sync::Arc::new(FakeCore(true, 4)));
    full.state.attach_about_comms(about);
    let (status, body) = get_json(full.router(), "/about").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["acceptingUsers"], false,
        "at MAX_USERS the realm stops accepting"
    );

    let stale = TestNode::without_db("about-comms-stale").await;
    stale.state.sync.set_state(SyncState::Syncing);
    let about = about_comms_state(None, None);
    about.attach_core(std::sync::Arc::new(FakeCore(false, 0)));
    stale.state.attach_about_comms(about);
    let (status, body) = get_json(stale.router(), "/about").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a stale core heartbeat takes the realm down, as the old probe did"
    );
    assert_eq!(body["comms"]["healthy"], false);
}

/// The old conformance suite's `example/about-golden.json`, the one stage-1
/// replay fixture v2 failed while `/about` had no comms block.
#[tokio::test]
async fn about_golden_replays_with_the_comms_block() {
    let node = TestNode::without_db("about-golden").await;
    node.state.sync.set_state(SyncState::Syncing);
    let about = about_comms_state(None, None);
    about.attach_core(std::sync::Arc::new(FakeCore(true, 12)));
    node.state.attach_about_comms(about);
    let path = PathBuf::from(env!("DCL_ONE_ROOT"))
        .join("services/catalyst/tests/fixtures/example/about-golden.json");
    let fx: Fixture = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let notes = replay_one(node.router(), "about-golden", &fx).await;
    assert!(notes.is_empty(), "about-golden: {}", notes.join("\n"));
}

/// Upstream mints one bare UUID per process (`logic/peer-cluster/component.ts:51`)
/// and answers it on every call.
#[tokio::test]
async fn challenge_is_one_uuid_per_process() {
    let node = TestNode::without_db("challenge").await;
    let (status, a) = get_json(node.router(), "/content/challenge").await;
    assert_eq!(status, StatusCode::OK);
    let (_, b) = get_json(node.router(), "/challenge").await;
    let a = a["challengeText"].as_str().unwrap().to_string();
    assert!(uuid::Uuid::parse_str(&a).is_ok(), "{a}");
    assert_eq!(a.len(), 36);
    assert_eq!(a, b["challengeText"]);
}

#[tokio::test]
async fn unknown_routes_and_write_routes() {
    let node = TestNode::without_db("routes").await;
    let resp = node
        .router()
        .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = node
        .router()
        .oneshot(
            Request::post("/content/entities")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
                .body(Body::from("--x--\r\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a node without CONTENT_WRITES_ENABLED refuses the deploy (write_path_tests has the rest)"
    );
}

async fn send(router: Router, request: Request<Body>) -> (StatusCode, header::HeaderMap, Vec<u8>) {
    let resp = router.oneshot(request).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (parts.status, parts.headers, bytes.to_vec())
}

/// `conformance/content-lambdas/captures/content-reads/CR-12-*.json`: the
/// production peer's answers to a method its router does not serve on a path.
#[tokio::test]
async fn wrong_methods_answer_like_the_captured_router() {
    let node = TestNode::without_db("methods").await;
    let (status, headers, body) = send(
        node.router(),
        Request::get("/content/entities/active")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(body, br#"{"error":"Unrecognized type: active"}"#);

    for (method, path, allow) in [
        ("PUT", "/content/status", "HEAD, GET"),
        ("PATCH", "/content/status", "HEAD, GET"),
        ("DELETE", "/content/entities/active", "HEAD, GET, POST"),
        ("POST", "/content/contents/bafkreiabc", "HEAD, GET"),
    ] {
        let (status, headers, body) = send(
            node.router(),
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
        assert_eq!(headers[header::ALLOW], allow, "{method} {path}");
        assert!(body.is_empty(), "{method} {path}");
    }
}

/// `CR-16-path-case-*.json`, `CS-23-path-case-deployments.json`: the static
/// segments under `/content/` match whatever their case.
#[tokio::test]
async fn content_paths_match_case_insensitively() {
    let node = TestNode::without_db("path-case").await;
    node.store(support::TEST_CID, bytes::Bytes::from_static(b"hello"))
        .await;
    let (status, headers, _) = send(
        node.router(),
        Request::head(format!("/content/Contents/{}", support::TEST_CID))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], "5");

    let (status, body) = get_json(node.router(), "/content/Status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["synchronizationStatus"].is_object(), "{body}");

    // The history read reaches its handler (the absent database's 500), not
    // the 404 fallback.
    let (status, _) = get_json(node.router(), "/content/Deployments?limit=1").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    // A GET keeps `/entities/{type}` and echoes the type as sent, as
    // upstream's `/entities/:type` does.
    let (status, _, body) = send(
        node.router(),
        Request::get("/content/Entities/Active")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, br#"{"error":"Unrecognized type: Active"}"#);

    // The write routes keep their case: the gateway's deploy location and
    // its limits match these spellings.
    let (status, _, _) = send(
        node.router(),
        Request::post("/content/Entities")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The root mounts stay case-sensitive: upstream production has none.
    let (status, _, _) = send(
        node.router(),
        Request::get("/Status").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The case rewrite's route lists name only routed paths: a `PATCH` (served
/// on none) is each one's 405, not the 404 fallback.
#[tokio::test]
async fn every_listed_content_route_is_routed() {
    let node = TestNode::without_db("route-list").await;
    for route in crate::routing::CONTENT_ROUTE_PATHS
        .iter()
        .chain(crate::routing::CASE_SENSITIVE_ROUTE_PATHS)
    {
        let path: Vec<&str> = route
            .split('/')
            .map(|s| if s.starts_with('{') { "x" } else { s })
            .collect();
        let path = format!("/content{}", path.join("/"));
        let (status, _, _) = send(
            node.router(),
            Request::patch(&path).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
}

/// Upstream's preflight allows `GET,HEAD,POST,OPTIONS` only, so a content
/// route that takes another method must keep the shared CORS rule
/// (`routing::upstream_cors_path` false), or browsers could not call it.
#[tokio::test]
async fn every_route_outside_upstreams_cors_methods_is_exempt() {
    let node = TestNode::without_db("cors-methods").await;
    for route in crate::routing::CONTENT_ROUTE_PATHS
        .iter()
        .chain(crate::routing::CASE_SENSITIVE_ROUTE_PATHS)
    {
        let path: Vec<&str> = route
            .split('/')
            .map(|s| if s.starts_with('{') { "x" } else { s })
            .collect();
        let path = format!("/content{}", path.join("/"));
        let (status, headers, _) = send(
            node.router(),
            Request::patch(&path).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        let allow = headers[header::ALLOW].to_str().unwrap().to_string();
        let other = allow
            .split(',')
            .map(str::trim)
            .any(|m| !["GET", "HEAD", "POST", "OPTIONS"].contains(&m));
        if other {
            assert!(
                !crate::routing::upstream_cors_path(&path),
                "{path} allows {allow} but takes upstream's CORS rule"
            );
        }
    }
}

/// The other direction: every path `content_routes` registers is in exactly
/// one of the two lists, so a new route cannot silently stay out of the
/// case rewrite.
#[test]
fn every_content_route_is_listed_once() {
    let source = include_str!("lib.rs").replace("\r\n", "\n");
    let body = source
        .split("fn content_routes(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("content_routes in lib.rs");
    let mut registered: Vec<&str> = body
        .split(".route(")
        .skip(1)
        .filter_map(|call| call.split('"').nth(1))
        .collect();
    registered.sort_unstable();
    let mut listed: Vec<&str> = crate::routing::CONTENT_ROUTE_PATHS
        .iter()
        .chain(crate::routing::CASE_SENSITIVE_ROUTE_PATHS)
        .copied()
        .collect();
    listed.sort_unstable();
    assert_eq!(registered, listed);
}

/// `CR-17-body-over-10mib.json`: upstream caps the `POST /entities/active`
/// body at `MAX_ACTIVE_ENTITIES_BODY_SIZE` (10 MiB) with a plain-text 413.
#[tokio::test]
async fn active_entities_body_cap_is_ten_mib() {
    let node = TestNode::without_db("body-cap").await;
    let body_of = |len: usize| {
        let mut body = br#"{"pointers":[""#.to_vec();
        body.resize(len - 3, b'a');
        body.extend_from_slice(br#""]}"#);
        body
    };
    let post = |body: Vec<u8>| {
        Request::post("/content/entities/active")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let (status, headers, body) = send(
        node.router(),
        post(body_of(
            crate::routing::MAX_ACTIVE_ENTITIES_BODY_BYTES + 512,
        )),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(body, b"Payload Too Large");

    // Under the cap the body reaches the lookup (the absent database's 500).
    let (status, _, _) = send(node.router(), post(body_of(1024 * 1024))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn database_outage_is_a_500_not_an_empty_answer() {
    let node = TestNode::without_db("outage").await;
    let (status, body) = post_json(
        node.router(),
        "/content/entities/active",
        json!({"pointers": ["0,0"]}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error": "Internal Server Error"}));
}

/// Upstream reads the POST /entities/active body as JSON whatever the
/// `Content-Type` (`ensureJsonContentType: false`): a `text/plain` or
/// header-less body reaches the lookup (the 500 of the absent database), and
/// a body that is not JSON is the schema validator's 400.
#[tokio::test]
async fn active_entities_parses_the_body_whatever_the_content_type() {
    let node = TestNode::without_db("active-any-type").await;
    let send = |content_type: Option<&'static str>, body: &'static str| {
        let mut request = Request::post("/content/entities/active");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let router = node.router();
        async move {
            let resp = router
                .oneshot(request.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    for content_type in [Some("text/plain;charset=UTF-8"), None] {
        let (status, _) = send(content_type, r#"{"pointers":["0,0"]}"#).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{content_type:?}"
        );
        let (status, body) = send(content_type, "{}").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{content_type:?}");
        assert_eq!(body["message"], "Invalid JSON body");
    }
    let (status, body) = send(Some("text/plain"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"ok": false, "message": "Unexpected end of JSON input"})
    );
}

#[tokio::test]
async fn snapshot_routes_over_the_database() {
    require_test_db!(Some(node) = TestNode::with_db("snapshots").await);
    let pool = node.state.pool.clone();
    sqlx::query(
        "DELETE FROM content.snapshots WHERE hash LIKE 'test-snap-%' OR (hash IS NULL AND init_timestamp = to_timestamp(5))",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = get_json(node.router(), "/content/snapshots").await;
    if status == StatusCode::OK {
        eprintln!("snapshots present from another run; skipping the not-yet-created checks");
    } else {
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, json!({"error": "New Snapshots not yet created"}));
        for uri in ["/snapshot", "/content/snapshot/scene"] {
            let (status, body) = get_json(node.router(), uri).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body, json!({"error": "Snapshot not yet created"}));
        }
    }

    for (hash, init, end) in [
        ("test-snap-year", 0i64, 1_000i64),
        ("test-snap-day", 1_900, 2_000),
        ("test-snap-month", 1_000, 2_000),
        ("test-snap-week", 1_500, 1_900),
    ] {
        sqlx::query(
            "INSERT INTO content.snapshots (hash, init_timestamp, end_timestamp, replaced_hashes, number_of_entities, generation_time)
             VALUES ($1, to_timestamp($2 / 1000.0), to_timestamp($3 / 1000.0), '{}', 1, to_timestamp($4 / 1000.0))",
        )
        .bind(hash)
        .bind(init as f64)
        .bind(end as f64)
        .bind((end + 1) as f64)
        .execute(&pool)
        .await
        .unwrap();
    }
    // A row without a hash is not a stored snapshot: upstream never lists one.
    sqlx::query(
        "INSERT INTO content.snapshots (hash, init_timestamp, end_timestamp, replaced_hashes, number_of_entities, generation_time)
         VALUES (NULL, to_timestamp(5), to_timestamp(6), '{}', 1, to_timestamp(7))",
    )
    .execute(&pool)
    .await
    .unwrap();
    node.state.deployments_cache.clear();

    let (status, body) = get_json(node.router(), "/content/snapshots").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .all(|s| !s["hash"].as_str().unwrap().is_empty()),
        "{body}"
    );
    let listed: Vec<&Value> = body
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["hash"].as_str().unwrap_or("").starts_with("test-snap-"))
        .collect();
    // Division order, as upstream: oldest window first, then the shorter
    // windows that start later.
    let order: Vec<&str> = listed.iter().map(|s| s["hash"].as_str().unwrap()).collect();
    assert_eq!(
        order,
        [
            "test-snap-year",
            "test-snap-month",
            "test-snap-week",
            "test-snap-day"
        ]
    );
    let month = listed
        .iter()
        .find(|s| s["hash"] == "test-snap-month")
        .unwrap();
    assert_eq!(
        **month,
        json!({
            "hash": "test-snap-month",
            "timeRange": {"initTimestamp": 1_000, "endTimestamp": 2_000},
            "numberOfEntities": 1,
            "replacedSnapshotHashes": [],
            "generationTimestamp": 2_001
        })
    );

    let (status, body) = get_json(node.router(), "/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    if body["hash"] == "test-snap-month" {
        assert_eq!(body["lastIncludedDeploymentTimestamp"], 2_000);
        let entities = body["entities"].as_object().unwrap();
        assert_eq!(entities.len(), super::query_params::ENTITY_TYPES.len());
        assert_eq!(entities["scene"]["hash"], "test-snap-month");
        let (status, body) = get_json(node.router(), "/content/snapshot/scenes").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"hash": "test-snap-month", "lastIncludedDeploymentTimestamp": 2_000})
        );
    }
    let (status, body) = get_json(node.router(), "/snapshot/banana").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({"error": "Unrecognized type: banana"}));

    sqlx::query(
        "DELETE FROM content.snapshots WHERE hash LIKE 'test-snap-%' OR (hash IS NULL AND init_timestamp = to_timestamp(5))",
    )
    .execute(&pool)
    .await
    .unwrap();
}

struct SeededEntity {
    id: String,
    pointer: String,
    hash: String,
}

async fn seed_deployment(
    pool: &sqlx::PgPool,
    tag: &str,
    n: u32,
    entity_type: &str,
    pointer: &str,
    ts_ms: i64,
) -> SeededEntity {
    seed_deployment_with_pointers(pool, tag, n, entity_type, &[pointer], ts_ms).await
}

/// One deployment active on every pointer of `pointers` (stored as given).
async fn seed_deployment_with_pointers(
    pool: &sqlx::PgPool,
    tag: &str,
    n: u32,
    entity_type: &str,
    pointers: &[&str],
    ts_ms: i64,
) -> SeededEntity {
    let id = format!("test-{tag}-{n}");
    let hash = format!("test-{tag}-hash-{n}");
    let pointers: Vec<String> = pointers.iter().map(|p| p.to_string()).collect();
    let deployment_id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments
            (deployer_address, version, entity_type, entity_id, entity_metadata, entity_timestamp,
             entity_pointers, local_timestamp, auth_chain)
         VALUES ('0xDEPLOYER', 'v3', $1, $2, $3::json, to_timestamp($4 / 1000.0), $5,
                 to_timestamp($4 / 1000.0), '[{\"type\":\"SIGNER\",\"payload\":\"0xdeployer\"}]'::json)
         RETURNING id",
    )
    .bind(entity_type)
    .bind(&id)
    .bind(json!({"v": {"name": id}}))
    .bind(ts_ms as f64)
    .bind(&pointers)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, 'scene.json')")
        .bind(deployment_id)
        .bind(&hash)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO content.active_pointers (pointer, entity_id, entity_type)
         SELECT p, $2, $3 FROM unnest($1::text[]) AS p",
    )
    .bind(&pointers)
    .bind(&id)
    .bind(entity_type)
    .execute(pool)
    .await
    .unwrap();
    SeededEntity {
        id,
        pointer: pointers[0].clone(),
        hash,
    }
}

/// Many request pointers of one entity yield that entity once; the answer is
/// ordered by each entity's first matching pointer, the request's casing does
/// not matter and unknown pointers are absent, not an error.
#[tokio::test]
async fn active_entities_by_pointers_dedupe_multi_pointer_entities() {
    require_test_db!(Some(node) = TestNode::with_db("multiptr").await);
    let pool = node.state.pool.clone();
    let tag = "multiptr";
    unseed(&pool, tag).await;
    let scene = seed_deployment_with_pointers(
        &pool,
        tag,
        1,
        "scene",
        &["7401,7401", "7401,7402", "7402,7401", "7402,7402"],
        1_700_000_000_000,
    )
    .await;
    let profile = seed_deployment(
        &pool,
        tag,
        2,
        "profile",
        "0x00000000000000000000000000000000000d0dac",
        1_700_000_001_000,
    )
    .await;
    let other = seed_deployment(&pool, tag, 3, "scene", "7403,7403", 1_700_000_002_000).await;

    let body = json!({"pointers": [
        "0X00000000000000000000000000000000000D0DAC",
        "7402,7402", "7401,7402", "7401,7401", "7401,7402",
        "7403,7403",
        "7404,7404",
    ]});
    let (status, answer) = post_json(node.router(), "/content/entities/active", body).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = answer
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![profile.id.as_str(), scene.id.as_str(), other.id.as_str()]
    );
    assert_eq!(
        answer[1]["pointers"],
        json!(["7401,7401", "7401,7402", "7402,7401", "7402,7402"])
    );
    assert_eq!(
        answer[1]["content"],
        json!([{"file": "scene.json", "hash": scene.hash}])
    );
    assert_eq!(answer[1]["metadata"], json!({"name": scene.id}));

    let (status, answer) = get_json(
        node.router(),
        "/content/entities/scene?pointer=7402,7401&pointer=7401,7401&pointer=7402,7402",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer.as_array().unwrap().len(), 1);
    assert_eq!(answer[0]["id"], scene.id);

    let (status, answer) = post_json(
        node.router(),
        "/content/entities/active",
        json!({"pointers": ["7404,7404", "7405,7405"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer, json!([]));

    // CR-8: upstream's POST answer has no `Cache-Control`, while the
    // `GET /entities/:type` one does (`CR-14-active-entity-key-order.json`,
    // `CR-16-trailing-slash-entities.json`).
    let (status, headers, _) = send(
        node.router(),
        Request::post("/content/entities/active")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"pointers":["7401,7401"]}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CACHE_CONTROL).is_none());
    let (status, headers, _) = send(
        node.router(),
        Request::get("/content/entities/scene?pointer=7401,7401")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=10");

    unseed(&pool, tag).await;
}

/// The content list sorts by file name under upstream's `en_US` collation,
/// whatever order the files were stored in: punctuation is ignored at the
/// first level, so `move_02` precedes `move-04` (byte order would reverse
/// them), and letters sort case-insensitively (`OpenFork_0.glb` between
/// `game.js` and `scene.json`).
#[tokio::test]
async fn content_files_list_in_upstream_collation_order() {
    require_test_db!(Some(node) = TestNode::with_db("cfor").await);
    let pool = node.state.pool.clone();
    let tag = "cfor";
    unseed(&pool, tag).await;
    let scene = seed_deployment(&pool, tag, 1, "scene", "7411,7411", 1_700_000_000_000).await;
    for (n, key) in ["move-04.glb", "game.js", "OpenFork_0.glb", "move_02.glb"]
        .iter()
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO content.content_files (deployment, content_hash, key)
             SELECT id, $2, $3 FROM content.deployments WHERE entity_id = $1",
        )
        .bind(&scene.id)
        .bind(format!("{}-{n}", scene.hash))
        .bind(key)
        .execute(&pool)
        .await
        .unwrap();
    }
    let expected = json!([
        {"file": "game.js", "hash": format!("{}-1", scene.hash)},
        {"file": "move_02.glb", "hash": format!("{}-3", scene.hash)},
        {"file": "move-04.glb", "hash": format!("{}-0", scene.hash)},
        {"file": "OpenFork_0.glb", "hash": format!("{}-2", scene.hash)},
        {"file": "scene.json", "hash": scene.hash},
    ]);
    let (status, answer) = post_json(
        node.router(),
        "/content/entities/active",
        json!({"pointers": ["7411,7411"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer[0]["content"], expected);
    // the deployments list names the file under `key`, as upstream does
    let (status, answer) = get_json(
        node.router(),
        "/content/deployments?entityType=scene&pointer=7411,7411&fields=content",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let by_key: Vec<serde_json::Value> = expected
        .as_array()
        .unwrap()
        .iter()
        .map(|e| json!({"key": e["file"], "hash": e["hash"]}))
        .collect();
    assert_eq!(answer["deployments"][0]["content"], json!(by_key));
    unseed(&pool, tag).await;
}

async fn unseed(pool: &sqlx::PgPool, tag: &str) {
    let like = format!("test-{tag}-%");
    sqlx::query("DELETE FROM content.content_files WHERE content_hash LIKE $1")
        .bind(&like)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.active_pointers WHERE entity_id LIKE $1")
        .bind(&like)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.deployments WHERE entity_id LIKE $1")
        .bind(&like)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.failed_deployments WHERE entity_id LIKE $1")
        .bind(&like)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn entity_reads_over_seeded_rows() {
    require_test_db!(Some(node) = TestNode::with_db("entities").await);
    let pool = node.state.pool.clone();
    let tag = "entities";
    unseed(&pool, tag).await;
    let scene = seed_deployment(&pool, tag, 1, "scene", "-7331,7331", 1_700_000_000_000).await;
    let wearable = seed_deployment(
        &pool,
        tag,
        2,
        "wearable",
        "urn:decentraland:matic:collections-v2:0x000000000000000000000000000000000000cafe:0",
        1_700_000_001_000,
    )
    .await;

    let (status, body) = get_json(
        node.router(),
        &format!(
            "/content/entities/scene?pointer={}",
            urlencoding::encode(&scene.pointer)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([{
            "version": "v3",
            "id": scene.id,
            "type": "scene",
            "timestamp": 1_700_000_000_000i64,
            "pointers": [scene.pointer],
            "content": [{"file": "scene.json", "hash": scene.hash}],
            "metadata": {"name": scene.id}
        }])
    );

    let (status, body) = get_json(
        node.router(),
        &format!("/content/entities/scenes?id={}&fields=metadata", scene.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["pointers"], json!([]));
    assert_eq!(body[0]["metadata"]["name"], scene.id);

    let (status, body) = post_json(
        node.router(),
        "/content/entities/active",
        json!({"ids": [scene.id, wearable.id, "test-entities-missing"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&scene.id.as_str()) && ids.contains(&wearable.id.as_str()));

    let (status, body) = post_json(
        node.router(),
        "/entities/active",
        json!({"pointers": [scene.pointer.to_uppercase()]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["id"], scene.id);

    let (status, body) = get_json(
        node.router(),
        &format!("/content/contents/{}/active-entities", scene.hash),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([scene.id]));

    let (status, body) = get_json(
        node.router(),
        "/content/entities/active/collections/urn:decentraland:matic:collections-v2:0x000000000000000000000000000000000000cafe",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["entities"][0]["id"], wearable.id);

    let (status, body) = get_json(
        node.router(),
        "/content/entities/active/collections/URN:Decentraland:matic:Collections-V2:0x000000000000000000000000000000000000CAFE",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1, "the prefix matches case-insensitively");

    let (status, body) = get_json(
        node.router(),
        &format!("/content/audit/scenes/{}", scene.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], "v3");
    assert_eq!(body["localTimestamp"], 1_700_000_000_000i64);
    assert_eq!(body["authChain"][0]["type"], "SIGNER");
    assert!(body.get("localProvenance").is_none());

    // Upstream filters the audit lookup on the type too: the scene's id under
    // another type is not found.
    let (status, body) = get_json(
        node.router(),
        &format!("/content/audit/wearable/{}", scene.id),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "No deployment found"}));

    node.state.denylist.add(&scene.id);
    let (status, body) = post_json(
        node.router(),
        "/content/entities/active",
        json!({"ids": [scene.id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
    let (status, _) = get_json(
        node.router(),
        &format!("/content/contents/{}/active-entities", scene.hash),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    unseed(&pool, tag).await;
}

/// An id only a remote list (`DENYLIST_URLS`) names is hidden from the
/// active-entity and history reads as an operator-listed one is
/// (`AB-10-peer-active-by-denylisted-id.json`: production answers `[]`).
#[tokio::test]
async fn a_remote_listed_entity_is_hidden_from_the_reads() {
    require_test_db!(Some(node) = TestNode::with_db("remotedeny").await);
    let pool = node.state.pool.clone();
    let tag = "remotedeny";
    unseed(&pool, tag).await;
    let scene = seed_deployment(&pool, tag, 1, "scene", "-7332,7332", 1_700_000_000_000).await;
    let (status, body) = post_json(
        node.router(),
        "/content/entities/active",
        json!({"ids": [scene.id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    node.state
        .denylist
        .set_remote("https://registry.example/denylist", [scene.id.clone()]);
    for body in [
        json!({"ids": [scene.id]}),
        json!({"pointers": [scene.pointer]}),
    ] {
        let (status, answer) = post_json(node.router(), "/content/entities/active", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer, json!([]));
    }
    let (status, answer) = get_json(
        node.router(),
        &format!("/content/entities/scene?id={}", scene.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer, json!([]));
    let (status, _) = get_json(
        node.router(),
        &format!("/content/contents/{}/active-entities", scene.hash),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, answer) = get_json(
        node.router(),
        &format!("/content/deployments?entityId={}", scene.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["deployments"], json!([]));

    unseed(&pool, tag).await;
}

#[tokio::test]
async fn history_reads_over_seeded_rows() {
    require_test_db!(Some(node) = TestNode::with_db("history").await);
    let pool = node.state.pool.clone();
    let tag = "history";
    unseed(&pool, tag).await;
    let base = 1_600_000_000_000i64;
    let mut seeded = Vec::new();
    for n in 1..=3 {
        seeded.push(
            seed_deployment(
                &pool,
                tag,
                n,
                "scene",
                &format!("-{n},7000"),
                base + n as i64 * 1000,
            )
            .await,
        );
    }
    let ids: Vec<String> = seeded.iter().map(|s| s.id.clone()).collect();
    let id_filter = ids
        .iter()
        .map(|id| format!("entityId={id}"))
        .collect::<Vec<_>>()
        .join("&");

    let (status, body) = get_json(
        node.router(),
        &format!("/content/deployments?{id_filter}&limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pagination"]["moreData"], true);
    assert_eq!(body["pagination"]["limit"], 2);
    let listed: Vec<&str> = body["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["entityId"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed,
        vec![ids[2].as_str(), ids[1].as_str()],
        "DESC by local timestamp"
    );
    assert_eq!(body["deployments"][0]["entityTimestamp"], base + 3000);
    assert_eq!(body["deployments"][0]["deployedBy"], "0xDEPLOYER");
    assert!(body["deployments"][0].get("auditInfo").is_none());
    assert_eq!(body["deployments"][0]["content"][0]["hash"], seeded[2].hash);
    assert_eq!(body["filters"]["entityIds"], json!(ids));
    let next = body["pagination"]["next"].as_str().unwrap().to_string();
    assert!(next.contains(&format!("lastId={}", ids[1])) && next.contains("sortingOrder=DESC"));

    let (status, body) = get_json(node.router(), &format!("/content/deployments{next}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deployments"].as_array().unwrap().len(), 1);
    assert_eq!(body["deployments"][0]["entityId"], ids[0]);
    assert_eq!(body["pagination"]["moreData"], false);

    // As upstream `qsGetArray`, a single empty value is no filter, while an
    // empty element inside a list is kept and matches nothing.
    let (status, body) = get_json(
        node.router(),
        &format!("/content/deployments?{id_filter}&pointer=&entityType="),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deployments"].as_array().unwrap().len(), 3);
    let (status, body) = get_json(
        node.router(),
        &format!("/content/deployments?{id_filter}&pointer=-1,7000&pointer="),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deployments"].as_array().unwrap().len(), 1);
    assert_eq!(body["filters"]["pointers"], json!(["-1,7000", ""]));

    // Upstream sends no cache headers on /deployments; the memo is internal.
    let resp = node
        .router()
        .oneshot(
            Request::get(format!("/content/deployments?{id_filter}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get(header::CACHE_CONTROL).is_none());
    assert!(resp.headers().get("x-cache").is_none());

    // Upstream drops denylisted entities from the page after computing
    // `moreData` (`logic/deployments/component.ts:316-318`): the page is
    // short, `moreData` still holds and `next` follows the last kept row.
    node.state.denylist.add(&ids[1]);
    let (status, body) = get_json(
        node.router(),
        &format!("/content/deployments?{id_filter}&limit=2&fields=pointers"),
    )
    .await;
    node.state.denylist.remove(&ids[1]);
    assert_eq!(status, StatusCode::OK);
    let listed: Vec<&str> = body["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["entityId"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec![ids[2].as_str()]);
    assert_eq!(body["pagination"]["moreData"], true);
    let next = body["pagination"]["next"].as_str().unwrap();
    assert!(next.contains(&format!("lastId={}", ids[2])), "{next}");

    let (status, body) = get_json(
        node.router(),
        &format!("/content/deployments?{id_filter}&sortingOrder=ASC&sortingField=entityTimestamp&fields=auditInfo"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deployments"][0]["entityId"], ids[0]);
    assert!(body["deployments"][0].get("pointers").is_none());
    assert_eq!(
        body["deployments"][0]["auditInfo"]["authChain"][0]["type"],
        "SIGNER"
    );

    let (status, body) = get_json(
        node.router(),
        &format!(
            "/content/pointer-changes?entityType=scene&from={}&to={}",
            base,
            base + 4000
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let deltas: Vec<&str> = body["deltas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["entityId"].as_str().unwrap())
        .collect();
    assert_eq!(
        deltas,
        vec![ids[2].as_str(), ids[1].as_str(), ids[0].as_str()]
    );
    assert_eq!(body["deltas"][0]["pointers"], json!(["-3,7000"]));
    assert_eq!(
        body["filters"],
        json!({"entityTypes": ["scene"], "from": base, "to": base + 4000, "includeAuthChain": false})
    );

    let (status, body) = get_json(
        node.router(),
        &format!(
            "/content/pointer-changes?entityType=scene&from={}&to={}&limit=2",
            base,
            base + 4000
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pagination"]["moreData"], true);
    let next = body["pagination"]["next"].as_str().unwrap().to_string();
    assert_eq!(
        next,
        format!(
            "?entityType=scene&from={base}&to={}&limit=2&lastId={}",
            base + 2000,
            ids[1]
        )
    );
    let (_, body) = get_json(node.router(), &format!("/content/pointer-changes{next}")).await;
    assert_eq!(body["deltas"].as_array().unwrap().len(), 1);
    assert_eq!(body["deltas"][0]["entityId"], ids[0]);

    sqlx::query(
        "INSERT INTO content.failed_deployments
            (entity_id, entity_type, failure_time, reason, auth_chain, error_description, snapshot_hash)
         VALUES ($1, 'scene', to_timestamp($2 / 1000.0), 'DEPLOYMENT_ERROR', '[]'::json, 'boom', 'test-history-snapshot')",
    )
    .bind(format!("test-{tag}-failed"))
    .bind(base as f64)
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = get_json(node.router(), "/content/failed-deployments").await;
    assert_eq!(status, StatusCode::OK);
    let failed = body
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["entityId"] == format!("test-{tag}-failed"))
        .expect("the seeded failure is listed");
    assert_eq!(failed["failureTimestamp"], base);
    assert_eq!(failed["retryCount"], 0);
    assert_eq!(failed["errorDescription"], "boom");
    assert_eq!(failed["reason"], "Deployment error");
    assert_eq!(failed["snapshotHash"], "test-history-snapshot");
    assert!(failed["nextRetryAt"].as_i64().unwrap() > 0);

    // CS-10: a failure outside a snapshot has no `snapshotHash` key, and every
    // reason reads as upstream's only one.
    sqlx::query(
        "INSERT INTO content.failed_deployments
            (entity_id, entity_type, failure_time, reason, auth_chain, error_description, snapshot_hash)
         VALUES ($1, 'scene', to_timestamp($2 / 1000.0), 'No entity', '[]'::json, 'gone', '')",
    )
    .bind(format!("test-{tag}-unsnapshotted"))
    .bind(base as f64)
    .execute(&pool)
    .await
    .unwrap();
    let (_, body) = get_json(node.router(), "/content/failed-deployments").await;
    let failed = body
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["entityId"] == format!("test-{tag}-unsnapshotted"))
        .expect("the seeded failure is listed");
    assert_eq!(failed["reason"], "Deployment error");
    assert!(failed.get("snapshotHash").is_none(), "{failed}");
    let (status, body) = get_json(
        node.router(),
        "/content/failed-deployments?offset=0&limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().len() <= 1);

    unseed(&pool, tag).await;
}

#[tokio::test]
async fn ordinary_content_routes_keep_value_metadata_projection() {
    let node = TestNode::with_db("ordered-metadata")
        .await
        .expect("test database required");
    let pool = &node.state.pool;
    let id = format!("ordered-{}", uuid::Uuid::new_v4());
    let pointer = id.to_lowercase();
    let metadata = r#"{"v":{"z":1,"a":2,"nested":{"z":true,"a":null}}}"#;
    sqlx::query("INSERT INTO content.deployments (deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) VALUES ('0x0000000000000000000000000000000000000001','v3','scene',$1,$2::json,to_timestamp(1),$3,to_timestamp(1),'[]'::json)")
        .bind(&id).bind(metadata).bind(vec![pointer.clone()]).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO content.active_pointers (pointer,entity_id,entity_type) VALUES ($1,$2,'scene')").bind(&pointer).bind(&id).execute(pool).await.unwrap();
    let expected = format!(
        r#"{{"id":"{id}","type":"scene","pointers":["{pointer}"],"content":[],"version":"v3","timestamp":1000,"metadata":{{"a":2,"nested":{{"a":null,"z":true}},"z":1}}}}"#
    );
    for selector in [
        json!({"pointers":[pointer.to_uppercase(),pointer]}),
        json!({"ids":[id]}),
    ] {
        for path in ["/entities/active", "/content/entities/active"] {
            let (status, _, body) = send(
                node.router(),
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(selector.to_string()))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(String::from_utf8(body).unwrap(), format!("[{expected}]"));
        }
    }
    let ids = vec![id.clone()];
    let pointers = vec![pointer.to_uppercase(), pointer.clone()];
    let columns = crate::queries::MaskedColumns {
        content: true,
        metadata: true,
    };
    for docs in [
        crate::queries::active_entity_docs_by_ids(pool, &ids)
            .await
            .unwrap(),
        crate::queries::masked_entity_docs_by_ids(pool, &ids, columns)
            .await
            .unwrap(),
        crate::queries::masked_entity_docs_by_pointers(pool, &pointers, columns)
            .await
            .unwrap(),
        crate::queries::active_entity_docs_by_prefix(pool, &pointer, 0, 10)
            .await
            .unwrap()
            .entities,
    ] {
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].json.as_ref(), expected.as_bytes());
    }
    let ordered =
        crate::queries::active_entity_docs_with_ordered_metadata_by_pointers(pool, &pointers)
            .await
            .unwrap();
    assert_eq!(ordered.len(), 1);
    assert_eq!(ordered[0].id, id);
    assert!(
        std::str::from_utf8(&ordered[0].json)
            .unwrap()
            .ends_with(r#""metadata":{"z":1,"a":2,"nested":{"z":true,"a":null}}}"#)
    );
    // Stored order is authoritative here: already normalized rows are not repaired.
    sqlx::query("UPDATE content.deployments SET entity_metadata=$2::json WHERE entity_id=$1")
        .bind(&id)
        .bind(r#"{"v":{"a":2,"z":1}}"#)
        .execute(pool)
        .await
        .unwrap();
    let ordered =
        crate::queries::active_entity_docs_with_ordered_metadata_by_pointers(pool, &pointers)
            .await
            .unwrap();
    assert!(
        std::str::from_utf8(&ordered[0].json)
            .unwrap()
            .ends_with(r#""metadata":{"a":2,"z":1}}"#)
    );
}
