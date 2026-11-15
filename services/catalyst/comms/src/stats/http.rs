//! Stats HTTP surface. Root merging claims only statistics paths; the standalone
//! router owns its complete fallback, methods and CORS contract.
use super::{Reader, content, render};
use axum::{
    Router,
    body::Body,
    extract::{Request, State as AxumState},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use serde::{Serialize, ser::SerializeMap};
use std::{sync::Arc, time::Instant};

/// A failed evaluation is not a nonmatch. The host owns compilation and execution.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MatcherError(pub String);

#[async_trait::async_trait]
pub trait MetricsMatcher: Send + Sync {
    async fn matches(&self, path: &str) -> Result<bool, MatcherError>;
}

/// An explicit literal fake for engine-free HTTP and socket tests.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    pub struct LiteralMatcher(pub String);
    #[async_trait::async_trait]
    impl MetricsMatcher for LiteralMatcher {
        async fn matches(&self, path: &str) -> Result<bool, MatcherError> {
            Ok(path.eq_ignore_ascii_case(&self.0)
                || path
                    .strip_suffix('/')
                    .is_some_and(|p| p.eq_ignore_ascii_case(&self.0)))
        }
    }
    pub fn matcher() -> Arc<dyn MetricsMatcher> {
        Arc::new(LiteralMatcher("/metrics".into()))
    }
}

#[derive(Clone)]
pub struct Settings {
    pub metrics_path: String,
    pub metrics_bearer_token: Option<String>,
    /// Base of `/hot-scenes` thumbnails. `serve` sets it from `CONTENT_URL` or this
    /// node's own `/content` (`stats_content_url`); the default serves tests.
    pub content_url: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            metrics_path: "/metrics".into(),
            metrics_bearer_token: None,
            content_url: "https://peer.decentraland.org/content/".into(),
        }
    }
}
impl Settings {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            metrics_path: std::env::var("WKC_METRICS_PUBLIC_PATH")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or(defaults.metrics_path),
            // The `_FILE` form too, read as the config's secret slot reads it.
            metrics_bearer_token: catalyst_config::config::Config::secret_from_env(
                "WKC_METRICS_BEARER_TOKEN",
            )
            .ok()
            .flatten(),
            content_url: defaults.content_url,
        }
    }
}
/// Lifecycle and named probe results from the running host, never inferred from peers.
/// The host maps rejected probes to false and supplies their registration order.
#[derive(Clone, Default)]
pub struct Lifecycle {
    pub initialized: bool,
    pub live: bool,
    pub started: bool,
    pub checks: Vec<(String, bool)>,
}
#[derive(Debug)]
pub struct Observation {
    pub method: String,
    pub handler: String,
    pub code: u16,
    pub request_size: f64,
    pub duration: std::time::Duration,
}
#[async_trait::async_trait]
pub trait Hooks: Send + Sync {
    async fn probe(&self, startup: bool) -> Lifecycle;
    /// Return the host's actual registry scrape. Reset-at-night/default collectors belong to it.
    async fn metrics(&self) -> Result<String, content::Error>;
    /// Existing dcl-one `/stats/health` diagnostics, supplied by the connector host.
    async fn diagnostics(&self) -> Result<String, content::Error>;
    fn observe(&self, observation: Observation);
}
#[derive(Clone)]
pub struct State {
    pub reader: Arc<Reader>,
    pub content: Arc<dyn content::Source>,
    pub hooks: Arc<dyn Hooks>,
    pub settings: Settings,
    pub matcher: Arc<dyn MetricsMatcher>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub environment: Arc<dyn Fn() -> (String, String) + Send + Sync>,
}
impl State {
    pub fn new(
        reader: Arc<Reader>,
        content: Arc<dyn content::Source>,
        hooks: Arc<dyn Hooks>,
        settings: Settings,
        matcher: Arc<dyn MetricsMatcher>,
    ) -> Self {
        Self {
            reader,
            content,
            hooks,
            settings,
            matcher,
            now: Arc::new(|| chrono::Utc::now().timestamp_millis()),
            environment: Arc::new(|| {
                (
                    std::env::var("CURRENT_VERSION").unwrap_or_default(),
                    std::env::var("COMMIT_HASH").unwrap_or_default(),
                )
            }),
        }
    }
}
pub fn standalone(state: State) -> Router {
    Router::new().fallback(dispatch).with_state(state)
}
/// Ordinary requests to the socket alias retain the standalone host's fallback.
pub fn socket_alias(router: Router, state: State) -> Router {
    router.layer(middleware::from_fn(move |request: Request, next: Next| {
        let state = state.clone();
        async move {
            let upgrade = request.method() == axum::http::Method::GET
                && request
                    .headers()
                    .get("upgrade")
                    .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
            if request.uri().path() == "/ws" && !upgrade {
                dispatch_scoped(state, request, false, Instant::now(), None).await
            } else {
                next.run(request).await
            }
        }
    }))
}
/// Install the complete public hosts outside shared path normalization and middleware.
pub fn mount_services(shared: Router, state: State, stats: Router, connector: Router) -> Router {
    // Camera Reel keeps its route spelling, including the Swagger UI directory.
    let camera_reel = shared.clone();
    let shared = tower_http::normalize_path::NormalizePath::trim_trailing_slash(shared);
    merge_root(
        Router::new()
            .fallback_service(shared)
            .nest("/archipelago-ea-stats", stats)
            .nest("/archipelago-ea-ws-connector", connector)
            .layer(middleware::from_fn(move |request: Request, next: Next| {
                let camera_reel = camera_reel.clone();
                async move {
                    let path = request.uri().path();
                    if path == "/camera-reel-service" || path.starts_with("/camera-reel-service/") {
                        // Dispatch without an outer route match: wildcard captures would leak
                        // into the inner Path extractor and alter its metrics route label.
                        use tower::ServiceExt;
                        camera_reel.oneshot(request).await.unwrap()
                    } else {
                        next.run(request).await
                    }
                }
            })),
        state,
    )
}
/// Apply after assembling root routes. Unknown paths, `/status`, `/ping`, health
/// probes and metrics remain the other services' responsibility at the shared root.
pub fn merge_root<S: Clone + Send + Sync + 'static>(router: Router<S>, state: State) -> Router<S> {
    router.layer(middleware::from_fn(move |request: Request, next: Next| {
        let state = state.clone();
        async move {
            let candidate = structural_route(request.uri().path(), true);
            if !candidate.as_ref().is_some_and(Route::root) {
                return next.run(request).await;
            }
            let selection_started = Instant::now();
            let selected_path = request.uri().path().to_owned();
            let selected = match state.matcher.matches(&selected_path).await {
                Ok(matched) => !matched,
                Err(error) => {
                    return matcher_failure(
                        &state,
                        error,
                        request.method().as_str(),
                        request.headers(),
                        selection_started.elapsed(),
                    );
                }
            };
            if selected {
                dispatch_scoped(
                    state,
                    request,
                    true,
                    selection_started,
                    Some(&selected_path),
                )
                .await
            } else {
                next.run(request).await
            }
        }
    }))
}
#[derive(Clone)]
enum Route {
    Parcels,
    Peers,
    Peer(String),
    Islands,
    Island(String),
    HotScenes,
    CoreStatus,
    Status,
    Live,
    Ready,
    Startup,
    Metrics,
    Ping,
    Diagnostics,
}
impl Route {
    fn root(&self) -> bool {
        matches!(
            self,
            Self::Parcels
                | Self::Peers
                | Self::Peer(_)
                | Self::Islands
                | Self::Island(_)
                | Self::HotScenes
                | Self::CoreStatus
                | Self::Diagnostics
        )
    }
    fn health(&self) -> bool {
        matches!(self, Self::Live | Self::Ready | Self::Startup)
    }
}
async fn route(
    path: &str,
    state: &State,
    shared_root: bool,
    previous_nonmatch: Option<&str>,
) -> Result<Option<Route>, MatcherError> {
    let structural = structural_route(path, shared_root);
    if structural.as_ref().is_some_and(Route::health) {
        return Ok(structural);
    }
    if previous_nonmatch != Some(path) && state.matcher.matches(path).await? {
        return Ok(Some(Route::Metrics));
    }
    Ok(structural)
}
fn structural_route(path: &str, shared_root: bool) -> Option<Route> {
    let path = path.strip_suffix('/').unwrap_or(path);
    // Probe and metrics middleware run before the application router.
    for (p, r) in [
        ("/health/live", Route::Live),
        ("/health/ready", Route::Ready),
        ("/health/startup", Route::Startup),
    ] {
        if path.eq_ignore_ascii_case(p) {
            return Some(r);
        }
    }
    for prefix in ["", "/comms", "/stats"] {
        if prefix == "/stats" && !shared_root {
            continue;
        }
        for (suffix, list) in [
            ("parcels", Route::Parcels),
            ("peers", Route::Peers),
            ("islands", Route::Islands),
        ] {
            let pattern = format!("{prefix}/{suffix}");
            if path.eq_ignore_ascii_case(&pattern) {
                return Some(list);
            }
            if suffix != "parcels"
                && path.len() > pattern.len() + 1
                && path
                    .get(..pattern.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(&pattern))
                && path.as_bytes()[pattern.len()] == b'/'
            {
                let id = &path[pattern.len() + 1..];
                if !id.contains('/') {
                    return Some(if suffix == "peers" {
                        Route::Peer(decode_path(id))
                    } else {
                        Route::Island(decode_path(id))
                    });
                }
            }
        }
    }
    for (p, r) in [
        ("/hot-scenes", Route::HotScenes),
        ("/core-status", Route::CoreStatus),
        ("/status", Route::Status),
        ("/ping", Route::Ping),
        ("/stats/health", Route::Diagnostics),
    ] {
        if path.eq_ignore_ascii_case(p) && (shared_root || !matches!(r, Route::Diagnostics)) {
            return Some(r);
        }
    }
    None
}
fn matcher_failure(
    state: &State,
    error: MatcherError,
    method: &str,
    headers: &HeaderMap,
    duration: std::time::Duration,
) -> Response {
    tracing::error!(%error, "statistics metrics route evaluation failed");
    if NODE_METHODS.contains(&method) && !["CONNECT", "TRACE"].contains(&method) {
        state.hooks.observe(Observation {
            method: method.into(),
            handler: String::new(),
            code: 200,
            request_size: headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.),
            // A failed selection has no handler interval; record its actual elapsed work.
            duration,
        });
    }
    Reply::empty(500).response(method == "HEAD")
}
fn decode_path(id: &str) -> String {
    let mut bytes = Vec::with_capacity(id.len());
    let raw = id.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            if i + 2 >= raw.len() {
                return id.into();
            }
            let hex = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
            let (Some(a), Some(b)) = (hex(raw[i + 1]), hex(raw[i + 2])) else {
                return id.into();
            };
            bytes.push(a * 16 + b);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|_| id.into())
}
struct Reply {
    status: u16,
    body: Option<String>,
    kind: Option<&'static str>,
    explicit_type: bool,
    headers: HeaderMap,
}
impl Reply {
    fn empty(status: u16) -> Self {
        Self {
            status,
            body: None,
            kind: None,
            explicit_type: false,
            headers: HeaderMap::new(),
        }
    }
    fn json(status: u16, body: String) -> Self {
        Self {
            body: Some(body),
            kind: Some("application/json"),
            ..Self::empty(status)
        }
    }
    fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            body: Some(body.into()),
            kind: Some("text/plain; charset=utf-8"),
            ..Self::empty(status)
        }
    }
    fn header(mut self, key: &'static str, value: &str) -> Self {
        self.headers.insert(
            key,
            HeaderValue::from_str(value).expect("statistics header"),
        );
        self
    }
    fn explicit(mut self, kind: &'static str) -> Self {
        self.kind = Some(kind);
        self.explicit_type = true;
        self
    }
    fn response(mut self, head: bool) -> Response {
        let suppressed = head || self.status == 204;
        if self.status != 204
            && (self.explicit_type || !head)
            && let Some(kind) = self.kind
        {
            self.headers
                .insert("content-type", HeaderValue::from_static(kind));
        }
        let body = if suppressed {
            // Unknown size avoids Axum synthesizing a Content-Length absent from HEAD/204.
            Body::from_stream(futures::stream::empty::<Result<bytes::Bytes, std::io::Error>>())
        } else {
            let body = self.body.unwrap_or_default();
            self.headers.insert(
                "content-length",
                HeaderValue::from_str(&body.len().to_string()).unwrap(),
            );
            Body::from(body)
        };
        let mut response = Response::new(body);
        *response.status_mut() = StatusCode::from_u16(self.status).unwrap();
        *response.headers_mut() = self.headers;
        response
    }
}
fn cors(reply: &mut Reply, headers: &HeaderMap, options: bool) {
    if let Some(origin) = headers.get("origin").filter(|origin| !origin.is_empty()) {
        reply
            .headers
            .insert("access-control-allow-origin", origin.clone());
        reply
            .headers
            .insert("vary", HeaderValue::from_static("Origin"));
    }
    if options || headers.contains_key("origin") {
        reply.headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    if options {
        for (k, v) in [
            (
                "access-control-allow-methods",
                "GET,HEAD,POST,PUT,DELETE,CONNECT,TRACE,PATCH",
            ),
            (
                "access-control-allow-headers",
                "Cache-Control,Content-Type,Origin,Accept,User-Agent,X-Upload-Origin",
            ),
            ("access-control-max-age", "86400"),
        ] {
            reply.headers.insert(k, HeaderValue::from_static(v));
        }
    }
}
async fn dispatch(AxumState(state): AxumState<State>, request: Request) -> Response {
    dispatch_scoped(state, request, false, Instant::now(), None).await
}
// The HTTP/1 parser's finite method set; native Request rejects TRACE before
// application middleware. Preserve accepted extension labels rather than inventing
// a label for tokens that never reach the original statistics middleware.
// Connector metrics also use this finite set as an explicit cardinality policy;
// the connector's native HTTP method admission remains unchanged.
pub(crate) const NODE_METHODS: &[&str] = &[
    "ACL",
    "BIND",
    "CHECKOUT",
    "CONNECT",
    "COPY",
    "DELETE",
    "GET",
    "HEAD",
    "LINK",
    "LOCK",
    "M-SEARCH",
    "MERGE",
    "MKACTIVITY",
    "MKCALENDAR",
    "MKCOL",
    "MOVE",
    "NOTIFY",
    "OPTIONS",
    "PATCH",
    "POST",
    "PROPFIND",
    "PROPPATCH",
    "PURGE",
    "PUT",
    "QUERY",
    "REBIND",
    "REPORT",
    "SEARCH",
    "SOURCE",
    "SUBSCRIBE",
    "TRACE",
    "UNBIND",
    "UNLINK",
    "UNLOCK",
    "UNSUBSCRIBE",
];
fn rejected_method(method: &str) -> Option<Reply> {
    if !NODE_METHODS.contains(&method) {
        Some(Reply::empty(400))
    } else if method == "TRACE" {
        Some(Reply::empty(500))
    } else {
        None
    }
}
async fn dispatch_scoped(
    state: State,
    request: Request,
    shared_root: bool,
    start: Instant,
    previous_nonmatch: Option<&str>,
) -> Response {
    let (request, _body) = request.into_parts();
    let method = request.method.as_str();
    if let Some(reply) = rejected_method(method) {
        return reply.response(false);
    }
    let head = method == "HEAD";
    if method == "OPTIONS" {
        let mut reply = Reply::empty(204);
        cors(&mut reply, &request.headers, true);
        return reply.response(false);
    }
    // URL parsing normalizes dot segments while retaining encoded slashes and repeats.
    let normalized =
        url::Url::parse(&format!("http://statistics.invalid{}", request.uri.path())).ok();
    let path = normalized
        .as_ref()
        .map_or(request.uri.path(), url::Url::path);
    let matched = match route(path, &state, shared_root, previous_nonmatch).await {
        Ok(route) => route,
        Err(error) => {
            return matcher_failure(&state, error, method, &request.headers, start.elapsed());
        }
    };
    let result = if !["GET", "HEAD", "PUT", "PATCH", "POST", "DELETE"].contains(&method) {
        Ok(Reply::empty(501).header("allow", if matched.is_some() { "HEAD, GET" } else { "" }))
    } else if matched.is_some() && !matches!(method, "GET" | "HEAD") {
        Ok(Reply::empty(405).header("allow", "HEAD, GET"))
    } else {
        handle(&state, matched.as_ref(), &request).await
    };
    // Health GET/HEAD short-circuits the registry middleware; OPTIONS short-circuits CORS.
    if method != "CONNECT"
        && !matched
            .as_ref()
            .is_some_and(|r| r.health() && matches!(method, "GET" | "HEAD"))
    {
        let mut handler = path.strip_suffix('/').unwrap_or(path).to_string();
        if matches!(matched, Some(Route::Peer(_)) | Some(Route::Island(_))) {
            handler.truncate(handler.rfind('/').unwrap() + 1);
            handler.push_str(":id");
        }
        handler.make_ascii_lowercase();
        if matched.is_none() || !matches!(method, "GET" | "HEAD") {
            handler.clear();
        } else if matches!(matched, Some(Route::Metrics)) {
            handler = state.settings.metrics_path.clone();
        }
        state.hooks.observe(Observation {
            method: method.into(),
            handler,
            code: result.as_ref().map_or(200, |r| r.status),
            request_size: request
                .headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.),
            duration: start.elapsed(),
        });
    }
    match result {
        Ok(mut reply) => {
            cors(&mut reply, &request.headers, false);
            reply.response(head)
        }
        Err(_) => Reply::empty(500).response(head),
    }
}
async fn handle(
    state: &State,
    route: Option<&Route>,
    request: &axum::http::request::Parts,
) -> Result<Reply, content::Error> {
    let snapshot = state.reader.snapshot();
    Ok(match route {
        Some(Route::Parcels) => Reply::json(200, render::parcels(&snapshot)),
        Some(Route::Peers) => {
            let ids = url::form_urlencoded::parse(request.uri.query().unwrap_or("").as_bytes())
                .filter_map(|(k, v)| (k == "id").then(|| v.into_owned()))
                .collect::<Vec<_>>();
            Reply::json(200, render::peers(&snapshot, &ids))
        }
        Some(Route::Peer(id)) => {
            let (status, body) = render::peer(&snapshot, id);
            Reply::json(status, body)
        }
        Some(Route::Islands) => Reply::json(200, render::islands(&snapshot)),
        Some(Route::Island(id)) => render::island(&snapshot, id)
            .map_or_else(|| Reply::empty(404), |body| Reply::json(200, body)),
        Some(Route::CoreStatus) => Reply::json(200, render::core_status(&snapshot, (state.now)())),
        Some(Route::HotScenes) => {
            let tiles = render::tiles(&snapshot);
            let scenes = if tiles.is_empty() {
                Vec::new()
            } else {
                state
                    .content
                    .fetch(
                        &tiles
                            .iter()
                            .map(|(tile, _)| tile.clone())
                            .collect::<Vec<_>>(),
                    )
                    .await?
            };
            Reply::json(
                200,
                content::hot_scenes(&scenes, &tiles, &state.settings.content_url)?,
            )
        }
        Some(Route::Status) => {
            #[derive(Serialize)]
            struct Status {
                version: String,
                #[serde(rename = "currentTime")]
                current_time: i64,
                #[serde(rename = "commitHash")]
                commit_hash: String,
            }
            let (version, commit_hash) = (state.environment)();
            Reply::json(
                200,
                render::json(&Status {
                    version,
                    current_time: (state.now)(),
                    commit_hash,
                }),
            )
            .header("access-control-allow-origin", "*")
        }
        Some(Route::Live) => Reply::text(200, "alive"),
        Some(Route::Ready) | Some(Route::Startup) => {
            let startup = matches!(route, Some(Route::Startup));
            let lifecycle = state.hooks.probe(startup).await;
            probe_reply(lifecycle, startup).explicit("application/health+json; charset=utf-8")
        }
        Some(Route::Metrics) => {
            if let Some(token) = state
                .settings
                .metrics_bearer_token
                .as_ref()
                .filter(|s| !s.is_empty())
            {
                let mut auth = request
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .split(' ');
                if auth.next() != Some("Bearer")
                    || !auth
                        .next()
                        .is_some_and(|candidate| token_matches(token, candidate))
                {
                    return Ok(Reply::empty(401));
                }
            }
            Reply::text(200, state.hooks.metrics().await?)
                .explicit("text/plain; version=0.0.4; charset=utf-8")
        }
        Some(Route::Ping) => Reply::text(200, "/ping"),
        Some(Route::Diagnostics) => Reply::json(200, state.hooks.diagnostics().await?),
        None => Reply::text(404, "Not found"),
    })
}
fn probe_reply(lifecycle: Lifecycle, startup: bool) -> Reply {
    if !lifecycle.initialized {
        return Reply::json(
            503,
            format!(
                r#"{{"status":"{}"}}"#,
                if startup {
                    "bootstrapping"
                } else {
                    "initializing"
                }
            ),
        );
    }
    if !lifecycle.started {
        return if startup {
            Reply::json(503, r#"{"status":"starting"}"#.into())
        } else {
            Reply::text(503, if lifecycle.live { "unready" } else { "waiting" })
        };
    }
    struct Details(Vec<(String, bool)>);
    impl Serialize for Details {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            #[derive(Serialize)]
            struct Check {
                status: &'static str,
            }
            let mut map = s.serialize_map(Some(self.0.len()))?;
            for (name, pass) in &self.0 {
                map.serialize_entry(
                    name,
                    &Check {
                        status: if *pass { "pass" } else { "fail" },
                    },
                )?;
            }
            map.end()
        }
    }
    #[derive(Serialize)]
    struct Probe {
        details: Details,
        status: &'static str,
    }
    let pass = lifecycle.checks.iter().all(|(_, pass)| *pass);
    Reply::json(
        if pass { 200 } else { 503 },
        render::json(&Probe {
            details: Details(lifecycle.checks),
            status: if pass { "pass" } else { "fail" },
        }),
    )
}

pub(crate) fn token_matches(expected: &str, candidate: &str) -> bool {
    use sha2::{Digest, Sha256, digest::CtOutput};
    // Compare fixed-length digests in constant time, including differing token lengths.
    CtOutput::<Sha256>::new(Sha256::digest(expected.as_bytes()))
        == CtOutput::<Sha256>::new(Sha256::digest(candidate.as_bytes()))
}
