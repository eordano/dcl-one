//! The scene workers' way to the third-party services scenes call.
//!
//! A scene in the browser explorer runs in a worker on the page's origin, and
//! the services scenes call (the fishing game's Colyseus room, the marketing
//! files bucket) answer no CORS preflight, so the browser refuses the calls.
//! The worker's fetch (`deploy/web/engine/scene_fetch.js`) reads `GET /_scene`
//! once, a map of host to a path prefix on this node, and sends what the map
//! names through this node instead: the node's own `/places` and
//! `/marketplace-api` routes stand in for those services, and every host in
//! `SCENE_PROXY_HOSTS` is forwarded under `/_scene/<host>/...`.
//!
//! The forward is narrow. GET, HEAD and POST only. Of the request, the headers
//! a scene's `fetch` and `signedFetch` carry (the content type, authorization,
//! the `x-identity-*` auth chain, ranges and validators) and nothing else, so
//! neither the page's cookies nor the node's own headers reach the upstream,
//! and a bounded body. Of the answer, its status, the body's type, range and
//! validators, and the body, bounded too; a `set-cookie` or a CORS grant never
//! pass. The upstream is dialled through the guarded client, like every
//! mirror: public hosts only, over pinned DNS. No redirect is followed: the
//! list names the only host a forward reaches, and a 3xx goes back with its
//! status and no `Location`.
//!
//! The route is public, so each client address has a budget of forwards and
//! the node forwards a bounded number at once. The address is the one the
//! server pins (`shared::client_ip`), never a header a caller chooses.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{any, get};
use futures::StreamExt;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::comms::limits::{RateLimitDecision, RateLimiter};
use crate::http::Outbound;
use crate::shared::client_ip::{client_ip, source_key};

pub type Shared = Arc<SceneProxy>;

/// Upstream services this node serves itself: the prefix stands in for the host.
const FIRST_PARTY: &[(&str, &str)] = &[
    ("places.decentraland.org", "/places"),
    ("marketplace-api.decentraland.org", "/marketplace-api"),
];

const REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "content-type",
    "authorization",
    "range",
    "if-none-match",
    "if-modified-since",
];
const IDENTITY_HEADER_PREFIX: &str = "x-identity-";
const RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "cache-control",
    "etag",
    "last-modified",
    "vary",
];
/// The body a hosted scene's fetch may send (`fetch_max_body_bytes` of the
/// scene host) and the gateway's cap for a route with none of its own, so a
/// scene meets one limit wherever it runs. The body is held in memory.
const REQUEST_BODY_LIMIT: usize = 1 << 20;
/// What one forward may relay. The answer is a stream, so this bounds the
/// node's bandwidth for one request, not its memory. It is above the 2 MiB a
/// hosted scene's fetch may read because the list names a files bucket.
const RESPONSE_BODY_LIMIT: usize = 32 << 20;
/// How long one forward holds its upstream connection and its place among
/// `CONCURRENT_FORWARDS`. It is above the 15 seconds the native scene server
/// gives a scene request, so the proxy ends no request that deadline allows.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);
/// Forwards one client address may start in `FORWARD_WINDOW`. A scene worker
/// sends five requests at once (the Explorer's scene fetch limit): 300 is
/// those five slots turning over once a second for the whole window.
const FORWARDS_PER_WINDOW: u32 = 5 * 60;
const FORWARD_WINDOW: Duration = Duration::from_secs(60);
/// Forwards in flight at once, for all clients. Each holds a request body of
/// up to `REQUEST_BODY_LIMIT` and one upstream connection, so 64 is at most
/// 64 MiB of bodies: twelve scenes with every fetch slot on a listed host.
const CONCURRENT_FORWARDS: usize = 64;

pub struct SceneProxy {
    hosts: Vec<String>,
    routes: BTreeMap<String, String>,
    http: Outbound,
    scheme: &'static str,
    budget: RateLimiter,
    in_flight: Arc<Semaphore>,
}

impl SceneProxy {
    pub fn new(hosts: impl IntoIterator<Item = String>, http: Outbound) -> Self {
        let hosts: Vec<String> = hosts
            .into_iter()
            .map(|host| host.trim().to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect();
        let mut routes: BTreeMap<String, String> = FIRST_PARTY
            .iter()
            .map(|(host, prefix)| (host.to_string(), prefix.to_string()))
            .collect();
        for host in &hosts {
            routes.insert(host.clone(), format!("/_scene/{host}"));
        }
        Self {
            hosts,
            routes,
            http,
            scheme: "https",
            budget: RateLimiter::new(FORWARDS_PER_WINDOW, FORWARD_WINDOW),
            in_flight: Arc::new(Semaphore::new(CONCURRENT_FORWARDS)),
        }
    }

    #[cfg(test)]
    fn over_plain_http(mut self) -> Self {
        self.scheme = "http";
        self
    }

    /// The listed host that `host` names, in the list's own spelling.
    fn listed(&self, host: &str) -> Option<&str> {
        self.hosts
            .iter()
            .find(|listed| listed.eq_ignore_ascii_case(host))
            .map(String::as_str)
    }
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(routing_map))
        .route("/{host}/{*path}", any(forward))
        .with_state(state)
}

/// The map the workers read: host to the prefix on this node that stands in for it.
/// A browser keeps it for a minute, the pause after which a page with no map
/// asks again (`ROUTES_RETRY_MS`): a reload within the minute reads none, and a
/// new list reaches a page within a minute of the restart that sets it.
async fn routing_map(State(state): State<Shared>) -> Response {
    (
        [(header::CACHE_CONTROL, "public, max-age=60")],
        Json(json!({ "routes": state.routes })),
    )
        .into_response()
}

async fn forward(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Under the mount the request path is `/<host>/<path>`. The path goes
    // upstream as the client sent it: a decoded path would turn an encoded
    // `?`, `#` or `/` into URL structure. The host of the URL is the listed one.
    let Some((host, path)) = uri
        .path()
        .strip_prefix('/')
        .and_then(|tail| tail.split_once('/'))
        .and_then(|(host, path)| Some((state.listed(host)?, path)))
    else {
        return (StatusCode::NOT_FOUND, "no such scene service").into_response();
    };
    if !matches!(method, Method::GET | Method::HEAD | Method::POST) {
        return (StatusCode::METHOD_NOT_ALLOWED, "GET, HEAD or POST").into_response();
    }
    let source = source_key(&client_ip(&headers));
    if state.budget.check(&source) == RateLimitDecision::Deny {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many scene requests from this address",
        )
            .into_response();
    }
    let Ok(permit) = state.in_flight.clone().try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "the scene proxy is busy").into_response();
    };
    let body = match axum::body::to_bytes(body, REQUEST_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "request body over the limit").into_response();
        }
    };
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("{}://{host}/{path}{query}", state.scheme);
    let mut request = state
        .http
        .request(method, &url)
        .timeout(UPSTREAM_TIMEOUT)
        .max_redirects(0)
        .headers(forwarded(&headers));
    if !body.is_empty() {
        request = request.body(body);
    }
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(host, "scene proxy: the upstream did not answer: {e}");
            return (StatusCode::BAD_GATEWAY, "the scene service did not answer").into_response();
        }
    };
    let status = upstream.status();
    let mut head = HeaderMap::new();
    for name in RESPONSE_HEADERS {
        for value in upstream.headers().get_all(*name) {
            head.append(HeaderName::from_static(name), value.clone());
        }
    }
    // Another origin's content answered on this one: opened as a page it runs
    // sandboxed, and its declared type is the only one a browser reads.
    head.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("sandbox"),
    );
    head.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    let mut seen = 0usize;
    let body = Body::from_stream(upstream.bytes_stream().map(move |chunk| {
        // The forward is in flight until its body ends or is dropped.
        let _in_flight = &permit;
        match chunk {
            Ok(bytes) => {
                seen += bytes.len();
                if seen > RESPONSE_BODY_LIMIT {
                    Err(std::io::Error::other("scene service answer over the limit"))
                } else {
                    Ok(bytes)
                }
            }
            Err(e) => Err(std::io::Error::other(e.without_url())),
        }
    }));
    (status, head, body).into_response()
}

/// The request headers that go upstream: the allow-list and the signed-fetch
/// identity headers, so nothing else of the request leaks.
fn forwarded(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if REQUEST_HEADERS.contains(&name.as_str())
            || name.as_str().starts_with(IDENTITY_HEADER_PREFIX)
        {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    struct Seen {
        method: String,
        path: String,
        headers: Vec<String>,
        body: String,
    }

    /// A loopback upstream recording what reaches it and answering with headers a
    /// scene must not see beside the ones it should.
    async fn stub() -> (String, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let app = Router::new().route(
            "/{*path}",
            any(
                move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
                    let record = record.clone();
                    async move {
                        record.lock().unwrap().push(Seen {
                            method: method.to_string(),
                            path: uri
                                .path_and_query()
                                .map(|p| p.to_string())
                                .unwrap_or_default(),
                            headers: headers.keys().map(|k| k.to_string()).collect(),
                            body,
                        });
                        (
                            StatusCode::CREATED,
                            [
                                ("content-type", "application/json"),
                                ("cache-control", "max-age=60"),
                                ("set-cookie", "upstream=1"),
                                ("access-control-allow-origin", "*"),
                            ],
                            r#"{"ok":true}"#,
                        )
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr.to_string(), seen)
    }

    fn proxy(hosts: &[&str]) -> SceneProxy {
        SceneProxy::new(
            hosts.iter().map(|host| host.to_string()),
            Outbound::for_tests(),
        )
        .over_plain_http()
    }

    fn app(hosts: &[&str]) -> Router {
        Router::new().nest("/_scene", router(Arc::new(proxy(hosts))))
    }

    async fn text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn the_map_names_the_nodes_own_routes_and_the_allow_listed_hosts() {
        let app = app(&[
            "us-mia-43272b05.colyseus.cloud",
            " Marketing-Files.decentraland.org ",
            "",
        ]);
        let response = app
            .oneshot(Request::get("/_scene").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("cache-control").unwrap(),
            "public, max-age=60"
        );
        let map: serde_json::Value = serde_json::from_str(&text(response).await).unwrap();
        let routes = map["routes"].as_object().unwrap();
        assert_eq!(routes["places.decentraland.org"], "/places");
        assert_eq!(
            routes["marketplace-api.decentraland.org"],
            "/marketplace-api"
        );
        assert_eq!(
            routes["us-mia-43272b05.colyseus.cloud"],
            "/_scene/us-mia-43272b05.colyseus.cloud"
        );
        assert_eq!(
            routes["marketing-files.decentraland.org"],
            "/_scene/marketing-files.decentraland.org"
        );
        assert_eq!(routes.len(), 4);
    }

    #[tokio::test]
    async fn a_host_off_the_list_and_a_method_off_the_three_are_refused() {
        let app = app(&["example.invalid"]);
        let response = app
            .clone()
            .oneshot(
                Request::get("/_scene/other.invalid/x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app
            .oneshot(
                Request::put("/_scene/example.invalid/x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn the_forward_carries_the_scenes_headers_and_body_and_nothing_of_the_pages() {
        let (upstream, seen) = stub().await;
        let app = app(&[&upstream]);
        let request = Request::post(format!(
            "/_scene/{upstream}/matchmake/joinOrCreate/fishing_room?x=1"
        ))
        .header("content-type", "application/json")
        .header("x-identity-auth-chain-0", "{}")
        .header("x-identity-timestamp", "1")
        .header("cookie", "session=secret")
        .header("origin", "https://site.example")
        .header("x-forwarded-for", "203.0.113.9")
        .body(Body::from(r#"{"hello":"room"}"#))
        .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
        assert_eq!(
            response.headers().get("cache-control").unwrap(),
            "max-age=60"
        );
        assert_eq!(
            response.headers().get("content-security-policy").unwrap(),
            "sandbox"
        );
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert!(response.headers().get("set-cookie").is_none());
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        assert_eq!(text(response).await, r#"{"ok":true}"#);
        let seen = seen.lock().unwrap();
        let call = &seen[0];
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/matchmake/joinOrCreate/fishing_room?x=1");
        assert_eq!(call.body, r#"{"hello":"room"}"#);
        for carried in [
            "content-type",
            "x-identity-auth-chain-0",
            "x-identity-timestamp",
        ] {
            assert!(
                call.headers.iter().any(|h| h == carried),
                "{carried} missing: {:?}",
                call.headers
            );
        }
        for kept_here in ["cookie", "origin", "x-forwarded-for"] {
            assert!(
                !call.headers.iter().any(|h| h == kept_here),
                "{kept_here} reached the upstream"
            );
        }
    }

    #[tokio::test]
    async fn an_encoded_path_reaches_the_upstream_as_the_client_sent_it() {
        let (upstream, seen) = stub().await;
        let app = app(&[&upstream]);
        let response = app
            .oneshot(
                Request::get(format!(
                    "/_scene/{upstream}/files/a%2Fb%3Fadmin%3D1%23c%20d?x=%26y"
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].path, "/files/a%2Fb%3Fadmin%3D1%23c%20d?x=%26y");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_refused_before_anything_leaves() {
        let (upstream, seen) = stub().await;
        let app = app(&[&upstream]);
        let response = app
            .oneshot(
                Request::post(format!("/_scene/{upstream}/x"))
                    .body(Body::from(vec![b'a'; REQUEST_BODY_LIMIT + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(seen.lock().unwrap().is_empty());
    }

    /// A loopback upstream that answers every request with a redirect to `to`.
    async fn redirecting_stub(to: String) -> String {
        let app = Router::new().route(
            "/{*path}",
            any(move || {
                let to = to.clone();
                async move { (StatusCode::TEMPORARY_REDIRECT, [("location", to)]) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr.to_string()
    }

    #[tokio::test]
    async fn a_redirect_is_answered_and_never_followed() {
        let (off_list, seen) = stub().await;
        let listed = redirecting_stub(format!("http://{off_list}/taken")).await;
        let app = app(&[&listed]);
        let response = app
            .oneshot(
                Request::post(format!("/_scene/{listed}/x"))
                    .header("x-identity-timestamp", "1")
                    .body(Body::from("secret"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert!(response.headers().get("location").is_none());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_address_over_its_budget_is_refused_and_another_is_served() {
        let (upstream, seen) = stub().await;
        let proxy = Arc::new(proxy(&[&upstream]));
        let app = Router::new().nest("/_scene", router(proxy.clone()));
        let from = |address: &str| {
            Request::get(format!("/_scene/{upstream}/x"))
                .header("x-real-ip", address)
                .body(Body::empty())
                .unwrap()
        };
        // Two addresses of one IPv6 /64 are one source.
        assert_eq!(
            proxy
                .budget
                .check_cost("2001:db8:0:1::/64", FORWARDS_PER_WINDOW),
            RateLimitDecision::Allow
        );
        for address in ["2001:db8:0:1::7", "2001:db8:0:1::8"] {
            let response = app.clone().oneshot(from(address)).await.unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        }
        assert!(seen.lock().unwrap().is_empty());
        let response = app.oneshot(from("203.0.113.9")).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn a_forward_past_the_concurrent_bound_is_refused_until_one_ends() {
        let (upstream, seen) = stub().await;
        let proxy = Arc::new(proxy(&[&upstream]));
        let app = Router::new().nest("/_scene", router(proxy.clone()));
        let get = || {
            Request::get(format!("/_scene/{upstream}/x"))
                .body(Body::empty())
                .unwrap()
        };
        let held = proxy
            .in_flight
            .clone()
            .acquire_many_owned(CONCURRENT_FORWARDS as u32 - 1)
            .await
            .unwrap();
        // The last place stays taken while the answer's body is unread.
        let open = app.clone().oneshot(get()).await.unwrap();
        assert_eq!(open.status(), StatusCode::CREATED);
        let response = app.clone().oneshot(get()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(seen.lock().unwrap().len(), 1);
        drop(open);
        let response = app.oneshot(get()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        drop(held);
    }
}
