//! `/satellite/{z}/{x}/{y}`: the 256 px satellite pyramid (zoom 0 to 9 over
//! the 512x512-parcel world), painted from the 17x17-parcel top-down region
//! renders over the parcel grid of the mirrored tile grid.
//!
//! The renders come out of the rendering rig behind the dev gateway, not from
//! this node. The old `catalyrst-satellite-mirror` timer copied them into a
//! directory the map service rescanned every 15 s; here the `satellite` pass
//! (a periodic job, `explore_api.mirror_passes`) copies them into the ONE blob
//! store and keys each one in `explore.satellite_region` by region centre and
//! variant (`<cx>/<cy>.png`, and the `<cy>.t.png` thumbnail the low zooms
//! sample, which the old timer never fetched): the manifest first, then one
//! conditional GET per listed region and variant. A region checked in this
//! pass carries `checked_at >= started_at`, so a killed pass resumes where it
//! stopped; a complete pass deletes the regions the manifest no longer lists
//! (refused when the manifest lists fewer than half of the held regions: a
//! truncated manifest must not empty the map). Nothing is served from the
//! upstream at request time. The upstream is `SATELLITE_MIRROR_UPSTREAM_URL`,
//! with no default: unset, the pass is not scheduled. A render rig on a
//! private (tailnet) address is refused by the SSRF guard: an operator whose
//! node reaches the rig privately sets
//! `SATELLITE_MIRROR_ALLOW_PRIVATE_UPSTREAM` (the relay's opt-in rule), and
//! the pass then reads that one configured upstream unguarded.
//!
//! The index is held in memory ([`SatelliteState`]) and reloaded after every
//! pass; its generation (a signature over the region set and blob ids) and
//! the grid's content tag key the output cache, so a new render or a new grid
//! is never served from a stale tile. Decoded regions sit in a byte-budgeted
//! LRU (`SATELLITE_SOURCE_BUDGET_MB`), encoded tiles in an entry-bounded one
//! (`SATELLITE_OUTPUT_ENTRIES`), and one decode per region runs at a time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::NaiveDateTime;
use parking_lot::{Mutex, RwLock};
use sqlx::PgPool;
use tiny_skia::{IntSize, Pixmap};

use super::Shared;
use super::map::Grid;
use super::map::tiles::TileType;
use super::mirror::{self, Conditional};
use crate::jobs::JobContext;
use crate::storage::Storage;

pub const MIRROR: &str = "satellite";
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(6 * 3600);
pub const DEFAULT_SOURCE_BUDGET_BYTES: usize = 256 << 20;
pub const DEFAULT_OUTPUT_ENTRIES: usize = 4096;

const MANIFEST_CAP: usize = 1 << 20;
const MANIFEST_MAX_REGIONS: usize = 16_384;
const REGION_CAP: usize = 32 << 20;
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(60);
const REGION_TIMEOUT: Duration = Duration::from_secs(300);
const COORD_BOUND: i32 = 512;
const CACHE_CONTROL: &str = "public, max-age=3600, stale-while-revalidate=3600";

const WORLD_MIN: f64 = -256.0;
const WORLD_MAX: f64 = 256.0;
const WORLD_PARCELS: f64 = WORLD_MAX - WORLD_MIN;

const TILE_PX: u32 = 256;

const REGION_PARCELS: f64 = 17.0;
const REGION_HALF: i32 = 8;

const MAX_ZOOM: i32 = 9;

const THUMB_ZOOM_MAX: i32 = 4;

const MAX_SAMPLES: i32 = 3;

// The engine's clear colour: every region pixel with no geometry under it is exactly
// this, so it is keyed out while sampling and the parcel base layer shows through.
const VOID: [u8; 4] = [15, 18, 26, 255];

// Parcel base layer, drawn under the renders from the parcel grid (same data as the
// minimap) so land the renderer has nothing for is still land, not a black slab.
// Daylight tones to sit with the renders; roads and plazas keep their minimap hue.
fn base_color(t: TileType) -> [u8; 4] {
    match t {
        TileType::Road => [0x8f, 0x8f, 0x97, 255],
        TileType::Plaza => [0x7f, 0xbf, 0x7a, 255],
        TileType::District => [0x8d, 0x91, 0xd9, 255],
        TileType::Owned => [0x78, 0xb0, 0x6b, 255],
        TileType::Unowned => [0x6e, 0xa5, 0x62, 255],
    }
}

// Parcel edges get a slightly darker line once a parcel is wide enough to show one;
// `top`/`left` mark an estate neighbour, so estates read as one block like the minimap.
const GRID_MIN_PX: f64 = 12.0;

fn darken(c: [u8; 4]) -> [u8; 4] {
    [
        (c[0] as u32 * 85 / 100) as u8,
        (c[1] as u32 * 85 / 100) as u8,
        (c[2] as u32 * 85 / 100) as u8,
        c[3],
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    Full,
    Thumb,
}

impl Variant {
    pub const ALL: [Variant; 2] = [Variant::Full, Variant::Thumb];

    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Full => "full",
            Variant::Thumb => "thumb",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Variant::Full),
            "thumb" => Some(Variant::Thumb),
            _ => None,
        }
    }

    /// The object under the upstream directory: `<cx>/<cy>.png`, `<cx>/<cy>.t.png`.
    fn upstream_path(self, cx: i32, cy: i32) -> String {
        match self {
            Variant::Full => format!("{cx}/{cy}.png"),
            Variant::Thumb => format!("{cx}/{cy}.t.png"),
        }
    }
}

struct Lru<K, V> {
    budget: usize,
    used: usize,
    seq: u64,
    map: HashMap<K, (V, usize, u64)>,
    order: BTreeMap<u64, K>,
}

impl<K: Clone + Hash + Eq, V: Clone> Lru<K, V> {
    fn new(budget: usize) -> Self {
        Self {
            budget: budget.max(1),
            used: 0,
            seq: 0,
            map: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let (value, _, seq) = self.map.get_mut(key)?;
        self.order.remove(seq);
        self.seq += 1;
        *seq = self.seq;
        self.order.insert(self.seq, key.clone());
        Some(value.clone())
    }

    /// Inserts `value` weighing `weight`, evicting the least recently used
    /// entries past the budget; the newest entry always stays.
    fn insert(&mut self, key: K, value: V, weight: usize) {
        if let Some((_, w, seq)) = self.map.remove(&key) {
            self.order.remove(&seq);
            self.used -= w;
        }
        self.seq += 1;
        self.used += weight;
        self.map.insert(key.clone(), (value, weight, self.seq));
        self.order.insert(self.seq, key);
        while self.used > self.budget && self.map.len() > 1 {
            let Some((_, victim)) = self.order.pop_first() else {
                break;
            };
            if let Some((_, w, _)) = self.map.remove(&victim) {
                self.used -= w;
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

struct Region {
    w: u32,
    h: u32,
    /// Source pixels per parcel.
    ppp: f64,
    data: Vec<u8>,
}

impl Region {
    fn bytes(&self) -> usize {
        self.data.len() + std::mem::size_of::<Region>()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RegionBlobs {
    full: Option<String>,
    thumb: Option<String>,
}

#[derive(Default)]
struct Index {
    regions: HashMap<(i32, i32), RegionBlobs>,
    generation: u64,
    signature: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TileKey {
    z: i32,
    x: i32,
    y: i32,
}

#[derive(Clone)]
struct CachedTile {
    generation: u64,
    /// The content tag of the grid the base layer was painted from (0: none).
    base_tag: u64,
    bytes: Bytes,
}

pub struct SatelliteState {
    index: RwLock<Arc<Index>>,
    sources: Mutex<Lru<String, Arc<Region>>>,
    decoding: crate::keyed_locks::KeyedLocks<String>,
    output: Mutex<Lru<TileKey, CachedTile>>,
    empty_png: Bytes,
    #[cfg(test)]
    overlap_scans: std::sync::atomic::AtomicUsize,
}

impl SatelliteState {
    pub fn new(source_budget_bytes: usize, output_entries: usize) -> Self {
        let empty = Pixmap::new(TILE_PX, TILE_PX)
            .and_then(|p| p.encode_png().ok())
            .unwrap_or_default();
        Self {
            index: RwLock::new(Arc::new(Index::default())),
            sources: Mutex::new(Lru::new(source_budget_bytes)),
            decoding: crate::keyed_locks::KeyedLocks::default(),
            output: Mutex::new(Lru::new(output_entries)),
            empty_png: Bytes::from(empty),
            #[cfg(test)]
            overlap_scans: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn generation(&self) -> u64 {
        self.index.read().generation
    }

    pub fn region_count(&self) -> usize {
        self.index.read().regions.len()
    }

    /// Replaces the index with `rows` (cx, cy, variant, blob); the generation
    /// moves only when the set or a blob id changed.
    fn install(&self, mut rows: Vec<(i32, i32, Variant, String)>) {
        rows.sort_by(|a, b| (a.0, a.1, a.2.as_str()).cmp(&(b.0, b.1, b.2.as_str())));
        let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                sig ^= *b as u64;
                sig = sig.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        let mut regions: HashMap<(i32, i32), RegionBlobs> = HashMap::new();
        for (cx, cy, variant, cid) in rows {
            mix(format!("{cx},{cy},{},{cid};", variant.as_str()).as_bytes());
            let entry = regions.entry((cx, cy)).or_default();
            match variant {
                Variant::Full => entry.full = Some(cid),
                Variant::Thumb => entry.thumb = Some(cid),
            }
        }
        let mut idx = self.index.write();
        if sig != idx.signature || idx.generation == 0 {
            let generation = idx.generation + 1;
            tracing::info!(
                regions = regions.len(),
                generation,
                "satellite region set changed; bumping generation"
            );
            *idx = Arc::new(Index {
                regions,
                generation,
                signature: sig,
            });
        }
    }

    /// The persisted index (at startup and after every pass).
    pub async fn load(&self, pool: &PgPool) -> anyhow::Result<()> {
        let rows: Vec<(i32, i32, String, String)> =
            sqlx::query_as("SELECT cx, cy, variant, blob_cid FROM explore.satellite_region")
                .fetch_all(pool)
                .await
                .context("reading explore.satellite_region")?;
        self.install(
            rows.into_iter()
                .filter_map(|(cx, cy, v, cid)| Variant::parse(&v).map(|v| (cx, cy, v, cid)))
                .collect(),
        );
        Ok(())
    }

    /// The PNG of tile `z/x/y`, `None` outside the pyramid. Regions whose
    /// blob is missing from the store are skipped (logged), as the old
    /// renderer skipped an unreadable file.
    pub async fn tile_png(
        &self,
        storage: &Storage,
        z: i32,
        x: i32,
        y: i32,
        base: Option<Arc<Grid>>,
    ) -> anyhow::Result<Option<Bytes>> {
        if !(0..=MAX_ZOOM).contains(&z) {
            return Ok(None);
        }
        let n = 1i32 << z;
        if x < 0 || y < 0 || x >= n || y >= n {
            return Ok(None);
        }
        let key = TileKey { z, x, y };
        let base_tag = base.as_ref().map_or(0, |g| g.content_tag);
        let index = self.index.read().clone();
        if let Some(hit) = self.output.lock().get(&key)
            && hit.generation == index.generation
            && hit.base_tag == base_tag
        {
            return Ok(Some(hit.bytes));
        }

        let want_thumb = z <= THUMB_ZOOM_MAX;
        let (wx0, wx1, wy0, wy1) = tile_world_bounds(z, x, y);
        let mut overlap: Vec<(i32, i32, String)> = index
            .regions
            .iter()
            .filter(|&(&(cx, cy), _)| {
                #[cfg(test)]
                self.overlap_scans
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let rx0 = (cx - REGION_HALF) as f64;
                let rx1 = (cx + REGION_HALF + 1) as f64;
                let ry0 = (cy - REGION_HALF) as f64;
                let ry1 = (cy + REGION_HALF + 1) as f64;
                rx0 < wx1 && rx1 > wx0 && ry0 < wy1 && ry1 > wy0
            })
            .filter_map(|(&(cx, cy), blobs)| {
                let cid = if want_thumb {
                    blobs.thumb.clone().or_else(|| blobs.full.clone())
                } else {
                    blobs.full.clone().or_else(|| blobs.thumb.clone())
                };
                cid.map(|cid| (cx, cy, cid))
            })
            .collect();
        overlap.sort_by_key(|&(cx, cy, _)| (cx, cy));

        let bytes = if overlap.is_empty() && base.is_none() {
            self.empty_png.clone()
        } else {
            let mut regions = Vec::with_capacity(overlap.len());
            for (cx, cy, cid) in overlap {
                match self.region(storage, &cid).await {
                    Ok(Some(r)) => regions.push((cx, cy, r)),
                    Ok(None) => {
                        tracing::warn!(cx, cy, blob = %cid, "satellite region blob missing from the store; skipped")
                    }
                    Err(e) => {
                        tracing::warn!(cx, cy, blob = %cid, error = %format!("{e:#}"), "satellite region unreadable; skipped")
                    }
                }
            }
            let empty = self.empty_png.clone();
            tokio::task::spawn_blocking(move || {
                render_tile(z, x, y, &regions, base.as_deref())
                    .map(Bytes::from)
                    .unwrap_or(empty)
            })
            .await
            .context("satellite render task")?
        };
        self.output.lock().insert(
            key,
            CachedTile {
                generation: index.generation,
                base_tag,
                bytes: bytes.clone(),
            },
            1,
        );
        Ok(Some(bytes))
    }

    /// The decoded region behind `cid`: the LRU, else one decode at a time
    /// per blob (a burst of tiles over one region decodes it once).
    async fn region(&self, storage: &Storage, cid: &str) -> anyhow::Result<Option<Arc<Region>>> {
        if let Some(r) = self.sources.lock().get(&cid.to_string()) {
            return Ok(Some(r));
        }
        let lock = self.decoding.claim(&cid.to_string());
        let result = async {
            let _guard = lock.lock().await;
            if let Some(r) = self.sources.lock().get(&cid.to_string()) {
                return Ok(Some(r));
            }
            let Some(raw) = storage
                .retrieve(cid)
                .await
                .context("reading the region blob")?
            else {
                return Ok(None);
            };
            let region = tokio::task::spawn_blocking(move || decode_region(&raw))
                .await
                .context("region decode task")??;
            let region = Arc::new(region);
            self.sources
                .lock()
                .insert(cid.to_string(), region.clone(), region.bytes());
            Ok(Some(region))
        }
        .await;
        result
    }

    #[cfg(test)]
    fn cached_sources(&self) -> usize {
        self.sources.lock().len()
    }
}

fn decode_region(raw: &[u8]) -> anyhow::Result<Region> {
    let pm = Pixmap::decode_png(raw)
        .map_err(|e| anyhow::anyhow!("region is not a decodable PNG: {e}"))?;
    let (w, h) = (pm.width(), pm.height());
    Ok(Region {
        w,
        h,
        ppp: w as f64 / REGION_PARCELS,
        data: pm.data().to_vec(),
    })
}

fn render_tile(
    z: i32,
    x: i32,
    y: i32,
    regions: &[(i32, i32, Arc<Region>)],
    base: Option<&Grid>,
) -> Option<Vec<u8>> {
    let (wx0, _wx1, _wy0, wy1) = tile_world_bounds(z, x, y);

    let ppp_out = parcels_per_tile(z) / TILE_PX as f64;

    let mut out = vec![0u8; (TILE_PX * TILE_PX * 4) as usize];

    if let Some(base) = base {
        paint_base(&mut out, base, wx0, wy1, ppp_out);
    }

    for (cx, cy, region) in regions {
        let (cx, cy) = (*cx, *cy);
        let rx0 = (cx - REGION_HALF) as f64;
        let ry_top = (cy + REGION_HALF + 1) as f64;
        let ry_bot = (cy - REGION_HALF) as f64;

        let ox_lo = (((cx - REGION_HALF) as f64 - wx0) / ppp_out)
            .floor()
            .max(0.0) as i32;
        let ox_hi = (((cx + REGION_HALF + 1) as f64 - wx0) / ppp_out).ceil() as i32;
        let ox_hi = ox_hi.min(TILE_PX as i32);
        let oy_lo = ((wy1 - ry_top) / ppp_out).floor().max(0.0) as i32;
        let oy_hi = ((wy1 - ry_bot) / ppp_out).ceil() as i32;
        let oy_hi = oy_hi.min(TILE_PX as i32);

        let fp = ppp_out * region.ppp;
        let samples = (fp.round() as i32).clamp(1, MAX_SAMPLES);

        for oy in oy_lo..oy_hi {
            let world_y = wy1 - (oy as f64 + 0.5) * ppp_out;
            for ox in ox_lo..ox_hi {
                let world_x = wx0 + (ox as f64 + 0.5) * ppp_out;

                let sc = (world_x - rx0) * region.ppp;
                let sr = (ry_top - world_y) * region.ppp;

                // `hit` counts samples that landed on rendered geometry; void samples
                // count toward `seen` only, so a pixel half on a building and half on
                // nothing blends the building over the base at 50%.
                let (mut r, mut g, mut b, mut a, mut hit, mut seen) =
                    (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
                for sy in 0..samples {
                    let fy = sr + (sy as f64 + 0.5) / samples as f64 * fp - fp / 2.0;
                    let py = fy.floor() as i32;
                    if py < 0 || py >= region.h as i32 {
                        continue;
                    }
                    for sx in 0..samples {
                        let fx = sc + (sx as f64 + 0.5) / samples as f64 * fp - fp / 2.0;
                        let px = fx.floor() as i32;
                        if px < 0 || px >= region.w as i32 {
                            continue;
                        }
                        let i = ((py as u32 * region.w + px as u32) * 4) as usize;
                        seen += 1;
                        if region.data[i..i + 4] == VOID {
                            continue;
                        }
                        r += region.data[i] as u32;
                        g += region.data[i + 1] as u32;
                        b += region.data[i + 2] as u32;
                        a += region.data[i + 3] as u32;
                        hit += 1;
                    }
                }
                if hit == 0 {
                    continue;
                }
                let o = ((oy as u32 * TILE_PX + ox as u32) * 4) as usize;
                let src = [
                    (r / hit) as u8,
                    (g / hit) as u8,
                    (b / hit) as u8,
                    (a / hit) as u8,
                ];
                if hit == seen || out[o + 3] == 0 {
                    out[o..o + 4].copy_from_slice(&src);
                } else {
                    // Coverage blend over whatever the base painted here.
                    for c in 0..4 {
                        let under = out[o + c] as u32;
                        let over = src[c] as u32;
                        out[o + c] = ((over * hit + under * (seen - hit)) / seen) as u8;
                    }
                }
            }
        }
    }

    Pixmap::from_vec(out, IntSize::from_wh(TILE_PX, TILE_PX)?)?
        .encode_png()
        .ok()
}

// Paint the parcel grid under a tile: one lookup per output pixel, colour by tile type,
// transparent where the grid has no parcel (outside the city the page's own surface
// shows). Parcels wider than GRID_MIN_PX get a darker top/left edge unless that edge
// joins an estate neighbour.
// The last parcel looked up while painting a row: coords, colour, `top`, `left`.
type LastParcel = ((i32, i32), [u8; 4], bool, bool);

fn paint_base(out: &mut [u8], base: &Grid, wx0: f64, wy1: f64, ppp_out: f64) {
    let px_per_parcel = 1.0 / ppp_out;
    let grid = px_per_parcel >= GRID_MIN_PX;
    let mut last: Option<LastParcel> = None;
    for oy in 0..TILE_PX {
        let world_y = wy1 - (oy as f64 + 0.5) * ppp_out;
        let py = world_y.floor() as i32;
        // Top edge of the parcel is its highest world row: the first pixel row that
        // maps into it.
        let top_edge = grid && ((wy1 - oy as f64 * ppp_out).floor() as i32 != py);
        for ox in 0..TILE_PX {
            let world_x = wx0 + (ox as f64 + 0.5) * ppp_out;
            let px = world_x.floor() as i32;
            let (color, top, left) = match last {
                Some((coords, c, t, l)) if coords == (px, py) => (c, t, l),
                _ => {
                    let Some(tile) = base.tiles.get(&super::map::tiles::coords_to_id(px, py))
                    else {
                        last = None;
                        continue;
                    };
                    let c = base_color(tile.tile_type);
                    last = Some(((px, py), c, tile.top, tile.left));
                    (c, tile.top, tile.left)
                }
            };
            let left_edge = grid && ((wx0 + ox as f64 * ppp_out).floor() as i32 != px);
            let c = if (top_edge && !top) || (left_edge && !left) {
                darken(color)
            } else {
                color
            };
            let o = ((oy * TILE_PX + ox) * 4) as usize;
            out[o..o + 4].copy_from_slice(&c);
        }
    }
}

#[inline]
fn parcels_per_tile(z: i32) -> f64 {
    WORLD_PARCELS / (1i32 << z) as f64
}

fn tile_world_bounds(z: i32, x: i32, y: i32) -> (f64, f64, f64, f64) {
    let span = parcels_per_tile(z);
    let x0 = WORLD_MIN + x as f64 * span;
    let x1 = x0 + span;
    let y1 = WORLD_MAX - y as f64 * span;
    let y0 = y1 - span;
    (x0, x1, y0, y1)
}

/// `GET /satellite/{z}/{x}/{y}[.png|.jpg]` (at the root, the explore bundle's
/// path, and under `/api`, the gateway's). `.jpg` is the suffix /about
/// advertises when the satellite view is this node's own; the tile is a PNG
/// either way.
pub async fn tile(
    State(state): State<Shared>,
    Path((z, x, y_ext)): Path<(i32, i32, String)>,
) -> Response {
    let y_str = y_ext
        .strip_suffix(".png")
        .or_else(|| y_ext.strip_suffix(".jpg"))
        .unwrap_or(&y_ext);
    let Ok(y) = y_str.parse::<i32>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let base = state.map.snapshot();
    match state
        .satellite
        .tile_png(&state.content.storage, z, x, y, base)
        .await
    {
        Ok(Some(bytes)) => {
            let mut resp = Response::new(axum::body::Body::from(bytes));
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            h.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_CONTROL),
            );
            resp
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => super::internal_error("rendering the satellite tile", format!("{e:#}")),
    }
}

/// The manifest: a JSON array of `[cx, cy]` region centres. Deduplicated and
/// sorted; a malformed entry or an out-of-range centre refuses the manifest.
pub fn parse_manifest(body: &[u8]) -> anyhow::Result<Vec<(i32, i32)>> {
    let pairs: Vec<(i64, i64)> =
        serde_json::from_slice(body).context("manifest is not a JSON array of [cx, cy] pairs")?;
    anyhow::ensure!(
        pairs.len() <= MANIFEST_MAX_REGIONS,
        "manifest lists {} regions, more than {MANIFEST_MAX_REGIONS}",
        pairs.len()
    );
    let mut out: Vec<(i32, i32)> = Vec::with_capacity(pairs.len());
    for (cx, cy) in pairs {
        anyhow::ensure!(
            (-(COORD_BOUND as i64)..=COORD_BOUND as i64).contains(&cx)
                && (-(COORD_BOUND as i64)..=COORD_BOUND as i64).contains(&cy),
            "manifest region {cx},{cy} is outside the world"
        );
        out.push((cx as i32, cy as i32));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// PNG signature and IHDR dimensions, without decoding the image.
pub fn png_dimensions(body: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if body.len() < 24 || &body[..8] != SIG || &body[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(body[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(body[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// cx, cy, variant, blob_cid, last_modified, etag, checked_at.
type HeldRow = (
    i32,
    i32,
    String,
    String,
    Option<String>,
    Option<String>,
    NaiveDateTime,
);

struct HeldRegion {
    blob_cid: String,
    last_modified: Option<String>,
    etag: Option<String>,
    checked_at: NaiveDateTime,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassCounts {
    pub listed: usize,
    pub fetched: usize,
    pub not_modified: usize,
    pub resumed_skips: usize,
    pub missing_thumbs: usize,
    pub failed: usize,
    pub pruned: u64,
}

/// What the manifest GET answered, fetched ahead of the pass marker (see
/// [`run_pass`]).
enum Manifest {
    Regions(Vec<(i32, i32)>),
    /// The upstream does not hold a manifest (404, 403 or 410).
    Missing(StatusCode),
}

fn upstream_base(state: &Shared) -> anyhow::Result<String> {
    state
        .settings
        .satellite_upstream
        .as_deref()
        .map(|u| u.trim_end_matches('/').to_string())
        .context("SATELLITE_MIRROR_UPSTREAM_URL is unset: no satellite mirror")
}

async fn fetch_manifest(state: &Shared, manifest_url: &str) -> anyhow::Result<Manifest> {
    let reach = if state.settings.satellite_allow_private {
        mirror::Reach::PrivateAllowed
    } else {
        mirror::Reach::Public
    };
    match mirror::fetch_conditional(
        &state.http,
        &state.settings,
        manifest_url,
        None,
        None,
        MANIFEST_CAP,
        MANIFEST_TIMEOUT,
        reach,
    )
    .await?
    {
        Conditional::Fetched { body, .. } => Ok(Manifest::Regions(parse_manifest(&body)?)),
        Conditional::Missing(status) => Ok(Manifest::Missing(status)),
        Conditional::NotModified => anyhow::bail!(
            "{} answered 304 to an unconditional GET",
            mirror::shown(manifest_url)
        ),
    }
}

/// A 404 for the manifest, the one answer that means "not seeded" when this
/// node holds no region (a 403 is the bucket refusing to say, and any
/// transport error is reported through the marker).
fn manifest_not_found(manifest: &anyhow::Result<Manifest>) -> bool {
    matches!(manifest, Ok(Manifest::Missing(status)) if *status == StatusCode::NOT_FOUND)
}

async fn holds_any_region(pool: &PgPool) -> anyhow::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM explore.satellite_region)")
        .fetch_one(pool)
        .await
        .context("checking explore.satellite_region for held regions")
}

/// One pass (see the module doc); `now` is the pass clock.
///
/// The manifest is fetched ahead of the pass marker: a 404 over a tree this
/// node never seeded (`explore.satellite_region` empty) is not a pass at all,
/// nothing to mirror and nothing to keep, so the marker is neither started
/// nor completed and the job sees a plain Ok (no error backoff), with one
/// notice per process. Every other answer, a 404 over a seeded tree
/// included, is carried into the marker and reported as a failed pass.
pub async fn run_pass(state: &Shared, ctx: &JobContext, now: NaiveDateTime) -> anyhow::Result<()> {
    let window = state
        .settings
        .satellite_interval
        .max(Duration::from_secs(3600))
        * 4;
    let manifest_url = format!("{}/manifest.json", upstream_base(state)?);
    let manifest = fetch_manifest(state, &manifest_url).await;
    if manifest_not_found(&manifest) && !holds_any_region(&state.pool).await? {
        static NOT_SEEDED: std::sync::Once = std::sync::Once::new();
        NOT_SEEDED.call_once(|| {
            tracing::info!("satellite render tree not seeded (run catalyst-satellite-seed)")
        });
        ctx.progress(format!(
            "{MIRROR}: render tree not seeded (run catalyst-satellite-seed)"
        ));
        return Ok(());
    }
    mirror::run_resumable(&state.pool, MIRROR, ctx, now, window, |started_at| async move {
        let manifest = match manifest? {
            Manifest::Regions(regions) => regions,
            Manifest::Missing(status) => anyhow::bail!("{} answered {status}; keeping the held regions", mirror::shown(&manifest_url)),
        };
        let counts = pass_body(state, ctx, started_at, manifest).await?;
        tracing::info!(
            listed = counts.listed,
            fetched = counts.fetched,
            not_modified = counts.not_modified,
            resumed_skips = counts.resumed_skips,
            thumbs_absent = counts.missing_thumbs,
            pruned = counts.pruned,
            %started_at,
            "satellite regions mirrored"
        );
        ctx.progress(format!(
            "{MIRROR}: {} regions listed, {} fetched, {} unchanged, {} resumed, {} thumbs absent, {} pruned",
            counts.listed, counts.fetched, counts.not_modified, counts.resumed_skips, counts.missing_thumbs, counts.pruned
        ));
        Ok(mirror::PassOutcome { items: counts.listed as i64, cursor: None })
    })
    .await
}

/// The body of a pass under its marker: `manifest` is what [`fetch_manifest`]
/// listed.
pub async fn pass_body(
    state: &Shared,
    ctx: &JobContext,
    started_at: NaiveDateTime,
    manifest: Vec<(i32, i32)>,
) -> anyhow::Result<PassCounts> {
    let base = upstream_base(state)?;
    anyhow::ensure!(
        !manifest.is_empty(),
        "the manifest lists no regions; keeping the held regions"
    );

    let rows: Vec<HeldRow> = sqlx::query_as(
        "SELECT cx, cy, variant, blob_cid, last_modified, etag, checked_at FROM explore.satellite_region",
    )
    .fetch_all(&state.pool)
    .await
    .context("reading explore.satellite_region")?;
    let mut held: HashMap<(i32, i32, Variant), HeldRegion> = HashMap::new();
    for (cx, cy, v, blob_cid, last_modified, etag, checked_at) in rows {
        if let Some(v) = Variant::parse(&v) {
            held.insert(
                (cx, cy, v),
                HeldRegion {
                    blob_cid,
                    last_modified,
                    etag,
                    checked_at,
                },
            );
        }
    }

    let mut counts = PassCounts {
        listed: manifest.len(),
        ..Default::default()
    };
    let mut last_error: Option<String> = None;
    for &(cx, cy) in &manifest {
        for variant in Variant::ALL {
            anyhow::ensure!(
                !ctx.is_cancelled(),
                "cancelled after {} fetches; the next pass resumes",
                counts.fetched
            );
            let current = held.get(&(cx, cy, variant));
            if current.is_some_and(|h| h.checked_at >= started_at) {
                counts.resumed_skips += 1;
                continue;
            }
            match mirror_region(state, &base, cx, cy, variant, current).await {
                Ok(RegionOutcome::Fetched) => counts.fetched += 1,
                Ok(RegionOutcome::NotModified) => counts.not_modified += 1,
                Ok(RegionOutcome::ThumbAbsent) => counts.missing_thumbs += 1,
                Err(e) => {
                    counts.failed += 1;
                    let message = format!("{cx},{cy} {}: {e:#}", variant.as_str());
                    tracing::warn!(cx, cy, variant = variant.as_str(), error = %message, "satellite region not mirrored");
                    last_error = Some(message);
                }
            }
        }
        ctx.progress(format!(
            "{MIRROR}: at region {cx},{cy}; {} fetched, {} unchanged, {} failed",
            counts.fetched, counts.not_modified, counts.failed
        ));
    }
    if counts.failed > 0 {
        tracing::warn!(
            failed = counts.failed,
            fetched = counts.fetched,
            not_modified = counts.not_modified,
            resumed_skips = counts.resumed_skips,
            "satellite pass incomplete"
        );
        state.satellite.load(&state.pool).await?;
        anyhow::bail!(
            "{} region fetches failed (last: {}); the next pass retries them",
            counts.failed,
            last_error.unwrap_or_default()
        );
    }

    let held_full = held.keys().filter(|(_, _, v)| *v == Variant::Full).count();
    let listed: HashSet<(i32, i32)> = manifest.iter().copied().collect();
    let stale = held
        .keys()
        .filter(|(cx, cy, _)| !listed.contains(&(*cx, *cy)))
        .count();
    if stale > 0 {
        if manifest.len() * 2 < held_full {
            tracing::warn!(
                listed = manifest.len(),
                held = held_full,
                "the manifest lists fewer than half the held regions; not pruning"
            );
        } else {
            let (xs, ys): (Vec<i32>, Vec<i32>) = manifest.iter().copied().unzip();
            counts.pruned = sqlx::query(
                "DELETE FROM explore.satellite_region r
                 WHERE NOT EXISTS (SELECT 1 FROM unnest($1::int[], $2::int[]) AS m(cx, cy) WHERE m.cx = r.cx AND m.cy = r.cy)",
            )
            .bind(&xs)
            .bind(&ys)
            .execute(&state.pool)
            .await
            .context("pruning the regions the manifest no longer lists")?
            .rows_affected();
        }
    }
    state.satellite.load(&state.pool).await?;
    Ok(counts)
}

enum RegionOutcome {
    Fetched,
    NotModified,
    ThumbAbsent,
}

async fn mirror_region(
    state: &Shared,
    base: &str,
    cx: i32,
    cy: i32,
    variant: Variant,
    current: Option<&HeldRegion>,
) -> anyhow::Result<RegionOutcome> {
    let url = format!("{base}/{}", variant.upstream_path(cx, cy));
    // Validators only for a copy the store still holds: a blob lost from the
    // store must be fetched again, never confirmed by a 304.
    let usable = match current {
        Some(h) => state
            .content
            .storage
            .exist(&h.blob_cid)
            .await
            .context("probing the held region blob")?,
        None => false,
    };
    let (etag, last_modified) = match (usable, current) {
        (true, Some(h)) => (h.etag.as_deref(), h.last_modified.as_deref()),
        _ => (None, None),
    };
    let reach = if state.settings.satellite_allow_private {
        mirror::Reach::PrivateAllowed
    } else {
        mirror::Reach::Public
    };
    match mirror::fetch_conditional(
        &state.http,
        &state.settings,
        &url,
        etag,
        last_modified,
        REGION_CAP,
        REGION_TIMEOUT,
        reach,
    )
    .await?
    {
        Conditional::NotModified => {
            touch_region(&state.pool, cx, cy, variant).await?;
            Ok(RegionOutcome::NotModified)
        }
        Conditional::Missing(status) if variant == Variant::Thumb => {
            sqlx::query("DELETE FROM explore.satellite_region WHERE cx = $1 AND cy = $2 AND variant = 'thumb'")
                .bind(cx)
                .bind(cy)
                .execute(&state.pool)
                .await
                .context("dropping an absent thumbnail")?;
            tracing::debug!(cx, cy, %status, "no thumbnail upstream");
            Ok(RegionOutcome::ThumbAbsent)
        }
        Conditional::Missing(status) => anyhow::bail!("{} answered {status}", mirror::shown(&url)),
        Conditional::Fetched {
            body,
            etag,
            last_modified,
            ..
        } => {
            let (width, height) = png_dimensions(&body)
                .with_context(|| format!("{} is not a PNG", mirror::shown(&url)))?;
            let body = Bytes::from(body);
            let (check, cid) = {
                let body = body.clone();
                tokio::task::spawn_blocking(move || {
                    (
                        decode_region(&body).map(|_| ()),
                        crate::auth::hash::hash_bytes_v1(&body),
                    )
                })
                .await
                .context("region verify task")?
            };
            check.with_context(|| mirror::shown(&url))?;
            if !state
                .content
                .storage
                .exist(&cid)
                .await
                .context("probing the region blob")?
            {
                state
                    .content
                    .storage
                    .store(&cid, body.clone())
                    .await
                    .context("storing the region blob")?;
            }
            sqlx::query(
                "INSERT INTO explore.satellite_region
                     (cx, cy, variant, blob_cid, byte_size, width, height, last_modified, etag, fetched_at, checked_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, timezone('UTC', now()), timezone('UTC', now()))
                 ON CONFLICT (cx, cy, variant) DO UPDATE
                 SET blob_cid = EXCLUDED.blob_cid, byte_size = EXCLUDED.byte_size, width = EXCLUDED.width,
                     height = EXCLUDED.height, last_modified = EXCLUDED.last_modified, etag = EXCLUDED.etag,
                     fetched_at = EXCLUDED.fetched_at, checked_at = EXCLUDED.checked_at",
            )
            .bind(cx)
            .bind(cy)
            .bind(variant.as_str())
            .bind(&cid)
            .bind(body.len() as i64)
            .bind(width as i32)
            .bind(height as i32)
            .bind(last_modified)
            .bind(etag)
            .execute(&state.pool)
            .await
            .context("recording the region")?;
            Ok(RegionOutcome::Fetched)
        }
    }
}

async fn touch_region(pool: &PgPool, cx: i32, cy: i32, variant: Variant) -> anyhow::Result<()> {
    sqlx::query("UPDATE explore.satellite_region SET checked_at = timezone('UTC', now()) WHERE cx = $1 AND cy = $2 AND variant = $3")
        .bind(cx)
        .bind(cy)
        .bind(variant.as_str())
        .execute(pool)
        .await
        .context("restamping the region")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::map::tiles::Tile;

    fn grid(tiles: &[(i32, i32, &str)], stamp: i64) -> Arc<Grid> {
        let tiles = tiles
            .iter()
            .map(|&(x, y, t)| {
                let tile: Tile = serde_json::from_value(serde_json::json!({
                    "id": format!("{x},{y}"), "x": x, "y": y, "type": t,
                    "top": false, "left": false, "topLeft": false, "updatedAt": stamp,
                }))
                .unwrap();
                (format!("{x},{y}"), tile)
            })
            .collect();
        Arc::new(Grid::from_tiles(
            tiles,
            stamp,
            None,
            chrono::Utc::now().naive_utc(),
        ))
    }

    fn pixel(png: &[u8], x: u32, y: u32) -> [u8; 4] {
        let pm = Pixmap::decode_png(png).unwrap();
        let i = ((y * pm.width() + x) * 4) as usize;
        pm.data()[i..i + 4].try_into().unwrap()
    }

    struct Tmp(std::path::PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn storage() -> (Storage, Tmp) {
        let dir = std::env::temp_dir().join(format!(
            "catalyst-satellite-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        (Storage::from_config(&dir).await.unwrap(), Tmp(dir))
    }

    #[tokio::test]
    async fn warm_hit_skips_overlap_scan() {
        let (store, _tmp) = storage().await;
        let st = SatelliteState::new(1 << 20, 64);
        st.install(
            (0..50)
                .map(|i| (10_000 + i, 10_000, Variant::Full, format!("bafy{i}")))
                .collect(),
        );

        st.overlap_scans
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let first = st.tile_png(&store, 9, 0, 0, None).await.unwrap().unwrap();
        assert_eq!(
            st.overlap_scans.load(std::sync::atomic::Ordering::Relaxed),
            50,
            "cold miss must scan every seeded region"
        );

        st.overlap_scans
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let second = st.tile_png(&store, 9, 0, 0, None).await.unwrap().unwrap();
        assert_eq!(
            st.overlap_scans.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "warm hit must not run the overlap scan"
        );
        assert_eq!(
            first.as_ptr(),
            second.as_ptr(),
            "the warm hit is the cached bytes"
        );
    }

    // z=9 tiles are one parcel each; tile (256, 255) is parcel (0, 0).
    #[tokio::test]
    async fn base_layer_paints_parcels_and_leaves_the_rest_clear() {
        let (store, _tmp) = storage().await;
        let st = SatelliteState::new(1 << 20, 64);
        let base = grid(&[(0, 0, "road")], 1);
        let road = st
            .tile_png(&store, 9, 256, 255, Some(base.clone()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pixel(&road, 128, 128), base_color(TileType::Road));
        let outside = st
            .tile_png(&store, 9, 0, 0, Some(base))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pixel(&outside, 128, 128)[3], 0, "no parcel -> transparent");

        // A newer grid must not be served from the tile cache.
        let base2 = grid(&[(0, 0, "plaza")], 2);
        let plaza = st
            .tile_png(&store, 9, 256, 255, Some(base2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pixel(&plaza, 128, 128), base_color(TileType::Plaza));
    }

    pub(crate) fn void_region_with_white_origin() -> Vec<u8> {
        // Region (0,0) covers parcels -8..=8; paint it all VOID except one white parcel.
        let px = 17 * 4;
        let mut pm = Pixmap::new(px, px).unwrap();
        for p in pm.data_mut().as_chunks_mut::<4>().0 {
            p.copy_from_slice(&VOID);
        }
        let white = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::WHITE),
            ..Default::default()
        };
        // Parcel (0,0) sits 8 parcels from the region's left and top edges.
        pm.fill_rect(
            tiny_skia::Rect::from_xywh(32.0, 32.0, 4.0, 4.0).unwrap(),
            &white,
            tiny_skia::Transform::identity(),
            None,
        );
        pm.encode_png().unwrap()
    }

    #[tokio::test]
    async fn void_render_pixels_show_the_base_layer() {
        let (store, _tmp) = storage().await;
        let png = void_region_with_white_origin();
        let cid = crate::auth::hash::hash_bytes_v1(&png);
        store.store(&cid, Bytes::from(png)).await.unwrap();

        let st = SatelliteState::new(1 << 20, 64);
        st.install(vec![(0, 0, Variant::Full, cid)]);
        let base = grid(&[(0, 0, "road"), (1, 0, "plaza")], 1);
        let rendered = st
            .tile_png(&store, 9, 256, 255, Some(base.clone()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pixel(&rendered, 128, 128), [255, 255, 255, 255]);
        let void = st
            .tile_png(&store, 9, 257, 255, Some(base))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pixel(&void, 128, 128), base_color(TileType::Plaza));
        assert_eq!(st.cached_sources(), 1, "both tiles decoded the region once");
    }

    #[tokio::test]
    async fn a_new_region_set_moves_the_generation_and_misses_the_output_cache() {
        let (store, _tmp) = storage().await;
        let png = void_region_with_white_origin();
        let cid = crate::auth::hash::hash_bytes_v1(&png);
        store.store(&cid, Bytes::from(png)).await.unwrap();
        let st = SatelliteState::new(1 << 20, 64);
        st.install(vec![]);
        let g0 = st.generation();
        let before = st
            .tile_png(&store, 9, 256, 255, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pixel(&before, 128, 128)[3], 0);
        st.install(vec![(0, 0, Variant::Full, cid.clone())]);
        assert_eq!(st.generation(), g0 + 1);
        let after = st
            .tile_png(&store, 9, 256, 255, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pixel(&after, 128, 128), [255, 255, 255, 255]);
        st.install(vec![(0, 0, Variant::Full, cid)]);
        assert_eq!(st.generation(), g0 + 1, "the same set keeps the generation");
    }

    #[tokio::test]
    async fn zooms_up_to_four_sample_the_thumbnail_and_a_missing_blob_is_skipped() {
        let (store, _tmp) = storage().await;
        let full = void_region_with_white_origin();
        let full_cid = crate::auth::hash::hash_bytes_v1(&full);
        store.store(&full_cid, Bytes::from(full)).await.unwrap();
        let st = SatelliteState::new(1 << 20, 64);
        st.install(vec![
            (0, 0, Variant::Full, full_cid),
            (
                0,
                0,
                Variant::Thumb,
                "bafkreiabsentthumbnailblobthatthestoredoesnothold".into(),
            ),
        ]);
        let low = st.tile_png(&store, 4, 8, 7, None).await.unwrap().unwrap();
        assert_eq!(
            pixel(&low, 0, 0)[3],
            0,
            "the absent thumbnail is skipped, not replaced by the full render"
        );
        assert_eq!(st.cached_sources(), 0);
        st.tile_png(&store, 9, 256, 255, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(st.cached_sources(), 1, "zoom 9 samples the full render");
    }

    #[tokio::test]
    async fn out_of_pyramid_tiles_are_none() {
        let (store, _tmp) = storage().await;
        let st = SatelliteState::new(1 << 20, 64);
        for (z, x, y) in [
            (-1, 0, 0),
            (10, 0, 0),
            (0, 1, 0),
            (0, 0, 1),
            (3, -1, 0),
            (3, 8, 0),
        ] {
            assert!(
                st.tile_png(&store, z, x, y, None).await.unwrap().is_none(),
                "{z}/{x}/{y}"
            );
        }
        assert!(st.tile_png(&store, 0, 0, 0, None).await.unwrap().is_some());
    }

    #[test]
    fn the_manifest_is_a_bounded_list_of_region_centres() {
        assert_eq!(
            parse_manifest(b"[[17,0],[-153,153],[17,0]]").unwrap(),
            vec![(-153, 153), (17, 0)]
        );
        assert_eq!(parse_manifest(b"[]").unwrap(), vec![]);
        for bad in [
            &b"{}"[..],
            b"[[1]]",
            b"[[1,2,3]]",
            b"[[\"a\",1]]",
            b"[[600,0]]",
            b"not json",
        ] {
            assert!(
                parse_manifest(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn only_a_manifest_404_can_mean_not_seeded() {
        assert!(manifest_not_found(&Ok(Manifest::Missing(
            StatusCode::NOT_FOUND
        ))));
        assert!(!manifest_not_found(&Ok(Manifest::Missing(
            StatusCode::FORBIDDEN
        ))));
        assert!(!manifest_not_found(&Ok(Manifest::Missing(
            StatusCode::GONE
        ))));
        assert!(!manifest_not_found(&Ok(Manifest::Regions(vec![(17, 0)]))));
        assert!(!manifest_not_found(&Ok(Manifest::Regions(Vec::new()))));
        assert!(!manifest_not_found(&Err(anyhow::anyhow!(
            "connect timeout"
        ))));
    }

    #[test]
    fn png_dimensions_read_the_ihdr_only() {
        let png = void_region_with_white_origin();
        assert_eq!(png_dimensions(&png), Some((68, 68)));
        assert_eq!(png_dimensions(b"GIF89a....................."), None);
        assert_eq!(png_dimensions(&png[..20]), None);
    }

    #[test]
    fn the_lru_keeps_the_newest_entry_and_evicts_the_least_recent() {
        let mut lru: Lru<u32, u32> = Lru::new(10);
        lru.insert(1, 1, 4);
        lru.insert(2, 2, 4);
        assert_eq!(lru.get(&1), Some(1));
        lru.insert(3, 3, 4);
        assert_eq!(
            (lru.get(&2), lru.get(&1), lru.get(&3)),
            (None, Some(1), Some(3))
        );
        lru.insert(4, 4, 100);
        assert_eq!(
            (lru.len(), lru.get(&4)),
            (1, Some(4)),
            "an entry over budget still stays alone"
        );
    }
}
