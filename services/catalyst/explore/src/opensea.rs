// from: catalyrst/nixos/opensea-resolver.mjs (the resolver), catalyrst/nixos/web-gateway.nix (the `opensea.<domain>` vhost: its proxy cache and the `@dcl_opensea` fallback), catalyrst/nixos/singles.nix (catalyrst-opensea-resolver: RPC_MAINNET/RPC_POLYGON)
//! The OpenSea metadata resolver behind `opensea.<domain>`: `GET
//! /api/v2/chain/{ethereum|matic|polygon}/contract/{0x..40}/nfts/{digits}`
//! answers OpenSea's v2 NFT envelope (`{nft: {identifier, collection,
//! contract, token_standard, name, description, image_url, ...}}`), plus
//! `GET /health`. Served under `/opensea` (the vhost's paths, as the other
//! per-subdomain services are); any other path is the old `{errors:
//! ["unknown route"]}` 404.
//!
//! Resolution, in the old order: this node's own data first (the node
//! script read the squid `nft` table; v2 has no squid, so a LAND or Estate
//! token answers from the mirrored tile grid, as `/api/v2/contracts/.../
//! tokens/...` does, and a collections-v2 item token from the `shop.items`
//! mirror), then CryptoKitties' API for its contract, then the chain:
//! `tokenURI(id)` (ERC-721) or `uri(id)` with `{id}` substituted (ERC-1155)
//! by `eth_call` against the network's JSON-RPC, and the metadata document
//! it names, read inline (`data:`), from four IPFS gateways in turn, from
//! Arweave or over http(s). Every outbound call goes through
//! `http::Outbound` (the SSRF guard, DNS pinned, redirects re-checked per
//! hop), with the script's timeouts (6 s RPC and IPFS, 10 s http) and its
//! 2,000,000-byte body cap.
//!
//! The nginx proxy cache in front of it is `explore.opensea_nft`, keyed by
//! the request (chain as asked, lowercased contract, token id digits): an
//! answer is fresh for 30 days (`proxy_cache_valid 200 30d`), a stale row is
//! re-resolved on the next request and served when that fails
//! (`proxy_cache_use_stale`), and a row unread past twice that window is
//! deleted by the hourly expiry job (`inactive=30d` bounded the old cache).
//! When the resolver has no answer (or fails) the request goes to
//! `OPENSEA_FALLBACK_URL` (`opensea.decentraland.org`, the old
//! `@dcl_opensea` location): its 200 is cached like a resolved one, its 404
//! remembered for five minutes. Deviations: redirects are the ONE client's
//! two guarded hops (the script followed five), the query string is not
//! forwarded to the fallback, and the item and LAND legs keep their upstream
//! image/metadata URLs where the script rewrote `peer.decentraland.org` to
//! the node's domain (v2's lambdas do not serve `/lambdas/collections/*`).
//! The remote legs (CryptoKitties, the chain and the metadata it names) run
//! only over a configured RPC upstream: with none (an island) the resolver
//! answers from this node's data alone.

use std::time::Duration;

use alloy_primitives::U256;
use anyhow::Context as _;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use bytes::Bytes;
use chrono::NaiveDateTime;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::api::Shared;
use crate::api::map::meta;
use crate::shared::cache::TtlMap;

pub const DEFAULT_FALLBACK: &str = "https://opensea.decentraland.org";
pub const DEFAULT_RPC_ETHEREUM: &str = "https://rpc.decentraland.org/mainnet";
pub const DEFAULT_RPC_POLYGON: &str = "https://rpc.decentraland.org/polygon";
pub const CRYPTOKITTIES: &str = "0x06012c8cf97bead5deae237070f9587f8e7a266d";
pub const CRYPTOKITTIES_API: &str = "https://api.cryptokitties.co";
pub const IPFS_GATEWAYS: [&str; 4] = [
    "https://ipfs.io/ipfs/",
    "https://dweb.link/ipfs/",
    "https://w3s.link/ipfs/",
    "https://nftstorage.link/ipfs/",
];

/// `proxy_cache_valid 200 30d`.
pub const FRESH_FOR: Duration = Duration::from_secs(30 * 86_400);
/// A row older than this is deleted by the expiry job.
pub const KEEP_FOR: Duration = Duration::from_secs(60 * 86_400);
/// `proxy_cache_valid 404 5m` on the fallback.
const FALLBACK_MISS_TTL: Duration = Duration::from_secs(300);
const FALLBACK_MISS_ENTRIES: usize = 10_000;
const BODY_CAP: usize = 2_000_000;
const RPC_TIMEOUT: Duration = Duration::from_secs(6);
const IPFS_TIMEOUT: Duration = Duration::from_secs(6);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(60);
const EXPIRY_BATCH: i64 = 1000;
const EXPIRY_MAX_BATCHES: usize = 50;
/// Collections v2 token ids carry the item id above the low 216 bits.
const ISSUED_ID_BITS: usize = 216;

#[derive(Clone, Debug)]
pub struct Settings {
    /// `OPENSEA_FALLBACK_URL`; `None` answers the resolver's own 404.
    pub fallback: Option<String>,
    /// JSON-RPC per network: the relay's `RPC_UPSTREAM_MAINNET` /
    /// `RPC_UPSTREAM_POLYGON` when set and public; empty skips the chain leg.
    pub rpc_ethereum: String,
    pub rpc_polygon: String,
    /// Empty skips the CryptoKitties leg (no Ethereum RPC upstream).
    pub cryptokitties_api: String,
    pub ipfs_gateways: Vec<String>,
}

impl Settings {
    pub fn defaults() -> Self {
        Self {
            fallback: Some(DEFAULT_FALLBACK.to_string()),
            rpc_ethereum: DEFAULT_RPC_ETHEREUM.to_string(),
            rpc_polygon: DEFAULT_RPC_POLYGON.to_string(),
            cryptokitties_api: CRYPTOKITTIES_API.to_string(),
            ipfs_gateways: IPFS_GATEWAYS.iter().map(|g| g.to_string()).collect(),
        }
    }

    /// A configured relay upstream when it passes the SSRF guard, else none
    /// (the chain leg is skipped); the refusal names the variable, never the
    /// URL (a gateway key may sit in its path). The mirror posture sets the
    /// relay upstreams; unset or empty reads no chain.
    pub fn pick_rpc(configured: Option<&str>, var: &str) -> String {
        match configured.map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) if crate::http::is_safe_http_url(u) => u.to_string(),
            Some(_) => {
                tracing::warn!(
                    variable = var,
                    "the configured RPC upstream is not a public endpoint; the OpenSea resolver reads no chain"
                );
                String::new()
            }
            None => String::new(),
        }
    }
}

/// What the resolver keeps between requests: the fallback's remembered 404s
/// and one resolution per key at a time.
pub struct Runtime {
    misses: TtlMap<String, Bytes>,
    locks: crate::keyed_locks::KeyedLocks<String>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            misses: TtlMap::bounded(FALLBACK_MISS_TTL, FALLBACK_MISS_ENTRIES),
            locks: crate::keyed_locks::KeyedLocks::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NftRequest {
    /// As asked: `ethereum`, `matic` or `polygon` (the answer's `opensea_url` carries it).
    pub chain: String,
    /// Lowercased.
    pub contract: String,
    /// The digits as asked.
    pub id: String,
}

impl NftRequest {
    /// The old route regex: `(ethereum|matic|polygon)`, `0x[0-9a-fA-F]{40}`,
    /// `\d+` (at most 78 digits: a uint256).
    pub fn parse(chain: &str, contract: &str, id: &str) -> Option<Self> {
        if !matches!(chain, "ethereum" | "matic" | "polygon") {
            return None;
        }
        let hex = contract.strip_prefix("0x")?;
        if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        if id.is_empty() || id.len() > 78 || !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            chain: chain.to_string(),
            contract: contract.to_ascii_lowercase(),
            id: id.to_string(),
        })
    }

    fn key(&self) -> String {
        format!("{}/{}/{}", self.chain, self.contract, self.id)
    }

    fn is_polygon(&self) -> bool {
        self.chain != "ethereum"
    }
}

#[derive(Serialize)]
struct Envelope {
    nft: Nft,
}

/// Field order as the node script wrote it.
#[derive(Serialize)]
struct Nft {
    identifier: String,
    collection: String,
    contract: String,
    token_standard: &'static str,
    name: Value,
    description: Value,
    image_url: Value,
    display_image_url: Value,
    animation_url: Value,
    display_animation_url: Value,
    metadata_url: Value,
    opensea_url: String,
    updated_at: &'static str,
    is_disabled: bool,
    is_nsfw: bool,
    is_suspicious: bool,
    creator: Value,
    traits: Vec<Value>,
    owners: Vec<Value>,
    rarity: Value,
}

#[derive(Default)]
struct Found {
    name: Value,
    description: Value,
    image: Value,
    animation: Value,
    metadata_url: Value,
    standard: &'static str,
}

fn render(req: &NftRequest, f: Found) -> String {
    let body = Envelope {
        nft: Nft {
            identifier: req.id.clone(),
            collection: req.contract.clone(),
            contract: req.contract.clone(),
            token_standard: if f.standard.is_empty() {
                "erc721"
            } else {
                f.standard
            },
            name: f.name,
            description: f.description,
            image_url: f.image.clone(),
            display_image_url: f.image,
            animation_url: f.animation.clone(),
            display_animation_url: f.animation,
            metadata_url: f.metadata_url,
            opensea_url: format!(
                "https://opensea.io/assets/{}/{}/{}",
                req.chain, req.contract, req.id
            ),
            updated_at: "",
            is_disabled: false,
            is_nsfw: false,
            is_suspicious: false,
            creator: Value::Null,
            traits: vec![],
            owners: vec![],
            rarity: Value::Null,
        },
    };
    serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string())
}

fn string_or_null(s: Option<String>) -> Value {
    s.map(Value::String).unwrap_or(Value::Null)
}

/// `x ?? null`: a missing or null field is null, any other value passes as is.
fn or_null(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

fn ipfs_path(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("ipfs://")?;
    let rest = rest.strip_prefix("ipfs/").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

/// The script's `toHttp`: ipfs:// through a gateway, ar:// through
/// arweave.net, http(s) and data: as they are, anything else null.
pub fn to_http(uri: Option<&Value>, gateway: &str) -> Value {
    let Some(uri) = uri.and_then(Value::as_str).filter(|u| !u.is_empty()) else {
        return Value::Null;
    };
    if let Some(p) = ipfs_path(uri) {
        if gateway.is_empty() {
            return Value::Null;
        }
        return Value::String(format!("{gateway}{p}"));
    }
    if let Some(p) = uri.strip_prefix("ar://") {
        return Value::String(format!("https://arweave.net/{p}"));
    }
    if uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("data:") {
        return Value::String(uri.to_string());
    }
    Value::Null
}

/// A `data:` URL's payload: base64 when the header says so, percent-decoded otherwise.
pub fn decode_data_url(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    if header.split(';').any(|p| p.eq_ignore_ascii_case("base64")) {
        let cleaned: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(cleaned.as_bytes())
            .ok()
    } else {
        Some(urlencoding::decode_binary(payload.as_bytes()).into_owned())
    }
}

/// The script's `decodeAbiString`: an ABI-encoded `string` return value,
/// `None` for anything shorter than one head word or an empty string.
pub fn decode_abi_string(hex_result: Option<&str>) -> Option<String> {
    let raw = hex::decode(hex_result?.strip_prefix("0x")?).ok()?;
    if raw.len() < 64 {
        return None;
    }
    let word = |at: usize| -> Option<usize> {
        let end = at.checked_add(32)?;
        let w = raw.get(at..end)?;
        let v = U256::from_be_slice(w);
        (v.bit_len() <= 48).then(|| v.to::<u64>() as usize)
    };
    let off = word(0)?;
    let len = word(off).unwrap_or(0);
    let start = off.checked_add(32)?.min(raw.len());
    let end = start.saturating_add(len).min(raw.len());
    let s = String::from_utf8_lossy(&raw[start..end]).into_owned();
    (!s.is_empty()).then_some(s)
}

/// `BigInt(id).toString(16).padStart(64, '0')`; `None` past a uint256.
pub fn id_hex(id: &str) -> Option<String> {
    let v = U256::from_str_radix(id, 10).ok()?;
    Some(format!("{:0>64}", format!("{v:x}")))
}

/// The script's `fetchText`: the body of a 2xx under the cap, else `None`.
async fn fetch_text(state: &Shared, url: &str, timeout: Duration, accept: &str) -> Option<Vec<u8>> {
    if url.starts_with("data:") {
        return decode_data_url(url);
    }
    let target = state.settings.guarded_url(url).ok()?;
    let resp = state
        .http
        .get(target)
        .timeout(timeout)
        .header(header::ACCEPT, accept)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    if resp.content_length().is_some_and(|n| n > BODY_CAP as u64) {
        return None;
    }
    crate::http::read_body_capped(resp, BODY_CAP).await.ok()
}

/// The first `OPENSEA_IPFS_GATEWAYS` entry; none (empty) leaves `ipfs://`
/// unresolved.
fn first_gateway(state: &Shared) -> String {
    state
        .settings
        .opensea
        .ipfs_gateways
        .first()
        .cloned()
        .unwrap_or_default()
}

/// The script's `fetchMetadata`: the document and the IPFS gateway that
/// served it (images from the same document go through that gateway).
async fn fetch_metadata(state: &Shared, uri: &str) -> Option<(Value, Option<String>)> {
    if let Some(rest) = uri.strip_prefix("data:application/json") {
        let (header, payload) = rest.split_once(',')?;
        let base64 = header.split(';').any(|p| p == "base64");
        let charset_only = header
            .split(';')
            .filter(|p| !p.is_empty())
            .all(|p| p == "base64" || p.starts_with("charset="));
        if charset_only {
            let body = if base64 {
                base64::engine::general_purpose::STANDARD
                    .decode(payload.as_bytes())
                    .ok()?
            } else {
                urlencoding::decode_binary(payload.as_bytes()).into_owned()
            };
            return serde_json::from_slice(&body).ok().map(|meta| (meta, None));
        }
    }
    let attempts: Vec<(String, Option<String>)> = match ipfs_path(uri) {
        Some(p) => state
            .settings
            .opensea
            .ipfs_gateways
            .iter()
            .map(|g| (format!("{g}{p}"), Some(g.clone())))
            .collect(),
        None => match to_http(
            Some(&Value::String(uri.to_string())),
            first_gateway(state).as_str(),
        ) {
            Value::String(u) => vec![(u, None)],
            _ => vec![],
        },
    };
    let timeout = if ipfs_path(uri).is_some() {
        IPFS_TIMEOUT
    } else {
        HTTP_TIMEOUT
    };
    for (url, gateway) in attempts {
        if let Some(text) = fetch_text(state, &url, timeout, "application/json").await
            && let Ok(meta) = serde_json::from_slice::<Value>(&text)
        {
            return Some((meta, gateway));
        }
    }
    None
}

async fn eth_call(state: &Shared, rpc: &str, to: &str, data: &str) -> Option<String> {
    let target = state.settings.guarded_url(rpc).ok()?;
    let rpc = crate::http::jsonrpc::Rpc::new(&state.http, target.as_str())
        .with_timeout(RPC_TIMEOUT)
        .with_body_cap(BODY_CAP);
    rpc.call_hex("eth_call", json!([{"to": to, "data": data}, "latest"]))
        .await
        .ok()
        .filter(|r| r.len() > 2)
}

/// This node's own copy of the token: the tile grid for LAND and Estates,
/// the items mirror for a collections-v2 item.
async fn local(state: &Shared, req: &NftRequest) -> anyhow::Result<Option<Found>> {
    let s = &state.settings;
    if !req.is_polygon()
        && (req.contract == s.land_contract_address || req.contract == s.estate_contract_address)
    {
        let Some(grid) = state.map.snapshot() else {
            return Ok(None);
        };
        let nft = if req.contract == s.land_contract_address {
            meta::decode_land_token_id(&req.id)
                .and_then(|(x, y)| meta::parcel_nft(state, &grid, x, y))
        } else {
            meta::estate_nft(state, &grid, &req.id)
        };
        return Ok(nft.map(|n| Found {
            name: Value::String(n.name),
            image: Value::String(n.image),
            metadata_url: Value::String(format!(
                "{}/api/v2/contracts/{}/tokens/{}",
                s.public_url, req.contract, req.id
            )),
            standard: "erc721",
            ..Found::default()
        }));
    }
    if req.is_polygon() {
        let Ok(token) = U256::from_str_radix(&req.id, 10) else {
            return Ok(None);
        };
        let item_id = token.wrapping_shr(ISSUED_ID_BITS);
        let issued_id = token & ((U256::from(1u8) << ISSUED_ID_BITS) - U256::from(1u8));
        let row: Option<(String, Option<String>, i32)> = sqlx::query_as(
            "SELECT name, image, chain_id FROM shop.items
             WHERE network = 'matic' AND contract_address = $1 AND item_id = $2::numeric",
        )
        .bind(&req.contract)
        .bind(item_id.to_string())
        .fetch_optional(&state.pool)
        .await
        .context("reading shop.items")?;
        return Ok(row.map(|(name, image, chain_id)| Found {
            name: Value::String(name),
            image: string_or_null(image),
            metadata_url: Value::String(format!(
                "https://peer.decentraland.org/lambdas/collections/standard/erc721/{chain_id}/{}/{item_id}/{issued_id}",
                req.contract
            )),
            standard: "erc721",
            ..Found::default()
        }));
    }
    Ok(None)
}

async fn cryptokitty(state: &Shared, req: &NftRequest) -> Option<Found> {
    let api = &state.settings.opensea.cryptokitties_api;
    if req.chain != "ethereum" || req.contract != CRYPTOKITTIES || api.is_empty() {
        return None;
    }
    let url = format!("{}/v3/kitties/{}", api.trim_end_matches('/'), req.id);
    let text = fetch_text(state, &url, HTTP_TIMEOUT, "application/json").await?;
    let k: Value = serde_json::from_slice(&text).ok()?;
    let image = match k.get("image_url_png") {
        Some(v) if !v.is_null() => v.clone(),
        _ => or_null(k.get("image_url")),
    };
    Some(Found {
        name: or_null(k.get("name")),
        description: or_null(k.get("bio")),
        image,
        standard: "erc721",
        ..Found::default()
    })
}

async fn on_chain(state: &Shared, req: &NftRequest) -> Option<Found> {
    let id_hex = id_hex(&req.id)?;
    let rpc = if req.is_polygon() {
        &state.settings.opensea.rpc_polygon
    } else {
        &state.settings.opensea.rpc_ethereum
    };
    if rpc.is_empty() {
        return None;
    }
    let mut standard = "erc721";
    let mut uri = decode_abi_string(
        eth_call(state, rpc, &req.contract, &format!("0xc87b56dd{id_hex}"))
            .await
            .as_deref(),
    );
    if uri.is_none() {
        uri = decode_abi_string(
            eth_call(state, rpc, &req.contract, &format!("0x0e89341c{id_hex}"))
                .await
                .as_deref(),
        )
        .map(|u| u.replace("{id}", &id_hex));
        if uri.is_some() {
            standard = "erc1155";
        }
    }
    let uri = uri?;
    let (meta, gateway) = fetch_metadata(state, &uri).await?;
    let gw = gateway.unwrap_or_else(|| first_gateway(state));
    let image = match meta.get("image") {
        Some(v) if !v.is_null() => Some(v),
        _ => meta.get("image_url"),
    };
    Some(Found {
        name: or_null(meta.get("name")),
        description: or_null(meta.get("description")),
        image: to_http(image, &gw),
        animation: to_http(meta.get("animation_url"), &gw),
        metadata_url: to_http(Some(&Value::String(uri.clone())), &gw),
        standard,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Local,
    CryptoKitties,
    Chain,
    Fallback,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Local => "local",
            Source::CryptoKitties => "cryptokitties",
            Source::Chain => "chain",
            Source::Fallback => "fallback",
        }
    }
}

/// The resolver alone (no cache, no fallback): the rendered envelope and
/// the leg that produced it, `Ok(None)` for "NFT not found".
pub async fn resolve(state: &Shared, req: &NftRequest) -> anyhow::Result<Option<(String, Source)>> {
    if let Some(found) = local(state, req).await? {
        return Ok(Some((render(req, found), Source::Local)));
    }
    if let Some(found) = cryptokitty(state, req).await {
        return Ok(Some((render(req, found), Source::CryptoKitties)));
    }
    Ok(on_chain(state, req)
        .await
        .map(|found| (render(req, found), Source::Chain)))
}

struct Cached {
    document: String,
    resolved_at: NaiveDateTime,
}

async fn read_cache(pool: &PgPool, req: &NftRequest) -> sqlx::Result<Option<Cached>> {
    let row: Option<(String, NaiveDateTime)> = sqlx::query_as(
        "SELECT document::text, resolved_at FROM explore.opensea_nft WHERE chain = $1 AND contract_address = $2 AND token_id = $3",
    )
    .bind(&req.chain)
    .bind(&req.contract)
    .bind(&req.id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(document, resolved_at)| Cached {
        document,
        resolved_at,
    }))
}

async fn write_cache(
    pool: &PgPool,
    req: &NftRequest,
    document: &str,
    source: Source,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO explore.opensea_nft (chain, contract_address, token_id, document, source, resolved_at)
         VALUES ($1, $2, $3, $4::json, $5, timezone('UTC', now()))
         ON CONFLICT (chain, contract_address, token_id) DO UPDATE
         SET document = EXCLUDED.document, source = EXCLUDED.source, resolved_at = EXCLUDED.resolved_at",
    )
    .bind(&req.chain)
    .bind(&req.contract)
    .bind(&req.id)
    .bind(document)
    .bind(source.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// The expiry job: rows resolved before `now - KEEP_FOR`, in bounded batches.
pub async fn expire(pool: &PgPool, now: NaiveDateTime) -> anyhow::Result<u64> {
    let cutoff = now - chrono::Duration::from_std(KEEP_FOR).unwrap_or(chrono::Duration::MAX);
    let mut total = 0;
    for _ in 0..EXPIRY_MAX_BATCHES {
        let n = sqlx::query(
            "DELETE FROM explore.opensea_nft WHERE ctid IN
                 (SELECT ctid FROM explore.opensea_nft WHERE resolved_at < $1 LIMIT $2)",
        )
        .bind(cutoff)
        .bind(EXPIRY_BATCH)
        .execute(pool)
        .await
        .context("expiring explore.opensea_nft")?
        .rows_affected();
        total += n;
        if n < EXPIRY_BATCH as u64 {
            break;
        }
    }
    Ok(total)
}

enum FallbackOutcome {
    Disabled,
    Found(String),
    NotFound(Bytes),
    Failed(StatusCode),
}

async fn fallback(state: &Shared, req: &NftRequest) -> FallbackOutcome {
    let Some(base) = state.settings.opensea.fallback.as_deref() else {
        return FallbackOutcome::Disabled;
    };
    let url = format!(
        "{}/api/v2/chain/{}/contract/{}/nfts/{}",
        base.trim_end_matches('/'),
        req.chain,
        req.contract,
        req.id
    );
    let Ok(target) = state.settings.guarded_url(&url) else {
        return FallbackOutcome::Failed(StatusCode::BAD_GATEWAY);
    };
    let resp = match state
        .http
        .get(target)
        .timeout(FALLBACK_TIMEOUT)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "the OpenSea fallback is unreachable");
            return FallbackOutcome::Failed(if e.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            });
        }
    };
    let status = resp.status();
    let Ok(body) = crate::http::read_body_capped(resp, BODY_CAP).await else {
        return FallbackOutcome::Failed(StatusCode::BAD_GATEWAY);
    };
    if status == reqwest::StatusCode::NOT_FOUND {
        return FallbackOutcome::NotFound(Bytes::from(body));
    }
    if !status.is_success() {
        return FallbackOutcome::Failed(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        );
    }
    match String::from_utf8(body) {
        Ok(text) if serde_json::from_str::<Value>(&text).is_ok() => FallbackOutcome::Found(text),
        _ => FallbackOutcome::Failed(StatusCode::BAD_GATEWAY),
    }
}

fn json_response(status: StatusCode, body: impl Into<axum::body::Body>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.into(),
    )
        .into_response()
}

fn errors(status: StatusCode, message: &str) -> Response {
    json_response(status, json!({ "errors": [message] }).to_string())
}

/// The cache, one resolution per key at a time, the fallback, and the
/// stale copy when both fail. `now` is the request clock.
pub async fn answer(state: &Shared, req: &NftRequest, now: NaiveDateTime) -> Response {
    let fresh_after = now - chrono::Duration::from_std(FRESH_FOR).unwrap_or(chrono::Duration::MAX);
    let cached = match read_cache(&state.pool, req).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "reading explore.opensea_nft");
            None
        }
    };
    if let Some(c) = &cached
        && c.resolved_at >= fresh_after
    {
        return json_response(StatusCode::OK, c.document.clone());
    }
    let key = req.key();
    if let Some(body) = state.opensea.misses.get_fresh(&key) {
        return json_response(StatusCode::NOT_FOUND, body);
    }
    let lock = state.opensea.locks.claim(&key);
    let response = async {
        let _guard = lock.lock().await;
        if let Ok(Some(c)) = read_cache(&state.pool, req).await
            && c.resolved_at >= fresh_after
        {
            return json_response(StatusCode::OK, c.document);
        }
        let resolved = resolve(state, req).await;
        if let Ok(Some((doc, source))) = &resolved {
            if let Err(e) = write_cache(&state.pool, req, doc, *source).await {
                tracing::warn!(error = %e, "writing explore.opensea_nft");
            }
            return json_response(StatusCode::OK, doc.clone());
        }
        if let Err(e) = &resolved {
            tracing::warn!(error = %format!("{e:#}"), key = %key, "the OpenSea resolver failed");
            if let Some(c) = &cached {
                return json_response(StatusCode::OK, c.document.clone());
            }
        }
        match fallback(state, req).await {
            FallbackOutcome::Found(doc) => {
                if let Err(e) = write_cache(&state.pool, req, &doc, Source::Fallback).await {
                    tracing::warn!(error = %e, "writing explore.opensea_nft");
                }
                json_response(StatusCode::OK, doc)
            }
            FallbackOutcome::NotFound(body) => {
                let body = if serde_json::from_slice::<Value>(&body).is_ok() {
                    body
                } else {
                    Bytes::from(json!({ "errors": ["NFT not found"] }).to_string())
                };
                state.opensea.misses.insert(key.clone(), body.clone());
                json_response(StatusCode::NOT_FOUND, body)
            }
            FallbackOutcome::Failed(status) => match &cached {
                Some(c) => json_response(StatusCode::OK, c.document.clone()),
                None => errors(status, "the OpenSea fallback failed"),
            },
            FallbackOutcome::Disabled => match (&resolved, &cached) {
                (_, Some(c)) => json_response(StatusCode::OK, c.document.clone()),
                (Err(_), None) => errors(StatusCode::INTERNAL_SERVER_ERROR, "resolver error"),
                (Ok(_), None) => errors(StatusCode::NOT_FOUND, "NFT not found"),
            },
        }
    }
    .await;
    response
}

async fn get_nft(
    State(state): State<Shared>,
    Path((chain, contract, id)): Path<(String, String, String)>,
) -> Response {
    match NftRequest::parse(&chain, &contract, &id) {
        Some(req) => answer(&state, &req, chrono::Utc::now().naive_utc()).await,
        None => errors(StatusCode::NOT_FOUND, "unknown route"),
    }
}

async fn health() -> Response {
    json_response(StatusCode::OK, json!({ "ok": true }).to_string())
}

async fn unknown_route() -> Response {
    errors(StatusCode::NOT_FOUND, "unknown route")
}

/// The vhost's routes; mounted under `/opensea`.
pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/api/v2/chain/{chain}/contract/{contract}/nfts/{id}",
            get(get_nft),
        )
        .route("/health", get(health))
        .fallback(unknown_route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_accepts_what_the_old_regex_did() {
        let ok = NftRequest::parse("matic", "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01", "007")
            .unwrap();
        assert_eq!(
            (ok.chain.as_str(), ok.contract.as_str(), ok.id.as_str()),
            ("matic", "0xabcdef0123456789abcdef0123456789abcdef01", "007")
        );
        assert!(
            NftRequest::parse("polygon", "0x06012c8cf97bead5deae237070f9587f8e7a266d", "1")
                .is_some()
        );
        for (c, a, i) in [
            ("bsc", "0x06012c8cf97bead5deae237070f9587f8e7a266d", "1"),
            ("ethereum", "06012c8cf97bead5deae237070f9587f8e7a266d", "1"),
            ("ethereum", "0x06012c8cf97bead5deae237070f9587f8e7a266", "1"),
            (
                "ethereum",
                "0x06012c8cf97bead5deae237070f9587f8e7a266g",
                "1",
            ),
            (
                "ethereum",
                "0x06012c8cf97bead5deae237070f9587f8e7a266d",
                "1a",
            ),
            ("ethereum", "0x06012c8cf97bead5deae237070f9587f8e7a266d", ""),
            (
                "ethereum",
                "0x06012c8cf97bead5deae237070f9587f8e7a266d",
                &"9".repeat(79),
            ),
        ] {
            assert!(NftRequest::parse(c, a, i).is_none(), "{c} {a} {i}");
        }
    }

    #[test]
    fn uris_become_http_the_way_the_script_did() {
        let g = "https://gw.example/ipfs/";
        let s = |v: &str| Value::String(v.to_string());
        assert_eq!(
            to_http(Some(&s("ipfs://Qmabc/1.json")), g),
            s("https://gw.example/ipfs/Qmabc/1.json")
        );
        assert_eq!(
            to_http(Some(&s("ipfs://ipfs/Qmabc")), g),
            s("https://gw.example/ipfs/Qmabc")
        );
        assert_eq!(
            to_http(Some(&s("ar://tx123")), g),
            s("https://arweave.net/tx123")
        );
        assert_eq!(
            to_http(Some(&s("https://x.example/a.png")), g),
            s("https://x.example/a.png")
        );
        assert_eq!(
            to_http(Some(&s("data:image/png;base64,AAA")), g),
            s("data:image/png;base64,AAA")
        );
        assert_eq!(to_http(Some(&s("ftp://x")), g), Value::Null);
        assert_eq!(to_http(Some(&s("")), g), Value::Null);
        assert_eq!(to_http(Some(&json!(5)), g), Value::Null);
        assert_eq!(to_http(None, g), Value::Null);
        // No gateway configured (island): an ipfs:// URI stays unresolved.
        assert_eq!(to_http(Some(&s("ipfs://bafy/1.json")), ""), Value::Null);
    }

    #[test]
    fn abi_strings_decode_like_decode_abi_string() {
        let word = |s: &str| format!("{:0<64}", hex::encode(s));
        let encoded = format!("0x{:064x}{:064x}{}", 32, 5, word("hello"));
        assert_eq!(decode_abi_string(Some(&encoded)).as_deref(), Some("hello"));
        let empty = format!("0x{:064x}{:064x}", 32, 0);
        assert_eq!(decode_abi_string(Some(&empty)), None);
        assert_eq!(decode_abi_string(Some("0x1234")), None);
        assert_eq!(decode_abi_string(Some("0xzz")), None);
        assert_eq!(decode_abi_string(None), None);
        let overlong = format!("0x{:064x}{:064x}{}", 32, 1000, word("abc"));
        assert!(
            decode_abi_string(Some(&overlong))
                .unwrap()
                .starts_with("abc"),
            "a length past the data is clamped"
        );
    }

    #[test]
    fn ids_pad_to_one_word_and_data_urls_decode() {
        assert_eq!(id_hex("255").unwrap(), format!("{:0>64}", "ff"));
        assert_eq!(id_hex(&"9".repeat(78)), None, "past a uint256");
        assert_eq!(
            decode_data_url("data:application/json;base64,eyJhIjoxfQ==").unwrap(),
            br#"{"a":1}"#
        );
        assert_eq!(
            decode_data_url("data:application/json;utf8,%7B%22a%22%3A1%7D").unwrap(),
            br#"{"a":1}"#
        );
        assert_eq!(decode_data_url("data:text/plain"), None);
    }

    #[test]
    fn the_envelope_keeps_the_script_field_order_and_nulls() {
        let req = NftRequest::parse(
            "ethereum",
            "0x06012C8cf97bead5deae237070f9587f8e7a266d",
            "42",
        )
        .unwrap();
        let doc = render(
            &req,
            Found {
                name: json!("Kitty"),
                image: json!("https://img"),
                standard: "erc721",
                ..Found::default()
            },
        );
        let v: Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(v["nft"]["identifier"], "42");
        assert_eq!(
            v["nft"]["contract"],
            "0x06012c8cf97bead5deae237070f9587f8e7a266d"
        );
        assert_eq!(v["nft"]["display_image_url"], "https://img");
        assert_eq!(v["nft"]["description"], Value::Null);
        assert_eq!(
            v["nft"]["opensea_url"],
            "https://opensea.io/assets/ethereum/0x06012c8cf97bead5deae237070f9587f8e7a266d/42"
        );
        assert_eq!(v["nft"]["traits"], json!([]));
        let keys: Vec<&str> = doc
            .split("\":")
            .filter_map(|p| p.rsplit('"').next())
            .collect();
        let order = [
            "identifier",
            "collection",
            "contract",
            "token_standard",
            "name",
            "description",
            "image_url",
            "display_image_url",
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|k| keys.iter().position(|x| x == k).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{doc}");
    }

    /// No upstream, a blank one (an island's) or a private one reads no
    /// chain: the old fallback to decentraland.org's RPC reached out from a
    /// node configured to reach nothing.
    #[test]
    fn an_unset_blank_or_private_rpc_upstream_reads_no_chain() {
        assert_eq!(Settings::pick_rpc(None, "RPC_UPSTREAM_MAINNET"), "");
        assert_eq!(Settings::pick_rpc(Some("  "), "RPC_UPSTREAM_MAINNET"), "");
        assert_eq!(
            Settings::pick_rpc(Some("http://10.0.0.5:8545"), "RPC_UPSTREAM_MAINNET"),
            ""
        );
        assert_eq!(
            Settings::pick_rpc(Some("https://eth.example.org/KEY"), "RPC_UPSTREAM_MAINNET"),
            "https://eth.example.org/KEY"
        );
    }
}
