use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::SecondsFormat;
use serde::Serialize;
use serde_json::Value;

use super::SharedState;
use super::queries::{self, WorldScene};
use crate::content::errors::{AppError, AppResult};
use crate::core::http::{QueryPairs, js_parse_int, query_param};
use crate::shared::cache::TtlMap;

const MAX_INDEX_LIMIT: i64 = 10_000;
/// The whole index is one wide scan; pages are served from a short memo with an ETag.
const INDEX_MEMO_TTL: Duration = Duration::from_secs(30);
const INDEX_MEMO_PAGES: usize = 256;

struct IndexPage {
    body: bytes::Bytes,
    etag: HeaderValue,
}

fn index_memo() -> &'static TtlMap<(i64, i64), Arc<IndexPage>> {
    static MEMO: OnceLock<TtlMap<(i64, i64), Arc<IndexPage>>> = OnceLock::new();
    MEMO.get_or_init(|| TtlMap::bounded(INDEX_MEMO_TTL, INDEX_MEMO_PAGES))
}

/// Drops every memoized page: the mirror calls it after a pass that wrote or
/// retired scenes, so the index never serves a world past its retirement
/// for the memo's lifetime.
pub fn invalidate_index_memo() {
    index_memo().clear();
}

fn etag_for(body: &[u8]) -> HeaderValue {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    HeaderValue::from_str(&format!("\"{:016x}\"", h.finish())).expect("hex etag")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct IndexSceneSummary {
    pub id: String,
    /// `display.title || ''`, as upstream's indexer.
    pub title: String,
    pub description: String,
    /// Absent when the scene names no thumbnail file in its content.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub thumbnail: Option<String>,
    pub pointers: Vec<String>,
    /// Absent when the metadata has no such key; an explicit `null` is kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "string | null"))]
    pub runtime_version: Option<Value>,
    /// `entity.timestamp || Date.now()`: the page's build time when missing or 0.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub timestamp: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct WorldIndexEntry {
    pub name: String,
    pub scenes: Vec<IndexSceneSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct IndexResponse {
    pub data: Vec<WorldIndexEntry>,
    pub last_updated: String,
}

/// GET /index: every unblocked world with scenes, name order, paged by
/// `limit`/`offset`. The protocol a downstream worlds mirror reads.
pub async fn get_index(
    State(state): State<SharedState>,
    Query(q): Query<QueryPairs>,
    headers: HeaderMap,
) -> AppResult<Response> {
    // `searchParams.get`: the first of a repeated key. Unparsable values fall
    // back to the defaults, like the old `parseInt`.
    let (limit, offset) = bound_index_params(
        query_param(&q, "limit").and_then(js_parse_int),
        query_param(&q, "offset").and_then(js_parse_int),
    );
    let page = match index_memo().get_fresh(&(limit, offset)) {
        Some(hit) => hit,
        None => {
            let index = build_index(&state, limit, offset).await?;
            let body = serde_json::to_vec(&index)
                .map_err(|e| AppError::Internal(format!("serialize index: {e}")))?;
            let page = Arc::new(IndexPage {
                etag: etag_for(&body),
                body: body.into(),
            });
            index_memo().insert((limit, offset), page.clone());
            page
        }
    };
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v == page.etag)
    {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, page.etag.clone())],
        )
            .into_response());
    }
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
            (header::ETAG, page.etag.clone()),
        ],
        page.body.clone(),
    )
        .into_response())
}

async fn build_index(state: &SharedState, limit: i64, offset: i64) -> AppResult<IndexResponse> {
    let base_url = state.settings.content_server_url();
    let banned = state.names.snapshot(&state.pool).await.banned_names();
    let scenes = queries::list_index_scenes(&state.pool, limit, offset, &banned).await?;
    let now = chrono::Utc::now();

    let mut data: Vec<WorldIndexEntry> = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_scenes: Vec<IndexSceneSummary> = Vec::new();
    for (world_name, scene) in scenes {
        if cur_name.as_deref() != Some(world_name.as_str()) {
            if let Some(name) = cur_name.take() {
                data.push(WorldIndexEntry {
                    name,
                    scenes: std::mem::take(&mut cur_scenes),
                });
            }
            cur_name = Some(world_name);
        }
        cur_scenes.push(scene_summary(&scene, &base_url, now.timestamp_millis()));
    }
    if let Some(name) = cur_name.take() {
        data.push(WorldIndexEntry {
            name,
            scenes: cur_scenes,
        });
    }
    Ok(IndexResponse {
        data,
        last_updated: now.to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

fn bound_index_params(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    let limit = limit
        .filter(|&l| l > 0)
        .map(|l| l.min(MAX_INDEX_LIMIT))
        .unwrap_or(MAX_INDEX_LIMIT);
    let offset = offset.filter(|&o| o >= 0).unwrap_or(0);
    (limit, offset)
}

fn scene_summary(scene: &WorldScene, base_url: &str, now_ms: i64) -> IndexSceneSummary {
    let display = scene.entity.get("metadata").and_then(|m| m.get("display"));
    let text = |key: &str| {
        display
            .and_then(|d| d.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let (title, description) = (text("title"), text("description"));
    let thumbnail = display
        .and_then(|d| d.get("navmapThumbnail"))
        .and_then(Value::as_str)
        .and_then(|file| content_file_hash(&scene.entity, file))
        .map(|hash| format!("{base_url}/contents/{hash}"));
    let timestamp = crate::core::store::entity_timestamp_ms(&scene.entity)
        .filter(|&t| t != 0)
        .unwrap_or(now_ms);
    // `entity.metadata?.runtimeVersion`: absent when the key is, an explicit `null` kept.
    let runtime_version = scene
        .entity
        .get("metadata")
        .and_then(|m| m.get("runtimeVersion"))
        .cloned();
    IndexSceneSummary {
        id: scene.entity_id.clone(),
        title,
        description,
        thumbnail,
        pointers: scene.parcels.clone(),
        runtime_version,
        timestamp,
    }
}

/// The hash of the entity's content file named `file`: the first content
/// entry with that name, as `entity.content.find(c => c.file === file).hash`.
pub(crate) fn content_file_hash<'a>(entity: &'a Value, file: &str) -> Option<&'a str> {
    entity
        .get("content")?
        .as_array()?
        .iter()
        .find(|c| c.get("file").and_then(Value::as_str) == Some(file))?
        .get("hash")?
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::{MAX_INDEX_LIMIT, bound_index_params, scene_summary};
    use crate::queries::WorldScene;
    use serde_json::json;

    #[test]
    fn scene_summary_includes_runtime_version_from_metadata() {
        let scene = WorldScene {
            entity_id: "bafy-scene".into(),
            entity: json!({
                "timestamp": 111,
                "metadata": { "runtimeVersion": "7", "display": { "title": "Hello" } }
            }),
            parcels: vec!["0,0".into()],
            deployer: "0xowner".into(),
            mirror_source: None,
        };
        let body =
            serde_json::to_value(scene_summary(&scene, "https://worlds.example", 5)).unwrap();
        assert_eq!(body["runtimeVersion"], json!("7"));
        assert_eq!(body["title"], json!("Hello"));
        assert_eq!(body["id"], json!("bafy-scene"));
    }

    /// `worlds-indexer.ts`: `title`/`description` `''`, `thumbnail` and `runtimeVersion` left
    /// out, `timestamp` `Date.now()` (`test/integration/index-handler.spec.ts:86`).
    #[test]
    fn scene_summary_fills_missing_fields_as_upstream() {
        let scene = WorldScene {
            entity_id: "s".into(),
            entity: json!({ "timestamp": 0, "metadata": { "display": {} } }),
            parcels: vec![],
            deployer: "0xowner".into(),
            mirror_source: None,
        };
        let body = serde_json::to_value(scene_summary(&scene, "https://x", 1234)).unwrap();
        assert_eq!(
            body,
            json!({ "id": "s", "title": "", "description": "", "pointers": [], "timestamp": 1234 })
        );
    }

    /// `runtimeVersion: entity.metadata?.runtimeVersion`: a stored `null` is
    /// serialized, only a missing key is left out.
    #[test]
    fn scene_summary_keeps_an_explicit_null_runtime_version() {
        let scene = WorldScene {
            entity_id: "s".into(),
            entity: json!({ "timestamp": 9, "metadata": { "runtimeVersion": null } }),
            parcels: vec![],
            deployer: "0xowner".into(),
            mirror_source: None,
        };
        let body = serde_json::to_value(scene_summary(&scene, "https://x", 1)).unwrap();
        assert_eq!(body.get("runtimeVersion"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn scene_summary_links_the_thumbnail_under_the_given_base() {
        let scene = WorldScene {
            entity_id: "s".into(),
            entity: json!({
                "timestamp": 7,
                "content": [{ "file": "t.png", "hash": "bafkreithumb" }],
                "metadata": { "display": { "navmapThumbnail": "t.png" } }
            }),
            parcels: vec![],
            deployer: "0xowner".into(),
            mirror_source: None,
        };
        let body = serde_json::to_value(scene_summary(&scene, "https://x/wcs", 1)).unwrap();
        assert_eq!(
            body["thumbnail"],
            json!("https://x/wcs/contents/bafkreithumb")
        );
        assert_eq!(body["timestamp"], json!(7));
    }

    #[test]
    fn index_params_default_when_absent() {
        assert_eq!(bound_index_params(None, None), (MAX_INDEX_LIMIT, 0));
    }

    #[test]
    fn index_limit_clamps_at_max() {
        assert_eq!(bound_index_params(Some(1_000_000), None).0, MAX_INDEX_LIMIT);
        assert_eq!(
            bound_index_params(Some(MAX_INDEX_LIMIT + 1), None).0,
            MAX_INDEX_LIMIT
        );
        assert_eq!(
            bound_index_params(Some(MAX_INDEX_LIMIT), None).0,
            MAX_INDEX_LIMIT
        );
        assert_eq!(bound_index_params(Some(50), None).0, 50);
        assert_eq!(bound_index_params(Some(0), None).0, MAX_INDEX_LIMIT);
        assert_eq!(bound_index_params(Some(-5), None).0, MAX_INDEX_LIMIT);
    }

    #[test]
    fn index_offset_parses_and_defaults() {
        assert_eq!(bound_index_params(None, Some(25)).1, 25);
        assert_eq!(bound_index_params(None, Some(0)).1, 0);
        assert_eq!(bound_index_params(None, Some(-1)).1, 0);
        assert_eq!(bound_index_params(None, Some(i64::MIN)).1, 0);
        assert_eq!(bound_index_params(Some(10), Some(20)), (10, 20));
    }
}
