// from: catalyrst/crates/catalyrst-explorer-api/src/modules/feature_flags.rs (document and route shapes; the served document is the mirrored upstream one with feature_flags_local applied, the embedded asset only the fallback before the first pass)
//! `GET /<app>.json` and `GET /flags/{name}`. Upstream serves the full flag set
//! (including `explorer-alfa-minimum-requirements`) only to the
//! `referer: https://decentraland.org` hostname strategy, so the mirror pass
//! sends that header and this node serves the full set to everyone. The
//! document is kept as received (`explore_api.feature_flags`, one row per app)
//! so a key upstream adds reaches the client; `feature_flags_local` overrides
//! individual flags.

use crate::shared::js::is_js_whitespace;
use std::sync::Arc;

use anyhow::Context as _;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::RwLock;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{Shared, mirror};
use crate::jobs::JobContext;

pub const MIRROR: &str = "feature-flags";
const BODY_CAP: usize = 4 << 20;
const EMBEDDED_FLAGS: &str = include_str!("feature-flags.explorer.json");

pub fn embedded_document() -> Value {
    serde_json::from_str(EMBEDDED_FLAGS).expect("feature-flags.explorer.json parses")
}

/// A flags document: an object with `flags` (name -> bool) and `variants` objects.
pub fn validate(doc: &Value) -> anyhow::Result<()> {
    let flags = doc
        .get("flags")
        .and_then(Value::as_object)
        .context("document has no `flags` object")?;
    if let Some((name, value)) = flags.iter().find(|(_, v)| !v.is_boolean()) {
        anyhow::bail!("flag {name} is not a boolean: {value}");
    }
    doc.get("variants")
        .and_then(Value::as_object)
        .context("document has no `variants` object")?;
    Ok(())
}

pub struct Snapshot {
    doc: RwLock<Arc<Value>>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            doc: RwLock::new(Arc::new(embedded_document())),
        }
    }
}

impl Snapshot {
    /// An application without a bundled document starts with every flag off.
    /// Explorer's bundled defaults must never enable another application's flags.
    pub fn empty() -> Self {
        Self {
            doc: RwLock::new(Arc::new(json!({"flags": {}, "variants": {}}))),
        }
    }

    /// A snapshot holding `doc` as received: a node's other reader of one application's flags, and tests.
    pub fn from_document(doc: Value) -> Self {
        Self {
            doc: RwLock::new(Arc::new(doc)),
        }
    }

    /// Replaces the held document: a reader that keeps its own copy of an application's flags.
    pub fn replace(&self, doc: Value) {
        *self.doc.write() = Arc::new(doc);
    }

    pub fn document(&self) -> Arc<Value> {
        self.doc.read().clone()
    }

    /// `@dcl/features-component` 1.0.2 addresses flags by `<app>-<feature>`.
    pub fn enabled(&self, app: &str, feature: &str) -> bool {
        self.document()["flags"][format!("{app}-{feature}")]
            .as_bool()
            .unwrap_or(false)
    }

    /// The SDK exposes variants only while the document's flag is enabled.
    /// Its payload is a comma-separated string; CRLF/LF is stripped before
    /// splitting, and even empty list entries are retained by the adapter.
    pub fn variant_values(&self, app: &str, feature: &str) -> Option<Vec<String>> {
        let doc = self.document();
        let key = format!("{app}-{feature}");
        if doc["flags"][&key].as_bool() != Some(true) {
            return None;
        }
        let value = doc["variants"][&key]["payload"]["value"].as_str()?;
        if value.is_empty() {
            return None;
        }
        Some(
            value
                .replace("\r\n", "")
                .replace('\n', "")
                .split(',')
                .map(|value| {
                    value
                        .to_lowercase()
                        .trim_matches(is_js_whitespace)
                        .to_string()
                })
                .collect(),
        )
    }

    /// The mirrored row for `app` (the embedded asset when there is none yet)
    /// with every local override applied to `flags`.
    pub async fn reload(&self, pool: &PgPool, app: &str) -> sqlx::Result<()> {
        let row: Option<(Value,)> =
            sqlx::query_as("SELECT document FROM explore_api.feature_flags WHERE app = $1")
                .bind(app)
                .fetch_optional(pool)
                .await?;
        let doc = row.map(|(d,)| d).unwrap_or_else(|| {
            if app == "explorer" {
                embedded_document()
            } else {
                json!({"flags": {}, "variants": {}})
            }
        });
        let doc = with_local_overrides(pool, app, doc).await?;
        *self.doc.write() = Arc::new(doc);
        Ok(())
    }
}

/// `doc` with every `feature_flags_local` override of `app` applied to its `flags`.
pub async fn with_local_overrides(pool: &PgPool, app: &str, mut doc: Value) -> sqlx::Result<Value> {
    let overrides: Vec<(String, bool)> = sqlx::query_as(
        "SELECT name, value FROM explore_api.feature_flags_local WHERE app = $1 ORDER BY name",
    )
    .bind(app)
    .fetch_all(pool)
    .await?;
    if !overrides.is_empty() {
        let flags = doc
            .as_object_mut()
            .map(|o| o.entry("flags").or_insert_with(|| json!({})))
            .and_then(Value::as_object_mut);
        if let Some(flags) = flags {
            for (name, value) in overrides {
                flags.insert(name, Value::Bool(value));
            }
        }
    }
    Ok(doc)
}

pub async fn set_local_flag(pool: &PgPool, app: &str, name: &str, value: bool) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO explore_api.feature_flags_local (app, name, value)
         VALUES ($1, $2, $3)
         ON CONFLICT (app, name) DO UPDATE
         SET value = EXCLUDED.value, updated_at = timezone('UTC', now())",
    )
    .bind(app)
    .bind(name)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_local_flag(pool: &PgPool, app: &str, name: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM explore_api.feature_flags_local WHERE app = $1 AND name = $2")
        .bind(app)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn store(pool: &PgPool, app: &str, doc: &Value, etag: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO explore_api.feature_flags (app, document, etag, fetched_at)
         VALUES ($1, $2, $3, timezone('UTC', now()))
         ON CONFLICT (app) DO UPDATE
         SET document = EXCLUDED.document, etag = EXCLUDED.etag, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(app)
    .bind(doc)
    .bind(etag)
    .execute(pool)
    .await?;
    Ok(())
}

/// The sibling document of the same feature-flags service: the source names
/// one `<app>.json`; `events.json` lives beside it.
fn sibling_document(url: &str, app: &str) -> Option<String> {
    let (dir, file) = url.rsplit_once('/')?;
    if dir.is_empty() || !file.ends_with(".json") {
        return None;
    }
    Some(format!("{dir}/{app}.json"))
}

/// One document: fetched with the referer strategy header, validated and
/// stored as received under `app`; `If-None-Match` from the stored etag makes
/// an unchanged upstream a 304 that only refreshes `fetched_at`. Answers the
/// stored document's etag.
async fn mirror_document(state: &Shared, app: &str, url: &str) -> anyhow::Result<Option<String>> {
    let stored: Option<(Option<String>,)> =
        sqlx::query_as("SELECT etag FROM explore_api.feature_flags WHERE app = $1")
            .bind(app)
            .fetch_optional(&state.pool)
            .await
            .context("reading the stored feature flags etag")?;
    let etag = stored.and_then(|(e,)| e);
    let mut headers: Vec<(&str, &str)> = vec![
        ("referer", "https://decentraland.org"),
        ("x-debug", "false"),
    ];
    if let Some(etag) = etag.as_deref() {
        headers.push(("if-none-match", etag));
    }
    let fetched = mirror::fetch(
        &state.http,
        &state.settings,
        reqwest::Method::GET,
        url,
        None,
        &headers,
        BODY_CAP,
    )
    .await?;
    if fetched.status == reqwest::StatusCode::NOT_MODIFIED {
        sqlx::query("UPDATE explore_api.feature_flags SET fetched_at = timezone('UTC', now()) WHERE app = $1")
            .bind(app)
            .execute(&state.pool)
            .await
            .context("touching the feature flags row")?;
    } else {
        let doc: Value = serde_json::from_slice(&fetched.body)
            .with_context(|| format!("{}: body is not JSON", mirror::shown(url)))?;
        validate(&doc).with_context(|| format!("{}: not a flags document", mirror::shown(url)))?;
        store(&state.pool, app, &doc, fetched.etag.as_deref())
            .await
            .context("storing explore_api.feature_flags")?;
    }
    Ok(fetched.etag.or(etag))
}

/// One application's document as the service answers it for `referer` (the service picks flags by Referer),
/// validated. `target` has already passed the caller's SSRF guard.
pub async fn fetch_document(
    http: &crate::http::Outbound,
    target: reqwest::Url,
    referer: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<Value> {
    let response = http
        .get(target)
        .header("referer", referer)
        .timeout(timeout)
        .send()
        .await
        .context("fetching application feature flags")?;
    if !response.status().is_success() {
        anyhow::bail!("application feature flags answered {}", response.status());
    }
    let body = crate::http::read_body_capped(response, BODY_CAP)
        .await
        .context("reading application feature flags")?;
    let doc: Value = serde_json::from_slice(&body).context("feature flags are not JSON")?;
    validate(&doc)?;
    Ok(doc)
}

/// Refresh a separate application's document through the same outbound client,
/// bounded body reader and persisted flag store as Explorer. A failed fetch or
/// validation leaves both the persisted and process snapshots unchanged.
pub async fn refresh_app(
    state: &Shared,
    snapshot: &Snapshot,
    app: &str,
    url: &str,
    referer: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let target = state.settings.guarded_url(url)?;
    let doc = fetch_document(&state.http, target, referer, timeout).await?;
    store(&state.pool, app, &doc, None).await?;
    snapshot.reload(&state.pool, app).await?;
    Ok(())
}

#[cfg(test)]
mod application_tests {
    use super::*;

    #[test]
    fn sdk_app_keys_and_enabled_variants_are_independent() {
        let snapshot = Snapshot::empty();
        assert!(!snapshot.enabled("dapps", "communities_ai_compliance"));
        *snapshot.doc.write() = Arc::new(json!({
            "flags": {
                "communities_ai_compliance": true,
                "explorer-communities_ai_compliance": true,
                "dapps-communities_ai_compliance": false,
                "dapps-communities_global_moderators": true
            },
            "variants": {"dapps-communities_global_moderators": {
                "payload": {"value": " 0xAbC,\r\n0xDeF, ,"}
            }}
        }));
        assert!(!snapshot.enabled("dapps", "communities_ai_compliance"));
        assert_eq!(
            snapshot.variant_values("dapps", "communities_global_moderators"),
            Some(vec!["0xabc".into(), "0xdef".into(), "".into(), "".into()])
        );
        assert!(
            snapshot
                .variant_values("explorer", "communities_global_moderators")
                .is_none()
        );
        *snapshot.doc.write() = Arc::new(json!({"flags": {}, "variants": {
            "dapps-communities_global_moderators": {"payload": {"value": "0xabc"}}
        }}));
        assert!(
            snapshot
                .variant_values("dapps", "communities_global_moderators")
                .is_none()
        );
    }
}

/// One pass: the explorer document (served by this mirror's routes) and the
/// `events` document beside it, which `events::foundation` reads for the
/// Foundation display names (upstream `refreshFoundationAddresses` fetched
/// `events.json` itself; review architecture-1 explore-change-4). Either
/// document failing fails the pass.
pub async fn run_pass(state: &Shared, ctx: &JobContext) -> anyhow::Result<()> {
    mirror::run(&state.pool, MIRROR, ctx, || async {
        let app = state.settings.feature_flags_app.as_str();
        let url = state.settings.sources.feature_flags.as_str();
        let etag = mirror_document(state, app, url).await?;
        // The explorer document is served from memory: reloaded as soon as
        // it is stored, before the events sibling can fail the pass.
        state
            .flags
            .reload(&state.pool, app)
            .await
            .context("reloading the feature flags")?;
        let mut items = count_entries(&state.flags.document());
        let events_app = crate::events::foundation::APP;
        if events_app != app
            && let Some(events_url) = sibling_document(url, events_app)
        {
            mirror_document(state, events_app, &events_url).await?;
            let (events_doc,): (Value,) =
                sqlx::query_as("SELECT document FROM explore_api.feature_flags WHERE app = $1")
                    .bind(events_app)
                    .fetch_one(&state.pool)
                    .await
                    .context("reading the stored events flags")?;
            items += count_entries(&events_doc);
        }
        Ok(mirror::PassOutcome {
            items,
            cursor: etag,
        })
    })
    .await
}

fn count_entries(doc: &Value) -> i64 {
    let count = |key: &str| {
        doc.get(key)
            .and_then(Value::as_object)
            .map_or(0, |o| o.len()) as i64
    };
    count("flags") + count("variants")
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/flags/{name}", get(get_flag))
        .route("/{app_name}", get(get_app_json))
}

/// Any app name answers the mirrored document, as the old service did;
/// upstream's `cache-control: private,no-store` is kept.
async fn get_app_json(State(state): State<Shared>, Path(_app_name): Path<String>) -> Response {
    (
        StatusCode::OK,
        [("cache-control", "private,no-store,max-age=0")],
        Json(state.flags.document()),
    )
        .into_response()
}

async fn get_flag(State(state): State<Shared>, Path(name): Path<String>) -> Response {
    let doc = state.flags.document();
    if let Some(variant) = doc.get("variants").and_then(|v| v.get(&name)) {
        return (StatusCode::OK, Json(variant.clone())).into_response();
    }
    if let Some(flag) = doc.get("flags").and_then(|f| f.get(&name)) {
        return (StatusCode::OK, Json(flag.clone())).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "flag_not_found", "name": name })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::sibling_document;

    #[test]
    fn the_events_document_sits_beside_the_explorer_one() {
        assert_eq!(
            sibling_document(
                "https://feature-flags.decentraland.org/explorer.json",
                "events"
            ),
            Some("https://feature-flags.decentraland.org/events.json".into())
        );
        assert_eq!(
            sibling_document("http://127.0.0.1:9/flags/explorer.json", "events"),
            Some("http://127.0.0.1:9/flags/events.json".into())
        );
        assert_eq!(sibling_document("explorer.json", "events"), None);
        assert_eq!(sibling_document("https://x.example/flags", "events"), None);
    }
}
