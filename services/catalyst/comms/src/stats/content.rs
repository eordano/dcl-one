//! Uncached content enrichment using serve's existing content pool.
use super::render;
use crate::content::OrderedJson;
use serde::Serialize;
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc};

#[derive(Debug)]
pub struct Error;
/// Active entity documents in the content endpoint's response order.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    async fn fetch(&self, pointers: &[String]) -> Result<Vec<OrderedJson>, Error>;
}
/// Fetches the active documents for a snapshot's hot-scene tiles, skipping the
/// query when no tile is occupied. Callers take the snapshot and render the
/// tiles first, so one retained view feeds both the query and the projection.
pub async fn tile_documents(
    source: &dyn Source,
    tiles: &[(String, usize)],
) -> Result<Vec<OrderedJson>, Error> {
    if tiles.is_empty() {
        return Ok(Vec::new());
    }
    let pointers = tiles
        .iter()
        .map(|(tile, _)| tile.clone())
        .collect::<Vec<_>>();
    source.fetch(&pointers).await
}
pub struct Content {
    pub pool: PgPool,
    pub denylist: Arc<crate::content::Denylist>,
}
#[async_trait::async_trait]
impl Source for Content {
    async fn fetch(&self, pointers: &[String]) -> Result<Vec<OrderedJson>, Error> {
        let mut docs =
            crate::content::queries::active_entity_docs_with_ordered_metadata_and_thumbnail_by_pointers(
                &self.pool, pointers,
            )
            .await
            .map_err(|error| {
                tracing::warn!(%error,"statistics active content query failed");
                Error
            })?;
        crate::content::queries::retain_non_denylisted(&mut docs, &self.denylist);
        docs.into_iter()
            .map(|doc| serde_json::from_slice(&doc.json).map_err(|_| Error))
            .collect()
    }
}
fn truthy(value: &OrderedJson) -> bool {
    match value {
        OrderedJson::Null => false,
        OrderedJson::Bool(b) => *b,
        OrderedJson::Number(n) => *n != 0.,
        OrderedJson::Signed(n) => *n != 0,
        OrderedJson::Unsigned(n) => *n != 0,
        OrderedJson::String(s) => !s.is_empty(),
        _ => true,
    }
}
fn js_whitespace(c: char) -> bool {
    matches!(c,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}
fn coords(value: &OrderedJson) -> Result<Vec<f64>, Error> {
    let text = value.as_str().ok_or(Error)?;
    Ok(text
        .split(',')
        .map(|part| {
            let part = part.trim_start_matches(js_whitespace);
            let digits = part.strip_prefix(['+', '-']).unwrap_or(part);
            let count = digits.bytes().take_while(u8::is_ascii_digit).count();
            if count == 0 {
                return f64::NAN;
            }
            let end = part.len() - digits.len() + count;
            part[..end].parse::<f64>().unwrap_or(f64::NAN)
        })
        .collect())
}
fn thumbnail(scene: &OrderedJson, url: &str) -> Result<Option<OrderedJson>, Error> {
    let Some(value) = scene
        .get("metadata")
        .and_then(|m| m.get("display"))
        .and_then(|d| d.get("navmapThumbnail"))
    else {
        return Ok(None);
    };
    if !truthy(value) {
        return Ok(Some(value.clone()));
    }
    let name = value.as_str().ok_or(Error)?;
    if name.starts_with("http") {
        return Ok(Some(value.clone()));
    }
    let Some(files) = scene.get("content").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let files = files.as_array().ok_or(Error)?;
    let mut hash = None;
    for file in files {
        if file.is_null() {
            return Err(Error);
        }
        if file.get("file") == Some(value) {
            hash = file.get("hash");
            break;
        }
    }
    Ok(hash
        .filter(|h| truthy(h))
        .map(|hash| OrderedJson::String(format!("{url}/contents/{}", js_string(hash)))))
}
fn js_string(value: &OrderedJson) -> String {
    match value {
        OrderedJson::String(s) => s.clone(),
        OrderedJson::Null => "null".into(),
        OrderedJson::Bool(b) => b.to_string(),
        OrderedJson::Number(n) => render::number(*n),
        OrderedJson::Signed(n) => render::number(*n as f64),
        OrderedJson::Unsigned(n) => render::number(*n as f64),
        OrderedJson::Object(_) => "[object Object]".into(),
        OrderedJson::Array(a) => a
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}
#[derive(Serialize)]
pub struct HotScene<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a OrderedJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a OrderedJson>,
    #[serde(rename = "baseCoords")]
    pub base_coords: Vec<f64>,
    #[serde(rename = "usersTotalCount")]
    pub users_total_count: usize,
    pub parcels: Vec<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<OrderedJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<&'a OrderedJson>,
    #[serde(rename = "projectId", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<&'a OrderedJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a OrderedJson>,
}
pub fn hot_scenes(
    scenes: &[OrderedJson],
    tiles: &[(String, usize)],
    url: &str,
) -> Result<String, Error> {
    Ok(render::json(&project_hot_scenes(scenes, tiles, url)?))
}
pub fn project_hot_scenes<'a>(
    scenes: &'a [OrderedJson],
    tiles: &[(String, usize)],
    url: &str,
) -> Result<Vec<HotScene<'a>>, Error> {
    let counts: HashMap<_, _> = tiles
        .iter()
        .map(|(tile, count)| (tile.as_str(), *count))
        .collect();
    let mut out = Vec::new();
    for scene in scenes {
        // Property access on null throws before optional metadata access can apply.
        if scene.is_null() {
            return Err(Error);
        }
        let metadata = scene.get("metadata").unwrap_or(&OrderedJson::Null);
        let location = metadata.get("scene").unwrap_or(&OrderedJson::Null);
        let base = location.get("base").unwrap_or(&OrderedJson::Null);
        let parcels = location.get("parcels").unwrap_or(&OrderedJson::Null);
        if !truthy(base) || !truthy(parcels) {
            continue;
        }
        let raw_parcels = parcels.as_array().ok_or(Error)?;
        let parsed = raw_parcels.iter().map(coords).collect::<Result<_, _>>()?;
        out.push(HotScene {
            id: scene.get("id"),
            name: metadata.get("display").and_then(|v| v.get("title")),
            base_coords: coords(base)?,
            users_total_count: raw_parcels
                .iter()
                .map(|p| counts.get(p.as_str().unwrap()).copied().unwrap_or(0))
                .sum(),
            parcels: parsed,
            thumbnail: thumbnail(scene, url)?,
            creator: metadata.get("contact").and_then(|v| v.get("name")),
            project_id: metadata.get("source").and_then(|v| v.get("projectId")),
            description: metadata.get("display").and_then(|v| v.get("description")),
        });
    }
    out.sort_by(|a, b| b.users_total_count.cmp(&a.users_total_count));
    out.truncate(100);
    Ok(out)
}
