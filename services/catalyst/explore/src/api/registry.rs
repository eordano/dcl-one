//! Registry gateway with uncached source responses. Profiles are served by Catalyst
//! itself (`shop::market::profiles::registry`); the read routes below go to the
//! backend named by `EXPLORER_REGISTRY_SOURCE_URL`:
//!
//! - a loopback URL is the local abgen runtime with native Registry state; it
//!   also receives the signed-fetch headers and the bound `x-original-path`;
//! - any other URL is a public Registry upstream behind the usual source SSRF
//!   guard; it receives only the request itself;
//! - empty answers the boot reads (`/entities/active`, `/entities/versions`)
//!   with `200 []`, `/denylist` with the node's own rows, and the other reads
//!   with 503.
//!
//! `GET /denylist` merges the source's list with this node's
//! `content.denylist` rows ([`denylist`]), cached briefly in [`LocalDenylist`].
//!
//! The boot reads check the Registry's request rule first, with every source
//! ([`pointers_rule`]): a body without a non-empty `pointers` answers the
//! Registry's `400 {"ok":false,"message":"No pointers provided"}` (`ids` is
//! not read), and a body that is not JSON, or is JSON `null`, its `500`. Their
//! bodies are capped at [`BOOT_READ_BODY_LIMIT`]. An `/entities/active` answer
//! drops every entity whose id is on this node's content denylist, as the
//! content reads do ([`crate::content::queries::retain_non_denylisted`]); that
//! read forwards no conditional request headers, and an answer the filter
//! changed loses the backend's `ETag` and `Last-Modified`.
//!
//! Registry writes (moderation, queue controls, registration, cache flush)
//! are not exposed here: the abgen admin bearer stays on this host and those
//! operations go through the ops console. No `Authorization` header is forwarded.
//!
//! Both halves of `/asset-bundle-registry` answer CORS as the Registry's own
//! http-server does ([`cors`]). The read half also answers the
//! well-known-components status checks (`/health/live`, `/health/ready`,
//! `/health/startup`) and holds the prefix's fallback, so a path neither half
//! routes still gets the Registry's CORS and its `404 Not found`.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, OriginalUri, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::{Settings, Shared};

/// Largest backend response buffered for one request. Registry answers are
/// far smaller; this bounds the worst case at `IN_FLIGHT * BODY_CAP` (512 MiB).
const BODY_CAP: usize = 8 << 20;
/// Backend requests in flight at once, so the buffered responses stay bounded
/// at `IN_FLIGHT * BODY_CAP`. Every configured-source read is a backend call.
const IN_FLIGHT: usize = 64;
/// How long a read waits for a free slot before it is refused with 503: a
/// boot burst queues briefly instead of failing at once.
const SLOT_WAIT: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(30);
static PERMITS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(IN_FLIGHT));
/// Largest `/entities/active` or `/entities/versions` request body. The
/// largest a client sends is unity-explorer's world read
/// (`decentraland/unity-explorer@59c3766`, `LoadFixedPointersSystem`): every
/// occupied parcel of the world manifest in one `{"pointers":["x,y",...]}`
/// body, at most the 301 x 301 parcels world coordinates allow
/// (`MAX_PARCEL_COORDINATE`), 865,088 bytes. Its scene (60 parcels) and
/// wearable (200 URNs) batches are far smaller.
const BOOT_READ_BODY_LIMIT: usize = 1024 * 1024;

pub fn routes(state: Shared) -> Router {
    Router::new()
        .route(
            "/entities/active",
            post(active).layer(DefaultBodyLimit::max(BOOT_READ_BODY_LIMIT)),
        )
        .route(
            "/entities/versions",
            post(versions).layer(DefaultBodyLimit::max(BOOT_READ_BODY_LIMIT)),
        )
        .route("/entities/status", get(read))
        .route("/entities/status/{id}", get(read))
        .route("/worlds/{world_name}/manifest", get(read))
        .route("/denylist", get(denylist))
        .route("/queues/status", get(read))
        .route("/status", get(status))
        .route("/health", get(health))
        .with_state(state)
        // The Registry names its http server `server` (`components.ts`).
        .merge(crate::shared::health::status_checks_router_with(
            crate::shared::health::SERVER_PASS,
        ))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(cors))
}

/// `@dcl/http-server`'s answer for a path no route serves
/// (`captures/abr/AB-12-unknown-route.json`).
async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

/// The Registry's CORS: `@dcl/http-server` with the methods its
/// `createServerComponent` lists and `maxAge: 86400` (`components.ts`),
/// captured as `captures/abr/AB-12-preflight.json`
/// ([`crate::shared::cors::HttpServerCors`]).
const CORS: crate::shared::cors::HttpServerCors = crate::shared::cors::HttpServerCors {
    methods: Some("GET,HEAD,OPTIONS,DELETE,POST,PUT"),
    max_age: "86400",
    nosniff: false,
    preflight_echoes_request: false,
};

/// The Registry's CORS ([`CORS`]): every `OPTIONS` is a 204 preflight with
/// origin `*`, with or without `Origin`; any other response gets origin `*`
/// only when the request carries an `Origin`, and never exposed headers.
pub async fn cors(req: Request, next: Next) -> Response {
    CORS.run(req, next).await
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    /// The local abgen runtime (loopback, the same rule as `ABGEN_ADMIN_URL`).
    Native(url::Url),
    /// A public Registry; the caller still applies the source SSRF guard.
    Upstream(url::Url),
}

impl Backend {
    /// The structural rules every Registry source meets: `http(s)`, a host,
    /// no credentials, query or fragment. A loopback host is native.
    pub(crate) fn parse(base: &str) -> anyhow::Result<Self> {
        let url = url::Url::parse(base)?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "a Registry source is an http(s) base URL without credentials, query or fragment"
        );
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            None => false,
        };
        Ok(if loopback {
            Self::Native(url)
        } else {
            Self::Upstream(url)
        })
    }
}

/// `POST /entities/active`: the backend's entities without those on this
/// node's content denylist. The backend's validators describe its unfiltered
/// answer, so no conditional request reaches it (a `304` would skip the
/// filter) and a filtered answer drops its `ETag` and `Last-Modified`.
async fn active(
    State(state): State<Shared>,
    OriginalUri(original_uri): OriginalUri,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = pointers_rule(&body) {
        return response;
    }
    if state.settings.sources.registry.is_empty() {
        return Json(json!([])).into_response();
    }
    match forward(
        &state,
        &original_uri,
        method,
        &uri,
        &headers,
        body,
        Conditional::Strip,
    )
    .await
    {
        Ok((status, mut out_headers, bytes)) if status == StatusCode::OK => {
            let bytes = match without_denylisted(&bytes, &state.content.denylist) {
                Some(filtered) => {
                    out_headers.remove(header::ETAG);
                    out_headers.remove(header::LAST_MODIFIED);
                    filtered
                }
                None => bytes,
            };
            (status, out_headers, bytes).into_response()
        }
        Ok(answer) => answer.into_response(),
        Err(response) => response,
    }
}

/// `POST /entities/versions`: the backend's answer as received.
async fn versions(
    state: State<Shared>,
    original_uri: OriginalUri,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = pointers_rule(&body) {
        return response;
    }
    if state.settings.sources.registry.is_empty() {
        return Json(json!([])).into_response();
    }
    read(state, original_uri, method, uri, headers, body).await
}

/// The Registry's request rule for the boot reads (`get-active-entities.ts`
/// and `get-entity-versions.ts`): `await context.request.json()` reads the
/// body as JSON whatever its `Content-Type`, then `body.pointers` and
/// `!pointers || pointers?.length === 0` answer the 400. A body that is not
/// JSON, or is JSON `null` (`null.pointers` throws), is the http-commons
/// `500`; any other non-object body has no `pointers`. `ids` is not read, and
/// the number of pointers is not capped.
///
/// Only `pointers` is kept, as raw text: the rest of the body is scanned
/// without being built, so a large body costs no more than its bytes.
fn pointers_rule(body: &[u8]) -> Result<(), Response> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let shape = Field("pointers")
        .deserialize(&mut de)
        .and_then(|shape| de.end().map(|()| shape));
    let present = match shape {
        // `JSON.parse` reads a number beyond `f64` (`1e400`) as an infinity,
        // which has no `pointers`; serde_json refuses it as a value.
        Err(_) if is_number(body) => false,
        Err(_) | Ok(Shape::Null) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"Internal Server Error"})),
            )
                .into_response());
        }
        Ok(Shape::Other | Shape::Object(None)) => false,
        Ok(Shape::Object(Some(pointers))) => pointers_given(pointers.get()),
    };
    if present {
        Ok(())
    } else {
        // Written out: `json!` would sort the keys, the Registry sends `ok` first.
        Err((
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"ok":false,"message":"No pointers provided"}"#,
        )
            .into_response())
    }
}

/// `!(!pointers || pointers?.length === 0)` for one JSON value's text: truthy,
/// and an array non-empty, an object without a `length` that is exactly `0`.
fn pointers_given(text: &str) -> bool {
    match text.as_bytes().first() {
        Some(b'[') => !text[1..].trim_start().starts_with(']'),
        Some(b'{') => {
            let mut de = serde_json::Deserializer::from_str(text);
            !matches!(
                Field("length").deserialize(&mut de),
                Ok(Shape::Object(Some(length))) if is_zero(length.get())
            )
        }
        Some(b'"') => text != r#""""#,
        Some(b't') => true,
        Some(b'f' | b'n') => false,
        // A number, falsy only at zero. JavaScript reads one beyond `f64`
        // as an infinity, as Rust's parser does.
        _ => !is_zero(text),
    }
}

/// The body is one JSON number, whatever its range.
fn is_number(body: &[u8]) -> bool {
    matches!(body.trim_ascii_start().first(), Some(b'-' | b'0'..=b'9'))
        && serde_json::from_slice::<IgnoredAny>(body).is_ok()
}

/// A JSON number equal to zero (`-0`, `0.0` and `0e5` included).
fn is_zero(text: &str) -> bool {
    matches!(text.as_bytes().first(), Some(b'-' | b'0'..=b'9'))
        && text.parse::<f64>().is_ok_and(|n| n == 0.0)
}

/// What [`Field`] found in one JSON value.
enum Shape<'a> {
    Null,
    /// An object, with its named member's raw value. A repeated member
    /// keeps the last one, as `JSON.parse` does.
    Object(Option<&'a RawValue>),
    Other,
}

/// Reads one JSON value, keeping only the named member of an object as raw
/// text; everything else is scanned, not built.
struct Field<'k>(&'k str);

impl<'de> DeserializeSeed<'de> for Field<'_> {
    type Value = Shape<'de>;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<Shape<'de>, D::Error> {
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Field<'_> {
    type Value = Shape<'de>;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_unit<E>(self) -> Result<Shape<'de>, E> {
        Ok(Shape::Null)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Shape<'de>, E> {
        Ok(Shape::Other)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Shape<'de>, E> {
        Ok(Shape::Other)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Shape<'de>, E> {
        Ok(Shape::Other)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Shape<'de>, E> {
        Ok(Shape::Other)
    }
    fn visit_str<E>(self, _: &str) -> Result<Shape<'de>, E> {
        Ok(Shape::Other)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Shape<'de>, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Shape::Other)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Shape<'de>, A::Error> {
        let mut found = None;
        while let Some(named) = map.next_key_seed(Key(self.0))? {
            if named {
                found = Some(map.next_value::<&'de RawValue>()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Shape::Object(found))
    }
}

/// An object key, compared with the wanted name without being kept.
struct Key<'k>(&'k str);

impl<'de> DeserializeSeed<'de> for Key<'_> {
    type Value = bool;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<bool, D::Error> {
        de.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for Key<'_> {
    type Value = bool;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an object key")
    }
    fn visit_str<E>(self, key: &str) -> Result<bool, E> {
        Ok(key == self.0)
    }
}

/// The entities of a backend answer without those whose id is on this node's
/// content denylist (the projection and rule of the content reads, failing
/// closed on an entity without an id), or `None` when the answer drops
/// nothing or is not a JSON array: it keeps its bytes.
fn without_denylisted(body: &[u8], denylist: &crate::content::Denylist) -> Option<Vec<u8>> {
    use crate::content::queries::{EntityDoc, retain_non_denylisted};
    #[derive(serde::Deserialize)]
    struct Id<'a> {
        #[serde(borrow, default)]
        id: Option<std::borrow::Cow<'a, str>>,
    }
    let items = serde_json::from_slice::<Vec<&RawValue>>(body).ok()?;
    let mut docs: Vec<EntityDoc> = items
        .iter()
        .map(|raw| EntityDoc {
            id: serde_json::from_str::<Id>(raw.get())
                .ok()
                .and_then(|entity| entity.id)
                .map(|id| id.into_owned())
                .unwrap_or_default(),
            json: Bytes::copy_from_slice(raw.get().as_bytes()),
        })
        .collect();
    let before = docs.len();
    retain_non_denylisted(&mut docs, denylist);
    (docs.len() != before).then(|| EntityDoc::json_array(&docs).to_vec())
}

async fn read(
    State(state): State<Shared>,
    OriginalUri(original_uri): OriginalUri,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match forward(
        &state,
        &original_uri,
        method,
        &uri,
        &headers,
        body,
        Conditional::Forward,
    )
    .await
    {
        Ok(answer) => answer.into_response(),
        Err(response) => response,
    }
}

/// How long [`LocalDenylist`] serves one read of `content.denylist`: the
/// freshness budget for a console edit to reach `GET /denylist`.
const LOCAL_DENYLIST_TTL: Duration = Duration::from_secs(5);

/// This node's `content.denylist` rows for `GET /denylist`, read at most once
/// per [`LOCAL_DENYLIST_TTL`] and by one request at a time, including failures.
/// The existing shared API state owns this cache; source responses, credentials
/// and request headers never enter it.
#[derive(Default)]
pub struct LocalDenylist {
    cached: tokio::sync::Mutex<Option<(std::time::Instant, LocalRows)>>,
    #[cfg(test)]
    loads: std::sync::atomic::AtomicUsize,
}

type LocalRows = Result<Arc<Vec<crate::content::denylist::Row>>, ()>;

impl LocalDenylist {
    async fn rows(&self, pool: &sqlx::PgPool) -> LocalRows {
        let mut slot = self.cached.lock().await;
        if let Some((at, rows)) = slot.as_ref()
            && at.elapsed() < LOCAL_DENYLIST_TTL
        {
            return rows.clone();
        }
        #[cfg(test)]
        self.loads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let rows = crate::content::denylist::rows(pool)
            .await
            .map(Arc::new)
            .map_err(|error| {
                tracing::warn!(%error, "content.denylist read failed");
            });
        *slot = Some((std::time::Instant::now(), rows.clone()));
        rows
    }
}

/// `GET /denylist` (and `HEAD`): the source's moderation list plus this
/// node's `content.denylist` rows, newest first. A local row whose id the
/// source names exactly is left out (ids compare exactly: CIDv0 is
/// case-sensitive). An operator row reads as the Registry's shape with
/// `reason: null` and `updated_at` equal to `created_at` (`content.denylist`
/// has neither); `created_by` is the console wallet, as the Registry
/// publishes its moderators'.
///
/// The source is always asked with `GET`, so a `HEAD` gets a body to check
/// (axum drops it from the answer). A source that fails, or answers anything
/// but a `200` JSON array, fails the read rather than serving the local half
/// alone: a content server polling this list keeps its last good copy on an
/// error, but would lift the source's ids on a shorter `200`. Without a
/// source the answer is the local rows.
async fn denylist(
    State(state): State<Shared>,
    OriginalUri(original_uri): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let source = if state.settings.sources.registry.is_empty() {
        Vec::new()
    } else {
        match forward(
            &state,
            &original_uri,
            Method::GET,
            &uri,
            &headers,
            body,
            Conditional::Strip,
        )
        .await
        {
            Ok((status, _, bytes)) if status == StatusCode::OK => {
                match serde_json::from_slice::<Vec<Box<RawValue>>>(&bytes) {
                    Ok(entries) => entries,
                    Err(error) => {
                        tracing::warn!(%error, "Registry denylist is not a JSON array");
                        return message(StatusCode::BAD_GATEWAY, "Registry service unavailable");
                    }
                }
            }
            Ok(answer) => return answer.into_response(),
            Err(response) => return response,
        }
    };
    let local = match state.registry_denylist.rows(&state.content.pool).await {
        Ok(rows) => rows,
        Err(()) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"Internal Server Error"})),
            )
                .into_response();
        }
    };
    (
        [(header::CONTENT_TYPE, "application/json")],
        merge_denylists(source, &local),
    )
        .into_response()
}

/// One Registry denylist entry in its key order.
#[derive(serde::Serialize)]
struct DenylistEntry<'a> {
    entity_id: &'a str,
    reason: Option<&'a str>,
    created_by: &'a str,
    created_at: &'a str,
    updated_at: &'a str,
}

/// The merged list as JSON bytes: source entries kept as received, local
/// rows whose id the source does not name exactly, all ordered by
/// `created_at` descending. The sort is stable, so equal timestamps keep
/// source entries first.
fn merge_denylists(source: Vec<Box<RawValue>>, local: &[crate::content::denylist::Row]) -> Vec<u8> {
    #[derive(serde::Deserialize)]
    struct Fields {
        #[serde(default)]
        entity_id: Option<String>,
        #[serde(default)]
        created_at: Option<Value>,
    }
    /// Milliseconds from a string or numeric `created_at`; 0 when absent.
    fn millis(value: Option<&Value>) -> i128 {
        match value {
            Some(Value::String(text)) => text.parse().unwrap_or(0),
            Some(Value::Number(number)) => number.as_i64().map_or(0, i128::from),
            _ => 0,
        }
    }
    let mut named = std::collections::HashSet::new();
    let mut entries: Vec<(i128, Box<RawValue>)> = source
        .into_iter()
        .map(|raw| {
            let fields = serde_json::from_str::<Fields>(raw.get()).ok();
            let at = millis(fields.as_ref().and_then(|f| f.created_at.as_ref()));
            if let Some(id) = fields.and_then(|f| f.entity_id) {
                named.insert(id);
            }
            (at, raw)
        })
        .collect();
    for row in local {
        if named.contains(&row.id) {
            continue;
        }
        let at = row.added_at_ms.to_string();
        let raw = serde_json::value::to_raw_value(&DenylistEntry {
            entity_id: &row.id,
            reason: None,
            created_by: &row.added_by,
            created_at: &at,
            updated_at: &at,
        })
        .expect("a denylist entry serializes");
        entries.push((i128::from(row.added_at_ms), raw));
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let raws: Vec<Box<RawValue>> = entries.into_iter().map(|(_, raw)| raw).collect();
    serde_json::to_vec(&raws).expect("raw JSON values serialize")
}

/// Whether the client's `If-None-Match` and `If-Modified-Since` reach the
/// backend.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Conditional {
    Forward,
    /// For an answer the gateway may rewrite, whose validators would not
    /// describe what the client receives.
    Strip,
}

/// One backend call: its status, the kept headers and the buffered body, or
/// the gateway's own answer when the call cannot be made or fails.
async fn forward(
    state: &Shared,
    original_uri: &Uri,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
    conditional: Conditional,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Response> {
    let source = &state.settings.sources.registry;
    if source.is_empty() {
        return Err(message(
            StatusCode::SERVICE_UNAVAILABLE,
            "Registry service is not configured",
        ));
    }
    let (native, mut url) = match backend(&state.settings, source) {
        Ok(backend) => backend,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "Registry source refused");
            return Err(message(
                StatusCode::SERVICE_UNAVAILABLE,
                "Registry service is not configured",
            ));
        }
    };
    let path = format!("{}{}", url.path().trim_end_matches('/'), uri.path());
    url.set_path(&path);
    if url.path() != path {
        // Encoded dot segments the URL parser would normalize out of the route.
        return Err(message(StatusCode::BAD_REQUEST, "Invalid Registry path"));
    }
    url.set_query(uri.query());
    let Some(_permit) = slot(&PERMITS, SLOT_WAIT).await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "1")],
            Json(json!({"ok":false,"message":"Registry service is busy"})),
        )
            .into_response());
    };
    let result = async {
        let mut http = state.http.clone();
        if native {
            http = http.allowing_configured_provider_hosts();
        }
        let mut request = http
            .request(method, url)
            .max_redirects(0)
            .timeout(TIMEOUT)
            .body(body);
        for (name, value) in headers {
            let name = name.as_str();
            if matches!(name, "content-type" | "accept")
                || (conditional == Conditional::Forward
                    && matches!(name, "if-none-match" | "if-modified-since"))
                || (native && name.starts_with("x-identity-"))
            {
                request = request.header(name, value);
            }
        }
        if native {
            // Never trust a caller-supplied path: the native verifier binds
            // signed fetch to the path Catalyst actually received, which may
            // carry the gateway's Registry prefix. The query is not signed.
            request = request.header("x-original-path", original_uri.path());
        }
        let mut response = request.send().await?;
        let status = response.status();
        let mut out_headers = HeaderMap::new();
        for name in [
            "content-type",
            "allow",
            "cache-control",
            "retry-after",
            "etag",
            "last-modified",
        ] {
            if let Some(value) = response.headers().get(name) {
                out_headers.insert(name, value.clone());
            }
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len().saturating_add(chunk.len()) <= BODY_CAP,
                "Registry response exceeds body limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, anyhow::Error>((status, out_headers, bytes))
    }
    .await;
    result.map_err(|error| {
        tracing::warn!(error = %format!("{error:#}"), "Registry request failed");
        message(StatusCode::BAD_GATEWAY, "Registry service unavailable")
    })
}

/// A backend slot, waiting at most `wait` for one to free up.
pub(crate) async fn slot(
    permits: &Semaphore,
    wait: Duration,
) -> Option<tokio::sync::SemaphorePermit<'_>> {
    tokio::time::timeout(wait, permits.acquire())
        .await
        .ok()?
        .ok()
}

/// `(native, base URL)`; an upstream goes through the SSRF guard on every call.
fn backend(settings: &Settings, source: &str) -> anyhow::Result<(bool, url::Url)> {
    Ok(match Backend::parse(source)? {
        Backend::Native(url) => (true, url),
        Backend::Upstream(url) => {
            settings.guarded_url(source)?;
            (false, url)
        }
    })
}

fn message(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"ok":false,"message":message}))).into_response()
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn status(State(state): State<Shared>) -> Json<Value> {
    Json(json!({"data": {
        "version": state.settings.version,
        "currentTime": chrono::Utc::now().timestamp_millis(),
        "commitHash": state.settings.commit_hash,
    }}))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use axum::http::{HeaderMap, Method, Uri};
    use axum::response::IntoResponse;
    use serde_json::json;

    async fn denylist_app(source: String) -> (axum::Router, super::Shared) {
        let mut settings = super::super::Settings::for_tests(super::super::Sources::defaults());
        settings.sources.registry = source;
        let state = super::super::tests::node_without_db_state(settings).await;
        state.content.pool.close().await;
        (super::routes(state.clone()), state)
    }

    async fn cached_rows(state: &super::Shared, rows: Vec<crate::content::denylist::Row>) {
        *state.registry_denylist.cached.lock().await =
            Some((std::time::Instant::now(), Ok(Arc::new(rows))));
    }

    async fn expire(cache: &super::LocalDenylist) {
        cache.cached.lock().await.as_mut().unwrap().0 =
            std::time::Instant::now() - super::LOCAL_DENYLIST_TTL;
    }

    async fn denylist_request(
        app: &axum::Router,
        method: &str,
        uri: &str,
        origin: bool,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer test-only")
            .header("if-none-match", "\"old\"");
        if origin {
            request = request.header("origin", "https://client.example.org");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, headers, bytes)
    }

    /// Failure caching also bounds anonymous polls during a database outage;
    /// expiry retries the database and never serves expired successful rows.
    #[tokio::test]
    async fn denylist_handler_coalesces_database_failures_and_retries_after_expiry() {
        let (app, state) = denylist_app(String::new()).await;
        let cache = &state.registry_denylist;
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let app = app.clone();
            tasks.spawn(async move { denylist_request(&app, "GET", "/denylist", false).await });
        }
        while let Some(response) = tasks.join_next().await {
            let (status, _, bytes) = response.unwrap();
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"error":"Internal Server Error"})
            );
        }
        assert_eq!(cache.loads.load(Ordering::Relaxed), 1);
        expire(cache).await;
        assert_eq!(
            denylist_request(&app, "GET", "/denylist", false).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(cache.loads.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn denylist_handler_cached_local_rows_expire_and_do_not_share_headers_or_state() {
        let (app, state) = denylist_app(String::new()).await;
        let cache = &state.registry_denylist;
        cached_rows(
            &state,
            vec![crate::content::denylist::Row {
                id: "QmCase".into(),
                added_by: "0xoperator".into(),
                added_at_ms: 2000,
            }],
        )
        .await;
        for origin in [true, false, true] {
            let (status, headers, bytes) = denylist_request(&app, "GET", "/denylist", origin).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers.contains_key("access-control-allow-origin"), origin);
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!([{
                    "entity_id":"QmCase", "reason":null, "created_by":"0xoperator",
                    "created_at":"2000", "updated_at":"2000"
                }])
            );
        }
        assert_eq!(cache.loads.load(Ordering::Relaxed), 0);
        let shared_router = super::routes(state.clone());
        assert_eq!(
            denylist_request(&shared_router, "GET", "/denylist", false)
                .await
                .0,
            StatusCode::OK
        );
        let (other, _) = denylist_app(String::new()).await;
        assert_eq!(
            denylist_request(&other, "GET", "/denylist", false).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        expire(cache).await;
        assert_eq!(
            denylist_request(&app, "GET", "/denylist", false).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(cache.loads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn denylist_handler_head_and_case_sensitive_merge_never_cache_the_source() {
        use std::future::IntoFuture;
        async fn source(method: Method, uri: Uri, headers: HeaderMap) -> axum::response::Response {
            assert_eq!(method, Method::GET);
            assert!(!headers.contains_key("if-none-match"));
            assert!(!headers.contains_key("authorization"));
            let stamp = if uri.query() == Some("new") {
                "3001"
            } else {
                "3000"
            };
            axum::Json(json!([
                {"entity_id":"qmcase","created_at":"1000"},
                {"entity_id":"shared","created_at":stamp}
            ]))
            .into_response()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server =
            tokio::spawn(axum::serve(listener, axum::Router::new().fallback(source)).into_future());
        let (app, state) = denylist_app(format!("http://{addr}")).await;
        cached_rows(
            &state,
            vec![
                crate::content::denylist::Row {
                    id: "QmCase".into(),
                    added_by: "0xop".into(),
                    added_at_ms: 2000,
                },
                crate::content::denylist::Row {
                    id: "shared".into(),
                    added_by: "0xop".into(),
                    added_at_ms: 4000,
                },
            ],
        )
        .await;
        let (status, _, bytes) = denylist_request(&app, "HEAD", "/denylist", false).await;
        assert_eq!(status, StatusCode::OK);
        assert!(bytes.is_empty());
        for (query, stamp) in [("", "3000"), ("?new", "3001")] {
            let (status, _, bytes) =
                denylist_request(&app, "GET", &format!("/denylist{query}"), false).await;
            assert_eq!(status, StatusCode::OK);
            let rows: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                rows.as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["entity_id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["shared", "QmCase", "qmcase"]
            );
            assert_eq!(rows[0]["created_at"], stamp);
            assert!(
                rows[0].get("created_by").is_none(),
                "the exact source duplicate wins"
            );
        }
        assert_eq!(state.registry_denylist.loads.load(Ordering::Relaxed), 0);
        let (refused, refused_state) =
            denylist_app("https://user:secret@registry.example.org".into()).await;
        cached_rows(&refused_state, vec![]).await;
        assert_eq!(
            denylist_request(&refused, "GET", "/denylist", false)
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        server.abort();
        let _ = server.await;
        // A fresh source has no pooled connection to the earlier mock server.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_addr = closed.local_addr().unwrap();
        drop(closed);
        let (unreachable, unreachable_state) = denylist_app(format!("http://{closed_addr}")).await;
        cached_rows(&unreachable_state, vec![]).await;
        assert_eq!(
            denylist_request(&unreachable, "GET", "/denylist", false)
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
    }

    /// The source's entries stay byte for byte; an operator row the source
    /// does not name joins in the Registry's key order; an id both name
    /// exactly keeps the source's entry, while a different case (a CIDv0 the
    /// Registry lowercased) is a different id; the whole list is newest first.
    #[test]
    fn denylist_merges_the_source_and_operator_rows() {
        use crate::content::denylist::Row;
        let source: Vec<Box<serde_json::value::RawValue>> = serde_json::from_str(
            r#"[{"entity_id":"bafkreinew","reason":"spam","created_by":"0xmod","created_at":"3000","updated_at":"3100"},
                {"entity_id":"bafkreishared","reason":null,"created_by":"0xmod","created_at":"1000","updated_at":"1000"},
                {"entity_id":"qmcase","reason":null,"created_by":"0xmod","created_at":"600","updated_at":"600"}]"#,
        )
        .unwrap();
        let local = [
            Row {
                id: "bafkreiLOCAL".into(),
                added_by: "0xop".into(),
                added_at_ms: 2000,
            },
            Row {
                id: "bafkreishared".into(),
                added_by: "0xop".into(),
                added_at_ms: 4000,
            },
            Row {
                id: "QmCase".into(),
                added_by: "0xop".into(),
                added_at_ms: 500,
            },
        ];
        let merged = super::merge_denylists(source, &local);
        assert_eq!(
            std::str::from_utf8(&merged).unwrap(),
            concat!(
                r#"[{"entity_id":"bafkreinew","reason":"spam","created_by":"0xmod","created_at":"3000","updated_at":"3100"},"#,
                r#"{"entity_id":"bafkreiLOCAL","reason":null,"created_by":"0xop","created_at":"2000","updated_at":"2000"},"#,
                r#"{"entity_id":"bafkreishared","reason":null,"created_by":"0xmod","created_at":"1000","updated_at":"1000"},"#,
                r#"{"entity_id":"qmcase","reason":null,"created_by":"0xmod","created_at":"600","updated_at":"600"},"#,
                r#"{"entity_id":"QmCase","reason":null,"created_by":"0xop","created_at":"500","updated_at":"500"}]"#,
            )
        );
        assert_eq!(super::merge_denylists(Vec::new(), &[]), b"[]");
    }

    /// `pointers_rule` against the Registry's handler as JavaScript reads it:
    /// `body.pointers` throws on `null`, and `!pointers || pointers?.length
    /// === 0` is the 400; a repeated key keeps the last value.
    #[test]
    fn pointers_rule_follows_the_registry_handler() {
        let status = |body: &str| match super::pointers_rule(body.as_bytes()) {
            Ok(()) => StatusCode::OK,
            Err(response) => response.status(),
        };
        for body in [
            "null",
            " null ",
            "",
            "{",
            "nul",
            r#"{"pointers":["0,0"]} x"#,
        ] {
            assert_eq!(status(body), StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        }
        for body in [
            "{}",
            "[]",
            r#"["0,0"]"#,
            "5",
            "true",
            r#""pointers""#,
            r#"{"pointers":[]}"#,
            r#"{"pointers": [ ] }"#,
            r#"{"pointers":null}"#,
            r#"{"pointers":false}"#,
            r#"{"pointers":0}"#,
            r#"{"pointers":-0.0}"#,
            r#"{"pointers":0e5}"#,
            r#"{"pointers":""}"#,
            r#"{"pointers":{"length":0}}"#,
            r#"{"pointers":{"length":-0}}"#,
            r#"{"pointers":{"length":1,"length":0}}"#,
            r#"{"pointers":["0,0"],"pointers":[]}"#,
            r#"{"ids":["bafkreiexample"]}"#,
            "1e400",
            " -1e400 ",
        ] {
            assert_eq!(status(body), StatusCode::BAD_REQUEST, "{body}");
        }
        for body in [
            r#"{"pointers":["0,0"]}"#,
            r#"{"pointers":[0]}"#,
            r#"{"pointers":"0,0"}"#,
            r#"{"pointers":1}"#,
            r#"{"pointers":1e400}"#,
            r#"{"pointers":true}"#,
            r#"{"pointers":{}}"#,
            r#"{"pointers":{"length":"0"}}"#,
            r#"{"pointers":{"length":1}}"#,
            r#"{"pointers":{"length":0,"length":1}}"#,
            r#"{"pointers":[],"pointers":["0,0"]}"#,
            r#"{"other":{"pointers":[]},"pointers":["0,0"],"x":[1e400,{"a":null}]}"#,
        ] {
            assert_eq!(status(body), StatusCode::OK, "{body}");
        }
    }

    async fn app() -> axum::Router {
        super::super::router(super::super::tests_support::state_without_db().await)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> axum::response::Response {
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// A preflight answers exactly the captured Registry headers; only the
    /// Registry leaves the gateway's permissive CORS.
    #[tokio::test]
    async fn registry_cors_matches_the_captured_preflight() {
        let capture: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/content-lambdas/captures/abr/AB-12-preflight.json"
        )))
        .unwrap();
        let request_headers: Vec<(&str, &str)> = capture["request"]["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str().unwrap()))
            .collect();
        let app = app().await;
        for path in [
            "/asset-bundle-registry/entities/active",
            "/asset-bundle-registry/entities/status",
            "/asset-bundle-registry/health/live",
        ] {
            let response = send(&app, "OPTIONS", path, &request_headers).await;
            assert_eq!(
                response.status().as_u16() as u64,
                capture["response"]["status"].as_u64().unwrap(),
                "{path}"
            );
            let headers: serde_json::Map<String, Value> = response
                .headers()
                .iter()
                // The capture keeps the CORS headers; axum also adds framing
                // (`content-length`) and `Allow` for a route without an
                // OPTIONS handler.
                .filter(|(name, _)| name.as_str().starts_with("access-control-") || *name == "vary")
                .map(|(name, value)| (name.to_string(), Value::from(value.to_str().unwrap())))
                .collect();
            assert_eq!(
                Value::Object(headers),
                capture["response"]["headers"],
                "{path}"
            );
            assert!(
                to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }

        let plain = send(&app, "GET", "/asset-bundle-registry/status", &[]).await;
        assert_eq!(plain.status(), StatusCode::OK);
        assert!(plain.headers().get("access-control-allow-origin").is_none());
        let cross = send(
            &app,
            "GET",
            "/asset-bundle-registry/status",
            &[("origin", "https://example.org")],
        )
        .await;
        assert_eq!(cross.status(), StatusCode::OK);
        assert_eq!(cross.headers()["access-control-allow-origin"], "*");
        assert!(
            cross
                .headers()
                .get("access-control-expose-headers")
                .is_none()
        );

        let other = send(
            &app,
            "OPTIONS",
            "/realm-provider-ea/main/about",
            &request_headers,
        )
        .await;
        assert_eq!(other.status(), StatusCode::OK);
        assert_eq!(other.headers()["access-control-allow-methods"], "*");
        assert_eq!(other.headers()["access-control-allow-headers"], "*");
    }

    /// The Registry's CORS runs before its routing: a path no route serves
    /// still answers the captured preflight, and its 404 (the captured
    /// unknown-route answer) gets origin `*` for a cross-origin request.
    #[tokio::test]
    async fn registry_cors_covers_unrouted_paths() {
        let preflight: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/content-lambdas/captures/abr/AB-12-preflight.json"
        )))
        .unwrap();
        let unknown: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/content-lambdas/captures/abr/AB-12-unknown-route.json"
        )))
        .unwrap();
        let request_headers: Vec<(&str, &str)> = preflight["request"]["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str().unwrap()))
            .collect();
        let app = app().await;
        for path in ["/asset-bundle-registry/nope", "/asset-bundle-registry"] {
            let response = send(&app, "OPTIONS", path, &request_headers).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
            let headers: serde_json::Map<String, Value> = response
                .headers()
                .iter()
                .filter(|(name, _)| name.as_str().starts_with("access-control-") || *name == "vary")
                .map(|(name, value)| (name.to_string(), Value::from(value.to_str().unwrap())))
                .collect();
            assert_eq!(
                Value::Object(headers),
                preflight["response"]["headers"],
                "{path}"
            );
        }
        for origin in [None, Some("https://example.org")] {
            let headers: Vec<(&str, &str)> = origin.map(|o| ("origin", o)).into_iter().collect();
            let response = send(&app, "GET", "/asset-bundle-registry/nope", &headers).await;
            assert_eq!(
                response.status().as_u16() as u64,
                unknown["response"]["status"].as_u64().unwrap()
            );
            assert_eq!(
                response.headers()["content-type"],
                unknown["response"]["headers"]["content-type"]
                    .as_str()
                    .unwrap()
            );
            assert_eq!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .map(|value| value.to_str().unwrap()),
                origin.map(|_| "*"),
                "{origin:?}"
            );
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                unknown["response"]["body"].as_str().unwrap().as_bytes()
            );
        }
    }

    /// `@dcl/http-server` answers every `OPTIONS` as a preflight: without
    /// `Origin` it still sends origin `*` (`cors.js` `configureOrigin`), and
    /// without `Access-Control-Request-Headers` it keeps the `Vary` and sends
    /// no `Allow-Headers` (`configureAllowedHeaders`).
    #[tokio::test]
    async fn registry_preflight_without_origin_or_requested_headers() {
        let app = app().await;
        let path = "/asset-bundle-registry/entities/active";
        for (headers, allow_headers) in [
            (
                vec![
                    ("access-control-request-method", "POST"),
                    ("access-control-request-headers", "content-type"),
                ],
                Some("content-type"),
            ),
            (
                vec![
                    ("origin", "https://example.org"),
                    ("access-control-request-method", "POST"),
                ],
                None,
            ),
            (vec![], None),
        ] {
            let response = send(&app, "OPTIONS", path, &headers).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{headers:?}");
            let h = response.headers();
            assert_eq!(h["access-control-allow-origin"], "*", "{headers:?}");
            assert_eq!(
                h["access-control-allow-methods"], "GET,HEAD,OPTIONS,DELETE,POST,PUT",
                "{headers:?}"
            );
            assert_eq!(h["vary"], "Access-Control-Request-Headers", "{headers:?}");
            assert_eq!(h["access-control-max-age"], "86400", "{headers:?}");
            assert_eq!(
                h.get("access-control-allow-headers")
                    .map(|value| value.to_str().unwrap()),
                allow_headers,
                "{headers:?}"
            );
            assert!(h.get("access-control-allow-credentials").is_none());
        }
    }

    /// `createStatusCheckComponent`'s probes under the Registry prefix; its
    /// only probe-capable component is the http server it names `server`.
    #[tokio::test]
    async fn registry_answers_the_status_check_probes() {
        let pass = r#"{"details":{"server":{"status":"pass"}},"status":"pass"}"#;
        let app = app().await;
        for (path, content_type, body) in [
            ("/health/live", "text/plain; charset=utf-8", "alive"),
            (
                "/health/ready",
                "application/health+json; charset=utf-8",
                pass,
            ),
            (
                "/health/startup",
                "application/health+json; charset=utf-8",
                pass,
            ),
        ] {
            let response = send(&app, "GET", &format!("/asset-bundle-registry{path}"), &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()["content-type"], content_type, "{path}");
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                body.as_bytes(),
                "{path}"
            );
        }
    }
}
