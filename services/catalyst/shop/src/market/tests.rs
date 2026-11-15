//! Router-level contracts for the lambdas and trades routes, and the mirror's
//! pass/resume proof against an in-process fake upstream. Tests that need
//! rows use the process's scratch database (`db::scratch`, created on the
//! cluster `CATALYST_TEST_DATABASE_URL` names; a live node's database is
//! never touched), seed uuid-scoped rows and delete them; the trades mirror
//! tests also own the one-row `shop.trades_mirror` marker while they
//! run. Without the variable those tests skip.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::trades::{self, MirrorState};
use super::{MarketplaceState, Settings, SharedState, router};

pub(crate) mod support {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    fn lazy_pool() -> sqlx::PgPool {
        PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
            .unwrap()
    }

    /// A state over a pool that never connects: every route that does not
    /// touch Postgres works, the others answer 500.
    pub fn state_without_db() -> SharedState {
        Arc::new(MarketplaceState::new(
            lazy_pool(),
            crate::http::Outbound::for_tests(),
            Settings::for_tests(),
        ))
    }

    pub fn state_without_db_settings(settings: Settings) -> SharedState {
        Arc::new(MarketplaceState::new(
            lazy_pool(),
            crate::http::Outbound::for_tests(),
            settings,
        ))
    }

    /// `None` (skip) without CATALYST_TEST_DATABASE_URL; otherwise over the
    /// process's scratch database.
    pub async fn state_with_db(settings: Settings) -> Option<SharedState> {
        let pool = crate::db::scratch::pool(4).await?;
        Some(Arc::new(MarketplaceState::new(
            pool,
            crate::http::Outbound::for_tests(),
            settings,
        )))
    }

    /// A schema-applied database of its own, for tests that own singleton
    /// rows; `None` (skip) without CATALYST_TEST_DATABASE_URL.
    pub struct OwnDb {
        pub pool: sqlx::PgPool,
        admin: sqlx::PgPool,
        name: String,
    }

    impl OwnDb {
        pub async fn create(tag: &str) -> Option<Self> {
            use sqlx::postgres::PgConnectOptions;
            let base = std::env::var("CATALYST_TEST_DATABASE_URL").ok()?;
            let opts: PgConnectOptions = base.parse().unwrap();
            let admin = PgPoolOptions::new()
                .max_connections(1)
                .connect_with(opts.database("postgres"))
                .await
                .unwrap();
            let name = format!(
                "catalyst_z_{tag}_{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            );
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
                .execute(&admin)
                .await
                .unwrap();
            let mut url = url::Url::parse(&base).unwrap();
            url.set_path(&format!("/{name}"));
            let pool = crate::db::connect_pool(
                url.as_str(),
                &crate::db::PoolSettings {
                    max_connections: 4,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            crate::db::apply_schema(&pool).await.unwrap();
            Some(Self { pool, admin, name })
        }

        pub async fn drop(self) {
            self.pool.close().await;
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE \"{}\" WITH (FORCE)",
                self.name
            )))
            .execute(&self.admin)
            .await;
        }
    }

    /// `shop.mirror_state` rows are shared across the parallel tests
    /// that write them: they take turns.
    pub static MIRROR_STATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A GraphQL endpoint on the loopback answering every query with no
    /// rows: a configured reader that truly has nothing.
    pub async fn serve_empty_graph() -> String {
        use axum::routing::post;
        let app = Router::new().route("/", post(|| async { axum::Json(json!({ "data": {} })) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    /// Every reader configured and answering nothing: the state the
    /// unknown-address fixtures replay against.
    pub async fn state_with_empty_readers(pool: Option<sqlx::PgPool>) -> SharedState {
        let url = serve_empty_graph().await;
        let settings = Settings {
            eth_collections_subgraph_url: Some(url.clone()),
            matic_collections_subgraph_url: Some(url.clone()),
            marketplace_subgraph_url: Some(url),
            ..Settings::for_tests()
        };
        let state = Arc::new(MarketplaceState::new(
            pool.unwrap_or_else(lazy_pool),
            crate::http::Outbound::for_tests(),
            settings,
        ));
        state
            .test_allow_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        state
    }
}

use support::{state_with_db, state_without_db};

/// `shop.trades_mirror` is one row: the tests that write it take turns.
static MARKER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
struct Fixture {
    request: FixtureRequest,
    response: FixtureResponse,
    #[serde(default)]
    volatile_paths: Vec<String>,
    /// `"unspecified"`: the upstream answer has no stable order, so a
    /// top-level array is compared as a multiset.
    #[serde(default)]
    order: Option<String>,
}

#[derive(Deserialize)]
struct FixtureRequest {
    method: String,
    path: String,
    #[serde(default)]
    query: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<Value>,
    /// A body sent as these bytes, for one that is not JSON.
    #[serde(default)]
    body_raw: Option<String>,
}

#[derive(Deserialize)]
struct FixtureResponse {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body_json: Option<Value>,
}

/// Fixtures whose answer comes out of Postgres (an empty one): skipped without a database.
const DB_FIXTURES: &[&str] = &[
    "profiles-post-200-unknown-id",
    "profile-by-id-404-unknown-address",
    "profile-alias-200-unknown-address",
];

/// Fixtures whose empty page is a configured reader's answer for an unknown
/// address; without a reader the same routes answer the unavailable
/// envelope, so these replay against readers that answer nothing.
const READER_FIXTURES: &[&str] = &[
    "users-wearables-200-unknown-address",
    "users-emotes-200-unknown-address",
    "collections-wearables-by-owner-200-unknown",
];

fn fixture_files() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/market/fixtures");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    out.sort();
    assert!(out.len() >= 13, "fixture set shrank: {}", out.len());
    out
}

fn scrub(value: &mut Value, path: &str) {
    let path = path.strip_prefix("response.body_json.").unwrap_or(path);
    let mut cur = value;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            if let Some(obj) = cur.as_object_mut() {
                obj.remove(*part);
            }
            return;
        }
        match cur.get_mut(*part) {
            Some(next) => cur = next,
            None => return,
        }
    }
}

fn fixture_request(fx: &FixtureRequest) -> Request<Body> {
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
    if let Some(raw) = &fx.body_raw {
        return builder.body(Body::from(raw.clone())).unwrap();
    }
    match &fx.body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn replay_one(router: Router, fx: &Fixture) -> Vec<String> {
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
            (
                expected.split(';').next().unwrap_or(expected).trim(),
                actual.split(';').next().unwrap_or(actual).trim(),
            )
        } else {
            (expected.trim(), actual.trim())
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
    if let Some(expected_json) = &fx.response.body_json {
        let mut baseline = expected_json.clone();
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut candidate) => {
                for path in &fx.volatile_paths {
                    scrub(&mut baseline, path);
                    scrub(&mut candidate, path);
                }
                if fx.order.as_deref() == Some("unspecified") {
                    for v in [&mut baseline, &mut candidate] {
                        if let Some(items) = v.as_array_mut() {
                            items.sort_by_cached_key(|i| i.to_string());
                        }
                    }
                }
                if baseline != candidate {
                    notes.push(format!("body: fixture={baseline} candidate={candidate}"));
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
async fn lambdas_fixtures_replay_exactly() {
    let state = match state_with_db(Settings::for_tests()).await {
        Some(state) => state,
        None => state_without_db(),
    };
    let readers = support::state_with_empty_readers(Some(state.pool.clone())).await;
    let has_db = std::env::var("CATALYST_TEST_DATABASE_URL").is_ok();
    let mut failures = Vec::new();
    let mut replayed = 0;
    for path in fixture_files() {
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        if !has_db && DB_FIXTURES.contains(&stem.as_str()) {
            eprintln!("skipping {stem}: CATALYST_TEST_DATABASE_URL not set");
            continue;
        }
        let fx: Fixture = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let app = if READER_FIXTURES.contains(&stem.as_str()) {
            router(readers.clone())
        } else {
            router(state.clone())
        };
        let notes = replay_one(app, &fx).await;
        replayed += 1;
        if !notes.is_empty() {
            failures.push(format!("{stem}:\n    {}", notes.join("\n    ")));
        }
    }
    assert!(replayed >= 10);
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

#[tokio::test]
async fn status_carries_version_commit_and_time_and_lamb2_cors() {
    let state = state_without_db();
    let (status, body) = get_json(router(state.clone()), "/lambdas/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], "test");
    assert_eq!(body["commitHash"], "unknown");
    assert!(body["currentTime"].as_i64().unwrap() > 1_700_000_000_000);

    let resp = router(state)
        .oneshot(
            Request::get("/lambdas/status")
                .header(header::ORIGIN, "https://example.org")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );
}

#[tokio::test]
async fn profiles_reject_too_many_ids_and_malformed_json() {
    let state = state_without_db();
    let ids: Vec<String> = (0..=1000).map(|i| format!("0x{i}")).collect();
    let (status, body) = post_json(
        router(state.clone()),
        "/lambdas/profiles",
        json!({ "ids": ids }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Bad request");
    assert_eq!(
        body["message"],
        "Too many profile ids were specified. Maximum allowed is 1000"
    );

    let resp = router(state)
        .oneshot(
            Request::post("/lambdas/profiles")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    // lamb2's `request.json()` throws: its bare 500.
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

/// lamb2 takes any `:address` on the item routes: one that is not an
/// Ethereum address owns nothing, answered before any reader call, so a
/// configured subgraph never sees the string. The names route still refuses it.
#[tokio::test]
async fn user_item_routes_answer_a_malformed_address_empty_before_any_reader_call() {
    let settings = Settings {
        eth_collections_subgraph_url: Some("http://127.0.0.1:1/collections".into()),
        marketplace_subgraph_url: Some("http://127.0.0.1:1/marketplace".into()),
        ..Settings::for_tests()
    };
    let state = support::state_without_db_settings(settings);
    for path in [
        "/lambdas/users/not-an-address/wearables",
        "/lambdas/users/0xZZ/emotes?pageSize=10abc",
    ] {
        let (status, body) = get_json(router(state.clone()), path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body["elements"], json!([]), "{path}");
        assert_eq!(body["totalAmount"], 0, "{path}");
    }
    let (_, body) = get_json(
        router(state.clone()),
        "/lambdas/users/0xZZ/emotes?pageSize=10abc&pageNum=x",
    )
    .await;
    assert_eq!(
        (&body["pageSize"], &body["pageNum"]),
        (&json!(10), &Value::Null)
    );
    // lamb2's names answer an empty page for a string that owns nothing
    // (the unreachable reader would answer the 502 if it were asked).
    let (status, body) = get_json(
        router(state),
        "/lambdas/users/0x8d6f/names?pageSize=2&pageNum=3",
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({ "elements": [], "totalAmount": 0, "pageNum": 3, "pageSize": 2 })
        )
    );
}

#[tokio::test]
async fn contracts_servers_answer_empty_without_an_rpc_endpoint() {
    let (status, body) = get_json(router(state_without_db()), "/lambdas/contracts/servers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn a_configured_but_unreachable_reader_fails_closed() {
    let settings = Settings {
        eth_collections_subgraph_url: Some("http://127.0.0.1:1/subgraph".into()),
        marketplace_subgraph_url: Some("http://127.0.0.1:1/marketplace".into()),
        ..Settings::for_tests()
    };
    // The guard refuses the loopback URL before any socket opens: the route
    // answers 503 rather than an unverified inventory.
    let state = support::state_without_db_settings(settings);
    let (status, body) = get_json(
        router(state.clone()),
        "/lambdas/users/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef/wearables",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "Service Unavailable");
    // Names answer lamb2's 502 `FetcherError`, still never an empty page.
    let (status, body) = get_json(
        router(state.clone()),
        "/lambdas/users/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef/names",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(
        body["message"],
        "Cannot fetch elements for 0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    );
    let (status, _) = get_json(
        router(state),
        "/lambdas/collections/emotes-by-owner/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn by_owner_with_a_third_party_collection_validates_the_urn() {
    let state = state_without_db();
    let (status, body) = get_json(
        router(state.clone()),
        "/lambdas/collections/wearables-by-owner/0xabc?collectionId=urn:decentraland:matic:collections-thirdparty:cryptomotors",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([]),
        "no registry subgraph configured: no provider, nothing owned (the old empty answer)"
    );
    let (status, body) = get_json(
        router(state),
        "/lambdas/collections/wearables-by-owner/0xabc?collectionId=not-a-urn",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["message"],
        "'collectionId' must be a valid third-party collection URN"
    );
}

/// The explorer reads these lambdas: without a reader they answer the
/// envelope, never an empty wardrobe (review finding 8).
#[tokio::test]
async fn the_lambdas_answer_the_envelope_without_a_reader() {
    let state = state_without_db();
    for (uri, source) in [
        (
            "/lambdas/users/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef/wearables",
            "collections-subgraph:matic",
        ),
        (
            "/lambdas/users/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef/emotes",
            "collections-subgraph:matic",
        ),
        (
            "/lambdas/users/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef/names",
            "marketplace-subgraph",
        ),
        (
            "/lambdas/collections/wearables-by-owner/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "collections-subgraph:matic",
        ),
        (
            "/lambdas/collections/emotes-by-owner/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "collections-subgraph:matic",
        ),
    ] {
        let (status, body) = get_json(router(state.clone()), uri).await;
        assert_eq!(
            (status, body["source"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some(source)),
            "{uri}: {body}"
        );
    }
    let eth_only = support::state_without_db_settings(Settings {
        eth_collections_subgraph_url: Some("https://subgraph.example/eth".into()),
        ..Settings::for_tests()
    });
    let (status, body) = get_json(
        router(eth_only),
        "/lambdas/users/0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef/wearables",
    )
    .await;
    assert_eq!(
        (status, body["configure"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("MATIC_COLLECTIONS_SUBGRAPH_URL")
        ),
        "one network is not the wardrobe: {body}"
    );
}

#[tokio::test]
async fn trades_routes_reject_a_non_numeric_page_and_an_unparseable_id() {
    let state = state_without_db();
    let (status, body) = get_json(router(state.clone()), "/v1/trades?first=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["message"],
        "The value of the first parameter is invalid: abc"
    );
    let (status, body) = get_json(router(state), "/v1/trades/not-a-uuid").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["ok"], false);
}

mod settings {
    use super::super::{Settings, Upstreams, check_upstream};

    fn config(args: &[&str]) -> crate::config::Config {
        let mut argv = vec!["x", "--database-url", "postgres:///c"];
        argv.extend_from_slice(args);
        crate::config::Config::parse_for_tests(&argv).unwrap()
    }

    #[test]
    fn upstreams_must_be_public_and_not_this_node() {
        let hosts = vec!["node.example".to_string()];
        assert!(check_upstream("X", "https://api.example/v1/trades", &hosts).is_ok());
        let err = check_upstream(
            "TRADES_SYNC_UPSTREAM_URL",
            "https://node.example/v1/trades",
            &hosts,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("own host node.example"), "{err}");
        let err = check_upstream(
            "MARKETPLACE_SUBGRAPH_URL",
            "http://127.0.0.1:8000/subgraphs",
            &hosts,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with("MARKETPLACE_SUBGRAPH_URL is not a publicly routable"),
            "{err}"
        );
        assert!(
            !err.contains("127.0.0.1"),
            "the refusal never echoes the value: {err}"
        );
        assert!(check_upstream("X", "ftp://api.example/", &hosts).is_err());
        assert!(check_upstream("X", "https://gateway.internal/", &hosts).is_err());
    }

    #[test]
    fn settings_derive_from_config_and_refuse_a_self_referential_mirror() {
        let cfg = config(&[
            "--public-url",
            "https://node.example",
            "--commit-hash",
            "abc",
        ]);
        let up = Upstreams {
            trades_sync_upstream_url: Some(" https://marketplace-api.example/v1/trades/ ".into()),
            trades_sync_interval_secs: 10,
            eth_collections_subgraph_url: Some(String::new()),
            ..Default::default()
        };
        let s = Settings::from_config(&cfg, &up).unwrap();
        assert_eq!(
            s.trades_sync_upstream_url.as_deref(),
            Some("https://marketplace-api.example/v1/trades")
        );
        assert_eq!(
            s.trades_sync_interval.as_secs(),
            60,
            "the interval floors at a minute"
        );
        assert_eq!(s.eth_collections_subgraph_url, None, "blank means unset");
        assert_eq!(s.commit_hash, "abc");
        assert_eq!(s.content_public_url, "https://node.example/content/");
        assert!(s.eth_rpc_url.is_none());

        let up = Upstreams {
            trades_sync_upstream_url: Some("https://node.example/v1/trades".into()),
            ..Default::default()
        };
        let err = Settings::from_config(&cfg, &up).unwrap_err().to_string();
        assert!(
            err.contains("TRADES_SYNC_UPSTREAM_URL points at this node's own host"),
            "{err}"
        );

        let up = Upstreams {
            marketplace_subgraph_url: Some("http://127.0.0.1:5210/lambdas".into()),
            ..Default::default()
        };
        let cfg = config(&[]);
        assert!(
            Settings::from_config(&cfg, &up).is_err(),
            "a loopback upstream is refused even when it is not our port"
        );
        let s = Settings::from_config(&cfg, &Upstreams::default()).unwrap();
        assert_eq!(s.commit_hash, "unknown");
        assert_eq!(s.trades_sync_interval.as_secs(), 60);
    }

    #[test]
    fn the_third_party_registry_is_a_public_https_upstream() {
        let registry = |url: &str| {
            let cfg = config(&[
                "--public-url",
                "https://node.example",
                "--third-party-registry-l2-subgraph-url",
                url,
            ]);
            Settings::from_config(&cfg, &Upstreams::default()).map_err(|e| e.to_string())
        };
        assert_eq!(
            registry("https://subgraph.example/tp/")
                .unwrap()
                .third_party_registry_subgraph_url
                .as_deref(),
            Some("https://subgraph.example/tp")
        );
        for (url, needle) in [
            ("http://subgraph.example/tp", "not https"),
            ("https://127.0.0.1/tp", "publicly routable"),
            ("https://node.example/tp", "own host"),
        ] {
            let err = registry(url).unwrap_err();
            assert!(
                err.contains("THIRD_PARTY_REGISTRY_L2_SUBGRAPH_URL") && err.contains(needle),
                "{url}: {err}"
            );
        }
    }
}

fn tag() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn seed_profile(pool: &sqlx::PgPool, address: &str, entity_id: &str, metadata: Value) {
    sqlx::query(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, \
             entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
         VALUES ($1, 'v3', 'profile', $2, $3, to_timestamp(1700000000), ARRAY[$1], timezone('UTC', now()), '[]'::json)",
    )
    .bind(address)
    .bind(entity_id)
    .bind(json!({ "v": metadata }))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'profile')")
        .bind(address)
        .bind(entity_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn unseed_profile(pool: &sqlx::PgPool, address: &str, entity_id: &str) {
    sqlx::query("DELETE FROM content.active_pointers WHERE pointer = $1")
        .bind(address)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.deployments WHERE entity_id = $1")
        .bind(entity_id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn profiles_metadata_validates_ids_as_asset_bundle_registry() {
    let state = state_without_db();
    let msg = "The ids property must be an array of non-empty strings";
    for body in [
        json!({}),
        json!({ "ids": "0xabc" }),
        json!({ "ids": ["0xabc", ""] }),
        json!({ "ids": [1] }),
    ] {
        let (status, resp) =
            post_json(router(state.clone()), "/lambdas/profiles/metadata", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp, json!({ "error": "Bad request", "message": msg }));
    }
    let mut ids: Vec<Value> = (0..30_000).map(|i| json!(format!("a{i}"))).collect();
    ids.push(Value::Null);
    assert!(json!({"ids": ids}).to_string().len() < 256 * 1024);
    let (status, _) = post_json(
        router(state.clone()),
        "/lambdas/profiles/metadata",
        json!({"ids": ids}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, resp) = post_json(
        router(state.clone()),
        "/lambdas/profiles/metadata",
        json!({ "ids": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp, json!([]));
}

#[tokio::test]
async fn profiles_metadata_takes_any_number_of_ids_and_answers_bad_json_with_a_bare_500() {
    let state = state_without_db();
    let resp = router(state.clone())
        .oneshot(
            Request::post("/lambdas/profiles/metadata")
                .header("content-type", "application/json")
                .body(Body::from("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({ "error": "Internal Server Error" })
    );
    let ids: Vec<String> = (0..=1000).map(|i| format!("0x{i:040x}")).collect();
    let (status, body) = post_json(
        router(state),
        "/lambdas/profiles/metadata",
        json!({ "ids": ids }),
    )
    .await;
    // No cap, as asset-bundle-registry (`AB-3-1001-ids.json`); without a
    // database nothing is found.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

/// Both paths, the per-id cache (50 ids or fewer) and the batch read, answer
/// a failed database read with asset-bundle-registry's 200 and what was found.
#[tokio::test]
async fn profiles_metadata_answers_a_failed_read_with_what_was_found_on_both_paths() {
    let state = state_without_db();
    for n in [3, 60] {
        let ids: Vec<String> = (0..n).map(|i| format!("0x{i:040x}")).collect();
        let (status, body) = post_json(
            router(state.clone()),
            "/lambdas/profiles/metadata",
            json!({ "ids": ids }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{n} ids");
        assert_eq!(body, json!([]), "{n} ids");
    }
}

#[tokio::test]
async fn a_synced_profile_is_served_shaped_and_cached() {
    let Some(state) = state_with_db(Settings::for_tests()).await else {
        return;
    };
    let t = tag();
    let address = format!("0x{}00000000", &t[..32]);
    let entity_id = format!("bafyprofile{t}");
    seed_profile(
        &state.pool,
        &address,
        &entity_id,
        json!({ "timestamp": 5, "extra": true, "avatars": [{
            "name": "tester",
            "hasClaimedName": true,
            "links": [{ "title": "ok", "url": "https://decentraland.org/" }, { "title": "bad", "url": "javascript:x" }],
            "avatar": {
                "wearables": ["urn:decentraland:off-chain:base-avatars:eyebrows_00", "urn:decentraland:matic:collections-v2:0xabc:1"],
                "snapshots": { "face256": "bafyface", "body": "bafybody" }
            }
        }] }),
    )
    .await;

    let (status, body) = get_json(
        router(state.clone()),
        &format!("/lambdas/profiles/{}", address.to_uppercase()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let avatar = &body["avatars"][0];
    assert_eq!(avatar["userId"], address);
    assert_eq!(avatar["ethAddress"], address);
    let mut keys: Vec<_> = body.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["avatars", "timestamp"],
        "lamb2 answers {{ timestamp, avatars }} only: {body}"
    );
    assert_eq!(
        body["timestamp"], 1_700_000_000_000_i64,
        "the entity's timestamp, not the deployed one"
    );
    assert_eq!(avatar["avatar"]["emotes"], json!([]));
    assert_eq!(avatar["links"].as_array().unwrap().len(), 1);
    assert_eq!(
        avatar["avatar"]["snapshots"]["face256"],
        format!("http://localhost/profile-images/entities/{entity_id}/face.png")
    );
    assert_eq!(
        avatar["avatar"]["wearables"].as_array().unwrap().len(),
        2,
        "without a configured reader the inventory is served unfiltered, as the old squid-less path did"
    );
    assert_eq!(
        avatar["hasClaimedName"], false,
        "no name is proven owned without a names reader"
    );

    let (status, batch) = post_json(
        router(state.clone()),
        "/lambdas/profiles",
        json!({ "ids": [address, address] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch.as_array().unwrap().len(), 1, "duplicate ids collapse");

    // If-Modified-Since: ISO 8601 as lamb2's `Date.parse` reads it, the
    // entity time rounded down to the second; an unknown id alone is 304.
    for (ims, ids, expected) in [
        (
            "2023-11-14T22:13:20.000Z",
            json!([address]),
            StatusCode::NOT_MODIFIED,
        ),
        (
            "Tue, 14 Nov 2023 22:13:20 GMT",
            json!([address]),
            StatusCode::NOT_MODIFIED,
        ),
        ("2023-11-14T22:13:19.000Z", json!([address]), StatusCode::OK),
        (
            "2000-01-01T00:00:00.000Z",
            json!(["0x00000000000000000000000000000000000000ff"]),
            StatusCode::NOT_MODIFIED,
        ),
    ] {
        let resp = router(state.clone())
            .oneshot(
                Request::post("/lambdas/profiles")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::IF_MODIFIED_SINCE, ims)
                    .body(Body::from(json!({ "ids": ids }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), expected, "{ims} {ids}");
    }

    // The 10 s cache answers after the row is gone.
    unseed_profile(&state.pool, &address, &entity_id).await;
    let (status, _) = get_json(
        router(state.clone()),
        &format!("/lambdas/profiles/{address}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    state.profile_cache.clear();
    let (status, body) = get_json(
        router(state.clone()),
        &format!("/lambdas/profiles/{address}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["message"], "Profile not found");
    let (status, body) = get_json(router(state), &format!("/lambdas/profile/{address}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "avatars": [], "timestamp": 0 }));
}

/// A `default*` pointer is served from its deployed entity (lamb2 has no
/// built-in default): nothing is owned, so only base items stay, no name is
/// claimed and the deployed identity is kept. Undeployed, it is 404 (the
/// stub on the alias).
#[tokio::test]
async fn default_profiles_are_the_deployed_entities_owning_nothing() {
    let Some(state) = state_with_db(Settings::for_tests()).await else {
        return;
    };
    let pointer = format!("default{}", u64::from_str_radix(&tag()[..12], 16).unwrap());
    let entity_id = format!("bafydefault{}", tag());
    seed_profile(
        &state.pool,
        &pointer,
        &entity_id,
        json!({ "avatars": [{
            "name": "Default",
            "userId": "deployed",
            "hasClaimedName": true,
            "avatar": {
                "wearables": ["urn:decentraland:off-chain:base-avatars:eyebrows_00", "urn:decentraland:matic:collections-v2:0xabc:1"],
                "emotes": [{ "slot": 0, "urn": "wave" }, { "slot": 1, "urn": "urn:decentraland:matic:collections-v2:0xabc:2" }]
            }
        }] }),
    )
    .await;
    let (status, body) = get_json(
        router(state.clone()),
        &format!("/lambdas/profiles/{pointer}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let avatar = &body["avatars"][0];
    assert_eq!(avatar["userId"], "deployed");
    assert_eq!(avatar["hasClaimedName"], false);
    assert_eq!(
        avatar["avatar"]["wearables"],
        json!(["urn:decentraland:off-chain:base-avatars:eyebrows_00"])
    );
    assert_eq!(
        avatar["avatar"]["emotes"],
        json!([{ "slot": 0, "urn": "wave" }])
    );
    unseed_profile(&state.pool, &pointer, &entity_id).await;
    state.profile_cache.clear();
    let (status, _) = get_json(router(state.clone()), "/lambdas/profiles/default99999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = get_json(router(state), "/lambdas/profile/default99999999").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "avatars": [], "timestamp": 0 }));
}

#[tokio::test]
async fn compact_profiles_agree_with_the_full_answer() {
    let Some(state) = state_with_db(Settings::for_tests()).await else {
        return;
    };
    let t = tag();
    let address = format!("0x{}00000001", &t[..32]);
    let unknown = format!("0x{}00000002", &t[..32]);
    let entity_id = format!("bafymeta{t}");
    seed_profile(
        &state.pool,
        &address,
        &entity_id,
        json!({ "avatars": [{
            "name": "tester",
            "avatar": { "snapshots": { "face256": "bafyface", "body": "bafybody" } }
        }] }),
    )
    .await;

    let ids = json!({ "ids": [address.to_uppercase(), unknown, address] });
    let (status, full) = post_json(router(state.clone()), "/lambdas/profiles", ids.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let face = full[0]["avatars"][0]["avatar"]["snapshots"]["face256"].clone();
    let claimed = full[0]["avatars"][0]["hasClaimedName"].clone();
    let (status, compact) =
        post_json(router(state.clone()), "/lambdas/profiles/metadata", ids).await;
    unseed_profile(&state.pool, &address, &entity_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        compact,
        json!([{
            "pointer": address, "name": "tester", "hasClaimedName": claimed, "thumbnailUrl": face
        }])
    );
}

/// Above 50 ids the batch read serves the same compact answer, once per
/// pointer whatever its case.
#[tokio::test]
async fn compact_profiles_above_the_cache_batch() {
    let Some(state) = state_with_db(Settings::for_tests()).await else {
        return;
    };
    let t = tag();
    let address = format!("0x{}00000003", &t[..32]);
    let entity_id = format!("bafymetabatch{t}");
    seed_profile(
        &state.pool,
        &address,
        &entity_id,
        json!({ "avatars": [{
            "name": "batched",
            "avatar": { "snapshots": { "face256": "bafyface", "body": "bafybody" } }
        }] }),
    )
    .await;
    let mut ids: Vec<String> = (0..60).map(|i| format!("0x{}{i:08x}", &t[..32])).collect();
    ids[30] = address.to_uppercase();
    ids.push(address.clone());

    let (status, compact) = post_json(
        router(state.clone()),
        "/lambdas/profiles/metadata",
        json!({ "ids": ids }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{compact}");
    assert_eq!(compact.as_array().unwrap().len(), 1, "{compact}");
    assert_eq!(compact[0]["pointer"], address);
    assert_eq!(compact[0]["name"], "batched");

    unseed_profile(&state.pool, &address, &entity_id).await;
}

fn head(t: &str, i: u64, created_ms: i64) -> Value {
    let mut row = trades::fixtures::list_row();
    row["id"] = json!(uuid::Uuid::new_v4().to_string());
    row["signature"] = json!(format!("0x{:0>130}", format!("{t}{i:x}")));
    row["hashed_signature"] = json!(format!("0x{:0>64}", format!("{}{i:x}", &t[..24])));
    row["created_at"] = json!(
        chrono::DateTime::from_timestamp_millis(created_ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    row
}

async fn delete_trades_with_prefix(pool: &sqlx::PgPool, t: &str) {
    sqlx::query("DELETE FROM shop.trades WHERE hashed_signature LIKE $1")
        .bind(format!("0x%{}%", &t[..24]))
        .execute(pool)
        .await
        .unwrap();
}

/// Trades are money data: once the last complete pass is older than the
/// budget (four 15 min intervals), the trade reads answer the envelope
/// naming the age instead of a stale order book, and serve again after a
/// fresh pass.
#[tokio::test]
async fn a_stale_trades_mirror_answers_503_past_its_budget() {
    let upstream = "https://marketplace-api.example/v1/trades";
    let Some(state) = state_with_db(Settings {
        trades_sync_upstream_url: Some(upstream.to_string()),
        ..Settings::for_tests()
    })
    .await
    else {
        return;
    };
    let _marker = MARKER_LOCK.lock().await;
    let mark = |hours_ago: i64| {
        sqlx::query(
            "INSERT INTO shop.trades_mirror (id, upstream_url, pass, completed_at) \
             VALUES (1, $1, 1, timezone('UTC', now()) - make_interval(hours => $2::int)) \
             ON CONFLICT (id) DO UPDATE SET upstream_url = EXCLUDED.upstream_url, completed_at = EXCLUDED.completed_at",
        )
        .bind(upstream)
        .bind(hours_ago as i32)
        .execute(&state.pool)
    };
    mark(2).await.unwrap();
    state.trades_ready.clear();
    let (status, body) = get_json(router(state.clone()), "/v1/trades?first=1").await;
    assert_eq!(
        (status, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("trades-mirror")),
        "{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("freshness budget")),
        "{body}"
    );
    mark(0).await.unwrap();
    state.trades_ready.clear();
    let (status, body) = get_json(router(state.clone()), "/v1/trades?first=1").await;
    assert_eq!(status, StatusCode::OK, "a fresh pass serves: {body}");
    // The marker is one shared row: leave none for the next holder of the lock.
    sqlx::query("DELETE FROM shop.trades_mirror")
        .execute(&state.pool)
        .await
        .unwrap();
}

/// The catalog copy: a route over the mirrored tables answers stamped with
/// the copy's `as_of` (the collections one keeps serving however old), while
/// the routes that carry item prices follow the money rule and answer the
/// envelope once the items copy is past its budget, whether or not this
/// node runs the mirror job: a frozen copy's prices are as old.
#[tokio::test]
async fn catalog_reads_carry_as_of_and_prices_answer_503_past_their_budget() {
    let settings = |mirrors_enabled: bool| {
        let mut settings = Settings {
            matic_collections_subgraph_url: Some("https://subgraph.example/matic".into()),
            ..Settings::for_tests()
        };
        settings.market.mirrors_enabled = mirrors_enabled;
        settings
    };
    let Some(state) = state_with_db(settings(true)).await else {
        return;
    };
    let _rows = support::MIRROR_STATE_LOCK.lock().await;
    let mark = |mirror: &'static str, minutes_ago: i32| {
        sqlx::query(
            "INSERT INTO shop.mirror_state (mirror, network, source_url, last_complete_pass_at) \
             VALUES ($1, 'matic', 'https://subgraph.example/matic', \
                     date_trunc('second', timezone('UTC', now())) - make_interval(mins => $2)) \
             ON CONFLICT (mirror, network) DO UPDATE SET last_complete_pass_at = EXCLUDED.last_complete_pass_at",
        )
        .bind(mirror)
        .bind(minutes_ago)
        .execute(&state.pool)
    };
    let marker = |mirror: &'static str| {
        let pool = state.pool.clone();
        async move {
            let at: chrono::NaiveDateTime = sqlx::query_scalar(
                "SELECT last_complete_pass_at FROM shop.mirror_state WHERE mirror = $1 AND network = 'matic'",
            )
            .bind(mirror)
            .fetch_one(&pool)
            .await
            .unwrap();
            at.and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }
    };
    let app = || super::market_router(state.clone());

    mark("collections", 300).await.unwrap();
    mark("items", 5).await.unwrap();
    state.mirror_ready.clear();
    let interval = state.settings.market.mirror_interval.as_secs();
    let (status, body) = get_json(app(), "/v1/catalog?first=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["as_of"], marker("items").await.as_str());
    assert_eq!(
        body["stale_after_secs"],
        2 * interval,
        "two items intervals"
    );
    let (status, body) = get_json(app(), "/v1/collections?first=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["as_of"],
        marker("collections").await.as_str(),
        "five hours old: descriptive, still served"
    );
    assert_eq!(
        body["stale_after_secs"],
        2 * 3 * interval,
        "two collections walks"
    );
    for uri in ["/v1/prices?category=wearable", "/v1/trendings"] {
        let (status, body) = get_json(app(), uri).await;
        assert_eq!(
            (status, body["source"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("trades-mirror")),
            "{uri} reads the trade legs too, and this node has no trades source: {body}"
        );
    }

    mark("items", 120).await.unwrap();
    state.mirror_ready.clear();
    for uri in [
        "/v1/catalog?first=1",
        "/v2/catalog?first=1",
        "/v1/prices?category=wearable",
    ] {
        let (status, body) = get_json(app(), uri).await;
        assert_eq!(
            (status, body["source"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("items-mirror")),
            "{uri}: {body}"
        );
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains("freshness budget")),
            "{uri}: {body}"
        );
    }
    let (status, body) = get_json(app(), "/v1/collections?first=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["as_of"].is_string(), "{body}");
    let frozen = state_with_db(settings(false)).await.unwrap();
    let (status, body) =
        get_json(super::market_router(frozen.clone()), "/v1/catalog?first=1").await;
    assert_eq!(
        (status, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("items-mirror")),
        "the job off, the copy past its budget: {body}"
    );
    mark("items", 5).await.unwrap();
    frozen.mirror_ready.clear();
    let (status, body) = get_json(super::market_router(frozen), "/v1/catalog?first=1").await;
    assert_eq!(status, StatusCode::OK, "the job off, a fresh copy: {body}");
    assert_eq!(body["as_of"], marker("items").await.as_str());

    sqlx::query("DELETE FROM shop.mirror_state WHERE mirror IN ('collections', 'items') AND network = 'matic'")
        .execute(&state.pool)
        .await
        .unwrap();
}

/// With no collections subgraph configured, a descriptive read serves the
/// copy the tables hold (the networks `shop.mirror_state` records) as of
/// its oldest pass, and answers 503 only when no pass ever completed; a
/// priced read still needs the configured networks.
#[tokio::test]
async fn without_a_subgraph_descriptive_reads_serve_the_recorded_copy() {
    const DESCRIPTIVE: [&str; 2] = [
        "/v1/collections?first=1",
        "/v1/contracts?category=wearable&network=MATIC",
    ];
    let Some(state) = state_with_db(Settings::for_tests()).await else {
        return;
    };
    assert!(state.settings.collection_networks().is_empty());
    let _rows = support::MIRROR_STATE_LOCK.lock().await;
    let pool = &state.pool;
    sqlx::query("DELETE FROM shop.mirror_state WHERE mirror IN ('collections', 'items')")
        .execute(pool)
        .await
        .unwrap();
    let app = || super::market_router(state.clone());
    state.mirror_ready.clear();
    for uri in DESCRIPTIVE {
        let (status, body) = get_json(app(), uri).await;
        assert_eq!(
            (status, body["source"].as_str()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("collections-subgraph:matic")
            ),
            "no copy ever: {uri}: {body}"
        );
    }

    sqlx::query(
        "INSERT INTO shop.mirror_state (mirror, network, source_url, last_complete_pass_at) VALUES \
           ('collections', 'ethereum', 'https://subgraph.example/eth', '2026-01-02T03:04:05'), \
           ('collections', 'matic', 'https://subgraph.example/matic', '2026-01-03T00:00:00'), \
           ('items', 'matic', 'https://subgraph.example/matic', timezone('UTC', now()))",
    )
    .execute(pool)
    .await
    .unwrap();
    state.mirror_ready.clear();
    for uri in DESCRIPTIVE {
        let (status, body) = get_json(app(), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(
            body["as_of"], "2026-01-02T03:04:05.000Z",
            "the oldest recorded pass: {uri}"
        );
    }
    let (status, body) = get_json(app(), "/v1/catalog?first=1").await;
    assert_eq!(
        (status, body["source"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("collections-subgraph:matic")
        ),
        "prices need the configured networks: {body}"
    );

    sqlx::query("DELETE FROM shop.mirror_state WHERE mirror IN ('collections', 'items')")
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn mirrored_rows_never_overwrite_a_local_trade_and_the_routes_serve_them() {
    let upstream = "https://marketplace-api.example/v1/trades";
    let Some(state) = state_with_db(Settings {
        trades_sync_upstream_url: Some(upstream.to_string()),
        ..Settings::for_tests()
    })
    .await
    else {
        return;
    };
    let _marker = MARKER_LOCK.lock().await;
    let t = tag();
    let pool = &state.pool;
    sqlx::query("DELETE FROM shop.trades_mirror")
        .execute(pool)
        .await
        .unwrap();
    // The read routes need one complete pass of this upstream (the marker,
    // not the rows, says so); a marker of another upstream does not count.
    let (status, body) = get_json(router(state.clone()), "/v1/trades?first=1").await;
    assert_eq!(
        (status, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("trades-mirror")),
        "{body}"
    );
    sqlx::query("INSERT INTO shop.trades_mirror (id, upstream_url, pass, completed_at) VALUES (1, 'https://other.example/v1/trades', 1, timezone('UTC', now())) ON CONFLICT (id) DO UPDATE SET upstream_url = EXCLUDED.upstream_url, completed_at = EXCLUDED.completed_at")
        .execute(pool)
        .await
        .unwrap();
    state.trades_ready.clear();
    let (status, _) = get_json(router(state.clone()), "/v1/trades?first=1").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "another upstream's pass is not this one's"
    );
    sqlx::query("UPDATE shop.trades_mirror SET upstream_url = $1 WHERE id = 1")
        .bind(upstream)
        .execute(pool)
        .await
        .unwrap();
    state.trades_ready.clear();
    let local_only = Arc::new(MarketplaceState::new(
        pool.clone(),
        crate::http::Outbound::for_tests(),
        Settings {
            data_writes_enabled: true,
            ..Settings::for_tests()
        },
    ));
    assert_eq!(
        trades::ready(&local_only).await.unwrap(),
        trades::Readiness::LocalAuthority,
        "no upstream + writes on: the node's own trades are its truth"
    );
    let no_source = state_without_db();
    let err = trades::ready(&no_source).await.unwrap_err();
    assert_eq!(
        err.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no upstream + writes off: no source"
    );

    let mirrored = trades::parse_list_row(&head(&t, 1, 1_733_691_667_473)).unwrap();
    let assets = trades::parse_detail_assets(&trades::fixtures::detail()).unwrap();
    assert!(
        trades::insert_trade(pool, &mirrored, &assets, upstream)
            .await
            .unwrap()
    );
    assert!(
        !trades::insert_trade(pool, &mirrored, &assets, upstream)
            .await
            .unwrap(),
        "the same signature is not inserted twice"
    );

    // A trade signed into this node (mirrored_from NULL) with the signature
    // the upstream later lists: the mirror leaves it alone.
    let local = trades::parse_list_row(&head(&t, 2, 1_733_691_667_474)).unwrap();
    sqlx::query(
        "INSERT INTO shop.trades (id, network, chain_id, signature, hashed_signature, checks, signer, type, \
             expires_at, effective_since, contract, created_at) \
         VALUES ($1::uuid, 'MATIC', 137, $2, $3, '{}'::jsonb, $4, 'bid', $5, $5, 'local-contract', $5)",
    )
    .bind(&local.id)
    .bind(&local.signature)
    .bind(&local.hashed_signature)
    .bind(&local.signer)
    .bind(local.created_at.naive_utc())
    .execute(pool)
    .await
    .unwrap();
    assert!(
        !trades::insert_trade(pool, &local, &assets, upstream)
            .await
            .unwrap()
    );
    let (kind, from): (String, Option<String>) =
        sqlx::query_as("SELECT type::text, mirrored_from FROM shop.trades WHERE id = $1::uuid")
            .bind(&local.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        (kind.as_str(), from),
        ("bid", None),
        "the local row keeps its own values and stays local"
    );

    let (status, body) = get_json(router(state.clone()), "/v1/trades?first=1000").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    let rows = body["data"]["data"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|r| r["id"] == mirrored.id)
        .expect("the mirrored trade is listed");
    assert_eq!(row["created_at"], "2024-12-08T21:01:07.473Z");
    assert_eq!(row["expires_at"], "2025-01-06T23:00:00.000Z");
    assert_eq!(row["type"], "public_nft_order");
    assert_eq!(row["mirrored_from"], upstream);
    assert!(
        rows.iter()
            .any(|r| r["id"] == local.id && r.get("mirrored_from").is_none())
    );
    assert!(body["data"]["count"].as_i64().unwrap() >= 2);

    let (status, body) = get_json(
        router(state.clone()),
        &format!("/v1/trades/{}", mirrored.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let trade = &body["data"];
    assert_eq!(trade["chainId"], 137);
    assert_eq!(trade["createdAt"], 1_733_691_667_473_i64);
    assert_eq!(
        trade["signer"],
        "0x747c6f502272129bf1ba872a1903045b837ee86c"
    );
    assert_eq!(trade["mirroredFrom"], upstream);
    assert_eq!(
        trade["sent"],
        json!([{ "assetType": 3, "contractAddress": "0x659704bdc5152b0348a7b526d5120cab98ee6f7f", "extra": "", "tokenId": "450" }])
    );
    assert_eq!(
        trade["received"],
        json!([{ "assetType": 1, "contractAddress": "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4", "extra": "", "amount": "150000000000000000000", "beneficiary": "0x747c6f502272129bf1ba872a1903045b837ee86c" }])
    );
    assert!(
        trade.get("status").is_none(),
        "no mv_trades in v2: no status"
    );

    let (status, body) = get_json(
        router(state.clone()),
        &format!("/v1/trades/{}", uuid::Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["ok"], false);

    sqlx::query("DELETE FROM shop.trades_mirror")
        .execute(pool)
        .await
        .unwrap();
    delete_trades_with_prefix(pool, &t).await;
}

/// `TRADES_SERVE_PARTIAL` (development only) opens the trade reads before the
/// mirror's first complete pass, without reading the pass marker at all.
#[tokio::test]
async fn a_development_stack_reads_a_partial_order_book_only_when_asked() {
    let settings = Settings {
        trades_sync_upstream_url: Some("https://marketplace-api.example/v1/trades".into()),
        trades_serve_partial: true,
        ..Settings::for_tests()
    };
    let state = support::state_without_db_settings(settings);
    assert_eq!(
        trades::ready(&state).await.unwrap(),
        trades::Readiness::Mirrored
    );
    assert!(
        !Settings::for_tests().trades_serve_partial,
        "off unless asked"
    );
}

/// After a mirror posture ends (no upstream, the node's own trades its
/// truth), the rows that mirror left count only within the trades budget
/// of their copy: the order book, `/v1/trades` and `/v1/trades/{id}` drop
/// a leftover past it, keep a fresh one, and always keep a local trade.
#[tokio::test]
async fn a_leftover_mirrored_trade_counts_under_local_authority_only_within_the_budget() {
    let settings = Settings {
        data_writes_enabled: true,
        ..Settings::for_tests()
    };
    let Some(state) = state_with_db(settings.clone()).await else {
        return;
    };
    assert_eq!(
        trades::ready(&state).await.unwrap(),
        trades::Readiness::LocalAuthority
    );
    let pool = &state.pool;
    let t = tag();
    let former = "https://former-upstream.example/v1/trades";
    let assets = trades::parse_detail_assets(&trades::fixtures::detail()).unwrap();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut ids = Vec::new();
    for i in 1..=3 {
        let row = trades::parse_list_row(&head(&t, i, now_ms - i as i64)).unwrap();
        assert!(
            trades::insert_trade(pool, &row, &assets, former)
                .await
                .unwrap()
        );
        ids.push(row.id);
    }
    let (fresh, stale, local) = (&ids[0], &ids[1], &ids[2]);
    let budget = trades::max_age(settings.trades_sync_interval).as_secs() as i32;
    sqlx::query(
        "UPDATE shop.trades SET expires_at = timezone('UTC', now()) + interval '1 day', \
                effective_since = timezone('UTC', now()) - interval '1 day', \
                mirrored_at = CASE WHEN id = $2::uuid THEN timezone('UTC', now()) - make_interval(secs => $4 + 60) ELSE mirrored_at END, \
                mirrored_from = CASE WHEN id = $3::uuid THEN NULL ELSE mirrored_from END \
         WHERE id IN ($1::uuid, $2::uuid, $3::uuid)",
    )
    .bind(fresh)
    .bind(stale)
    .bind(local)
    .bind(budget)
    .execute(pool)
    .await
    .unwrap();

    let (status, body) = get_json(router(state.clone()), "/v1/trades?first=1000").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<&str> = body["data"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert!(
        listed.contains(&fresh.as_str()) && listed.contains(&local.as_str()),
        "{listed:?}"
    );
    assert!(
        !listed.contains(&stale.as_str()),
        "a leftover past the budget: {listed:?}"
    );
    for (id, want) in [
        (fresh, StatusCode::OK),
        (stale, StatusCode::NOT_FOUND),
        (local, StatusCode::OK),
    ] {
        let (status, body) = get_json(router(state.clone()), &format!("/v1/trades/{id}")).await;
        assert_eq!(status, want, "{id}: {body}");
    }

    let book = |since: Option<chrono::NaiveDateTime>| {
        let ids = ids.clone();
        async move {
            let mut open: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "WITH {} SELECT id::text FROM unified_trades WHERE id::text = ANY($3)",
                super::catalog::unified_trades_cte("$1", "$2")
            )))
            .bind(chrono::Utc::now().naive_utc())
            .bind(since)
            .bind(&ids)
            .fetch_all(pool)
            .await
            .unwrap();
            open.sort();
            open
        }
    };
    let now = chrono::Utc::now().naive_utc();
    let mut want = vec![fresh.clone(), local.clone()];
    want.sort();
    assert_eq!(
        book(trades::mirrored_since(&settings, now)).await,
        want,
        "the order book under local authority"
    );
    let mut all = ids.clone();
    all.sort();
    let mirroring = Settings {
        trades_sync_upstream_url: Some(former.to_string()),
        ..settings.clone()
    };
    assert_eq!(
        trades::mirrored_since(&mirroring, now),
        None,
        "with an upstream its pass marker is the gate"
    );
    assert_eq!(book(None).await, all);

    delete_trades_with_prefix(pool, &t).await;
}

/// A fake marketplace API: `count` heads oldest first, paged by
/// `first`/`skip` (or, with `ignore_paging`, the whole list to every request
/// as the production upstream does), and each one's detail. `fail_at_skip`
/// makes that page answer 500 until it is cleared; every list request is
/// recorded.
struct FakeUpstream {
    heads: Vec<Value>,
    fail_at_skip: std::sync::Mutex<Option<i64>>,
    requested_skips: std::sync::Mutex<Vec<i64>>,
    ignore_paging: bool,
}

impl FakeUpstream {
    fn new(heads: Vec<Value>, fail_at_skip: Option<i64>, ignore_paging: bool) -> Arc<Self> {
        Arc::new(Self {
            heads,
            fail_at_skip: std::sync::Mutex::new(fail_at_skip),
            requested_skips: std::sync::Mutex::new(Vec::new()),
            ignore_paging,
        })
    }

    fn take_skips(&self) -> Vec<i64> {
        std::mem::take(&mut *self.requested_skips.lock().unwrap())
    }
}

async fn serve_fake(up: Arc<FakeUpstream>) -> String {
    use axum::extract::{Path, Query, State};
    use axum::routing::get;
    async fn list(
        State(up): State<Arc<FakeUpstream>>,
        Query(q): Query<BTreeMap<String, String>>,
    ) -> axum::response::Response {
        let first: usize = q.get("first").and_then(|v| v.parse().ok()).unwrap_or(100);
        let skip: usize = q.get("skip").and_then(|v| v.parse().ok()).unwrap_or(0);
        up.requested_skips.lock().unwrap().push(skip as i64);
        if *up.fail_at_skip.lock().unwrap() == Some(skip as i64) {
            return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
        let page: Vec<Value> = if up.ignore_paging {
            up.heads.clone()
        } else {
            up.heads.iter().skip(skip).take(first).cloned().collect()
        };
        axum::Json(json!({ "ok": true, "data": { "data": page, "count": up.heads.len() } }))
            .into_response()
    }
    async fn detail(
        State(up): State<Arc<FakeUpstream>>,
        Path(id): Path<String>,
    ) -> axum::response::Response {
        match up.heads.iter().find(|h| h["id"] == id) {
            Some(_) => axum::Json(json!({ "ok": true, "data": trades::fixtures::detail() }))
                .into_response(),
            None => (StatusCode::NOT_FOUND, "no").into_response(),
        }
    }
    use axum::response::IntoResponse;
    let app = Router::new()
        .route("/v1/trades", get(list))
        .route("/v1/trades/{id}", get(detail))
        .with_state(up);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/v1/trades")
}

async fn marker(pool: &sqlx::PgPool) -> MirrorState {
    trades::load_state(pool)
        .await
        .unwrap()
        .expect("the mirror persisted its marker")
}

async fn mirrored_count(pool: &sqlx::PgPool, upstream: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM shop.trades WHERE mirrored_from = $1")
        .bind(upstream)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_trades_mirror_completes_a_pass_resumes_after_a_failure_and_reports_freshness() {
    let Some(state) = state_with_db(Settings::for_tests()).await else {
        return;
    };
    state
        .test_allow_loopback
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _marker = MARKER_LOCK.lock().await;
    let pool = &state.pool;
    let t = tag();
    // 2 full pages and a short one: 205 trades, newest first.
    let heads: Vec<Value> = (0..205)
        .map(|i| head(&t, i, 1_733_691_667_473 - i as i64))
        .collect();
    let up = FakeUpstream::new(heads, Some(100), false);
    let upstream = serve_fake(up.clone()).await;
    let jobs = crate::jobs::Jobs::new();
    let ctx = {
        let (tx, rx) = tokio::sync::oneshot::channel();
        jobs.spawn("ctx", move |ctx| async move {
            let _ = tx.send(ctx);
            Ok(())
        });
        rx.await.unwrap()
    };
    sqlx::query("DELETE FROM shop.trades_mirror")
        .execute(pool)
        .await
        .unwrap();

    // Run 1: page 1 lands, page 2 fails; the cursor stays at 100 and the pass is open.
    let err = trades::run_pass(&state, &upstream, &ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("upstream list at skip 100"), "{err}");
    let m = marker(pool).await;
    assert_eq!(
        (
            m.pass,
            m.next_skip,
            m.pages_fetched,
            m.rows_fetched,
            m.rows_inserted
        ),
        (1, 100, 1, 100, 100)
    );
    assert!(m.in_progress() && m.completed_at.is_none());
    assert!(m.last_error.as_deref().unwrap().contains("skip 100"));
    assert_eq!(mirrored_count(pool, &upstream).await, 100);

    // Run 2 (a restart): resumes at the persisted cursor, never re-reads page 1, completes.
    *up.fail_at_skip.lock().unwrap() = None;
    up.requested_skips.lock().unwrap().clear();
    trades::run_pass(&state, &upstream, &ctx).await.unwrap();
    assert_eq!(*up.requested_skips.lock().unwrap(), vec![100, 200]);
    let m = marker(pool).await;
    assert_eq!(
        (
            m.pass,
            m.next_skip,
            m.pages_fetched,
            m.rows_fetched,
            m.rows_inserted,
            m.rows_failed
        ),
        (1, 0, 3, 205, 205, 0)
    );
    assert!(!m.in_progress() && m.completed_at.is_some());
    assert_eq!(mirrored_count(pool, &upstream).await, 205);

    // Run 3: a new pass walks everything again and inserts nothing.
    up.requested_skips.lock().unwrap().clear();
    trades::run_pass(&state, &upstream, &ctx).await.unwrap();
    assert_eq!(*up.requested_skips.lock().unwrap(), vec![0, 100, 200]);
    let m = marker(pool).await;
    assert_eq!((m.pass, m.rows_fetched, m.rows_inserted), (2, 205, 0));
    assert_eq!(mirrored_count(pool, &upstream).await, 205);

    let settings = Settings {
        trades_sync_upstream_url: Some(upstream.clone()),
        ..Settings::for_tests()
    };
    let visible = Arc::new(MarketplaceState::new(
        pool.clone(),
        crate::http::Outbound::for_tests(),
        settings,
    ));
    let (status, body) = get_json(router(visible), "/v1/trades-mirror").await;
    assert_eq!(status, StatusCode::OK);
    let d = &body["data"];
    assert_eq!(d["enabled"], true);
    assert_eq!(d["pass"], 2);
    assert_eq!(d["inProgress"], false);
    assert_eq!(d["rowsFetched"], 205);
    assert!(d["mirroredTrades"].as_i64().unwrap() >= 205);
    assert!(d["completedAt"].as_str().unwrap().ends_with('Z'));

    // Cancellation before a page: nothing fetched, the cursor untouched.
    ctx.cancel.cancel();
    up.requested_skips.lock().unwrap().clear();
    trades::run_pass(&state, &upstream, &ctx).await.unwrap();
    assert!(up.requested_skips.lock().unwrap().is_empty());
    let m = marker(pool).await;
    assert_eq!((m.pass, m.next_skip), (3, 0));
    assert!(
        m.in_progress(),
        "the pass opened by the cancelled run resumes on the next tick"
    );

    sqlx::query("DELETE FROM shop.trades_mirror")
        .execute(pool)
        .await
        .unwrap();
    delete_trades_with_prefix(pool, &t).await;
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
}

/// The production shape: the upstream answers the whole list to every
/// request. One pass downloads it once and holds it across its ticks; each
/// tick pays for at most `trades_sync_pages_per_run` pages of new trades and
/// walks every page of already-held rows for free, so a caught-up mirror
/// sees the tail (where new trades append) on every tick. The read routes
/// answer the `trades-mirror` envelope until the first pass completes.
#[tokio::test]
async fn a_whole_list_upstream_is_walked_every_tick_within_the_detail_budget() {
    let settings = Settings {
        trades_sync_pages_per_run: 1,
        ..Settings::for_tests()
    };
    let Some(state) = state_with_db(settings).await else {
        return;
    };
    state
        .test_allow_loopback
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _marker = MARKER_LOCK.lock().await;
    let pool = &state.pool;
    let t = tag();
    let heads: Vec<Value> = (0..205)
        .map(|i| head(&t, i, 1_733_691_667_473 + i as i64))
        .collect();
    let up = FakeUpstream::new(heads, None, true);
    let upstream = serve_fake(up.clone()).await;
    let jobs = crate::jobs::Jobs::new();
    let ctx = {
        let (tx, rx) = tokio::sync::oneshot::channel();
        jobs.spawn("ctx", move |ctx| async move {
            let _ = tx.send(ctx);
            Ok(())
        });
        rx.await.unwrap()
    };
    sqlx::query("DELETE FROM shop.trades_mirror")
        .execute(pool)
        .await
        .unwrap();

    let visible = Arc::new(MarketplaceState::new(
        pool.clone(),
        crate::http::Outbound::for_tests(),
        Settings {
            trades_sync_upstream_url: Some(upstream.clone()),
            ..Settings::for_tests()
        },
    ));
    let (status, body) = get_json(router(visible.clone()), "/v1/trades?first=1").await;
    assert_eq!(
        (status, body["source"].as_str(), body["configure"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("trades-mirror"),
            Some("TRADES_SYNC_UPSTREAM_URL")
        ),
        "no pass yet: {body}"
    );

    // Tick 1: one list download, held for the pass; 100 details, the budget spent at the second page.
    trades::run_pass(&state, &upstream, &ctx).await.unwrap();
    assert_eq!(up.take_skips(), vec![0]);
    let m = marker(pool).await;
    assert_eq!(
        (
            m.pass,
            m.next_skip,
            m.pages_fetched,
            m.rows_inserted,
            m.list_downloads
        ),
        (1, 100, 1, 100, 1)
    );
    assert!(m.in_progress());
    assert!(m.bytes_downloaded > 0);
    assert_eq!(mirrored_count(pool, &upstream).await, 100);
    let (status, body) = get_json(router(visible.clone()), "/v1/trades?first=1").await;
    assert_eq!(
        (status, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("trades-mirror")),
        "a pass 100 rows in is not the truth: {body}"
    );

    // Tick 2: no download (the held list), the next 100; the last page's 5 wait.
    trades::run_pass(&state, &upstream, &ctx).await.unwrap();
    assert!(
        up.take_skips().is_empty(),
        "the list is held across the ticks of one pass"
    );
    let m = marker(pool).await;
    assert_eq!(
        (
            m.next_skip,
            m.pages_fetched,
            m.rows_inserted,
            m.list_downloads
        ),
        (200, 2, 200, 1)
    );
    assert!(m.in_progress());

    // Tick 3: the short page completes the pass and drops the held list.
    trades::run_pass(&state, &upstream, &ctx).await.unwrap();
    assert!(up.take_skips().is_empty());
    let m = marker(pool).await;
    assert_eq!(
        (
            m.pass,
            m.next_skip,
            m.pages_fetched,
            m.rows_fetched,
            m.rows_inserted,
            m.list_downloads
        ),
        (1, 0, 3, 205, 205, 1)
    );
    assert!(!m.in_progress() && m.completed_at.is_some());
    assert_eq!(mirrored_count(pool, &upstream).await, 205);
    assert!(
        state.trades_list.lock().unwrap().is_none(),
        "a complete pass holds nothing"
    );
    visible.trades_ready.clear();
    let (status, body) = get_json(router(visible.clone()), "/v1/trades?first=1").await;
    assert_eq!(
        (status, body["ok"].as_bool()),
        (StatusCode::OK, Some(true)),
        "a complete pass serves: {body}"
    );

    // Tick 4, everything held: a new pass, one download, every page walked, complete in the one tick.
    trades::run_pass(&state, &upstream, &ctx).await.unwrap();
    assert_eq!(up.take_skips(), vec![0]);
    let m = marker(pool).await;
    assert_eq!(
        (
            m.pass,
            m.pages_fetched,
            m.rows_fetched,
            m.rows_inserted,
            m.list_downloads
        ),
        (2, 3, 205, 0, 1)
    );
    assert!(!m.in_progress() && m.completed_at.is_some());
    let bytes = m.bytes_downloaded;

    let (status, body) = get_json(router(visible), "/v1/trades-mirror").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["listDownloads"], 1);
    assert_eq!(body["data"]["bytesDownloaded"], bytes);

    sqlx::query("DELETE FROM shop.trades_mirror")
        .execute(pool)
        .await
        .unwrap();
    delete_trades_with_prefix(pool, &t).await;
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
}

mod data_plane_shared_types {
    use super::support::state_without_db;
    use crate::market::{AssetKey, DataWrites, Source, graph::Network, unavailable};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    #[test]
    fn asset_keys_keep_network_contract_and_id_and_render_the_wire_id() {
        let key =
            AssetKey::parse("matic", "0xAbC0000000000000000000000000000000000001-42").unwrap();
        assert_eq!(
            key,
            AssetKey::new(
                "polygon",
                "0xabc0000000000000000000000000000000000001",
                "42"
            )
        );
        assert_eq!(
            key.render(),
            "0xabc0000000000000000000000000000000000001-42"
        );
        assert_eq!(AssetKey::new("mainnet", "0xA", "1").network, "ethereum");
        assert!(
            AssetKey::parse("matic", "0xabc-1").is_none(),
            "a short contract is not a key"
        );
        assert!(AssetKey::parse("matic", "0xabc0000000000000000000000000000000000001-").is_none());
        assert!(
            AssetKey::parse("matic", "0xabc0000000000000000000000000000000000001-x1").is_none()
        );
    }

    #[tokio::test]
    async fn the_unavailable_envelope_names_the_source_and_the_variable() {
        let res = unavailable(
            Source::CollectionsSubgraph(Network::Matic),
            "the items mirror has no complete pass",
        );
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "60");
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "Service Unavailable");
        assert_eq!(v["source"], "collections-subgraph:matic");
        assert_eq!(v["configure"], "MATIC_COLLECTIONS_SUBGRAPH_URL");
        let res = unavailable(Source::Deferred("nft-indexer"), "not carried");
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["source"], "nft-indexer");
        assert!(
            v.get("configure").is_none(),
            "a deferred source has nothing to configure"
        );
        assert_eq!(
            Source::ItemsMirror.configure(),
            Some("MARKETPLACE_MIRRORS_ENABLED")
        );
    }

    fn app(enabled: bool) -> Router {
        let inner = Router::new()
            .route(
                "/v1/trades",
                get(|| async { "list" }).post(|| async { "created" }),
            )
            .route("/v1/payments/verify", post(|| async { "receipt" }));
        Router::new()
            .merge(DataWrites::guard(inner.clone(), enabled))
            .nest("/marketplace-api", DataWrites::guard(inner, enabled))
    }

    async fn call(app: &Router, method: &str, path: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn data_writes_off_answers_503_on_writes_and_serves_reads() {
        assert!(DataWrites::is_gated(&Method::POST, "/v1/trades"));
        assert!(DataWrites::is_gated(&Method::DELETE, "/v1/picks/x"));
        assert!(!DataWrites::is_gated(&Method::GET, "/v1/trades"));
        assert!(!DataWrites::is_gated(&Method::POST, "/v1/payments/verify"));
        assert!(
            !DataWrites::is_gated(&Method::POST, "/v1/admin/relayer/toggle"),
            "process-local relayer controls stay open, behind their bearer"
        );
        assert!(!DataWrites::is_gated(
            &Method::POST,
            "/v1/admin/relayer/signer"
        ));
        assert!(
            DataWrites::is_gated(&Method::POST, "/v1/admin/collections/approve-mana"),
            "a money route stays gated"
        );
        let off = app(false);
        let (status, body) = call(&off, "POST", "/v1/trades").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains("DATA_WRITES_ENABLED=false")
        );
        let (status, _) = call(&off, "POST", "/marketplace-api/v1/trades").await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "the nested copy is gated on its route-local path"
        );
        assert_eq!(
            call(&off, "GET", "/v1/trades").await,
            (StatusCode::OK, "list".to_string())
        );
        assert_eq!(
            call(&off, "POST", "/v1/payments/verify").await,
            (StatusCode::OK, "receipt".to_string())
        );
        let res = DataWrites::disabled_response();
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "3600");
        assert_eq!(
            call(&app(true), "POST", "/v1/trades").await,
            (StatusCode::OK, "created".to_string())
        );
        assert!(
            !state_without_db().settings.data_writes_enabled,
            "the test settings keep writes off"
        );
    }
}

#[tokio::test]
async fn the_venues_read_eth_rpc_url_on_mainnet_only() {
    let settings = |network: &str, trade: Option<&str>| {
        let mut s = Settings::for_tests();
        s.eth_network = network.to_string();
        s.eth_rpc_url = Some("https://eth.example/".into());
        s.market.trade_rpc_urls = super::listings::RpcEndpoints::from_env(trade);
        support::state_without_db_settings(s)
    };
    let mainnet = settings("mainnet", None);
    assert_eq!(
        mainnet.venue_rpc_url(1).as_deref(),
        Some("https://eth.example/")
    );
    assert_eq!(mainnet.venue_rpc_url(137), None);
    let sepolia = settings("sepolia", None);
    assert_eq!(sepolia.venue_rpc_url(1), None);
    let sepolia = settings("sepolia", Some("1=https://mainnet.example/"));
    assert_eq!(
        sepolia.venue_rpc_url(1).as_deref(),
        Some("https://mainnet.example/")
    );
}
