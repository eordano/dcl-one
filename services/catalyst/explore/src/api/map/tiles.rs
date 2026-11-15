//! The tile types as api.decentraland.org serves them, and `/v2/tiles`
//! (bbox + `include`/`exclude` projection), `/v1/tiles` (the legacy numeric
//! shape) and `/v2/tiles/info` over the mirrored grid.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::super::Shared;
use super::http_cache;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "map/", rename_all = "lowercase")
)]
pub enum TileType {
    Owned,
    Unowned,
    Plaza,
    Road,
    District,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "map/"))]
pub struct RentalPeriod {
    #[serde(rename = "minDays")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub min_days: i64,
    #[serde(rename = "maxDays")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub max_days: i64,
    #[serde(rename = "pricePerDay")]
    pub price_per_day: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "map/"))]
pub struct TileRentalListing {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub expiration: i64,
    pub periods: Vec<RentalPeriod>,
    #[serde(rename = "updatedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub updated_at: i64,
}

impl TileRentalListing {
    pub fn max_price_per_day(&self) -> String {
        let mut best = "0".to_string();
        for p in &self.periods {
            if numeric_str_gt(&p.price_per_day, &best) {
                best = p.price_per_day.clone();
            }
        }
        best
    }
}

fn numeric_str_gt(a: &str, b: &str) -> bool {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    if a.len() != b.len() {
        return a.len() > b.len();
    }
    a > b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "map/"))]
pub struct Tile {
    pub id: String,
    pub x: i32,
    pub y: i32,
    #[serde(rename = "nftId", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub nft_id: Option<String>,
    #[serde(rename = "type")]
    pub tile_type: TileType,
    pub top: bool,
    pub left: bool,
    #[serde(rename = "topLeft")]
    pub top_left: bool,
    #[serde(rename = "updatedAt", default)]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub owner: Option<String>,
    #[serde(rename = "estateId", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub estate_id: Option<String>,
    #[serde(rename = "tokenId", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub token_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub price: Option<f64>,
    #[serde(rename = "expiresAt", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub expires_at: Option<i64>,
    #[serde(
        rename = "rentalListing",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub rental_listing: Option<TileRentalListing>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "map/"))]
pub struct LegacyTile {
    #[serde(rename = "type")]
    pub tile_type: i32,
    pub x: i32,
    pub y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub top: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub left: Option<u8>,
    #[serde(rename = "topLeft", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub top_left: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub estate_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub price: Option<f64>,
    #[serde(rename = "rentalPricePerDay", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub rental_price_per_day: Option<String>,
}

pub fn coords_to_id(x: i32, y: i32) -> String {
    format!("{x},{y}")
}

/// The bbox and projection a tiles request resolves to; the cache key is
/// built from these, so spellings that select the same tiles share one entry.
struct TileQuery {
    bbox: Option<(i32, i32, i32, i32)>,
    fields: Option<Vec<&'static str>>,
}

impl TileQuery {
    fn parse(q: &HashMap<String, String>) -> Self {
        let bbox = match (
            q.get("x1").and_then(|s| s.parse::<i32>().ok()),
            q.get("x2").and_then(|s| s.parse::<i32>().ok()),
            q.get("y1").and_then(|s| s.parse::<i32>().ok()),
            q.get("y2").and_then(|s| s.parse::<i32>().ok()),
        ) {
            (Some(x1), Some(x2), Some(y1), Some(y2)) => {
                Some((x1.min(x2), x1.max(x2), y1.min(y2), y1.max(y2)))
            }
            _ => None,
        };
        let fields = if let Some(s) = q.get("include") {
            let included: Vec<&str> = s.split(',').collect();
            Some(
                VALID_FIELDS
                    .iter()
                    .copied()
                    .filter(|f| included.contains(f))
                    .collect(),
            )
        } else {
            q.get("exclude").map(|s| {
                let excluded: Vec<&str> = s.split(',').collect();
                VALID_FIELDS
                    .iter()
                    .copied()
                    .filter(|f| !excluded.contains(f))
                    .collect()
            })
        };
        Self { bbox, fields }
    }
}

/// `bounds` is the grid's extent: a bbox is clamped to it, so every bbox
/// covering the grid keys as no bbox and every disjoint one as empty.
fn canonical_key(
    prefix: &str,
    q: &HashMap<String, String>,
    bounds: Option<(i32, i32, i32, i32)>,
    tag: u64,
) -> String {
    let tq = TileQuery::parse(q);
    let bbox = match (tq.bbox, bounds) {
        (None, _) => "all".to_string(),
        (Some(b), None) => format!("{},{},{},{}", b.0, b.1, b.2, b.3),
        (Some((x1, x2, y1, y2)), Some((bx1, bx2, by1, by2))) => {
            let (x1, x2, y1, y2) = (x1.max(bx1), x2.min(bx2), y1.max(by1), y2.min(by2));
            if x1 > x2 || y1 > y2 {
                "none".to_string()
            } else if (x1, x2, y1, y2) == (bx1, bx2, by1, by2) {
                "all".to_string()
            } else {
                format!("{x1},{x2},{y1},{y2}")
            }
        }
    };
    // The legacy shape ignores the projection.
    let fields = match &tq.fields {
        _ if prefix == "v1" => "-".to_string(),
        None => "*".to_string(),
        Some(f) => f.join(","),
    };
    format!("{prefix}?bbox={bbox}&fields={fields}:{tag}")
}

const VALID_FIELDS: &[&str] = &[
    "id",
    "x",
    "y",
    "type",
    "name",
    "top",
    "left",
    "topLeft",
    "updatedAt",
    "owner",
    "estateId",
    "tokenId",
    "price",
    "expiresAt",
];

/// The tiles inside the bbox (when all four bounds parse), projected to the
/// `include`d fields or everything but the `exclude`d ones. A bbox smaller
/// than the grid is walked coordinate by coordinate; otherwise the grid is.
pub fn filter_tiles<'a>(
    tiles: &'a HashMap<String, Tile>,
    q: &HashMap<String, String>,
) -> Vec<(&'a String, Value)> {
    let TileQuery { bbox, fields } = TileQuery::parse(q);
    let use_bbox = match bbox {
        Some((min_x, max_x, min_y, max_y)) => {
            let w = (max_x as i64 - min_x as i64 + 1) as u64;
            let h = (max_y as i64 - min_y as i64 + 1) as u64;
            w.checked_mul(h)
                .map(|area| area < tiles.len() as u64)
                .unwrap_or(false)
        }
        None => false,
    };
    let mut out = Vec::new();
    if let (true, Some((min_x, max_x, min_y, max_y))) = (use_bbox, bbox) {
        for x in min_x..=max_x {
            for y in min_y..=max_y {
                if let Some((id, tile)) = tiles.get_key_value(&coords_to_id(x, y)) {
                    out.push((id, project(tile, &fields)));
                }
            }
        }
    } else {
        for (id, tile) in tiles {
            if let Some((min_x, max_x, min_y, max_y)) = bbox
                && (tile.x < min_x || tile.x > max_x || tile.y < min_y || tile.y > max_y)
            {
                continue;
            }
            out.push((id, project(tile, &fields)));
        }
    }
    out
}

fn project(tile: &Tile, fields: &Option<Vec<&'static str>>) -> Value {
    let obj = serde_json::to_value(tile).unwrap_or(Value::Null);
    let Some(fields) = fields else { return obj };
    let mut m = Map::new();
    if let Value::Object(src) = obj {
        for f in fields {
            if let Some(v) = src.get(*f) {
                m.insert(f.to_string(), v.clone());
            }
        }
    }
    Value::Object(m)
}

fn legacy_type(tile: &Tile) -> i32 {
    if tile.price.is_some() {
        return 10;
    }
    match tile.tile_type {
        TileType::District => 5,
        TileType::Owned => 9,
        TileType::Unowned => 11,
        TileType::Plaza => 8,
        TileType::Road => 7,
    }
}

pub fn to_legacy(tile: &Tile) -> LegacyTile {
    LegacyTile {
        tile_type: legacy_type(tile),
        x: tile.x,
        y: tile.y,
        top: tile.top.then_some(1),
        left: tile.left.then_some(1),
        top_left: tile.top_left.then_some(1),
        owner: tile.owner.clone(),
        name: tile.name.clone(),
        estate_id: tile.estate_id.clone(),
        price: tile.price,
        rental_price_per_day: tile
            .rental_listing
            .as_ref()
            .map(|rl| rl.max_price_per_day()),
    }
}

fn not_ready(last: i64) -> Response {
    let mut resp = (StatusCode::SERVICE_UNAVAILABLE, "Not ready").into_response();
    http_cache::apply(
        &mut resp,
        last,
        http_cache::DEFAULT_MAX_AGE,
        http_cache::DEFAULT_SWR,
    );
    resp
}

fn cached_json(body: Arc<Vec<u8>>, last: i64, key: &str) -> Response {
    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.as_ref().clone(),
    )
        .into_response();
    let etag = http_cache::etag_for(last, key);
    http_cache::apply_etag(
        &mut resp,
        last,
        &etag,
        http_cache::DEFAULT_MAX_AGE,
        http_cache::DEFAULT_SWR,
    );
    resp
}

async fn serve_tiles(
    state: &Shared,
    headers: &HeaderMap,
    q: &HashMap<String, String>,
    legacy: bool,
) -> Response {
    // One grid for the key, the stamp and the body, so a concurrent swap
    // cannot key one grid's body under the other's tag.
    let grid = state.map.snapshot();
    let last = grid.as_ref().map_or(0, |g| g.last_updated_at);
    let key = canonical_key(
        if legacy { "v1" } else { "v2" },
        q,
        grid.as_ref().and_then(|g| g.bounds),
        grid.as_ref().map_or(0, |g| g.content_tag),
    );
    if let Some(r) = http_cache::not_modified_etag(
        headers,
        last,
        &http_cache::etag_for(last, &key),
        http_cache::DEFAULT_MAX_AGE,
        http_cache::DEFAULT_SWR,
    ) {
        return r;
    }
    let Some(grid) = grid else {
        return not_ready(last);
    };
    if let Some(body) = state.map.cached_tiles(&key) {
        return cached_json(body, last, &key);
    }
    // Concurrent misses on one key build it once.
    let flight = state.map.tiles_flight(&key);
    let _building = flight.lock().await;
    if let Some(body) = state.map.cached_tiles(&key) {
        return cached_json(body, last, &key);
    }
    state.map.count_tiles_build();
    let mut map = Map::new();
    for (id, value) in filter_tiles(&grid.tiles, q) {
        let value = if legacy {
            grid.tiles
                .get(id)
                .map(|t| serde_json::to_value(to_legacy(t)).unwrap_or(Value::Null))
                .unwrap_or(Value::Null)
        } else {
            value
        };
        map.insert(id.clone(), value);
    }
    let body = Arc::new(
        serde_json::to_vec(&json!({ "ok": true, "data": Value::Object(map) })).unwrap_or_default(),
    );
    state.map.store_tiles(key.clone(), body.clone());
    cached_json(body, last, &key)
}

pub async fn get_tiles(
    State(state): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    serve_tiles(&state, &headers, &q, false).await
}

pub async fn get_legacy_tiles(
    State(state): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    serve_tiles(&state, &headers, &q, true).await
}

pub async fn tiles_info(State(state): State<Shared>) -> Response {
    if !state.map.is_ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("cache-control", "no-cache")],
            "Not ready",
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [("cache-control", "no-cache")],
        Json(json!({ "lastUpdatedAt": state.map.last_updated_at() })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn owned_tile() -> Tile {
        Tile {
            id: "1,2".into(),
            x: 1,
            y: 2,
            nft_id: Some("parcel-0xf87e-5".into()),
            tile_type: TileType::Owned,
            top: true,
            left: false,
            top_left: false,
            updated_at: 1_700_000_000_000,
            name: Some("A parcel".into()),
            owner: Some("0xabc".into()),
            estate_id: Some("77".into()),
            token_id: Some("5".into()),
            price: Some(1000.0),
            expires_at: Some(1_800_000_000),
            rental_listing: Some(TileRentalListing {
                expiration: 1_800_000_000_000,
                periods: vec![
                    RentalPeriod {
                        min_days: 1,
                        max_days: 7,
                        price_per_day: "500".into(),
                    },
                    RentalPeriod {
                        min_days: 30,
                        max_days: 30,
                        price_per_day: "10000000000000000000".into(),
                    },
                ],
                updated_at: 1_700_000_000_000,
            }),
        }
    }

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn v2_tile_round_trips_the_upstream_shape() {
        let upstream = json!({
            "id": "-150,-3", "x": -150, "y": -3,
            "nftId": "parcel-0xf87e31492faf9a91b02ee0deaad50d51d56d5d4d-1157",
            "updatedAt": 1768950167312_i64, "type": "owned", "top": false, "left": false, "topLeft": false,
            "name": "WESTERN COAST OCEAN LOT", "owner": "0xcecbe670c11d4d28678955f23e0d2d708d79c893", "tokenId": "1157",
            "rentalListing": {"expiration": 1676304000000_i64, "periods": [{"minDays": 365, "maxDays": 365, "pricePerDay": "10000000000000000000"}], "updatedAt": 1673753917265_i64},
            "price": 10000
        });
        let tile: Tile = serde_json::from_value(upstream.clone()).unwrap();
        assert_eq!(tile.tile_type, TileType::Owned);
        assert_eq!(tile.price, Some(10000.0));
        let back = serde_json::to_value(&tile).unwrap();
        assert_eq!(keys(&back), keys(&upstream));
        assert_eq!(back["rentalListing"], upstream["rentalListing"]);
    }

    #[test]
    fn unowned_tile_omits_optional_fields() {
        let tile: Tile = serde_json::from_value(json!({"id": "3,4", "x": 3, "y": 4, "type": "road", "top": true, "left": true, "topLeft": true})).unwrap();
        let v = serde_json::to_value(&tile).unwrap();
        assert_eq!(
            keys(&v),
            [
                "id",
                "left",
                "top",
                "topLeft",
                "type",
                "updatedAt",
                "x",
                "y"
            ]
        );
    }

    #[test]
    fn legacy_tile_carries_rental_price_per_day_and_the_numeric_type() {
        let legacy = to_legacy(&owned_tile());
        assert_eq!(legacy.tile_type, 10, "a priced tile is type 10");
        assert_eq!(
            legacy.rental_price_per_day.as_deref(),
            Some("10000000000000000000")
        );
        assert_eq!(legacy.top, Some(1));
        assert_eq!(legacy.left, None);
        let mut unpriced = owned_tile();
        unpriced.price = None;
        assert_eq!(to_legacy(&unpriced).tile_type, 9);
    }

    #[test]
    fn include_projects_only_valid_fields_and_exclude_keeps_the_rest() {
        let mut tiles = HashMap::new();
        tiles.insert("1,2".to_string(), owned_tile());
        let q: HashMap<String, String> =
            [("include".to_string(), "x,y,owner,bogus".to_string())].into();
        let out = filter_tiles(&tiles, &q);
        assert_eq!(keys(&out[0].1), ["owner", "x", "y"]);
        let q: HashMap<String, String> = [("exclude".to_string(), "owner,name".to_string())].into();
        let out = filter_tiles(&tiles, &q);
        let k = keys(&out[0].1);
        assert!(
            !k.contains(&"owner".to_string()) && k.contains(&"estateId".to_string()),
            "{k:?}"
        );
    }

    #[test]
    fn bbox_filters_and_walks_the_area_when_it_is_smaller_than_the_grid() {
        let mut tiles = HashMap::new();
        for x in 0..20 {
            for y in 0..20 {
                let mut t = owned_tile();
                t.id = coords_to_id(x, y);
                t.x = x;
                t.y = y;
                tiles.insert(t.id.clone(), t);
            }
        }
        let bbox = |x1: i32, x2: i32, y1: i32, y2: i32| -> HashMap<String, String> {
            [("x1", x1), ("x2", x2), ("y1", y1), ("y2", y2)]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(filter_tiles(&tiles, &bbox(0, 1, 0, 1)).len(), 4);
        assert_eq!(
            filter_tiles(&tiles, &bbox(19, 0, 19, 0)).len(),
            400,
            "reversed bounds are normalized"
        );
        assert_eq!(filter_tiles(&tiles, &bbox(-5, -1, 0, 1)).len(), 0);
    }

    #[test]
    fn param_order_collapses_to_one_key() {
        let a: HashMap<String, String> = [("include".to_string(), "x,y,owner".to_string())].into();
        let b: HashMap<String, String> = [("include".to_string(), "owner,y,x".to_string())].into();
        assert_eq!(
            canonical_key("v2", &a, None, 7),
            canonical_key("v2", &b, None, 7)
        );
        assert_ne!(
            canonical_key("v2", &a, None, 7),
            canonical_key("v2", &a, None, 8),
            "the grid tag is part of the key"
        );
    }

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn junk_spellings_collapse_to_the_same_key() {
        let bounds = Some((-150, 150, -150, 150));
        let key = |pairs: &[(&str, &str)]| canonical_key("v2", &query(pairs), bounds, 1);
        assert_eq!(key(&[("exclude", "junk1")]), key(&[("exclude", "junk2")]));
        assert_eq!(
            key(&[("include", "x,bogus")]),
            key(&[("include", "x,x,other")])
        );
        assert_eq!(
            key(&[("x1", "01"), ("x2", "5"), ("y1", "0"), ("y2", "5")]),
            key(&[("x1", "1"), ("x2", "5"), ("y1", "0"), ("y2", "5")])
        );
        assert_eq!(
            key(&[
                ("x1", "-9000"),
                ("x2", "9000"),
                ("y1", "-200"),
                ("y2", "200")
            ]),
            key(&[("x1", "x")]),
            "a bbox covering the grid keys as no bbox"
        );
        assert_eq!(
            key(&[("x1", "500"), ("x2", "600"), ("y1", "0"), ("y2", "0")]),
            key(&[("x1", "700"), ("x2", "800"), ("y1", "0"), ("y2", "0")]),
            "disjoint bboxes share the empty key"
        );
        assert_ne!(
            key(&[("x1", "0"), ("x2", "1"), ("y1", "0"), ("y2", "1")]),
            key(&[("x1", "0"), ("x2", "2"), ("y1", "0"), ("y2", "1")]),
            "distinct areas keep distinct keys"
        );
        assert_ne!(key(&[("include", "x")]), key(&[("include", "y")]));
        assert_eq!(
            key(&[("x1", "-200"), ("x2", "10"), ("y1", "5"), ("y2", "500")]),
            key(&[("x1", "-150"), ("x2", "10"), ("y1", "5"), ("y2", "150")]),
            "a partly overlapping bbox keys as its part inside the grid"
        );
        let legacy = |pairs: &[(&str, &str)]| canonical_key("v1", &query(pairs), bounds, 1);
        assert_eq!(
            legacy(&[("include", "x")]),
            legacy(&[("exclude", "owner")]),
            "the legacy shape ignores the projection"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_tile_misses_build_the_body_exactly_once() {
        let state = crate::api::tests_support::state_without_db().await;
        let mut tiles = HashMap::new();
        for x in 0..50 {
            for y in 0..50 {
                let mut t = owned_tile();
                t.id = coords_to_id(x, y);
                t.x = x;
                t.y = y;
                tiles.insert(t.id.clone(), t);
            }
        }
        state.map.set(super::super::Grid::from_tiles(
            tiles,
            1,
            None,
            chrono::Utc::now().naive_utc(),
        ));
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let calls: Vec<_> = (0..16)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    serve_tiles(&state, &HeaderMap::new(), &HashMap::new(), false)
                        .await
                        .status()
                })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap(), StatusCode::OK);
        }
        assert_eq!(state.map.tiles_builds(), 1);
    }
}
