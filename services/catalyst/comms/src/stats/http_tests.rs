use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

#[derive(Default)]
struct Documents {
    calls: AtomicUsize,
    values: Mutex<Vec<Value>>,
    pointers: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl content::Source for Documents {
    async fn fetch(
        &self,
        pointers: &[String],
    ) -> Result<Vec<crate::content::OrderedJson>, content::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.pointers.lock() = pointers.to_vec();
        Ok(self
            .values
            .lock()
            .iter()
            .map(|v| serde_json::from_value(v.clone()).unwrap())
            .collect())
    }
}
#[derive(Default)]
struct Runtime {
    lifecycle: Mutex<http::Lifecycle>,
    scrapes: AtomicUsize,
    observations: Mutex<Vec<http::Observation>>,
    metrics_fail: bool,
}
#[async_trait::async_trait]
impl http::Hooks for Runtime {
    fn observe(&self, observation: http::Observation) {
        self.observations.lock().push(observation);
    }
    async fn probe(&self, _startup: bool) -> http::Lifecycle {
        self.lifecycle.lock().clone()
    }
    async fn metrics(&self) -> Result<String, content::Error> {
        self.scrapes.fetch_add(1, Ordering::SeqCst);
        if self.metrics_fail {
            Err(content::Error)
        } else {
            Ok("real_counter 7\n".into())
        }
    }
    async fn diagnostics(&self) -> Result<String, content::Error> {
        Ok(r#"{"peers_total":3}"#.into())
    }
}
fn setup() -> (
    http::State,
    Arc<Documents>,
    Arc<Runtime>,
    InProcessBus,
    NatsSubscription,
) {
    let reader = Arc::new(Reader::default());
    let bus = InProcessBus::new();
    let sub = reader.subscribe(&bus);
    let docs = Arc::new(Documents::default());
    let runtime = Arc::new(Runtime::default());
    (
        http::State {
            reader,
            content: docs.clone(),
            hooks: runtime.clone(),
            settings: http::Settings::default(),
            matcher: http::test_support::matcher(),
            now: Arc::new(|| 1234),
            environment: Arc::new(|| ("v".into(), "hash".into())),
        },
        docs,
        runtime,
        bus,
        sub,
    )
}
async fn request(
    router: Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut request = Request::builder().method(method).uri(path);
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    let response = router
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}
fn publish(bus: &InProcessBus, s: Snapshot) {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    for bytes in frames(&s, uuid::Uuid::nil(), sequence, 200).unwrap() {
        bus.inject(SNAPSHOTS_SUBJECT, &bytes);
    }
}

#[derive(Default)]
struct RecordingMatcher {
    paths: Mutex<Vec<String>>,
    matches: bool,
    fails: bool,
}
#[async_trait::async_trait]
impl http::MetricsMatcher for RecordingMatcher {
    async fn matches(&self, path: &str) -> Result<bool, http::MatcherError> {
        self.paths.lock().push(path.into());
        if self.fails {
            Err(http::MatcherError("controlled failure".into()))
        } else {
            Ok(self.matches)
        }
    }
}

struct PendingMatch {
    path: String,
    release: tokio::sync::oneshot::Sender<Result<bool, http::MatcherError>>,
}
struct GatedMatcher(tokio::sync::mpsc::UnboundedSender<PendingMatch>);
#[async_trait::async_trait]
impl http::MetricsMatcher for GatedMatcher {
    async fn matches(&self, path: &str) -> Result<bool, http::MatcherError> {
        let (release, result) = tokio::sync::oneshot::channel();
        self.0
            .send(PendingMatch {
                path: path.into(),
                release,
            })
            .expect("selection controller remains alive");
        result.await.expect("selection is released")
    }
}
async fn release_selection(
    calls: &mut tokio::sync::mpsc::UnboundedReceiver<PendingMatch>,
    path: &str,
    result: Result<bool, http::MatcherError>,
) -> std::time::Duration {
    let call = tokio::time::timeout(std::time::Duration::from_secs(5), calls.recv())
        .await
        .expect("dispatcher reaches selection")
        .expect("matcher remains alive");
    assert_eq!(call.path, path);
    let held = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let held = held.elapsed();
    call.release.send(result).expect("request awaits selection");
    held
}

#[tokio::test]
async fn standalone_selection_duration_includes_match_and_nonmatch() {
    let mut timings = Vec::new();
    for (name, matched, bearer, metrics_fail, status, handler, code, scrapes) in [
        ("match", true, None, false, 200, "/metrics", 200, 1),
        ("nonmatch", false, None, false, 404, "", 404, 0),
        (
            "unauthorized",
            true,
            Some("fixture"),
            false,
            401,
            "/metrics",
            401,
            0,
        ),
        ("handler-error", true, None, true, 500, "/metrics", 200, 1),
    ] {
        let (mut state, _, _, _, _sub) = setup();
        let hooks = Arc::new(Runtime {
            metrics_fail,
            ..Default::default()
        });
        state.hooks = hooks.clone();
        state.settings.metrics_bearer_token = bearer.map(str::to_string);
        let (calls, mut selections) = tokio::sync::mpsc::unbounded_channel();
        state.matcher = Arc::new(GatedMatcher(calls));
        let path = if matched { "/metrics" } else { "/unknown" };
        let started = std::time::Instant::now();
        let (response, held) = tokio::join!(
            request(
                http::standalone(state),
                "GET",
                path,
                &[("content-length", "17")]
            ),
            release_selection(&mut selections, path, Ok(matched)),
        );
        let elapsed = started.elapsed();
        assert_eq!(response.0.as_u16(), status, "{name}");
        let observations = hooks.observations.lock();
        assert_eq!(observations.len(), 1, "{name}");
        let observation = &observations[0];
        assert_eq!(observation.method, "GET");
        assert_eq!(observation.handler, handler, "{name}");
        assert_eq!(observation.code, code, "{name}");
        assert_eq!(observation.request_size, 17.);
        assert_eq!(hooks.scrapes.load(Ordering::SeqCst), scrapes, "{name}");
        assert!(selections.try_recv().is_err());
        println!(
            "{name}: selection held {held:?}, observed {:?}, request {elapsed:?}",
            observation.duration
        );
        timings.push((name, observation.duration, held, elapsed));
    }
    assert!(
        timings
            .iter()
            .all(|(_, observed, held, elapsed)| observed >= held && observed <= elapsed),
        "selection must be included once: {timings:?}"
    );
}

#[tokio::test]
async fn claimed_root_duration_includes_raw_and_normalized_selection() {
    let (mut state, _, hooks, _, _sub) = setup();
    let (calls, mut selections) = tokio::sync::mpsc::unbounded_channel();
    state.matcher = Arc::new(GatedMatcher(calls));
    let app = http::mount_services(
        Router::new().fallback(|| async { "shared" }),
        state.clone(),
        http::standalone(state),
        Router::new(),
    );
    let started = std::time::Instant::now();
    let (response, held) = tokio::join!(
        request(app, "GET", "/Peers/%2e", &[("content-length", "17")]),
        async {
            let raw = release_selection(&mut selections, "/Peers/%2e", Ok(false)).await;
            let normalized = release_selection(&mut selections, "/Peers/", Ok(false)).await;
            raw + normalized
        },
    );
    let elapsed = started.elapsed();
    assert_eq!(response.0, 200);
    assert_eq!(response.2, r#"{"ok":true,"peers":[]}"#);
    let observations = hooks.observations.lock();
    assert_eq!(observations.len(), 1);
    let observation = &observations[0];
    assert_eq!(observation.handler, "/peers");
    assert_eq!(observation.code, 200);
    assert_eq!(observation.request_size, 17.);
    assert_eq!(hooks.scrapes.load(Ordering::SeqCst), 0);
    assert!(selections.try_recv().is_err());
    println!(
        "claimed root: selections held {held:?}, observed {:?}, request {elapsed:?}",
        observation.duration
    );
    assert!(observation.duration >= held && observation.duration <= elapsed);
}

#[tokio::test]
async fn normalized_selection_failure_keeps_raw_selection_duration() {
    let mut timings = Vec::new();
    for method in ["GET", "HEAD"] {
        let (mut state, _, hooks, _, _sub) = setup();
        let (calls, mut selections) = tokio::sync::mpsc::unbounded_channel();
        state.matcher = Arc::new(GatedMatcher(calls));
        let app = http::mount_services(
            Router::new().fallback(|| async { "shared" }),
            state.clone(),
            http::standalone(state),
            Router::new(),
        );
        let started = std::time::Instant::now();
        let (response, held) = tokio::join!(
            request(
                app,
                method,
                "/Peers/%2e",
                &[
                    ("origin", "https://example.invalid"),
                    ("content-length", "17")
                ]
            ),
            async {
                let raw = release_selection(&mut selections, "/Peers/%2e", Ok(false)).await;
                let normalized = release_selection(
                    &mut selections,
                    "/Peers/",
                    Err(http::MatcherError("controlled normalized failure".into())),
                )
                .await;
                raw + normalized
            },
        );
        let elapsed = started.elapsed();
        assert_eq!(response.0, 500);
        assert!(response.2.is_empty());
        assert!(!response.1.contains_key("content-type"));
        assert!(!response.1.contains_key("access-control-allow-origin"));
        if method == "HEAD" {
            assert!(!response.1.contains_key("content-length"));
        }
        let observations = hooks.observations.lock();
        assert_eq!(observations.len(), 1);
        let observation = &observations[0];
        assert_eq!(observation.method, method);
        assert_eq!(observation.handler, "");
        assert_eq!(observation.code, 200);
        assert_eq!(observation.request_size, 17.);
        assert_eq!(hooks.scrapes.load(Ordering::SeqCst), 0);
        assert!(selections.try_recv().is_err());
        println!(
            "normalized failure {method}: selections held {held:?}, observed {:?}, request {elapsed:?}",
            observation.duration
        );
        timings.push((method, observation.duration, held, elapsed));
    }
    assert!(
        timings
            .iter()
            .all(|(_, observed, held, elapsed)| observed >= held && observed <= elapsed),
        "both selection intervals must be included once: {timings:?}"
    );
}

#[tokio::test]
async fn root_bypasses_non_candidates_and_records_exact_selection_paths() {
    let (mut state, _, _, _, _sub) = setup();
    let matcher = Arc::new(RecordingMatcher::default());
    state.matcher = matcher.clone();
    let stats = http::standalone(state.clone());
    let app = http::mount_services(
        Router::new().fallback(|| async { "shared" }),
        state,
        stats,
        Router::new().fallback(|| async { "connector" }),
    );
    for path in [
        "/content/entities",
        "/metrics",
        "/status",
        "/ping",
        "/health/live",
        "/ws",
        "/archipelago-ea-ws-connector/x",
    ] {
        request(app.clone(), "GET", path, &[]).await;
    }
    assert!(matcher.paths.lock().is_empty());
    for (path, expected) in [
        ("/peers", vec!["/peers"]),
        ("/Peers/%2e", vec!["/Peers/%2e", "/Peers/"]),
        ("/peers/id%2Fmore", vec!["/peers/id%2Fmore"]),
        ("/archipelago-ea-stats/x/../peers", vec!["/peers"]),
        ("/archipelago-ea-stats/unknown", vec!["/unknown"]),
    ] {
        request(app.clone(), "GET", path, &[]).await;
        assert_eq!(
            std::mem::take(&mut *matcher.paths.lock()),
            expected,
            "{path}"
        );
    }
    request(app.clone(), "OPTIONS", "/archipelago-ea-stats/unknown", &[]).await;
    request(app, "GET", "/archipelago-ea-stats/health/live", &[]).await;
    assert!(matcher.paths.lock().is_empty());
}

#[tokio::test]
async fn root_collision_suppresses_claim_and_matcher_fault_is_not_a_nonmatch() {
    let (mut state, _, hooks, _, _sub) = setup();
    state.matcher = Arc::new(RecordingMatcher {
        matches: true,
        ..Default::default()
    });
    let shared =
        Router::new().fallback(|| async { ([("x-shared-handler", "selected")], "shared") });
    let app = http::merge_root(shared.clone(), state.clone());
    for method in ["GET", "AAAA_COLLISION", "TRACE", "CONNECT"] {
        let baseline = request(shared.clone(), method, "/peers", &[]).await;
        let response = request(app.clone(), method, "/peers", &[]).await;
        assert_eq!(response.0, StatusCode::OK, "delegated {method}");
        assert_eq!(
            response.1["x-shared-handler"], "selected",
            "delegated {method}"
        );
        // Axum suppresses a successful CONNECT body even without Stats middleware.
        assert_eq!(response.2, baseline.2, "delegated {method}");
    }
    assert!(hooks.observations.lock().is_empty());
    state.matcher = Arc::new(RecordingMatcher {
        fails: true,
        ..Default::default()
    });
    for root in [false, true] {
        let app = if root {
            http::merge_root(Router::new().fallback(|| async { "shared" }), state.clone())
        } else {
            http::standalone(state.clone())
        };
        for method in ["GET", "HEAD"] {
            let (status, headers, body) = request(
                app.clone(),
                method,
                "/peers",
                &[
                    ("origin", "https://example.invalid"),
                    ("content-length", "17"),
                ],
            )
            .await;
            assert_eq!(status, 500);
            assert!(body.is_empty());
            assert!(!headers.contains_key("content-type"));
            assert!(!headers.contains_key("access-control-allow-origin"));
            if method == "HEAD" {
                assert!(!headers.contains_key("content-length"));
            }
            let observation = hooks.observations.lock().pop().unwrap();
            assert_eq!(
                (
                    observation.handler.as_str(),
                    observation.code,
                    observation.request_size
                ),
                ("", 200, 17.)
            );
        }
        for method in ["AAAA_FAILURE", "TRACE", "CONNECT"] {
            let response = request(app.clone(), method, "/peers", &[]).await;
            let expected = if !root && method == "AAAA_FAILURE" {
                400
            } else {
                500
            };
            assert_eq!(response.0.as_u16(), expected, "root={root} {method}");
            assert!(
                hooks.observations.lock().is_empty(),
                "{method} creates no series on a matcher fault"
            );
        }
    }
    assert_eq!(hooks.scrapes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn method_admission_bounds_labels_without_relabeling_accepted_extensions() {
    for scope in ["standalone", "root", "stats-alias", "connector-alias"] {
        let (mut state, _, hooks, _, _sub) = setup();
        let matcher = Arc::new(RecordingMatcher::default());
        state.matcher = matcher.clone();
        let (app, path) = if scope == "standalone" {
            (http::standalone(state), "/peers")
        } else {
            let app = http::mount_services(
                Router::new().fallback(|| async { "shared" }),
                state.clone(),
                http::standalone(state.clone()),
                http::socket_alias(http::standalone(state.clone()), state),
            );
            let path = match scope {
                "root" => "/peers",
                "stats-alias" => "/archipelago-ea-stats/peers",
                _ => "/archipelago-ea-ws-connector/peers",
            };
            (app, path)
        };
        for method in (0..64)
            .map(|i| format!("AAAA{i}"))
            .chain(["TRACK".into(), "get".into()])
        {
            let response = request(
                app.clone(),
                &method,
                path,
                &[("origin", "https://example.invalid")],
            )
            .await;
            assert_eq!(response.0, StatusCode::BAD_REQUEST, "{scope} {method}");
            assert!(response.2.is_empty());
            assert!(!response.1.contains_key("access-control-allow-origin"));
        }
        if scope == "root" {
            let paths = matcher.paths.lock();
            assert_eq!(paths.len(), 66, "root checks ownership once per request");
            assert!(paths.iter().all(|path| path == "/peers"));
        } else {
            assert!(
                matcher.paths.lock().is_empty(),
                "{scope}: rejected methods reach no matcher"
            );
        }
        assert!(
            hooks.observations.lock().is_empty(),
            "{scope}: rejected methods create no series"
        );
        for (method, status) in [("OPTIONS", 204), ("TRACE", 500), ("CONNECT", 501)] {
            assert_eq!(
                request(app.clone(), method, path, &[]).await.0.as_u16(),
                status,
                "{scope} {method}"
            );
        }
        assert!(hooks.observations.lock().is_empty());
        let extensions = ["ACL", "PROPFIND", "QUERY", "M-SEARCH"];
        for method in extensions {
            assert_eq!(
                request(app.clone(), method, path, &[]).await.0.as_u16(),
                501,
                "{scope} {method}"
            );
        }
        let observations = hooks.observations.lock();
        assert_eq!(observations.len(), extensions.len());
        for (observation, method) in observations.iter().zip(extensions) {
            assert_eq!(observation.method, method);
            assert_eq!(observation.code, 501);
            assert!(observation.handler.is_empty());
        }
        drop(observations);
        if scope == "root" {
            matcher.paths.lock().clear();
            let before = hooks.observations.lock().len();
            let response = request(app, "AAAA_DOMAIN", "/other-domain", &[]).await;
            assert_eq!(response.0, StatusCode::OK);
            assert_eq!(response.2, "shared");
            assert!(matcher.paths.lock().is_empty());
            assert_eq!(hooks.observations.lock().len(), before);
        }
    }
}

#[tokio::test]
async fn selected_sql_metadata_order_reaches_hot_scenes_http() {
    let pool = crate::db::scratch::pool(2)
        .await
        .expect("test database required");
    // Synthetic persisted handler input: current scene deployment schemas require string titles.
    let metadata = r#"{"v":{"id":"spoof","scene":{"base":"0,0","parcels":["0,0"]},"display":{"title":{"z":1,"a":2},"description":[{"z":true,"a":null}]},"contact":{"name":{"z":3,"a":4}},"source":{"projectId":{"$serde_json::private::Number":"12"}}}}"#;
    sqlx::query("INSERT INTO content.deployments (deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) VALUES ('0x0000000000000000000000000000000000000001','v3','scene','ordered-scene',$1::json,timezone('UTC',now()),ARRAY['0,0'],timezone('UTC',now()),'[]'::json)").bind(metadata).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO content.active_pointers (pointer,entity_id,entity_type) VALUES ('0,0','ordered-scene','scene')").execute(&pool).await.unwrap();
    let (mut state, _, _, bus, _sub) = setup();
    state.content = Arc::new(content::Content {
        pool: pool.clone(),
        denylist: Arc::new(crate::content::Denylist::default()),
    });
    publish(
        &bus,
        Snapshot {
            peers: vec![Peer {
                address: "one".into(),
                position: [0.; 3],
                time: 1,
            }],
            ..Default::default()
        },
    );
    let app = http::standalone(state);
    let (status, headers, body) = request(app.clone(), "GET", "/hot-scenes", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"[{"id":"ordered-scene","name":{"z":1,"a":2},"baseCoords":[0,0],"usersTotalCount":1,"parcels":[[0,0]],"creator":{"z":3,"a":4},"projectId":{"$serde_json::private::Number":"12"},"description":[{"z":true,"a":null}]}]"#
    );
    assert_eq!(headers["content-length"], body.len().to_string());
    let (status, head_headers, body) = request(app.clone(), "HEAD", "/hot-scenes", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert!(!head_headers.contains_key("content-length"));
    assert!(!head_headers.contains_key("content-type"));
    for (title, expected) in [
        (
            r#"{"z":1,"10":10,"2":2,"01":1,"0":0,"-0":0,"4294967294":4,"4294967295":5,"a":1,"\u0061":2,"z":9,"2":3}"#,
            r#"{"0":0,"2":3,"10":10,"4294967294":4,"z":9,"01":1,"-0":0,"4294967295":5,"a":2}"#,
        ),
        (
            "[9007199254740991,9007199254740992,9007199254740993,18446744073709551615,-0,5e-324,1e-7,1e-6,1e20,1e21]",
            "[9007199254740991,9007199254740992,9007199254740992,18446744073709552000,0,5e-324,1e-7,0.000001,100000000000000000000,1e+21]",
        ),
        (
            r#"["\ud83d\ude00","é\n/\u2028",false,null]"#,
            "[\"😀\",\"é\\n/\u{2028}\",false,null]",
        ),
        ("null", "null"),
        ("false", "false"),
        (r#""""#, r#""""#),
    ] {
        let metadata = format!(
            r#"{{"v":{{"scene":{{"base":"0,0","parcels":["0,0"]}},"display":{{"title":{title}}}}}}}"#
        );
        sqlx::query("UPDATE content.deployments SET entity_metadata=$1::json WHERE entity_id='ordered-scene'").bind(metadata).execute(&pool).await.unwrap();
        let (status, _, body) = request(app.clone(), "GET", "/hot-scenes", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!(
                r#"[{{"id":"ordered-scene","name":{expected},"baseCoords":[0,0],"usersTotalCount":1,"parcels":[[0,0]]}}]"#
            )
        );
    }
    // Synthetic selected documents bypass the scene logs-permissions expression index,
    // which rejects unpaired surrogates before they can reach either read projection.
    sqlx::query(
        "UPDATE content.active_pointers SET entity_type='profile' WHERE entity_id='ordered-scene'",
    )
    .execute(&pool)
    .await
    .unwrap();
    for invalid in ["1e400", r#""\ud800""#] {
        let metadata = format!(
            r#"{{"v":{{"scene":{{"base":"0,0","parcels":["0,0"]}},"display":{{"title":{invalid}}}}}}}"#
        );
        sqlx::query("UPDATE content.deployments SET entity_type='profile',entity_metadata=$1::json WHERE entity_id='ordered-scene'").bind(metadata).execute(&pool).await.unwrap();
        assert!(
            crate::content::queries::active_entity_docs_by_pointers(&pool, &["0,0".into()])
                .await
                .is_err()
        );
        for method in ["GET", "HEAD"] {
            let (status, headers, body) = request(app.clone(), method, "/hot-scenes", &[]).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(body.is_empty());
            assert!(!headers.contains_key("content-type"));
        }
    }
    sqlx::query("DELETE FROM content.active_pointers WHERE entity_id='ordered-scene'")
        .execute(&pool)
        .await
        .unwrap();
}
#[tokio::test]
async fn standalone_normalizes_dot_segments_without_collapsing_slashes() {
    let (state, _, _, _, _sub) = setup();
    let app = http::standalone(state);
    for path in ["/x/../peers", "/x/%2e%2e/peers", "/./peers"] {
        let (status, _, body) = request(app.clone(), "GET", path, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body, r#"{"ok":true,"peers":[]}"#);
    }
    assert_eq!(
        request(app, "GET", "/peers//", &[]).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn socket_alias_uses_stats_fallback_except_for_get_upgrades() {
    let (state, _, _, _, _sub) = setup();
    let app = http::socket_alias(
        http::standalone(state.clone())
            .route("/ws", get(|| async { StatusCode::SWITCHING_PROTOCOLS })),
        state,
    );
    for (method, expected) in [
        ("GET", 404),
        ("HEAD", 404),
        ("POST", 404),
        ("OPTIONS", 204),
        ("PROPFIND", 501),
    ] {
        let (status, headers, body) = request(
            app.clone(),
            method,
            "/ws",
            &[("origin", "https://client.example")],
        )
        .await;
        assert_eq!(status.as_u16(), expected, "{method}");
        assert_eq!(
            headers["access-control-allow-origin"],
            "https://client.example"
        );
        if method == "GET" {
            assert_eq!(body, "Not found");
        }
    }
    assert_eq!(
        request(app, "GET", "/ws", &[("upgrade", "websocket")])
            .await
            .0,
        StatusCode::SWITCHING_PROTOCOLS
    );
}
#[tokio::test]
async fn all_application_aliases_and_missing_bodies_match_the_stats_contract() {
    let (state, _, _, bus, _sub) = setup();
    publish(
        &bus,
        Snapshot {
            peers: vec![Peer {
                address: "A/B+%".into(),
                position: [16., 0., -0.1],
                time: 123,
            }],
            ..Default::default()
        },
    );
    let app = http::merge_root(http::standalone(state.clone()), state);
    for prefix in ["", "/comms", "/stats"] {
        for (suffix, key) in [
            ("peers", "peers"),
            ("parcels", "parcels"),
            ("islands", "islands"),
        ] {
            let (status, headers, body) =
                request(app.clone(), "GET", &format!("{prefix}/{suffix}"), &[]).await;
            assert_eq!(status, 200);
            assert_eq!(headers["content-type"], "application/json");
            assert!(serde_json::from_str::<Value>(&body).unwrap()[key].is_array());
        }
        assert_eq!(
            request(
                app.clone(),
                "GET",
                &format!("{prefix}/peers/A%2FB%2B%25"),
                &[]
            )
            .await
            .0,
            200
        );
        assert_eq!(
            request(app.clone(), "GET", &format!("{prefix}/peers/missing"), &[])
                .await
                .2,
            r#"{"ok":false,"peer":null}"#
        );
        let (status, headers, body) = request(
            app.clone(),
            "GET",
            &format!("{prefix}/islands/missing"),
            &[],
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(body, "");
        assert!(!headers.contains_key("content-type"));
    }
    assert_eq!(request(app.clone(), "GET", "/PeErS/", &[]).await.0, 200);
    assert_eq!(
        request(app.clone(), "GET", "/unknown", &[]).await.2,
        "Not found"
    );
    assert_eq!(request(app.clone(), "GET", "/peers//", &[]).await.0, 404);
    assert_eq!(
        request(app.clone(), "GET", "/core-status", &[]).await.2,
        r#"{"healthy":false,"userCount":0}"#
    );
    assert_eq!(
        request(app.clone(), "GET", "/status", &[]).await.2,
        r#"{"version":"v","currentTime":1234,"commitHash":"hash"}"#
    );
    assert_eq!(request(app.clone(), "GET", "/ping", &[]).await.2, "/ping");
    assert_eq!(
        request(app, "GET", "/stats/health", &[]).await.2,
        r#"{"peers_total":3}"#
    );
}
#[tokio::test]
async fn query_filters_are_urlsearchparams_with_exact_case_and_store_order() {
    let (state, _, _, bus, _sub) = setup();
    publish(
        &bus,
        Snapshot {
            peers: ["B", "a", "A", "a b", "%FF"]
                .into_iter()
                .map(|a| Peer {
                    address: a.into(),
                    position: [0.; 3],
                    time: 1,
                })
                .collect(),
            ..Default::default()
        },
    );
    let app = http::standalone(state);
    for (query, want) in [
        ("id=a&id=B&id=a", vec!["B", "a"]),
        ("id=", vec![]),
        ("id=a,B", vec![]),
        ("id[]=A&ID=a", vec!["B", "a", "A", "a b", "%FF"]),
        ("id=a+b", vec!["a b"]),
        ("id=%FF", vec![]),
    ] {
        let response = request(app.clone(), "GET", &format!("/peers?{query}"), &[]).await;
        let v: Value = serde_json::from_str(&response.2).unwrap();
        assert_eq!(
            v["peers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            want
        );
    }
    assert_eq!(request(app.clone(), "GET", "/peers/%FF", &[]).await.0, 200);
    assert_eq!(request(app, "GET", "/peers/A?id=a", &[]).await.0, 200);
}
#[tokio::test]
async fn head_options_method_and_cors_headers_follow_the_original_middleware() {
    let (state, _, _, _, _sub) = setup();
    let app = http::standalone(state);
    for path in ["/peers", "/islands/missing", "/unknown", "/status"] {
        let (_, headers, body) =
            request(app.clone(), "HEAD", path, &[("Origin", "https://caller")]).await;
        assert_eq!(body, "");
        assert!(!headers.contains_key("content-type"));
        assert!(!headers.contains_key("content-length"));
        assert_eq!(headers["access-control-allow-origin"], "https://caller");
        assert_eq!(headers["vary"], "Origin");
        assert_eq!(headers["access-control-allow-credentials"], "true");
    }
    for path in ["/peers", "/unknown"] {
        let (status, headers, body) = request(app.clone(), "OPTIONS", path, &[]).await;
        assert_eq!(status, 204);
        assert_eq!(body, "");
        assert!(!headers.contains_key("content-length"));
        assert_eq!(
            headers["access-control-allow-methods"],
            "GET,HEAD,POST,PUT,DELETE,CONNECT,TRACE,PATCH"
        );
        assert_eq!(
            headers["access-control-allow-headers"],
            "Cache-Control,Content-Type,Origin,Accept,User-Agent,X-Upload-Origin"
        );
        assert_eq!(headers["access-control-max-age"], "86400");
    }
    for path in ["/peers", "/health/live", "/metrics"] {
        let r = request(app.clone(), "POST", path, &[]).await;
        assert_eq!(r.0, 405);
        assert_eq!(r.1["allow"], "HEAD, GET");
        assert_eq!(r.2, "");
    }
    let r = request(app.clone(), "PROPFIND", "/unknown", &[]).await;
    assert_eq!(r.0, 501);
    assert_eq!(r.1["allow"], "");
    assert_eq!(request(app.clone(), "POST", "/unknown", &[]).await.0, 404);
    assert_eq!(
        request(app.clone(), "GET", "/status", &[]).await.1["access-control-allow-origin"],
        "*"
    );
}
#[tokio::test]
async fn configured_metrics_auth_scrapes_and_actual_lifecycle_values_are_used() {
    let (mut state, _, runtime, _, _sub) = setup();
    state.settings.metrics_path = "/custom/metrics".into();
    state.matcher = Arc::new(http::test_support::LiteralMatcher("/custom/metrics".into()));
    state.settings.metrics_bearer_token = Some("secret".into());
    let app = http::standalone(state);
    assert_eq!(request(app.clone(), "GET", "/metrics", &[]).await.0, 404);
    for auth in ["bearer secret", "Bearer  secret", "Bearer secretx", ""] {
        let r = request(
            app.clone(),
            "GET",
            "/custom/metrics",
            &[("Authorization", auth)],
        )
        .await;
        assert_eq!(r.0, 401);
        assert_eq!(r.2, "");
        assert!(!r.1.contains_key("www-authenticate"));
        assert!(!r.1.contains_key("content-type"));
    }
    let r = request(
        app.clone(),
        "HEAD",
        "/custom/metrics",
        &[("Authorization", "Bearer secret ignored")],
    )
    .await;
    assert_eq!(r.0, 200);
    assert_eq!(r.2, "");
    assert_eq!(
        r.1["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    assert_eq!(runtime.scrapes.load(Ordering::SeqCst), 1);
    assert_eq!(
        request(app.clone(), "GET", "/health/live", &[]).await.2,
        "alive"
    );
    assert_eq!(
        request(app.clone(), "GET", "/health/ready", &[]).await.2,
        r#"{"status":"initializing"}"#
    );
    assert_eq!(
        request(app.clone(), "GET", "/health/startup", &[]).await.2,
        r#"{"status":"bootstrapping"}"#
    );
    *runtime.lifecycle.lock() = http::Lifecycle {
        initialized: true,
        ..Default::default()
    };
    assert_eq!(
        request(app.clone(), "GET", "/health/ready", &[]).await.2,
        "waiting"
    );
    runtime.lifecycle.lock().live = true;
    assert_eq!(
        request(app.clone(), "GET", "/health/ready", &[]).await.2,
        "unready"
    );
    assert_eq!(
        request(app.clone(), "GET", "/health/startup", &[]).await.2,
        r#"{"status":"starting"}"#
    );
    runtime.lifecycle.lock().started = true;
    runtime.lifecycle.lock().checks = vec![("bus".into(), false), ("content".into(), true)];
    let r = request(app.clone(), "GET", "/health/ready", &[]).await;
    assert_eq!(r.0, 503);
    assert_eq!(
        r.2,
        r#"{"details":{"bus":{"status":"fail"},"content":{"status":"pass"}},"status":"fail"}"#
    );
    runtime.lifecycle.lock().checks[0].1 = true;
    let r = request(app, "HEAD", "/health/startup", &[]).await;
    assert_eq!(r.0, 200);
    assert_eq!(
        r.1["content-type"],
        "application/health+json; charset=utf-8"
    );
}
#[tokio::test]
async fn hot_scenes_fetches_ordered_tiles_on_each_request_and_returns_empty_500_on_throw() {
    let (state, docs, _, bus, _sub) = setup();
    let app = http::standalone(state);
    assert_eq!(
        request(app.clone(), "GET", "/hot-scenes", &[]).await.2,
        "[]"
    );
    assert_eq!(docs.calls.load(Ordering::SeqCst), 0);
    publish(
        &bus,
        Snapshot {
            peers: vec![
                Peer {
                    address: "a".into(),
                    position: [32., 0., 0.],
                    time: 1,
                },
                Peer {
                    address: "b".into(),
                    position: [0.; 3],
                    time: 1,
                },
            ],
            ..Default::default()
        },
    );
    *docs.values.lock() = vec![
        json!({"id":"s","metadata":{"scene":{"base":"0","parcels":["0,0"]},"display":{"title":"old"}}}),
    ];
    let r = request(app.clone(), "GET", "/hot-scenes", &[]).await;
    assert!(r.2.contains("old"));
    assert_eq!(*docs.pointers.lock(), vec!["2,0", "0,0"]);
    docs.values.lock()[0]["metadata"]["display"]["title"] = json!("new");
    assert!(
        request(app.clone(), "GET", "/hot-scenes", &[])
            .await
            .2
            .contains("new")
    );
    assert_eq!(docs.calls.load(Ordering::SeqCst), 2);
    docs.values.lock()[0]["metadata"]["scene"]["base"] = json!(true);
    let r = request(app, "GET", "/hot-scenes", &[("Origin", "https://caller")]).await;
    assert_eq!(r.0, 500);
    assert_eq!(r.2, "");
    assert!(!r.1.contains_key("content-type"));
    assert!(!r.1.contains_key("access-control-allow-origin"));
}
#[tokio::test]
async fn root_projection_preserves_other_service_routes_and_fallback() {
    let (state, _, _, _, _sub) = setup();
    let root = Router::new()
        .route("/status", get(|| async { "content-status" }))
        .fallback(|| async { (StatusCode::IM_A_TEAPOT, "other") });
    let app = http::merge_root(root, state.clone())
        .nest("/archipelago-ea-stats", http::standalone(state));
    assert_eq!(
        request(app.clone(), "GET", "/status", &[]).await.2,
        "content-status"
    );
    assert_eq!(request(app.clone(), "GET", "/unknown", &[]).await.0, 418);
    assert_eq!(
        request(app.clone(), "OPTIONS", "/unknown", &[]).await.0,
        418
    );
    assert_eq!(
        request(app.clone(), "GET", "/CoMmS/PeErS/", &[]).await.2,
        r#"{"ok":true,"peers":[]}"#
    );
    assert_eq!(
        request(app.clone(), "GET", "/archipelago-ea-stats/status", &[])
            .await
            .2,
        r#"{"version":"v","currentTime":1234,"commitHash":"hash"}"#
    );
    assert_eq!(
        request(app, "OPTIONS", "/archipelago-ea-stats/unknown", &[])
            .await
            .0,
        204
    );
}
#[tokio::test]
async fn metrics_observe_route_templates_skip_probes_and_keep_request_semantics() {
    let (state, _, runtime, _, _sub) = setup();
    let app = http::standalone(state);
    request(app.clone(), "GET", "/health/live", &[]).await;
    request(app.clone(), "HEAD", "/health/ready", &[]).await;
    request(app.clone(), "OPTIONS", "/peers", &[]).await;
    assert!(runtime.observations.lock().is_empty());
    request(
        app.clone(),
        "GET",
        "/CoMmS/PeErS/Missing/",
        &[("content-length", "17")],
    )
    .await;
    let records = runtime.observations.lock();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].handler, "/comms/peers/:id");
    assert_eq!(records[0].request_size, 17.);
    assert_eq!(records[0].code, 404);
    drop(records);
    request(app.clone(), "POST", "/peers", &[]).await;
    assert_eq!(runtime.observations.lock()[1].handler, "");
    request(app, "GET", "/metrics", &[]).await;
    assert_eq!(runtime.observations.lock()[2].handler, "/metrics");
}
#[tokio::test]
async fn nested_head_and_options_do_not_invent_content_length_and_status_reads_each_time() {
    let (mut state, _, _, _, _sub) = setup();
    let version = Arc::new(Mutex::new("first".to_string()));
    let read = version.clone();
    state.environment = Arc::new(move || (read.lock().clone(), String::new()));
    let app = Router::new().nest("/archipelago-ea-stats", http::standalone(state));
    assert!(
        request(app.clone(), "GET", "/archipelago-ea-stats/status", &[])
            .await
            .2
            .contains("first")
    );
    *version.lock() = "second".into();
    assert!(
        request(app.clone(), "GET", "/archipelago-ea-stats/status", &[])
            .await
            .2
            .contains("second")
    );
    for method in ["HEAD", "OPTIONS"] {
        let r = request(app.clone(), method, "/archipelago-ea-stats/peers", &[]).await;
        assert!(!r.1.contains_key("content-length"), "{method}: {:?}", r.1);
        assert_eq!(r.2, "");
    }
}

#[tokio::test]
async fn an_empty_origin_sets_credentials_but_does_not_reflect_or_replace_status_origin() {
    let (state, _, _, _, _sub) = setup();
    let app = http::standalone(state);
    for method in ["GET", "OPTIONS"] {
        let r = request(app.clone(), method, "/peers", &[("Origin", "")]).await;
        assert!(!r.1.contains_key("access-control-allow-origin"));
        assert!(!r.1.contains_key("vary"));
        assert_eq!(r.1["access-control-allow-credentials"], "true");
    }
    assert_eq!(
        request(app, "GET", "/status", &[("Origin", "")]).await.1["access-control-allow-origin"],
        "*"
    );
}

/// Replays output generated by the pinned TypeScript handlers, preserving JSON bytes.
#[tokio::test]
async fn source_handler_fixtures_match_pulse_state_and_all_http_aliases() {
    use crate::protocol::archipelago::archipelago::Heartbeat;
    use crate::protocol::bus::statistics::{Event, Observation, ProducerReply, ProducerRequest};
    use crate::protocol::pulse::decentraland::kernel::comms::v3::{
        IslandStatusMessage, ServiceDiscoveryMessage,
    };
    use prost::Message;
    let fixture: Value = serde_json::from_str(include_str!("source-fixtures.json")).unwrap();
    let (mut state, docs, _, bus, _sub) = setup();
    state.settings.content_url = fixture["baseUrl"].as_str().unwrap().into();
    let mut store = crate::pulse::statistics::Statistics::default();
    let ProducerReply::Grant(grant) = store.control(ProducerRequest::Begin {
        incarnation: uuid::Uuid::nil(),
        producer: uuid::Uuid::nil(),
    }) else {
        panic!("grant")
    };
    assert_eq!(
        store.control(ProducerRequest::Confirm(grant.clone())),
        ProducerReply::Active(grant.clone())
    );
    let mut sequence = 0;
    for case in fixture["cases"].as_array().unwrap() {
        for event in case["events"].as_array().unwrap() {
            let subject = event["subject"].as_str().unwrap();
            let bytes: Vec<u8> = event["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect();
            let observation = if subject.ends_with(".heartbeat") {
                Heartbeat::decode(bytes.as_slice())
                    .unwrap()
                    .position
                    .map(|p| Event::Heartbeat {
                        address: subject.split('.').nth(1).unwrap().into(),
                        position: [p.x as f64, p.y as f64, p.z as f64],
                    })
            } else if subject.ends_with(".disconnect") {
                Some(Event::Disconnect {
                    address: subject.split('.').nth(1).unwrap().into(),
                })
            } else {
                if subject == "engine.islands"
                    && IslandStatusMessage::decode(bytes.as_slice()).is_ok()
                {
                    store.topology(bytes.clone());
                }
                if subject == "engine.discovery"
                    && ServiceDiscoveryMessage::decode(bytes.as_slice()).is_ok()
                {
                    store.discovery(bytes);
                }
                None
            };
            if let Some(event_kind) = observation {
                sequence += 1;
                assert!(store.observe(
                    Observation {
                        version: 2,
                        incarnation: grant.incarnation,
                        generation: grant.generation,
                        producer: uuid::Uuid::nil(),
                        sequence,
                        event: event_kind
                    },
                    event["time"].as_i64().unwrap()
                ));
            }
        }
        let snapshot = store.snapshot();
        assert_eq!(
            snapshot.peers.len(),
            case["peers"].as_array().unwrap().len()
        );
        for (actual, expected) in snapshot.peers.iter().zip(case["peers"].as_array().unwrap()) {
            assert_eq!(actual.address, expected["address"].as_str().unwrap());
            assert_eq!(actual.time, expected["time"].as_i64().unwrap());
            for (actual, expected) in actual
                .position
                .iter()
                .zip(expected["positionBits"].as_array().unwrap())
            {
                let expected = f64::from_bits(expected.as_str().unwrap().parse().unwrap());
                assert!(
                    (actual.is_nan() && expected.is_nan())
                        || actual.to_bits() == expected.to_bits()
                );
            }
        }
        publish(&bus, snapshot);
        let now = case["now"].as_i64().unwrap();
        state.now = Arc::new(move || now);
        *docs.values.lock() = case["documents"].as_array().unwrap().clone();
        for read in case["reads"].as_array().unwrap() {
            let path = read["path"].as_str().unwrap();
            let paths = if ["/peers", "/islands", "/parcels"]
                .iter()
                .any(|prefix| path.starts_with(prefix))
            {
                vec![
                    path.to_string(),
                    format!("/comms{path}"),
                    format!("/stats{path}"),
                ]
            } else {
                vec![path.to_string()]
            };
            for path in paths {
                let (status, headers, body) = request(
                    http::merge_root(http::standalone(state.clone()), state.clone()),
                    "GET",
                    &path,
                    &[],
                )
                .await;
                assert_eq!(
                    status.as_u16() as u64,
                    read["status"].as_u64().unwrap(),
                    "{} {path}",
                    case["name"]
                );
                assert_eq!(
                    body,
                    read["body"].as_str().unwrap(),
                    "{} {path}",
                    case["name"]
                );
                if !body.is_empty() {
                    assert_eq!(headers["content-type"], "application/json");
                }
            }
        }
    }
}

#[tokio::test]
async fn source_host_fallback_excludes_shared_root_statistics_aliases() {
    let (state, _, _, _, _sub) = setup();
    let standalone = http::standalone(state.clone());
    let root = http::merge_root(Router::new(), state);
    for path in [
        "/stats/peers",
        "/stats/parcels",
        "/stats/islands",
        "/stats/health",
    ] {
        let (code, headers, body) = request(standalone.clone(), "GET", path, &[]).await;
        assert_eq!(code, 404, "standalone {path}");
        assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
        assert_eq!(body, "Not found");
        assert_eq!(
            request(root.clone(), "GET", path, &[]).await.0,
            200,
            "shared root {path}"
        );
    }
}

#[tokio::test]
async fn camera_reel_paths_bypass_shared_trailing_slash_normalization() {
    let (state, _, _, _, _sub) = setup();
    let shared = Router::new()
        .route(
            "/camera-reel-service/api/users/{address}",
            get(
                |axum::extract::Path(address): axum::extract::Path<String>,
                 matched: axum::extract::MatchedPath,
                 axum::extract::OriginalUri(original): axum::extract::OriginalUri| async move {
                    axum::Json(json!({
                        "address": address,
                        "handler": matched.as_str().strip_prefix("/camera-reel-service").unwrap(),
                        "original": original.to_string(),
                    }))
                },
            ),
        )
        .route(
            "/camera-reel-service/api/docs/ui/",
            get(|| async { "docs" }),
        )
        .route("/other", get(|| async { "other" }));
    let app = http::mount_services(
        shared,
        state.clone(),
        http::standalone(state),
        Router::new(),
    );
    for (path, status) in [
        ("/camera-reel-service/api/users/example", StatusCode::OK),
        (
            "/camera-reel-service/api/users/example/",
            StatusCode::NOT_FOUND,
        ),
        ("/camera-reel-service/api/docs/ui/", StatusCode::OK),
        ("/camera-reel-service/api/docs/ui", StatusCode::NOT_FOUND),
        ("/other/", StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        if path == "/camera-reel-service/api/users/example" {
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(
                body,
                json!({
                    "address": "example",
                    "handler": "/api/users/{address}",
                    "original": "/camera-reel-service/api/users/example",
                })
            );
        }
    }
}
