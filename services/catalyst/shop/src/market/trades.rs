//! The off-chain trades mirror and its read routes. The old poller fetched
//! the whole upstream list every 15 min and inserted what it did not have;
//! v2 walks it page by page (`first`/`skip`; the list is oldest first, so
//! new trades append at the tail and `skip` stays stable), persists the page
//! cursor and the counters in `shop.trades_mirror` after every page,
//! and marks a pass complete when a short page comes back. A restart resumes
//! at the persisted `next_skip`. The production upstream ignores paging and
//! answers the whole list (58 MB, 74,531 rows on 2026-09-25; 14 MB on the
//! wire, gzip: asked for with `Accept-Encoding: gzip` and inflated here by
//! [`inflate_if_gzip`], since the ONE reqwest has no automatic decompression:
//! the content sync path hashes the decoded bytes itself and a crate-wide
//! `gzip` feature would silently undo its transfer-decoding checks) to every
//! request: that answer is downloaded **once per pass**,
//! parsed into its heads and held in this process ([`PassList`], about 1 KB
//! a row, so the first pass holds the whole catalogue for its duration and a
//! caught-up pass drops it within the tick), and paged over locally, so the
//! cursor, the per-page persistence and the short-page completion keep
//! their meaning. What bounds a tick is the detail fetches (one per new
//! trade): `TRADES_SYNC_PAGES_PER_RUN` x [`PAGE_SIZE`] of them (0 =
//! unbounded); when the budget runs out the cursor stays on the page and the
//! next tick continues there over the held list. A restart mid-pass
//! downloads the list again (once); a caught-up mirror pays one list
//! download per interval, the old poller's cost, since the upstream sends no
//! validators and no since-cursor. Every byte read is counted in the marker
//! (`bytes_downloaded` counts decoded bytes, `list_downloads`) and shown by
//! `/v1/trades-mirror`. Rows carry `mirrored_from` and are inserted with
//! `ON CONFLICT DO NOTHING`, so a trade signed into this node under the same
//! signature is never overwritten. `mv_trades` (status, owner checks) needs
//! the squid and is not copied: `GET /v1/trades/{id}` carries no `status`.
//!
//! Readiness ([`require_ready`]): a route that reads `shop.trades`
//! (`/v1/trades`, `/v1/trades/{id}`, the v3 legs of `/v1/orders` and
//! `/v1/bids`, the v2 catalog's prices and `isOnSale`, `/v1/items`, the
//! `/v3/catalog` feeds) answers the `trades-mirror` envelope until the
//! configured upstream has one complete pass (`trades_mirror.completed_at`,
//! cached 10 s): a table 0.3% through its first pass is not the order book.
//! Trades are money data, so a pass also has a freshness budget
//! ([`max_age`]): past it (a dead or failing mirror) the same routes answer
//! the envelope naming the pass's age instead of a stale order book.
//! A node with no `TRADES_SYNC_UPSTREAM_URL` holds only the trades signed
//! into it, which are its own truth when it accepts them
//! (`DATA_WRITES_ENABLED`); with neither, there is no source. Rows an
//! earlier mirror posture left in the table count there only within
//! [`max_age`] of their copy ([`mirrored_since`]): a leftover is not a live
//! order book.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use futures::StreamExt;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::Row;
use sqlx::types::JsonValue;

use crate::http::{
    RetryDecision, is_transient_status, parse_retry_after, read_body_capped, retry_with_backoff,
};
use crate::jobs::JobContext;

use super::{MarketplaceState, Settings, SharedState, Source, lambdas_error, unavailable};

pub const DEFAULT_TRADES_SYNC_INTERVAL_SECS: u64 = 900;
/// Upstream rows per page; the marketplace API caps `first` at 1000.
pub const PAGE_SIZE: i64 = 100;
/// Default `TRADES_SYNC_PAGES_PER_RUN`: pages of new trades (detail fetches,
/// `PAGE_SIZE` each) one periodic tick pays for before yielding; the cursor
/// persists, so the next tick continues the same pass. Also the upstream
/// page requests per tick when the upstream honours paging.
pub const MAX_PAGES_PER_RUN: usize = 50;
const DETAIL_CONCURRENCY: usize = 4;
const BODY_CAP: usize = 4 * 1024 * 1024;
/// The list answer of an upstream that ignores paging is the whole catalogue
/// (58 MB for 74,510 trades on 2026-09-24); a detail keeps the small cap.
const LIST_BODY_CAP: usize = 96 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The whole-list answer at a constrained link: the old poller's budget.
const LIST_TIMEOUT: Duration = Duration::from_secs(300);

use crate::types::{
    ASSET_TYPE_COLLECTION_ITEM, ASSET_TYPE_ERC20, ASSET_TYPE_ERC721, ASSET_TYPE_USD_PEGGED_MANA,
};

#[derive(Debug, Clone)]
pub struct UpstreamTradeHead {
    pub id: String,
    pub network: String,
    pub chain_id: i32,
    pub signature: String,
    pub hashed_signature: String,
    pub checks: Value,
    pub signer: String,
    pub trade_type: String,
    pub expires_at: DateTime<Utc>,
    pub effective_since: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub contract: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetValue {
    Erc20 { amount: String },
    Erc721 { token_id: String },
    Item { item_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamTradeAsset {
    pub direction: &'static str,
    pub asset_type: i16,
    pub contract_address: String,
    pub beneficiary: Option<String>,
    pub extra: String,
    pub value: AssetValue,
}

fn str_field(v: &Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field {key:?}"))
}

fn ts_field(v: &Value, key: &str) -> Result<DateTime<Utc>, String> {
    let raw = str_field(v, key)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("bad timestamp {key:?}={raw:?}: {e}"))
}

pub fn parse_list_row(v: &Value) -> Result<UpstreamTradeHead, String> {
    let trade_type = str_field(v, "type")?;
    match trade_type.as_str() {
        "bid" | "public_nft_order" | "public_item_order" => {}
        other => return Err(format!("unknown trade type {other:?}")),
    }
    Ok(UpstreamTradeHead {
        id: str_field(v, "id")?,
        network: str_field(v, "network")?,
        chain_id: v
            .get("chain_id")
            .and_then(|x| x.as_i64())
            .ok_or("missing chain_id")? as i32,
        signature: str_field(v, "signature")?.to_lowercase(),
        hashed_signature: str_field(v, "hashed_signature")?.to_lowercase(),
        checks: v.get("checks").cloned().ok_or("missing checks")?,
        signer: str_field(v, "signer")?.to_lowercase(),
        trade_type,
        expires_at: ts_field(v, "expires_at")?,
        effective_since: ts_field(v, "effective_since")?,
        created_at: ts_field(v, "created_at")?,
        contract: str_field(v, "contract")?,
    })
}

fn normalize_extra(raw: Option<&str>) -> String {
    match raw {
        None | Some("") | Some("0x") => String::new(),
        Some(s) => s.to_string(),
    }
}

fn parse_asset(v: &Value, direction: &'static str) -> Result<UpstreamTradeAsset, String> {
    let asset_type = v
        .get("assetType")
        .and_then(|x| x.as_i64())
        .ok_or("asset missing assetType")?;
    let value = match asset_type {
        1 | 2 => AssetValue::Erc20 {
            amount: str_field(v, "amount")?,
        },
        3 => AssetValue::Erc721 {
            token_id: str_field(v, "tokenId")?,
        },
        4 => AssetValue::Item {
            item_id: str_field(v, "itemId")?,
        },
        other => return Err(format!("unsupported assetType {other}")),
    };
    let beneficiary = v
        .get("beneficiary")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase());
    Ok(UpstreamTradeAsset {
        direction,
        asset_type: asset_type as i16,
        contract_address: str_field(v, "contractAddress")?.to_lowercase(),
        beneficiary,
        extra: normalize_extra(v.get("extra").and_then(|x| x.as_str())),
        value,
    })
}

pub fn parse_detail_assets(detail: &Value) -> Result<Vec<UpstreamTradeAsset>, String> {
    let mut out = Vec::new();
    for (key, direction) in [("sent", "sent"), ("received", "received")] {
        let arr = detail
            .get(key)
            .and_then(|x| x.as_array())
            .ok_or_else(|| format!("detail missing {key:?} array"))?;
        for a in arr {
            out.push(parse_asset(a, direction)?);
        }
    }
    if out.is_empty() {
        return Err("trade detail carries no assets".into());
    }
    Ok(out)
}

pub async fn known_signatures_among(
    pool: &PgPool,
    batch: &[String],
) -> Result<HashSet<String>, sqlx::Error> {
    if batch.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT hashed_signature FROM shop.trades WHERE hashed_signature = ANY($1)",
    )
    .bind(batch)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Inserts the head and its assets in one transaction; `Ok(false)` when a
/// row with that signature already exists (a local or an earlier mirrored one).
pub async fn insert_trade(
    pool: &PgPool,
    head: &UpstreamTradeHead,
    assets: &[UpstreamTradeAsset],
    mirrored_from: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let inserted: Option<(String,)> = sqlx::query_as(
        "INSERT INTO shop.trades \
             (id, network, chain_id, signature, hashed_signature, checks, signer, type, \
              expires_at, effective_since, contract, created_at, mirrored_from, mirrored_at) \
         VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8::shop.trade_type, $9, $10, $11, $12, $13, \
                 timezone('UTC', now())) \
         ON CONFLICT (hashed_signature) DO NOTHING \
         RETURNING id::text",
    )
    .bind(&head.id)
    .bind(&head.network)
    .bind(head.chain_id)
    .bind(&head.signature)
    .bind(&head.hashed_signature)
    .bind(&head.checks)
    .bind(&head.signer)
    .bind(&head.trade_type)
    .bind(head.expires_at.naive_utc())
    .bind(head.effective_since.naive_utc())
    .bind(&head.contract)
    .bind(head.created_at.naive_utc())
    .bind(mirrored_from)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((trade_id,)) = inserted else {
        tx.rollback().await?;
        return Ok(false);
    };

    for asset in assets {
        let (asset_id,): (String,) = sqlx::query_as(
            "INSERT INTO shop.trade_assets \
                 (trade_id, direction, asset_type, contract_address, beneficiary, extra) \
             VALUES ($1::uuid, $2::shop.asset_direction_type, $3, $4, $5, $6) \
             RETURNING id::text",
        )
        .bind(&trade_id)
        .bind(asset.direction)
        .bind(asset.asset_type)
        .bind(&asset.contract_address)
        .bind(asset.beneficiary.as_deref())
        .bind(&asset.extra)
        .fetch_one(&mut *tx)
        .await?;

        let (sql, value) = match &asset.value {
            AssetValue::Erc20 { amount } => (
                "INSERT INTO shop.trade_assets_erc20 (asset_id, amount) VALUES ($1::uuid, $2::numeric)",
                amount,
            ),
            AssetValue::Erc721 { token_id } => (
                "INSERT INTO shop.trade_assets_erc721 (asset_id, token_id) VALUES ($1::uuid, $2)",
                token_id,
            ),
            AssetValue::Item { item_id } => (
                "INSERT INTO shop.trade_assets_item (asset_id, item_id) VALUES ($1::uuid, $2)",
                item_id,
            ),
        };
        sqlx::query(sql)
            .bind(&asset_id)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(true)
}

/// Why a trades read may answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// The configured upstream has a complete pass in `shop.trades`.
    Mirrored,
    /// No upstream: the table holds the trades signed into this node, which
    /// accepts them (`DATA_WRITES_ENABLED`), so they are its own truth.
    LocalAuthority,
}

/// A complete pass stays the order book for this many sync intervals, and
/// never less than [`MIN_MAX_AGE`]: a caught-up pass completes every tick,
/// so the budget tolerates a few failed ticks and no more.
pub const MAX_AGE_INTERVALS: u32 = 4;
/// The other money sources' budget (prices, the chain indexes, LAND authz).
pub const MIN_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// How old the last complete pass may be before the trade reads refuse.
pub fn max_age(interval: Duration) -> Duration {
    interval.saturating_mul(MAX_AGE_INTERVALS).max(MIN_MAX_AGE)
}

/// The money rule over the pass marker at `now`: no complete pass, or one
/// older than `budget`, is a refusal (the message says which and how old).
pub fn pass_verdict(
    completed_at: Option<NaiveDateTime>,
    now: DateTime<Utc>,
    budget: Duration,
) -> Result<(), String> {
    let Some(completed_at) = completed_at else {
        return Err("the trades mirror has no complete pass yet for the configured upstream; /v1/trades-mirror shows the pass".to_string());
    };
    let age = now
        .naive_utc()
        .signed_duration_since(completed_at)
        .num_seconds()
        .max(0) as u64;
    if age > budget.as_secs() {
        return Err(format!(
            "the trades mirror's last complete pass is {age} s old, past the {} s freshness budget (the trades-mirror job is off or failing); /v1/trades-mirror shows the pass",
            budget.as_secs()
        ));
    }
    Ok(())
}

/// The oldest copy of a mirrored trade the reads count ([`counted_sql`]):
/// no floor while an upstream is configured (its pass marker gates the
/// reads, [`ready`]); under [`Readiness::LocalAuthority`] a row a former
/// upstream left counts within [`max_age`] of its `mirrored_at`, and the
/// node's own trades always count.
pub fn mirrored_since(settings: &Settings, now: NaiveDateTime) -> Option<NaiveDateTime> {
    if settings.trades_sync_upstream_url.is_some() {
        return None;
    }
    let budget = chrono::Duration::from_std(max_age(settings.trades_sync_interval))
        .unwrap_or(chrono::Duration::MAX);
    Some(now.checked_sub_signed(budget).unwrap_or(NaiveDateTime::MIN))
}

/// The predicate that keeps a trade of `alias` (with its dot, `"t."`) in a
/// read: a local trade always, a mirrored one while `since_p` (a bound
/// nullable timestamp, [`mirrored_since`]) is NULL or not newer than its copy.
pub fn counted_sql(alias: &str, since_p: &str) -> String {
    format!(
        "({alias}mirrored_from IS NULL OR {since_p}::timestamp IS NULL OR {alias}mirrored_at >= {since_p}::timestamp)"
    )
}

/// Whether `shop.trades` may be read as the truth, or the envelope to
/// answer instead. `trades_mirror.completed_at` for the configured upstream
/// (a completed pass stays completed while the next one runs), cached 10 s,
/// within [`max_age`] of now.
pub async fn ready(state: &MarketplaceState) -> Result<Readiness, Response> {
    let Some(upstream) = state.settings.trades_sync_upstream_url.clone() else {
        if state.settings.data_writes_enabled {
            return Ok(Readiness::LocalAuthority);
        }
        return Err(unavailable(
            Source::TradesMirror,
            "no trades upstream is configured and this node accepts no local trades (DATA_WRITES_ENABLED=false), so shop.trades has no source",
        ));
    };
    if state.settings.trades_serve_partial {
        // TRADES_SERVE_PARTIAL: a development stack reads what has arrived.
        return Ok(Readiness::Mirrored);
    }
    let completed = state
        .trades_ready
        .get_or_fetch((), || async move {
            sqlx::query_scalar::<_, Option<NaiveDateTime>>(
                "SELECT completed_at FROM shop.trades_mirror WHERE id = 1 AND upstream_url = $1",
            )
            .bind(upstream)
            .fetch_optional(&state.pool)
            .await
            .map(Option::flatten)
        })
        .await;
    match completed {
        Ok(completed_at) => pass_verdict(
            completed_at,
            Utc::now(),
            max_age(state.settings.trades_sync_interval),
        )
        .map(|()| Readiness::Mirrored)
        .map_err(|why| unavailable(Source::TradesMirror, &why)),
        Err(e) => {
            tracing::error!(error = %e, "trades_mirror read failed");
            Err(super::internal_server_error())
        }
    }
}

/// The gate every route over `shop.trades` passes first.
pub async fn require_ready(state: &MarketplaceState) -> Result<(), Response> {
    ready(state).await.map(|_| ())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MirrorState {
    pub upstream_url: String,
    pub pass: i64,
    pub next_skip: i64,
    pub started_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub pages_fetched: i64,
    pub rows_fetched: i64,
    pub rows_inserted: i64,
    pub rows_failed: i64,
    /// Body bytes read from the upstream during the pass (list + details).
    pub bytes_downloaded: i64,
    /// List requests made during the pass: one per tick when the upstream
    /// answers the whole list, one per page when it honours paging.
    pub list_downloads: i64,
    pub last_error: Option<String>,
    pub updated_at: Option<NaiveDateTime>,
}

impl MirrorState {
    fn fresh(upstream_url: &str) -> Self {
        Self {
            upstream_url: upstream_url.to_string(),
            pass: 0,
            next_skip: 0,
            started_at: None,
            completed_at: None,
            pages_fetched: 0,
            rows_fetched: 0,
            rows_inserted: 0,
            rows_failed: 0,
            bytes_downloaded: 0,
            list_downloads: 0,
            last_error: None,
            updated_at: None,
        }
    }

    /// True while a pass is under way (started and not yet completed).
    pub fn in_progress(&self) -> bool {
        match (self.started_at, self.completed_at) {
            (Some(_), None) => true,
            (Some(s), Some(c)) => c < s,
            _ => false,
        }
    }
}

pub async fn load_state(pool: &PgPool) -> Result<Option<MirrorState>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT upstream_url, pass, next_skip, started_at, completed_at, pages_fetched, rows_fetched, \
                rows_inserted, rows_failed, bytes_downloaded, list_downloads, last_error, updated_at \
         FROM shop.trades_mirror WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| MirrorState {
        upstream_url: r.get("upstream_url"),
        pass: r.get("pass"),
        next_skip: r.get("next_skip"),
        started_at: r.get("started_at"),
        completed_at: r.get("completed_at"),
        pages_fetched: r.get("pages_fetched"),
        rows_fetched: r.get("rows_fetched"),
        rows_inserted: r.get("rows_inserted"),
        rows_failed: r.get("rows_failed"),
        bytes_downloaded: r.get("bytes_downloaded"),
        list_downloads: r.get("list_downloads"),
        last_error: r.get("last_error"),
        updated_at: r.get("updated_at"),
    }))
}

async fn save_state(pool: &PgPool, s: &MirrorState) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO shop.trades_mirror \
             (id, upstream_url, pass, next_skip, started_at, completed_at, pages_fetched, rows_fetched, \
              rows_inserted, rows_failed, bytes_downloaded, list_downloads, last_error, updated_at) \
         VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, timezone('UTC', now())) \
         ON CONFLICT (id) DO UPDATE SET \
             upstream_url = EXCLUDED.upstream_url, pass = EXCLUDED.pass, next_skip = EXCLUDED.next_skip, \
             started_at = EXCLUDED.started_at, completed_at = EXCLUDED.completed_at, \
             pages_fetched = EXCLUDED.pages_fetched, rows_fetched = EXCLUDED.rows_fetched, \
             rows_inserted = EXCLUDED.rows_inserted, rows_failed = EXCLUDED.rows_failed, \
             bytes_downloaded = EXCLUDED.bytes_downloaded, list_downloads = EXCLUDED.list_downloads, \
             last_error = EXCLUDED.last_error, updated_at = EXCLUDED.updated_at",
    )
    .bind(&s.upstream_url)
    .bind(s.pass)
    .bind(s.next_skip)
    .bind(s.started_at)
    .bind(s.completed_at)
    .bind(s.pages_fetched)
    .bind(s.rows_fetched)
    .bind(s.rows_inserted)
    .bind(s.rows_failed)
    .bind(s.bytes_downloaded)
    .bind(s.list_downloads)
    .bind(&s.last_error)
    .execute(pool)
    .await?;
    Ok(())
}

/// The parsed body and its size in bytes.
async fn fetch_json(
    state: &MarketplaceState,
    url: &str,
    cap: usize,
    timeout: Duration,
) -> Result<(Value, usize), String> {
    if !state.upstream_allowed(url) {
        return Err("upstream URL is not a publicly routable http(s) endpoint".to_string());
    }
    let client = &state.http;
    let outcome = retry_with_backoff("trades-upstream", 3, 1000, || async move {
        let resp = match client
            .get(url)
            .timeout(timeout)
            .header(reqwest::header::ACCEPT_ENCODING, "gzip")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_transient() => {
                return Ok(RetryDecision::Retry {
                    after: None,
                    rate_limited: false,
                });
            }
            Err(e) => return Err(anyhow::anyhow!("request failed: {e}")),
        };
        let status = resp.status();
        if is_transient_status(status) {
            let after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            return Ok(RetryDecision::Retry {
                after,
                rate_limited: status == reqwest::StatusCode::TOO_MANY_REQUESTS,
            });
        }
        if !status.is_success() {
            return Err(anyhow::anyhow!("status {}", status.as_u16()));
        }
        let encoding = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_ascii_lowercase());
        let bytes = read_body_capped(resp, cap).await?;
        let bytes = inflate_if_gzip(encoding.as_deref(), bytes, cap)?;
        let body: Value =
            serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("parse failed: {e}"))?;
        Ok(RetryDecision::Done((body, bytes.len())))
    })
    .await
    .map_err(|e| e.to_string())?;
    outcome.ok_or_else(|| "upstream unavailable after retries".to_string())
}

/// Undoes a `Content-Encoding: gzip` the upstream applied, bounded by `cap`
/// decoded bytes (a bomb fails, never grows); any other coding is an error
/// rather than a body parsed as sent.
fn inflate_if_gzip(encoding: Option<&str>, bytes: Vec<u8>, cap: usize) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    match encoding.unwrap_or("") {
        "" | "identity" => Ok(bytes),
        "gzip" | "x-gzip" => {
            let mut decoded = Vec::new();
            flate2::read::MultiGzDecoder::new(bytes.as_slice())
                .take(cap as u64 + 1)
                .read_to_end(&mut decoded)
                .map_err(|e| anyhow::anyhow!("gzip transfer decoding failed: {e}"))?;
            if decoded.len() > cap {
                anyhow::bail!("decoded body exceeds {cap} byte cap");
            }
            Ok(decoded)
        }
        other => anyhow::bail!("unsupported content-encoding {other:?}"),
    }
}

fn list_url(base: &str, skip: i64) -> String {
    let base = base.trim_end_matches('/');
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}first={PAGE_SIZE}&skip={skip}")
}

/// The `PAGE_SIZE` rows at `skip` of a list held in memory; empty past its end.
fn page_of<T: Clone>(all: &[T], skip: i64) -> Vec<T> {
    let start = (skip.max(0) as usize).min(all.len());
    let end = (start + PAGE_SIZE as usize).min(all.len());
    all[start..end].to_vec()
}

/// One upstream list row: its head, or why it does not parse (counted as a
/// failed row by the page that reaches it).
pub type ListRow = Result<UpstreamTradeHead, String>;

fn parse_rows(rows: Vec<Value>) -> Vec<ListRow> {
    rows.iter().map(parse_list_row).collect()
}

/// The whole-list answer of an upstream that ignores paging, parsed once
/// and held for the pass it belongs to (`MarketplaceState::trades_list`):
/// the next tick of the same pass pages over it instead of downloading it
/// again. Dropped when the pass completes, when the pass or upstream
/// changes, and with the process.
pub struct PassList {
    pass: i64,
    upstream: String,
    rows: Vec<ListRow>,
}

impl PassList {
    fn matches(&self, pass: i64, upstream: &str) -> bool {
        self.pass == pass && self.upstream == upstream
    }
}

fn held_list(state: &MarketplaceState, pass: i64, upstream: &str) -> Option<Arc<PassList>> {
    state
        .trades_list
        .lock()
        .ok()?
        .as_ref()
        .filter(|l| l.matches(pass, upstream))
        .cloned()
}

fn hold_list(state: &MarketplaceState, list: Option<Arc<PassList>>) {
    if let Ok(mut slot) = state.trades_list.lock() {
        *slot = list;
    }
}

fn detail_url(base: &str, id: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = base.split('?').next().unwrap_or(base);
    format!("{path}/{id}")
}

async fn fetch_detail_assets(
    state: &MarketplaceState,
    url: &str,
) -> Result<(Vec<UpstreamTradeAsset>, usize), String> {
    let (body, bytes) = fetch_json(state, url, BODY_CAP, REQUEST_TIMEOUT).await?;
    let detail = body.get("data").ok_or("detail missing data")?;
    Ok((parse_detail_assets(detail)?, bytes))
}

pub struct PageStats {
    pub fetched: usize,
    pub new: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Detail fetches made (the budget this page consumed).
    pub attempted: usize,
    /// New trades left for the next tick because the budget ran out; the
    /// cursor must not move past a page with any.
    pub deferred: usize,
    pub bytes: usize,
}

/// One upstream page: parse, drop the signatures we hold, fetch each new
/// trade's assets (at most `budget`, `None` unbounded) and insert.
async fn mirror_page(
    state: &MarketplaceState,
    upstream: &str,
    rows: &[ListRow],
    budget: Option<usize>,
) -> Result<PageStats, String> {
    let pool = &state.pool;
    let batch: Vec<String> = rows
        .iter()
        .filter_map(|r| r.as_ref().ok().map(|h| h.hashed_signature.clone()))
        .collect();
    let known = known_signatures_among(pool, &batch)
        .await
        .map_err(|e| format!("local hashed_signature scan failed: {e}"))?;

    let mut stats = PageStats {
        fetched: rows.len(),
        new: 0,
        skipped: 0,
        failed: 0,
        attempted: 0,
        deferred: 0,
        bytes: 0,
    };
    let mut fresh: Vec<UpstreamTradeHead> = Vec::new();
    for parsed_row in rows {
        match parsed_row {
            Ok(h) if known.contains(&h.hashed_signature) => stats.skipped += 1,
            Ok(h) => fresh.push(h.clone()),
            Err(e) => {
                stats.failed += 1;
                tracing::warn!(error = %e, "trades mirror: unparseable upstream list row");
            }
        }
    }
    if let Some(budget) = budget
        && fresh.len() > budget
    {
        stats.deferred = fresh.len() - budget;
        fresh.truncate(budget);
    }
    stats.attempted = fresh.len();

    let mirrored_from = upstream.split('?').next().unwrap_or(upstream);
    let mut details = futures::stream::iter(fresh.into_iter().map(|head| async move {
        let assets = fetch_detail_assets(state, &detail_url(upstream, &head.id)).await;
        (head, assets)
    }))
    .buffered(DETAIL_CONCURRENCY);
    while let Some((head, assets)) = details.next().await {
        let assets = match assets {
            Ok((a, bytes)) => {
                stats.bytes += bytes;
                a
            }
            Err(e) => {
                stats.failed += 1;
                tracing::warn!(trade_id = %head.id, error = %e, "trades mirror: detail fetch failed");
                continue;
            }
        };
        match insert_trade(pool, &head, &assets, mirrored_from).await {
            Ok(true) => stats.new += 1,
            Ok(false) => stats.skipped += 1,
            Err(e) => {
                stats.failed += 1;
                tracing::warn!(trade_id = %head.id, error = %e, "trades mirror: insert failed");
            }
        }
    }
    Ok(stats)
}

/// One periodic tick: resume the pass at the persisted cursor and walk pages
/// until the pass completes (a short page) or the tick's budget is spent:
/// `Settings::trades_sync_pages_per_run` x [`PAGE_SIZE`] detail fetches, and
/// as many upstream page requests when the upstream honours paging (0 =
/// unbounded). Persists after each page. Cancellation between pages leaves
/// the cursor where it is.
pub async fn run_pass(
    state: &MarketplaceState,
    upstream: &str,
    ctx: &JobContext,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    let mut s = match load_state(pool).await? {
        Some(s) if s.upstream_url == upstream => s,
        Some(s) => {
            tracing::warn!(
                old = %s.upstream_url,
                "trades mirror: upstream changed; the pass cursor restarts from the first page"
            );
            MirrorState::fresh(upstream)
        }
        None => MirrorState::fresh(upstream),
    };
    if !s.in_progress() {
        s.pass += 1;
        s.next_skip = 0;
        s.started_at = Some(Utc::now().naive_utc());
        s.pages_fetched = 0;
        s.rows_fetched = 0;
        s.rows_inserted = 0;
        s.rows_failed = 0;
        s.bytes_downloaded = 0;
        s.list_downloads = 0;
        s.last_error = None;
        save_state(pool, &s).await?;
        tracing::info!(pass = s.pass, "trades mirror: pass started");
    } else {
        tracing::info!(
            pass = s.pass,
            skip = s.next_skip,
            "trades mirror: resuming pass at the persisted cursor"
        );
    }

    let page_budget = state.settings.trades_sync_pages_per_run;
    let mut details_left: Option<usize> =
        (page_budget > 0).then_some(page_budget * PAGE_SIZE as usize);
    let mut page_requests = 0usize;
    // An upstream that ignores `first`/`skip` (marketplace-api.decentraland.org
    // answers the whole 58 MB list to every page request, 2026-09-24) is
    // downloaded once per pass, held across the pass's ticks and paged over
    // locally, so the cursor, the per-page persistence and the short-page
    // completion keep their meaning.
    let mut whole: Option<Arc<PassList>> = held_list(state, s.pass, upstream);
    if let Some(l) = &whole {
        tracing::info!(
            pass = s.pass,
            rows = l.rows.len(),
            "trades mirror: paging over the list held from an earlier tick of this pass"
        );
    }
    loop {
        if ctx.is_cancelled() {
            ctx.progress(format!(
                "pass {} paused at skip {} (shutdown)",
                s.pass, s.next_skip
            ));
            return Ok(());
        }
        if whole.is_none() && page_budget > 0 && page_requests >= page_budget {
            break;
        }
        let rows: Vec<ListRow> = match &whole {
            Some(all) => page_of(&all.rows, s.next_skip),
            None => {
                let (mut body, bytes) = match fetch_json(
                    state,
                    &list_url(upstream, s.next_skip),
                    LIST_BODY_CAP,
                    LIST_TIMEOUT,
                )
                .await
                {
                    Ok(b) => b,
                    Err(e) => {
                        s.last_error = Some(format!("page at skip {}: {e}", s.next_skip));
                        save_state(pool, &s).await?;
                        anyhow::bail!("upstream list at skip {}: {e}", s.next_skip);
                    }
                };
                s.bytes_downloaded += bytes as i64;
                s.list_downloads += 1;
                page_requests += 1;
                // Moved out, not cloned: the whole-list answer is 74,531 rows.
                let rows = body
                    .get_mut("data")
                    .and_then(|d| d.get_mut("data"))
                    .and_then(|d| d.as_array_mut())
                    .map(std::mem::take)
                    .ok_or_else(|| anyhow::anyhow!("upstream list missing data.data array"))?;
                drop(body);
                if rows.len() as i64 > PAGE_SIZE {
                    // The steady state against production: marketplace-server's
                    // getTradesHandler has no paging, so first/skip are sent for a
                    // v2 peer that honours them and the whole list arrives here.
                    tracing::info!(
                        rows = rows.len(),
                        bytes,
                        skip = s.next_skip,
                        "trades mirror: upstream answers the whole list (no paging upstream); holding it for this pass and paging over it locally"
                    );
                    let list = Arc::new(PassList {
                        pass: s.pass,
                        upstream: upstream.to_string(),
                        rows: parse_rows(rows),
                    });
                    let page = page_of(&list.rows, s.next_skip);
                    hold_list(state, Some(list.clone()));
                    whole = Some(list);
                    page
                } else {
                    parse_rows(rows)
                }
            }
        };
        let stats = mirror_page(state, upstream, &rows, details_left)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        s.bytes_downloaded += stats.bytes as i64;
        s.last_error = None;
        if let Some(left) = details_left.as_mut() {
            *left = left.saturating_sub(stats.attempted);
        }
        if stats.deferred > 0 {
            // The page is only partly held: the cursor stays on it and the
            // next tick fetches the rest (the held rows then cost a scan).
            s.rows_inserted += stats.new as i64;
            s.rows_failed += stats.failed as i64;
            save_state(pool, &s).await?;
            tracing::info!(
                pass = s.pass,
                skip = s.next_skip,
                inserted = stats.new,
                deferred = stats.deferred,
                "trades mirror: detail budget spent mid-page; continuing next tick"
            );
            ctx.progress(format!(
                "pass {}: {} pages, {} rows, {} inserted, {} failed; budget spent at skip {}",
                s.pass,
                s.pages_fetched,
                s.rows_fetched,
                s.rows_inserted,
                s.rows_failed,
                s.next_skip
            ));
            return Ok(());
        }
        s.pages_fetched += 1;
        s.rows_fetched += stats.fetched as i64;
        s.rows_inserted += stats.new as i64;
        s.rows_failed += stats.failed as i64;
        s.next_skip += rows.len() as i64;
        let short = (rows.len() as i64) < PAGE_SIZE;
        if short {
            s.completed_at = Some(Utc::now().naive_utc());
            s.next_skip = 0;
            if whole.is_some() {
                hold_list(state, None);
            }
        }
        save_state(pool, &s).await?;
        tracing::info!(
            pass = s.pass,
            fetched = stats.fetched,
            new = stats.new,
            skipped = stats.skipped,
            failed = stats.failed,
            next_skip = s.next_skip,
            "trades mirror: page done"
        );
        ctx.progress(format!(
            "pass {}: {} pages, {} rows, {} inserted, {} failed{}",
            s.pass,
            s.pages_fetched,
            s.rows_fetched,
            s.rows_inserted,
            s.rows_failed,
            if short { " (complete)" } else { "" }
        ));
        if short {
            tracing::info!(
                pass = s.pass,
                rows = s.rows_fetched,
                inserted = s.rows_inserted,
                bytes = s.bytes_downloaded,
                list_downloads = s.list_downloads,
                "trades mirror: pass complete"
            );
            return Ok(());
        }
    }
    tracing::info!(
        pass = s.pass,
        skip = s.next_skip,
        "trades mirror: page budget spent; continuing next tick"
    );
    Ok(())
}

fn ms<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_i64(dt.timestamp_millis())
}

fn iso<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}

#[derive(Debug, Serialize)]
pub struct DbTradeListRow {
    pub id: String,
    pub chain_id: i32,
    pub checks: JsonValue,
    #[serde(serialize_with = "iso")]
    pub created_at: DateTime<Utc>,
    #[serde(serialize_with = "iso")]
    pub effective_since: DateTime<Utc>,
    #[serde(serialize_with = "iso")]
    pub expires_at: DateTime<Utc>,
    pub network: String,
    pub signature: String,
    pub signer: String,
    #[serde(rename = "type")]
    pub trade_type: String,
    pub contract: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirrored_from: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TradeAsset {
    #[serde(rename = "assetType")]
    pub asset_type: i32,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    pub extra: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    #[serde(rename = "tokenId", skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    #[serde(rename = "itemId", skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beneficiary: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Trade {
    pub id: String,
    pub signer: String,
    pub signature: String,
    #[serde(rename = "type")]
    pub trade_type: String,
    pub network: String,
    #[serde(rename = "chainId")]
    pub chain_id: i32,
    pub checks: JsonValue,
    #[serde(rename = "createdAt", serialize_with = "ms")]
    pub created_at: DateTime<Utc>,
    pub sent: Vec<TradeAsset>,
    pub received: Vec<TradeAsset>,
    pub contract: String,
    #[serde(rename = "mirroredFrom", skip_serializing_if = "Option::is_none")]
    pub mirrored_from: Option<String>,
}

fn utc(r: &sqlx::postgres::PgRow, col: &str) -> DateTime<Utc> {
    r.try_get::<NaiveDateTime, _>(col)
        .map(|n| n.and_utc())
        .unwrap_or_else(|_| Utc::now())
}

fn row_to_list_row(r: &sqlx::postgres::PgRow) -> DbTradeListRow {
    DbTradeListRow {
        id: r.try_get("id").unwrap_or_default(),
        chain_id: r.try_get::<i32, _>("chain_id").unwrap_or(0),
        checks: r.try_get("checks").unwrap_or(JsonValue::Null),
        created_at: utc(r, "created_at"),
        effective_since: utc(r, "effective_since"),
        expires_at: utc(r, "expires_at"),
        network: r.try_get("network").unwrap_or_default(),
        signature: r.try_get("signature").unwrap_or_default(),
        signer: r.try_get("signer").unwrap_or_default(),
        trade_type: r.try_get("type").unwrap_or_default(),
        contract: r.try_get("contract").unwrap_or_default(),
        mirrored_from: r.try_get("mirrored_from").unwrap_or(None),
    }
}

const TRADE_COLUMNS: &str = "id::text AS id, chain_id::int4 AS chain_id, checks, created_at, \
       effective_since, expires_at, network, signature, signer, type::text AS type, contract, mirrored_from";

/// The `/v1/trades` page over the trades that count ([`counted_sql`]).
pub async fn list_trades(
    pool: &PgPool,
    first: Option<i64>,
    skip: Option<i64>,
    since: Option<NaiveDateTime>,
) -> Result<(Vec<DbTradeListRow>, i64), sqlx::Error> {
    let limit = first.unwrap_or(100).clamp(0, 1000);
    let offset = skip.unwrap_or(0).max(0);
    let counted = counted_sql("", "$3");
    let served = super::catalog::served_trade_sql("t");
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {TRADE_COLUMNS}, COUNT(*) OVER()::int8 AS total \
         FROM shop.trades t WHERE {counted} AND {served} ORDER BY created_at DESC LIMIT $1 OFFSET $2"
    )))
    .bind(limit)
    .bind(offset)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let total: i64 = match rows.first() {
        Some(row) => row.try_get("total").unwrap_or(0),
        // An empty page past the end (or a zero-row page) says nothing about the total.
        None if offset > 0 || limit == 0 => {
            let counted = counted_sql("", "$1");
            let served = super::catalog::served_trade_sql("t");
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM shop.trades t WHERE {counted} AND {served}"
            )))
            .bind(since)
            .fetch_one(pool)
            .await?
        }
        None => 0,
    };
    Ok((rows.iter().map(row_to_list_row).collect(), total))
}

fn public_asset(r: &sqlx::postgres::PgRow, with_beneficiary: bool) -> TradeAsset {
    let asset_type = r.try_get::<i32, _>("asset_type").unwrap_or(0);
    let amount = r.try_get::<Option<String>, _>("amount").unwrap_or(None);
    let token_id = r.try_get::<Option<String>, _>("token_id").unwrap_or(None);
    let item_id = r.try_get::<Option<String>, _>("item_id").unwrap_or(None);
    let (amount, token_id, item_id) = match asset_type {
        ASSET_TYPE_ERC20 | ASSET_TYPE_USD_PEGGED_MANA => (amount, None, None),
        ASSET_TYPE_ERC721 => (None, token_id, None),
        ASSET_TYPE_COLLECTION_ITEM => (None, None, item_id),
        _ => (None, None, None),
    };
    TradeAsset {
        asset_type,
        contract_address: r.try_get("asset_contract_address").unwrap_or_default(),
        extra: r
            .try_get::<Option<String>, _>("asset_extra")
            .unwrap_or(None)
            .unwrap_or_default(),
        amount,
        token_id,
        item_id,
        beneficiary: if with_beneficiary {
            r.try_get::<Option<String>, _>("asset_beneficiary")
                .unwrap_or(None)
        } else {
            None
        },
    }
}

/// The newest `limit` trades an address signed or is the beneficiary of an
/// asset in (the old `get_trades_by_address`, for `/v1/activity`), with
/// their assets, newest first. Both legs are bounded by the limit before
/// the union (`idx_trades_signer_created`, `idx_trade_assets_beneficiary_created`);
/// signer and beneficiary are stored lowercase. `since` is the trades floor
/// ([`mirrored_since`]), applied inside each leg so a dropped copy never
/// takes a place under the limit.
pub async fn trades_of_address(
    pool: &PgPool,
    address: &str,
    limit: i64,
    since: Option<NaiveDateTime>,
) -> Result<Vec<Trade>, sqlx::Error> {
    let address = address.to_lowercase();
    let heads = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {TRADE_COLUMNS} FROM shop.trades \
         WHERE id IN ( \
            (SELECT s.id FROM shop.trades s WHERE s.signer = $1 AND {signer_counted} AND {signer_served} ORDER BY s.created_at DESC LIMIT $2) \
            UNION \
            (SELECT ta.trade_id FROM shop.trade_assets ta JOIN shop.trades t ON t.id = ta.trade_id \
             WHERE ta.beneficiary = $1 AND {beneficiary_counted} AND {beneficiary_served} ORDER BY ta.created_at DESC LIMIT $2)) \
         ORDER BY created_at DESC LIMIT $2",
        signer_counted = counted_sql("s.", "$3"),
        beneficiary_counted = counted_sql("t.", "$3"),
        signer_served = super::catalog::served_trade_sql("s"),
        beneficiary_served = super::catalog::served_trade_sql("t"),
    )))
    .bind(&address)
    .bind(limit)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let ids: Vec<String> = heads
        .iter()
        .map(|r| r.try_get::<String, _>("id").unwrap_or_default())
        .collect();
    let asset_rows = sqlx::query(
        "SELECT ta.trade_id::text AS trade_id, ta.asset_type::int4 AS asset_type, ta.contract_address AS asset_contract_address, \
                ta.beneficiary AS asset_beneficiary, ta.direction::text AS asset_direction, \
                ta.extra AS asset_extra, \
                erc721.token_id AS token_id, erc20.amount::text AS amount, item.item_id AS item_id \
         FROM shop.trade_assets AS ta \
         LEFT JOIN shop.trade_assets_erc721 AS erc721 ON ta.id = erc721.asset_id \
         LEFT JOIN shop.trade_assets_erc20  AS erc20  ON ta.id = erc20.asset_id \
         LEFT JOIN shop.trade_assets_item   AS item   ON ta.id = item.asset_id \
         WHERE ta.trade_id = ANY($1::uuid[]) \
         ORDER BY ta.direction ASC, ta.created_at ASC",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let mut assets: HashMap<String, (Vec<TradeAsset>, Vec<TradeAsset>)> = HashMap::new();
    for r in &asset_rows {
        let trade_id: String = r.try_get("trade_id").unwrap_or_default();
        let entry = assets.entry(trade_id).or_default();
        if r.try_get::<String, _>("asset_direction")
            .unwrap_or_default()
            == "sent"
        {
            entry.0.push(public_asset(r, false));
        } else {
            entry.1.push(public_asset(r, true));
        }
    }
    Ok(heads
        .iter()
        .map(|head| {
            let list = row_to_list_row(head);
            let (sent, received) = assets.remove(&list.id).unwrap_or_default();
            Trade {
                id: list.id,
                signer: list.signer,
                signature: list.signature,
                trade_type: list.trade_type,
                network: list.network,
                chain_id: list.chain_id,
                checks: list.checks,
                created_at: list.created_at,
                sent,
                received,
                contract: list.contract,
                mirrored_from: list.mirrored_from,
            }
        })
        .collect())
}

/// Which stored trades a lookup by id may answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeScope {
    /// Only the trades this node serves ([`super::catalog::served_trade_sql`]):
    /// what it lists or prices.
    Served,
    /// Any stored trade: cancelling or answering one must keep working.
    Any,
}

/// The trade `id` with its assets, when it counts at `since` ([`counted_sql`])
/// and is in `scope`.
pub async fn get_trade_by_id(
    pool: &PgPool,
    id: &str,
    since: Option<NaiveDateTime>,
    scope: TradeScope,
) -> Result<Option<Trade>, sqlx::Error> {
    let counted = counted_sql("", "$2");
    let served = match scope {
        TradeScope::Served => super::catalog::served_trade_sql("t"),
        TradeScope::Any => "TRUE".to_string(),
    };
    let Some(head) = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {TRADE_COLUMNS} FROM shop.trades t WHERE id = $1::uuid AND {counted} AND {served}"
    )))
    .bind(id)
    .bind(since)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let asset_rows = sqlx::query(
        "SELECT ta.asset_type::int4 AS asset_type, ta.contract_address AS asset_contract_address, \
                ta.beneficiary AS asset_beneficiary, ta.direction::text AS asset_direction, \
                ta.extra AS asset_extra, \
                erc721.token_id AS token_id, erc20.amount::text AS amount, item.item_id AS item_id \
         FROM shop.trade_assets AS ta \
         LEFT JOIN shop.trade_assets_erc721 AS erc721 ON ta.id = erc721.asset_id \
         LEFT JOIN shop.trade_assets_erc20  AS erc20  ON ta.id = erc20.asset_id \
         LEFT JOIN shop.trade_assets_item   AS item   ON ta.id = item.asset_id \
         WHERE ta.trade_id = $1::uuid \
         ORDER BY ta.direction ASC, ta.created_at ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let mut sent = Vec::new();
    let mut received = Vec::new();
    for r in &asset_rows {
        let dir: String = r.try_get("asset_direction").unwrap_or_default();
        if dir == "sent" {
            sent.push(public_asset(r, false));
        } else {
            received.push(public_asset(r, true));
        }
    }
    let list = row_to_list_row(&head);
    Ok(Some(Trade {
        id: list.id,
        signer: list.signer,
        signature: list.signature,
        trade_type: list.trade_type,
        network: list.network,
        chain_id: list.chain_id,
        checks: list.checks,
        created_at: list.created_at,
        sent,
        received,
        contract: list.contract,
        mirrored_from: list.mirrored_from,
    }))
}

fn number_param(pairs: &[(String, String)], key: &str) -> Result<Option<i64>, Response> {
    match pairs.iter().find(|(k, _)| k == key) {
        None => Ok(None),
        Some((_, v)) => v.trim().parse::<i64>().map(Some).map_err(|_| {
            lambdas_error(
                axum::http::StatusCode::BAD_REQUEST,
                "Bad request",
                &format!("The value of the {key} parameter is invalid: {v}"),
            )
        }),
    }
}

fn market_error(e: sqlx::Error) -> Response {
    tracing::error!(error = %e, "trades query failed");
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "ok": false, "error": "Internal Server Error", "message": "Internal Server Error" })),
    )
        .into_response()
}

pub async fn get_trades(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let first = match number_param(&pairs, "first") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let skip = match number_param(&pairs, "skip") {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(envelope) = require_ready(&state).await {
        return envelope;
    }
    let since = mirrored_since(&state.settings, Utc::now().naive_utc());
    match list_trades(&state.pool, first, skip, since).await {
        Ok((data, count)) => {
            Json(json!({ "ok": true, "data": { "data": data, "count": count } })).into_response()
        }
        Err(e) => market_error(e),
    }
}

pub async fn get_trade(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("Trade with id {id} not found"), "message": format!("Trade with id {id} not found") })),
        )
            .into_response();
    }
    if let Err(envelope) = require_ready(&state).await {
        return envelope;
    }
    let since = mirrored_since(&state.settings, Utc::now().naive_utc());
    match get_trade_by_id(&state.pool, &id, since, TradeScope::Served).await {
        Ok(Some(trade)) => Json(json!({ "ok": true, "data": trade })).into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("Trade with id {id} not found"), "message": format!("Trade with id {id} not found") })),
        )
            .into_response(),
        Err(e) => market_error(e),
    }
}

/// The mirror's pass marker and row counts: the freshness a client or an
/// operator can read (`completedAt` is the last complete pass).
pub async fn get_mirror_status(State(state): State<SharedState>) -> Response {
    let configured = state.settings.trades_sync_upstream_url.is_some();
    let counts: Result<(i64, i64), sqlx::Error> = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE mirrored_from IS NOT NULL), count(*) FILTER (WHERE mirrored_from IS NULL) \
         FROM shop.trades",
    )
    .fetch_one(&state.pool)
    .await;
    let (mirrored, local) = match counts {
        Ok(c) => c,
        Err(e) => return market_error(e),
    };
    let marker = match load_state(&state.pool).await {
        Ok(m) => m,
        Err(e) => return market_error(e),
    };
    let iso = |t: Option<NaiveDateTime>| {
        t.map(|t| t.and_utc().to_rfc3339_opts(SecondsFormat::Millis, true))
    };
    Json(json!({
        "ok": true,
        "data": {
            "enabled": configured,
            "mirroredTrades": mirrored,
            "localTrades": local,
            "pass": marker.as_ref().map(|m| m.pass),
            "inProgress": marker.as_ref().is_some_and(|m| m.in_progress()),
            "nextSkip": marker.as_ref().map(|m| m.next_skip),
            "startedAt": marker.as_ref().and_then(|m| iso(m.started_at)),
            "completedAt": marker.as_ref().and_then(|m| iso(m.completed_at)),
            "pagesFetched": marker.as_ref().map(|m| m.pages_fetched),
            "rowsFetched": marker.as_ref().map(|m| m.rows_fetched),
            "rowsInserted": marker.as_ref().map(|m| m.rows_inserted),
            "rowsFailed": marker.as_ref().map(|m| m.rows_failed),
            "bytesDownloaded": marker.as_ref().map(|m| m.bytes_downloaded),
            "listDownloads": marker.as_ref().map(|m| m.list_downloads),
            "lastError": marker.as_ref().and_then(|m| m.last_error.clone()),
            "updatedAt": marker.as_ref().and_then(|m| iso(m.updated_at)),
        }
    }))
    .into_response()
}

#[cfg(test)]
pub(crate) mod fixtures {
    use serde_json::{Value, json};

    pub fn list_row() -> Value {
        json!({
            "id": "6aa4130b-42b1-4ee5-84c1-6bac9e6b63f5",
            "network": "MATIC",
            "chain_id": 137,
            "signature": "0x7FD2afc813e91b1b5ddabe66cd81b132a881ff9a5d73440c100afe0a338d2d6b3598857ffa3add6acc1389d4b9d067ea14b138d9ad7488865b966920ee7e41181b",
            "hashed_signature": "0xF573a15c62efabc7ad5328d649fc89fd0bb10c66f7d524e336d4b1f5c10ad673",
            "checks": {
                "salt": "0x988c1638e5",
                "uses": 1,
                "effective": 1733691659485i64,
                "expiration": 1736204400000i64,
                "allowedRoot": "0x",
                "externalChecks": [],
                "signerSignatureIndex": 0,
                "contractSignatureIndex": 0
            },
            "signer": "0x747C6f502272129bf1ba872a1903045b837ee86c",
            "type": "public_nft_order",
            "expires_at": "2025-01-06T23:00:00.000Z",
            "effective_since": "2024-12-08T21:00:59.485Z",
            "created_at": "2024-12-08T21:01:07.473Z",
            "contract": "0x540fb08eDb56AaE562864B390542C97F562825BA"
        })
    }

    pub fn detail() -> Value {
        json!({
            "id": "568da193-be7a-474f-a2c5-e51fcd2e2d53",
            "signer": "0x747c6f502272129bf1ba872a1903045b837ee86c",
            "type": "public_nft_order",
            "sent": [
                {
                    "contractAddress": "0x659704BDC5152b0348a7b526d5120CAB98ee6f7F",
                    "extra": "",
                    "assetType": 3,
                    "tokenId": "450"
                }
            ],
            "received": [
                {
                    "contractAddress": "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4",
                    "extra": "0x",
                    "assetType": 1,
                    "amount": "150000000000000000000",
                    "beneficiary": "0x747C6F502272129bf1ba872a1903045b837ee86c"
                }
            ],
            "contract": "0x540fb08eDb56AaE562864B390542C97F562825BA"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{detail as fixture_detail, list_row as fixture_list_row};
    use super::*;

    #[test]
    fn the_list_is_inflated_here_not_by_reqwest() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"[{\"id\":1}]").unwrap();
        let wire = gz.finish().unwrap();
        assert_eq!(
            inflate_if_gzip(Some("gzip"), wire.clone(), 1024).unwrap(),
            b"[{\"id\":1}]"
        );
        assert_eq!(inflate_if_gzip(None, b"[]".to_vec(), 1024).unwrap(), b"[]");
        assert!(
            inflate_if_gzip(Some("gzip"), wire, 4)
                .unwrap_err()
                .to_string()
                .contains("exceeds 4 byte cap")
        );
        assert!(
            inflate_if_gzip(Some("gzip"), b"plain".to_vec(), 1024)
                .unwrap_err()
                .to_string()
                .contains("gzip transfer decoding failed")
        );
        assert!(inflate_if_gzip(Some("br"), b"x".to_vec(), 1024).is_err());
    }

    #[test]
    fn list_row_maps_to_trades_columns() {
        let head = parse_list_row(&fixture_list_row()).unwrap();
        assert_eq!(head.id, "6aa4130b-42b1-4ee5-84c1-6bac9e6b63f5");
        assert_eq!(head.network, "MATIC");
        assert_eq!(head.chain_id, 137);
        assert!(head.signature.starts_with("0x7fd2afc813e91b1b"));
        assert_eq!(
            head.hashed_signature,
            "0xf573a15c62efabc7ad5328d649fc89fd0bb10c66f7d524e336d4b1f5c10ad673"
        );
        assert_eq!(head.signer, "0x747c6f502272129bf1ba872a1903045b837ee86c");
        assert_eq!(head.trade_type, "public_nft_order");
        assert_eq!(head.checks["salt"], "0x988c1638e5");
        assert_eq!(head.checks["uses"], 1);
        assert_eq!(head.created_at.timestamp_millis(), 1_733_691_667_473);
        assert_eq!(head.expires_at.timestamp_millis(), 1_736_204_400_000);
        assert_eq!(head.effective_since.timestamp_millis(), 1_733_691_659_485);
        assert_eq!(head.contract, "0x540fb08eDb56AaE562864B390542C97F562825BA");
    }

    #[test]
    fn detail_assets_map_to_trade_asset_rows() {
        let assets = parse_detail_assets(&fixture_detail()).unwrap();
        assert_eq!(assets.len(), 2);
        let sent = &assets[0];
        assert_eq!(sent.direction, "sent");
        assert_eq!(sent.asset_type, 3);
        assert_eq!(
            sent.contract_address, "0x659704bdc5152b0348a7b526d5120cab98ee6f7f",
            "asset contract addresses are stored lowercase"
        );
        assert_eq!(sent.beneficiary, None, "sent assets carry no beneficiary");
        assert_eq!(sent.extra, "");
        assert_eq!(
            sent.value,
            AssetValue::Erc721 {
                token_id: "450".into()
            }
        );
        let received = &assets[1];
        assert_eq!(received.direction, "received");
        assert_eq!(received.asset_type, 1);
        assert_eq!(
            received.beneficiary.as_deref(),
            Some("0x747c6f502272129bf1ba872a1903045b837ee86c"),
            "received beneficiary is stored lowercase"
        );
        assert_eq!(received.extra, "", "a bare '0x' extra normalizes to ''");
        assert_eq!(
            received.value,
            AssetValue::Erc20 {
                amount: "150000000000000000000".into()
            }
        );
    }

    #[test]
    fn item_order_assets_route_to_the_item_sub_table() {
        let detail = json!({
            "sent": [
                {"contractAddress": "0xAAA0000000000000000000000000000000000aaa", "assetType": 4, "itemId": "7", "extra": ""}
            ],
            "received": [
                {"contractAddress": "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4", "assetType": 1, "amount": "1000", "extra": "",
                 "beneficiary": "0xbbb0000000000000000000000000000000000bbb"}
            ]
        });
        let assets = parse_detail_assets(&detail).unwrap();
        assert_eq!(
            assets[0].value,
            AssetValue::Item {
                item_id: "7".into()
            }
        );
        assert_eq!(assets[0].asset_type, 4);
    }

    #[test]
    fn unknown_asset_type_and_missing_typed_values_are_refused() {
        let detail = json!({ "sent": [{"contractAddress": "0xaaa", "assetType": 9, "extra": ""}], "received": [] });
        assert!(parse_detail_assets(&detail).is_err());
        let detail = json!({ "sent": [{"contractAddress": "0xaaa", "assetType": 3, "extra": ""}], "received": [] });
        assert!(parse_detail_assets(&detail).is_err());
        let detail = json!({ "sent": [], "received": [{"contractAddress": "0xaaa", "assetType": 1, "extra": ""}] });
        assert!(parse_detail_assets(&detail).is_err());
    }

    #[test]
    fn tradeless_detail_is_refused() {
        assert!(parse_detail_assets(&json!({"sent": [], "received": []})).is_err());
        assert!(parse_detail_assets(&json!({})).is_err());
    }

    #[test]
    fn bid_rows_are_synced_and_unknown_types_refused() {
        let mut row = fixture_list_row();
        row["type"] = json!("bid");
        assert_eq!(parse_list_row(&row).unwrap().trade_type, "bid");
        row["type"] = json!("mystery_order");
        assert!(parse_list_row(&row).is_err());
    }

    #[test]
    fn extra_normalization_matches_snapshot_convention() {
        assert_eq!(normalize_extra(None), "");
        assert_eq!(normalize_extra(Some("")), "");
        assert_eq!(normalize_extra(Some("0x")), "");
        assert_eq!(normalize_extra(Some("0xdead")), "0xdead");
    }

    #[test]
    fn upstream_urls_page_the_list_and_address_the_detail() {
        assert_eq!(
            list_url("https://m.example/v1/trades/", 200),
            "https://m.example/v1/trades?first=100&skip=200"
        );
        assert_eq!(
            list_url("https://m.example/v1/trades?x=1", 0),
            "https://m.example/v1/trades?x=1&first=100&skip=0"
        );
        assert_eq!(
            detail_url("https://m.example/v1/trades?x=1", "abc"),
            "https://m.example/v1/trades/abc"
        );
        let all: Vec<Value> = (0..250).map(|i| json!(i)).collect();
        assert_eq!(page_of(&all, 0).len(), PAGE_SIZE as usize);
        assert_eq!(page_of(&all, 200).len(), 50, "the last page is short");
        assert!(page_of(&all, 250).is_empty() && page_of(&all, 9_999).is_empty());
        assert_eq!(page_of(&all, 100)[0], json!(100));
    }

    /// Trades are money data: a complete pass serves within the budget and
    /// is refused past it, naming its age; no pass is refused too.
    #[test]
    fn a_pass_older_than_the_budget_is_not_the_order_book() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let budget = max_age(Duration::from_secs(900));
        assert_eq!(budget, Duration::from_secs(3600));
        assert_eq!(max_age(Duration::from_secs(60)), MIN_MAX_AGE);
        let at = |secs_ago: i64| Some((now - chrono::Duration::seconds(secs_ago)).naive_utc());
        assert!(pass_verdict(at(0), now, budget).is_ok());
        assert!(pass_verdict(at(3600), now, budget).is_ok());
        let why = pass_verdict(at(3601), now, budget).unwrap_err();
        assert!(
            why.contains("3601 s old") && why.contains("3600 s"),
            "{why}"
        );
        let why = pass_verdict(None, now, budget).unwrap_err();
        assert!(why.contains("no complete pass"), "{why}");
        assert!(
            pass_verdict(at(-30), now, budget).is_ok(),
            "a clock a little ahead is fresh"
        );
    }

    #[test]
    fn a_pass_is_in_progress_between_its_start_and_its_completion() {
        let mut s = MirrorState::fresh("https://m.example/v1/trades");
        assert!(!s.in_progress());
        let t0 = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .naive_utc();
        s.started_at = Some(t0);
        assert!(s.in_progress());
        s.completed_at = Some(t0 + chrono::Duration::seconds(5));
        assert!(!s.in_progress());
        s.started_at = Some(t0 + chrono::Duration::seconds(10));
        assert!(
            s.in_progress(),
            "a newer start than the last completion is a running pass"
        );
    }

    #[tokio::test]
    async fn a_private_upstream_is_refused_before_any_request() {
        let state = crate::market::tests::support::state_without_db();
        let err = fetch_json(
            &state,
            "http://127.0.0.1:1/v1/trades",
            BODY_CAP,
            REQUEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(err.contains("not a publicly routable"), "{err}");
    }
}
