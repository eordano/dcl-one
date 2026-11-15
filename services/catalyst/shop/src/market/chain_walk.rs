//! The walk the marketplace chain indexes share (`nft`, `trade_events`):
//! `eth_getLogs` through the land_authz client over one window at a time,
//! every filter group's address list split in chunks the endpoint accepts,
//! the window's logs and the cursor advance committed together by the
//! caller together with the cursor hash, a request budget per pass (the backpressure: a catch-up is many
//! passes, each leaving a durable `last_block`), Polygon milestone finality
//! and Ethereum confirmation depth, a reorg check of the cursor block's hash at the start of
//! every pass, and the publish marker (`folded_block`) set only when the
//! index reaches the head within the publish lag.
//!
//! `shop.chain_cursor` is one row per (indexer, network). A cursor
//! created at the indexer's start block records `full_history`; one started
//! at `MARKETPLACE_INDEX_START_BLOCK_*` (a bounded development or
//! measurement window) never publishes, and neither does one held short by
//! `MARKETPLACE_INDEX_STOP_BLOCK_*`. Readers trust an index through
//! [`is_ready`]: `folded_block = last_block` and `folded_at` at most
//! [`READY_WINDOW`] old at the caller's clock.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use crate::chain_index::rpc::{
    BlockHeader, Rpc, RpcError as IndexError, RpcLog, hex_to_i64, is_address_limit,
};

pub(crate) fn topic_address(topic: &str) -> Option<String> {
    let hex = topic.strip_prefix("0x")?;
    (hex.len() == 64).then(|| format!("0x{}", &hex[24..]).to_ascii_lowercase())
}

pub(crate) fn topic_u256(topic: &str) -> Option<String> {
    alloy_primitives::U256::from_str_radix(topic.strip_prefix("0x")?, 16)
        .ok()
        .map(|v| v.to_string())
}

pub const INDEXER_MANA: &str = "mana_transfers";

pub const INDEXER_NFT: &str = "nft";
pub const INDEXER_SALES: &str = "sales";
pub const INDEXER_TRADE_EVENTS: &str = "trade_events";

/// How old a publish may be before readers stop trusting it (a stopped job).
pub const READY_WINDOW: ChronoDuration = ChronoDuration::minutes(30);

/// The two chains the marketplace indexes walk; the `network` column's spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChainNet {
    Ethereum,
    Matic,
}

impl ChainNet {
    pub const ALL: [ChainNet; 2] = [ChainNet::Ethereum, ChainNet::Matic];

    pub fn db_name(self) -> &'static str {
        match self {
            ChainNet::Ethereum => "ethereum",
            ChainNet::Matic => "matic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ethereum" | "mainnet" => Some(ChainNet::Ethereum),
            "matic" | "polygon" => Some(ChainNet::Matic),
            _ => None,
        }
    }

    pub fn chain_id(self) -> i64 {
        match self {
            ChainNet::Ethereum => 1,
            ChainNet::Matic => 137,
        }
    }

    /// Blocks the walk stays behind the head: a reorg shallower than this
    /// never reaches the log (Ethereum finalizes in two epochs; Polygon's
    /// milestones finalize in seconds, its historical reorgs ran deeper).
    pub fn confirmations(self) -> u64 {
        match self {
            ChainNet::Ethereum => 12,
            ChainNet::Matic => 64,
        }
    }

    /// A publish needs `head - last_block <= confirmations + publish_lag`
    /// at the moment the fold committed.
    pub fn publish_lag(self) -> u64 {
        match self {
            ChainNet::Ethereum => 25,
            ChainNet::Matic => 150,
        }
    }

    /// The fewest milliseconds two blocks are apart on average over a span:
    /// `block_at_or_before` never lands after a timestamp when it divides
    /// by this.
    pub fn min_block_ms(self) -> u64 {
        match self {
            ChainNet::Ethereum => 1_000,
            ChainNet::Matic => 500,
        }
    }

    /// The most milliseconds two blocks are apart on average over a span:
    /// `time_at_most` never lands before the real block time with it (a
    /// longer halt is caught by the late-contract check with exact times).
    pub fn max_block_ms(self) -> u64 {
        match self {
            ChainNet::Ethereum => 30_000,
            ChainNet::Matic => 6_000,
        }
    }
}

/// The indexes' configuration (`MarketSettings::indexes`); every default
/// leaves the indexers off.
#[derive(Clone, Debug)]
pub struct IndexSettings {
    /// `MARKETPLACE_NFT_INDEX_ENABLED`.
    pub nft_enabled: bool,
    pub mana_transfers_enabled: bool,
    /// `MARKETPLACE_TRADE_EVENTS_INDEX_ENABLED`.
    pub trade_events_enabled: bool,
    /// `MARKETPLACE_SALES_INDEX_ENABLED`.
    pub sales_enabled: bool,
    /// `MARKETPLACE_INDEX_INTERVAL_SECS` (5): pause between passes.
    pub interval: Duration,
    /// `MARKETPLACE_INDEX_REQUESTS_PER_PASS` (100): `eth_getLogs` calls one
    /// network's pass may make before it yields with its cursor durable.
    pub requests_per_pass: usize,
    /// `MARKETPLACE_INDEX_WINDOW_BLOCKS` (10,000): the widest window asked
    /// for (the strictest public backend's limit).
    pub window_blocks: u64,
    /// `MARKETPLACE_INDEX_ADDRESS_CHUNK` (1,000): addresses per request; an
    /// endpoint refusing the count halves it for the rest of the pass.
    pub address_chunk: usize,
    /// `MARKETPLACE_INDEX_START_BLOCK_{ETHEREUM,MATIC}`: begin a fresh cursor
    /// here instead of the contracts' deployment (never published).
    pub start_block_ethereum: Option<u64>,
    pub start_block_matic: Option<u64>,
    /// `MARKETPLACE_INDEX_STOP_BLOCK_{ETHEREUM,MATIC}`: index no further (never published).
    pub stop_block_ethereum: Option<u64>,
    pub stop_block_matic: Option<u64>,
    /// `MARKETPLACE_NEIGHBOURS_INTERVAL_SECS` (21,600): between two rebuilds of the item neighbours.
    pub neighbours_interval: Duration,
    /// `MARKETPLACE_COUPON_REFRESH_INTERVAL_SECS` (300): between two coupon state reads.
    pub coupon_refresh_interval: Duration,
    /// `MARKETPLACE_INDEX_CHAIN_ID_{ETHEREUM,MATIC}` (1, 137): the chain the
    /// trade-events walk of each network reads, its mainnet or its testnet
    /// (11155111, 80002). The other indexes read mainnet only.
    pub chain_id_ethereum: i64,
    pub chain_id_matic: i64,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            nft_enabled: false,
            mana_transfers_enabled: false,
            trade_events_enabled: false,
            sales_enabled: false,
            interval: Duration::from_secs(5),
            requests_per_pass: 100,
            window_blocks: 10_000,
            address_chunk: 1_000,
            start_block_ethereum: None,
            start_block_matic: None,
            stop_block_ethereum: None,
            stop_block_matic: None,
            neighbours_interval: Duration::from_secs(6 * 3600),
            coupon_refresh_interval: Duration::from_secs(300),
            chain_id_ethereum: ChainNet::Ethereum.chain_id(),
            chain_id_matic: ChainNet::Matic.chain_id(),
        }
    }
}

impl IndexSettings {
    /// The chain the trade-events walk of `net` reads.
    pub fn chain_id(&self, net: ChainNet) -> i64 {
        match net {
            ChainNet::Ethereum => self.chain_id_ethereum,
            ChainNet::Matic => self.chain_id_matic,
        }
    }

    /// Refuses a chain that is neither the network's mainnet nor its
    /// testnet: the walk would read another network's contracts.
    pub fn check_chains(&self) -> Result<(), String> {
        for (net, allowed, var) in [
            (
                ChainNet::Ethereum,
                [1, 11_155_111],
                "MARKETPLACE_INDEX_CHAIN_ID_ETHEREUM",
            ),
            (
                ChainNet::Matic,
                [137, 80_002],
                "MARKETPLACE_INDEX_CHAIN_ID_MATIC",
            ),
        ] {
            let id = self.chain_id(net);
            if !allowed.contains(&id) {
                return Err(format!(
                    "{var}={id}: {} indexes chain {} or {}",
                    net.db_name(),
                    allowed[0],
                    allowed[1]
                ));
            }
        }
        Ok(())
    }

    pub fn start_override(&self, net: ChainNet) -> Option<u64> {
        match net {
            ChainNet::Ethereum => self.start_block_ethereum,
            ChainNet::Matic => self.start_block_matic,
        }
    }

    pub fn stop_block(&self, net: ChainNet) -> Option<u64> {
        match net {
            ChainNet::Ethereum => self.stop_block_ethereum,
            ChainNet::Matic => self.stop_block_matic,
        }
    }
}

/// `shop.chain_cursor`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Cursor {
    pub indexer: String,
    pub network: String,
    pub start_block: i64,
    pub last_block: i64,
    pub last_block_hash: Option<String>,
    pub last_block_time: Option<i64>,
    /// The block the last fold covered, published or not: the next fold's `since`.
    pub fold_through: Option<i64>,
    pub folded_block: Option<i64>,
    pub folded_at: Option<NaiveDateTime>,
    pub full_history: bool,
    pub reorg_rewinds: i32,
    pub updated_at: NaiveDateTime,
}

const CURSOR_COLUMNS: &str = "indexer, network, start_block, last_block, last_block_hash, last_block_time, fold_through, folded_block, folded_at, full_history, reorg_rewinds, updated_at";

/// Runs `ANALYZE <table>` in the fold's transaction only when the fold needs
/// statistics the table does not have yet: a rebuild (`since` -1), a table
/// never analyzed, or rows changed since the last analyze past 5 % of the
/// table (at least 1,000): a catch-up window's bulk insert that autoanalyze
/// has not seen (the land_authz fold misplanned such a window for 169 s
/// without statistics). A steady-state fold leaves the table to autovacuum:
/// on a 3M-row `nft_event` the ANALYZE was 790 to 807 ms of a 797 to 819 ms
/// incremental fold, twice a minute, and its ShareUpdateExclusiveLock held
/// to commit cancelled autovacuum on the table (2026-09-25 review). `table`
/// is a schema-qualified name from code. Returns whether it analyzed.
pub async fn analyze_if_stale(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
    rebuild: bool,
) -> Result<bool, sqlx::Error> {
    let stale: bool = rebuild
        || sqlx::query_scalar(
            "SELECT COALESCE((
                 SELECT (s.last_analyze IS NULL AND s.last_autoanalyze IS NULL)
                     OR s.n_mod_since_analyze >= GREATEST(1000, 0.05 * GREATEST(c.reltuples, 0))
                 FROM pg_catalog.pg_stat_user_tables s JOIN pg_catalog.pg_class c ON c.oid = s.relid
                 WHERE s.schemaname = $1 AND s.relname = $2), true)",
        )
        .bind(schema)
        .bind(table)
        .fetch_one(&mut *conn)
        .await?;
    if stale {
        sqlx::query(sqlx::AssertSqlSafe(format!("ANALYZE {schema}.{table}")))
            .execute(&mut *conn)
            .await?;
    }
    Ok(stale)
}

pub async fn cursor<'e, E>(
    executor: E,
    indexer: &str,
    net: ChainNet,
) -> Result<Option<Cursor>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CURSOR_COLUMNS} FROM shop.chain_cursor WHERE indexer = $1 AND network = $2"
    )))
    .bind(indexer)
    .bind(net.db_name())
    .fetch_optional(executor)
    .await
}

/// The row a fresh database starts from (`last_block = start - 1`); an
/// existing row is left alone, whatever start this process was given.
pub async fn ensure_cursor(
    pool: &PgPool,
    indexer: &str,
    net: ChainNet,
    start_block: u64,
    full_history: bool,
) -> anyhow::Result<Cursor> {
    sqlx::query(
        "INSERT INTO shop.chain_cursor (indexer, network, start_block, last_block, full_history) \
         VALUES ($1, $2, $3, $3 - 1, $4) ON CONFLICT (indexer, network) DO NOTHING",
    )
    .bind(indexer)
    .bind(net.db_name())
    .bind(start_block as i64)
    .bind(full_history)
    .execute(pool)
    .await?;
    let row = cursor(pool, indexer, net)
        .await?
        .context("the chain cursor row vanished after its insert")?;
    if row.start_block != start_block as i64 {
        tracing::info!(
            indexer,
            network = net.db_name(),
            stored = row.start_block,
            configured = start_block,
            "chain cursor keeps its stored start block"
        );
    }
    Ok(row)
}

/// Moves `last_block` from `expected` to `to` in the caller's transaction
/// (the one that wrote the window's rows); a cursor someone else moved
/// fails the transaction instead of skipping or repeating a range.
pub async fn advance(
    conn: &mut PgConnection,
    indexer: &str,
    net: ChainNet,
    expected: i64,
    header: &BlockHeader,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE shop.chain_cursor SET last_block = $4, last_block_hash = $5, last_block_time = $6, \
             updated_at = timezone('UTC', now()) \
         WHERE indexer = $1 AND network = $2 AND last_block = $3 AND last_block < $4",
    )
    .bind(indexer)
    .bind(net.db_name())
    .bind(expected)
    .bind(header.number as i64)
    .bind(&header.hash)
    .bind(header.timestamp)
    .execute(conn)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Rewinds after a reorg deeper than the confirmation depth, in the
/// caller's transaction (which also deletes the log past `to`): the publish
/// marker goes with it, so readers fall back until the refold publishes.
pub async fn rewind(
    conn: &mut PgConnection,
    indexer: &str,
    net: ChainNet,
    to: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE shop.chain_cursor SET last_block = LEAST(last_block, $3), last_block_hash = NULL, last_block_time = NULL, \
             fold_through = NULL, folded_block = NULL, folded_at = NULL, reorg_rewinds = reorg_rewinds + 1, updated_at = timezone('UTC', now()) \
         WHERE indexer = $1 AND network = $2",
    )
    .bind(indexer)
    .bind(net.db_name())
    .bind(to)
    .execute(conn)
    .await?;
    Ok(())
}

/// Records the block a completed fold covered (the next fold's `since`),
/// on the row still at that block; a publish is [`mark_folded`]'s.
pub async fn record_fold(
    pool: &PgPool,
    indexer: &str,
    net: ChainNet,
    block: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE shop.chain_cursor SET fold_through = $3 WHERE indexer = $1 AND network = $2 AND last_block = $3")
        .bind(indexer)
        .bind(net.db_name())
        .bind(block)
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets `folded_block` to the block a completed fold covered, only on the
/// row still at that block and with its whole history walked.
pub async fn mark_folded(
    pool: &PgPool,
    indexer: &str,
    net: ChainNet,
    block: i64,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE shop.chain_cursor SET folded_block = last_block, folded_at = $4 \
         WHERE indexer = $1 AND network = $2 AND last_block = $3 AND full_history AND last_block_hash IS NOT NULL AND fold_through = $3",
    )
    .bind(indexer)
    .bind(net.db_name())
    .bind(block)
    .bind(now.naive_utc())
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// The readiness rule, at the caller's clock.
pub fn is_ready(c: &Cursor, now: DateTime<Utc>) -> bool {
    let Some(folded_at) = c.folded_at else {
        return false;
    };
    let age = now.naive_utc().signed_duration_since(folded_at);
    c.full_history
        && c.folded_block == Some(c.last_block)
        && age >= ChronoDuration::zero()
        && age <= READY_WINDOW
}

/// The same rule as SQL over a `shop.chain_cursor` alias, for
/// statements that decide per row with a bound `now` (`now_p`).
pub fn ready_sql(alias: &str, now_p: &str) -> String {
    format!(
        "({alias}.full_history AND {alias}.folded_block = {alias}.last_block \
          AND {alias}.folded_at <= {now_p} AND {alias}.folded_at >= {now_p} - interval '30 minutes')"
    )
}

/// `Some(cursor)` when `indexer` may answer for `net` at `now`; a database
/// error reads as not ready (the envelope, never a guess).
pub async fn ready_cursor(
    pool: &PgPool,
    indexer: &str,
    net: ChainNet,
    now: DateTime<Utc>,
) -> Option<Cursor> {
    match cursor(pool, indexer, net).await {
        Ok(Some(c)) if is_ready(&c, now) => Some(c),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, indexer, network = net.db_name(), "cannot read shop.chain_cursor; treating the index as not ready");
            None
        }
    }
}

/// One `eth_getLogs` filter: these addresses (split in chunks) and this
/// `topics` array. The groups of one walk name disjoint addresses.
#[derive(Clone, Debug)]
pub struct LogGroup {
    pub addresses: Vec<String>,
    pub topics: Value,
}

/// What one window fetch returned.
#[derive(Debug, Default)]
pub struct Window {
    pub logs: Vec<RpcLog>,
    /// Every log of `[from, reached]` of every group is in `logs`.
    pub reached: u64,
    pub requests: usize,
}

pub fn log_position(log: &RpcLog) -> (i64, i64) {
    (
        hex_to_i64(&log.block_number).unwrap_or(i64::MAX),
        hex_to_i64(&log.log_index).unwrap_or(i64::MAX),
    )
}

/// The requests one window will need at the current chunk size.
pub fn requests_for(groups: &[LogGroup], chunk: usize) -> usize {
    groups
        .iter()
        .map(|g| g.addresses.len().div_ceil(chunk.max(1)))
        .sum()
}

/// Every log of `[from, reached]` (some `reached <= to` every request
/// accepted) of every group: a request that narrows the range narrows the
/// window for all of them (the logs past it are dropped and re-read next
/// window), and an address-count refusal halves `chunk` and repeats.
pub async fn fetch_window(
    rpc: &Rpc<'_>,
    from: u64,
    to: u64,
    groups: &[LogGroup],
    chunk: &mut usize,
) -> Result<Window, IndexError> {
    let mut out = Window {
        logs: Vec::new(),
        reached: to,
        requests: 0,
    };
    for group in groups {
        let mut i = 0usize;
        while i < group.addresses.len() {
            let end = (i + (*chunk).max(1)).min(group.addresses.len());
            let slice = &group.addresses[i..end];
            let address = if slice.len() == 1 {
                json!(slice[0])
            } else {
                json!(slice)
            };
            match rpc
                .get_logs_matching(from, out.reached, &address, &group.topics)
                .await
            {
                Ok((logs, upper, used)) => {
                    if upper < from
                        || upper > out.reached
                        || logs.iter().any(|l| {
                            let (block, index) = log_position(l);
                            block < from as i64
                                || block > upper as i64
                                || index < 0
                                || index > i32::MAX as i64
                        })
                    {
                        return Err(IndexError::Rpc(
                            "invalid log window bounds or position".into(),
                        ));
                    }
                    out.requests += used;
                    if upper < out.reached {
                        out.reached = upper;
                        let cut = upper as i64;
                        out.logs.retain(|l| log_position(l).0 <= cut);
                    }
                    let cut = out.reached as i64;
                    out.logs
                        .extend(logs.into_iter().filter(|l| log_position(l).0 <= cut));
                    i = end;
                }
                Err(IndexError::Rpc(message)) if is_address_limit(&message) && *chunk > 1 => {
                    out.requests += 1;
                    *chunk = (*chunk / 2).max(1);
                    tracing::info!(
                        chunk = *chunk,
                        "marketplace index: the endpoint refused the address count; halving the chunk"
                    );
                }
                Err(e) => return Err(e),
            }
        }
    }
    out.logs.sort_by_key(log_position);
    out.logs.dedup_by(|a, b| log_position(a) == log_position(b));
    Ok(out)
}

/// The block time of every log: the endpoint's `blockTimestamp` when it sent
/// one, else the headers of the distinct blocks without one, fetched in
/// JSON-RPC batches (`Rpc::block_headers`; the requests are counted).
pub async fn block_times(
    rpc: &Rpc<'_>,
    logs: &[RpcLog],
) -> Result<(HashMap<i64, i64>, usize), IndexError> {
    let mut times: HashMap<i64, i64> = HashMap::new();
    let mut missing: BTreeSet<i64> = BTreeSet::new();
    for log in logs {
        let block = log_position(log).0;
        match log.block_timestamp.as_deref().and_then(hex_to_i64) {
            Some(t) => {
                times.insert(block, t);
            }
            None => {
                missing.insert(block);
            }
        }
    }
    let wanted: Vec<u64> = missing
        .into_iter()
        .filter(|b| !times.contains_key(b))
        .map(|b| b as u64)
        .collect();
    if wanted.is_empty() {
        return Ok((times, 0));
    }
    let (headers, requests) = rpc.block_headers(&wanted).await?;
    for (number, header) in headers {
        times.insert(number as i64, header.timestamp);
    }
    Ok((times, requests))
}

/// The newest block whose time is surely at or before `t`, from an exact
/// anchor at or after it: `anchor.number - (anchor.timestamp - t) /
/// min_block`, never later than the real one.
pub fn block_at_or_before(net: ChainNet, anchor: (u64, i64), t: i64) -> u64 {
    let (block, time) = anchor;
    if t >= time {
        return block;
    }
    let blocks = ((time - t) as u128 * 1000).div_ceil(net.min_block_ms() as u128) as u64;
    block.saturating_sub(blocks)
}

/// A time the chain surely had not passed at `block`, from an exact anchor
/// at or before it: `anchor.timestamp + (block - anchor.number) * max_block`.
pub fn time_at_most(net: ChainNet, anchor: (u64, i64), block: u64) -> i64 {
    let (at, time) = anchor;
    time + (block.saturating_sub(at) as i64).saturating_mul(net.max_block_ms() as i64) / 1000
}

/// A pass's outcome, for the job trace, the tests and the measurement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassOutcome {
    pub network: &'static str,
    pub head: u64,
    pub from_block: i64,
    pub indexed_to: i64,
    pub requests: usize,
    pub logs: usize,
    pub rows_written: usize,
    /// The budget ran out (or shutdown came) before the target.
    pub paused: bool,
    pub reorg_rewound_to: Option<i64>,
    pub folded: bool,
    pub published: bool,
    pub elapsed_ms: u64,
}

impl PassOutcome {
    pub fn blocks(&self) -> i64 {
        (self.indexed_to - self.from_block + 1).max(0)
    }

    /// `blocks/s` and `rows/s` of the pass, the catch-up measure.
    pub fn rates(&self) -> (f64, f64) {
        let secs = (self.elapsed_ms.max(1) as f64) / 1000.0;
        (self.blocks() as f64 / secs, self.rows_written as f64 / secs)
    }

    pub fn summary(&self) -> String {
        let (bps, rps) = self.rates();
        format!(
            "{}: blocks {}..{} of head {} ({} requests, {} logs, {} rows, {:.0} blocks/s, {:.0} rows/s){}{}{}",
            self.network,
            self.from_block,
            self.indexed_to,
            self.head,
            self.requests,
            self.logs,
            self.rows_written,
            bps,
            rps,
            if self.paused {
                "; paused at the budget"
            } else {
                ""
            },
            if self.folded { "; folded" } else { "" },
            if self.published { "; published" } else { "" },
        )
    }
}

/// Clamp the chain's safe head to an optional operator stop block.
pub fn target_block(safe: u64, stop: Option<u64>) -> u64 {
    stop.map_or(safe, |s| s.min(safe))
}

/// Whether a fold at `indexed` may be published against a head read after it.
pub fn within_publish_lag(net: ChainNet, head: u64, indexed: u64) -> bool {
    head.saturating_sub(indexed) <= net.confirmations() + net.publish_lag()
}

/// One indexer's side of the walk: where it starts, what it asks for, how
/// it writes a window, how it folds, and how it undoes a reorg.
#[async_trait::async_trait]
pub trait ChainIndex: Send + Sync {
    fn indexer(&self) -> &'static str;
    fn net(&self) -> ChainNet;
    /// The first block a full history reads (the contracts' deployment).
    fn start_block(&self) -> u64;
    /// The filter groups of window `[from, to]`; `anchor` is an exact
    /// (block, time) at or before `from` for time-bounded address sets.
    async fn groups(
        &self,
        pool: &PgPool,
        from: u64,
        to: u64,
        anchor: (u64, i64),
    ) -> anyhow::Result<Vec<LogGroup>>;
    /// Decodes and inserts one window's logs in the cursor's transaction;
    /// the rows actually written (duplicates dropped).
    async fn write(
        &self,
        conn: &mut PgConnection,
        logs: &[RpcLog],
        times: &HashMap<i64, i64>,
    ) -> anyhow::Result<usize>;
    /// Deletes the log past `to` and refolds what it touched, in the
    /// transaction that rewinds the cursor.
    async fn rewind(&self, conn: &mut PgConnection, to: i64) -> anyhow::Result<()>;
    /// Folds the keys touched past `since` (`None`: rebuild) up to `upto`;
    /// whether the result may be published (nothing still owed).
    async fn fold(&self, pool: &PgPool, since: Option<i64>, upto: i64) -> anyhow::Result<bool>;
    /// Work walked before the main range under the same budget (the nft
    /// backfill lane); the requests it spent.
    async fn lanes(
        &self,
        _pool: &PgPool,
        _rpc: &Rpc<'_>,
        _anchor: (u64, i64),
        _budget: usize,
        _chunk: &mut usize,
    ) -> anyhow::Result<usize> {
        Ok(0)
    }
}

/// One pass of `ix` over `net`: the reorg check, the backfill lanes, the
/// windows from the cursor to the confirmed head under the request budget
/// (each committed with its cursor advance), the cursor block's header,
/// then, once caught up, the fold and the publish.
pub async fn run_pass(
    pool: &PgPool,
    rpc: &Rpc<'_>,
    ix: &dyn ChainIndex,
    settings: &IndexSettings,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    progress: &(dyn Fn(String) + Send + Sync),
) -> anyhow::Result<PassOutcome> {
    let started = std::time::Instant::now();
    let net = ix.net();
    let name = ix.indexer();
    let start = settings.start_override(net).unwrap_or(ix.start_block());
    let full_history = start <= ix.start_block();
    let cur = ensure_cursor(pool, name, net, start, full_history).await?;
    let mut out = PassOutcome {
        network: net.db_name(),
        from_block: cur.last_block + 1,
        indexed_to: cur.last_block,
        ..Default::default()
    };
    if cur.last_block >= cur.start_block {
        let stored = cur.last_block_hash.as_deref().unwrap_or("");
        let header = rpc
            .block_header(cur.last_block as u64)
            .await
            .context("eth_getBlockByNumber at the cursor")?;
        out.requests += 1;
        if header.hash != stored {
            let to = cur.start_block - 1;
            let mut tx = pool.begin().await?;
            let replay =
                super::notice_cursor::plan_replay(&mut tx, name, net, to, cur.last_block).await?;
            ix.rewind(&mut tx, to)
                .await
                .context("undoing the reorged range")?;
            rewind(&mut tx, name, net, to).await?;
            // Cursor rows are locked after the deletes: producers wait only for the commit.
            if let Some(replay) = replay {
                replay.apply(&mut tx).await?;
            }
            tx.commit().await?;
            tracing::warn!(indexer = name, network = net.db_name(), block = cur.last_block, stored, chain = %header.hash, rewound_to = to, "marketplace index: the cursor block's hash changed (a reorg past the confirmation depth); rewound");
            out.reorg_rewound_to = Some(to);
            out.elapsed_ms = started.elapsed().as_millis() as u64;
            return Ok(out);
        }
    }
    let head = rpc.head_block().await.context("eth_blockNumber")?;
    out.requests += 1;
    out.head = head;
    let stop = settings.stop_block(net);
    let safe_head = match net {
        ChainNet::Matic => {
            out.requests += 1;
            rpc.finalized_header()
                .await
                .context("Polygon finalized head")?
                .number
        }
        ChainNet::Ethereum => head.saturating_sub(net.confirmations()),
    };
    anyhow::ensure!(safe_head <= head, "safe head exceeds latest head");
    let target = target_block(safe_head, stop);
    let anchor = match cur.last_block_time {
        Some(t) => (cur.last_block.max(0) as u64, t),
        None => {
            let h = rpc
                .block_header(cur.last_block.max(0) as u64)
                .await
                .context("the time anchor")?;
            out.requests += 1;
            (h.number, h.timestamp)
        }
    };
    let budget = settings.requests_per_pass.max(1);
    let mut chunk = settings.address_chunk.max(1);
    let mut log_requests = ix.lanes(pool, rpc, anchor, budget, &mut chunk).await?;
    out.requests += log_requests;
    let mut next = (cur.last_block + 1) as u64;
    let mut expected = cur.last_block;
    while next <= target {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            out.paused = true;
            break;
        }
        let to = (next + settings.window_blocks.max(1) - 1).min(target);
        let groups = ix.groups(pool, next, to, anchor).await?;
        let need = requests_for(&groups, chunk).max(1);
        if log_requests + need > budget && log_requests > 0 {
            out.paused = true;
            break;
        }
        let before = rpc.block_header(to).await?;
        out.requests += 1;
        let window = fetch_window(rpc, next, to, &groups, &mut chunk)
            .await
            .with_context(|| format!("eth_getLogs [{next}, {to}]"))?;
        out.requests += window.requests;
        log_requests += window.requests;
        let (times, extra) = block_times(rpc, &window.logs)
            .await
            .context("block times")?;
        out.requests += extra;
        let after = rpc.block_header(to).await?;
        out.requests += 1;
        anyhow::ensure!(before == after, "chain changed while fetching window");
        let header = if window.reached == to {
            after
        } else {
            out.requests += 1;
            rpc.block_header(window.reached).await?
        };
        let mut tx = pool.begin().await?;
        let written = ix.write(&mut tx, &window.logs, &times).await?;
        advance(&mut tx, name, net, expected, &header)
            .await
            .context("advancing the cursor")?;
        tx.commit().await?;
        out.logs += window.logs.len();
        out.rows_written += written;
        out.indexed_to = window.reached as i64;
        expected = window.reached as i64;
        next = window.reached + 1;
        progress(format!(
            "{} {}: indexed to {} of {} ({} requests this pass)",
            name,
            net.db_name(),
            window.reached,
            target,
            out.requests
        ));
    }
    if out.paused || (out.indexed_to as u64) < target {
        out.elapsed_ms = started.elapsed().as_millis() as u64;
        return Ok(out);
    }
    let may_publish = ix
        .fold(pool, cur.fold_through, out.indexed_to)
        .await
        .context("folding")?;
    record_fold(pool, name, net, out.indexed_to).await?;
    out.folded = true;
    let latest = rpc
        .head_block()
        .await
        .context("eth_blockNumber after the fold")?;
    out.requests += 1;
    let latest_safe = match net {
        ChainNet::Matic => {
            out.requests += 1;
            rpc.finalized_header()
                .await
                .context("Polygon finalized head after fold")?
                .number
        }
        ChainNet::Ethereum => latest.saturating_sub(net.confirmations()),
    };
    let held_short = stop.is_some_and(|s| s < latest_safe);
    if may_publish
        && !held_short
        && out.indexed_to >= 0
        && out.indexed_to as u64 <= latest_safe
        && within_publish_lag(net, latest, out.indexed_to as u64)
    {
        out.published = mark_folded(pool, name, net, out.indexed_to, Utc::now()).await?;
    } else {
        sqlx::query("UPDATE shop.chain_cursor SET folded_block = NULL, folded_at = NULL WHERE indexer = $1 AND network = $2 AND last_block = $3")
            .bind(name).bind(net.db_name()).bind(out.indexed_to).execute(pool).await?;
    }
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_topic_decoders_preserve_prefix_width_and_integer_boundaries() {
        let address = format!("0x{}{}", "F".repeat(24), "AB".repeat(20));
        assert_eq!(
            super::topic_address(&address),
            Some(format!("0x{}", "ab".repeat(20)))
        );
        assert_eq!(super::topic_address(&address[2..]), None);
        assert_eq!(super::topic_address("0x01"), None);
        assert_eq!(super::topic_u256("0x0"), Some("0".into()));
        assert_eq!(super::topic_u256("0xFF"), Some("255".into()));
        assert_eq!(
            super::topic_u256(&format!("0x{}", "f".repeat(64))),
            Some(alloy_primitives::U256::MAX.to_string())
        );
        assert_eq!(super::topic_u256("0x"), Some("0".into()));
        for invalid in ["ff", "0xgg"] {
            assert_eq!(super::topic_u256(invalid), None);
        }
        assert_eq!(super::topic_u256(&format!("0x1{}", "0".repeat(64))), None);
    }

    use super::*;

    #[test]
    fn each_network_indexes_its_mainnet_or_its_testnet_only() {
        let defaults = IndexSettings::default();
        assert_eq!(
            (
                defaults.chain_id(ChainNet::Ethereum),
                defaults.chain_id(ChainNet::Matic)
            ),
            (1, 137)
        );
        assert!(defaults.check_chains().is_ok());
        let testnet = IndexSettings {
            chain_id_ethereum: 11_155_111,
            chain_id_matic: 80_002,
            ..defaults.clone()
        };
        assert!(testnet.check_chains().is_ok());
        for (ethereum, matic) in [(137, 137), (1, 1), (1, 80_001), (80_002, 80_002), (1, 0)] {
            let wrong = IndexSettings {
                chain_id_ethereum: ethereum,
                chain_id_matic: matic,
                ..defaults.clone()
            };
            assert!(wrong.check_chains().is_err(), "{ethereum} / {matic}");
        }
    }

    fn cursor_row(last: i64, folded: Option<i64>, at: Option<DateTime<Utc>>, full: bool) -> Cursor {
        Cursor {
            indexer: INDEXER_NFT.into(),
            network: "matic".into(),
            start_block: 1,
            last_block: last,
            last_block_hash: None,
            last_block_time: None,
            fold_through: folded,
            folded_block: folded,
            folded_at: at.map(|t| t.naive_utc()),
            full_history: full,
            reorg_rewinds: 0,
            updated_at: Utc::now().naive_utc(),
        }
    }

    #[test]
    fn readiness_needs_a_whole_history_folded_at_its_cursor_and_recently() {
        let now = Utc::now();
        assert!(is_ready(
            &cursor_row(100, Some(100), Some(now - ChronoDuration::minutes(5)), true),
            now
        ));
        assert!(
            !is_ready(
                &cursor_row(
                    100,
                    Some(100),
                    Some(now - ChronoDuration::minutes(5)),
                    false
                ),
                now
            ),
            "a bounded window never publishes"
        );
        assert!(!is_ready(&cursor_row(101, Some(100), Some(now), true), now));
        assert!(!is_ready(
            &cursor_row(
                100,
                Some(100),
                Some(now - ChronoDuration::minutes(31)),
                true
            ),
            now
        ));
        assert!(!is_ready(&cursor_row(100, None, None, true), now));
        assert!(!is_ready(
            &cursor_row(100, Some(100), Some(now + ChronoDuration::seconds(5)), true),
            now
        ));
    }

    #[test]
    fn the_time_bounds_are_conservative_both_ways() {
        // Polygon at 2 s a block: 3,600 s before the anchor is 1,800 blocks back;
        // the lower bound divides by 0.5 s and goes 7,200 back.
        let anchor = (1_000_000u64, 1_700_000_000i64);
        assert_eq!(
            block_at_or_before(ChainNet::Matic, anchor, 1_700_000_000 - 3_600),
            1_000_000 - 7_200
        );
        assert_eq!(
            block_at_or_before(ChainNet::Matic, anchor, 1_800_000_000),
            1_000_000,
            "a time after the anchor is the anchor"
        );
        assert_eq!(
            block_at_or_before(ChainNet::Matic, (10, 1_000), 0),
            0,
            "never below genesis"
        );
        // 1,000 blocks after the anchor are at most 6,000 s later on Polygon.
        assert_eq!(
            time_at_most(ChainNet::Matic, anchor, 1_001_000),
            1_700_000_000 + 6_000
        );
        assert_eq!(
            time_at_most(ChainNet::Ethereum, anchor, 1_000_100),
            1_700_000_000 + 3_000
        );
    }

    #[test]
    fn the_target_keeps_the_confirmation_depth_and_the_stop_block() {
        assert_eq!(target_block(1_000, None), 1_000);
        assert_eq!(target_block(1_000, Some(500)), 500);
        assert_eq!(target_block(0, None), 0);
        assert!(within_publish_lag(ChainNet::Ethereum, 1_000, 963));
        assert!(!within_publish_lag(ChainNet::Ethereum, 1_000, 962));
    }

    #[test]
    fn the_ready_sql_is_the_rust_rule() {
        let sql = ready_sql("c", "$9");
        assert!(
            sql.contains("c.full_history")
                && sql.contains("c.folded_block = c.last_block")
                && sql.contains("interval '30 minutes'"),
            "{sql}"
        );
        assert_eq!(
            READY_WINDOW,
            ChronoDuration::minutes(30),
            "ready_sql spells the window out"
        );
    }

    #[test]
    fn a_window_needs_one_request_per_address_chunk_per_group() {
        let g = |n: usize| LogGroup {
            addresses: (0..n).map(|i| format!("0x{i:040x}")).collect(),
            topics: json!([]),
        };
        assert_eq!(requests_for(&[g(2_500), g(2)], 1_000), 4);
        assert_eq!(requests_for(&[g(0)], 1_000), 0);
    }

    #[test]
    fn rates_divide_by_the_elapsed_time() {
        let o = PassOutcome {
            network: "matic",
            from_block: 101,
            indexed_to: 200,
            rows_written: 50,
            elapsed_ms: 2_000,
            ..Default::default()
        };
        assert_eq!(o.blocks(), 100);
        assert_eq!(o.rates(), (50.0, 25.0));
        assert!(o.summary().contains("50 blocks/s"), "{}", o.summary());
    }

    /// A rebuild or a never-analyzed table analyzes; a steady-state fold over
    /// a table whose statistics are current does not.
    #[tokio::test]
    async fn a_fold_analyzes_only_on_a_rebuild_or_stale_statistics() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let schema = format!("an_{}", uuid::Uuid::new_v4().simple());
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.ev (n int); INSERT INTO {schema}.ev SELECT generate_series(1, 5000)")))
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        assert!(
            super::analyze_if_stale(&mut conn, &schema, "ev", false)
                .await
                .unwrap(),
            "never analyzed"
        );
        assert!(
            crate::shared::test_wait::until(async || {
                sqlx::query_scalar("SELECT last_analyze IS NOT NULL FROM pg_stat_user_tables WHERE schemaname = $1 AND relname = 'ev'")
                    .bind(&schema)
                    .fetch_one(&mut *conn)
                    .await
                    .unwrap()
            })
            .await,
            "the ANALYZE never showed in pg_stat_user_tables"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {schema}.ev SELECT generate_series(1, 10)"
        )))
        .execute(&mut *conn)
        .await
        .unwrap();
        assert!(
            !super::analyze_if_stale(&mut conn, &schema, "ev", false)
                .await
                .unwrap(),
            "10 new rows in 5,010: the steady state keeps autovacuum's statistics"
        );
        assert!(
            super::analyze_if_stale(&mut conn, &schema, "ev", true)
                .await
                .unwrap(),
            "a rebuild always analyzes"
        );
        drop(conn);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }
}
