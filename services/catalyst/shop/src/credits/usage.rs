//! The CreditUsed usage index: the one fresh chain index of the credits
//! module (`credits.usages`, `credits.usage_index_cursor`). The old crate's
//! reconciler joined a `credit_usages` table the
//! squid was meant to fill (it never did: the squid kept CreditUsed only as
//! a cross-chain order detail), so a pledge minted by `POST
//! /credits/authorize` was never debited; that is why the lane stayed off.
//!
//! One `jobs::spawn_periodic` pass (`credits-usage-index`), the
//! `land_authz` pattern:
//! - `eth_getLogs` over `CREDITS_MANAGER_CONTRACT` and the CreditUsed topic
//!   on the collections chain (`RPC_ENDPOINT_POLYGON`, `COLLECTIONS_CHAIN_ID`)
//!   through the shared JSON-RPC log client (`chain_index::rpc::Rpc`,
//!   over the ONE guarded client), up to the chain's `finalized` block
//!   (Polygon milestones: a few blocks behind the head; an endpoint without
//!   the tag gets head - [`FALLBACK_CONFIRMATIONS`]), so a reorg never
//!   inserts a log that later vanishes. The client narrows a refused window
//!   (a `[from, to]` hint, "exceeds limit of N", else halving); this module
//!   remembers the width the endpoint took, so the next window asks for it
//!   directly, and doubles it after a success up to that learned ceiling.
//! - each accepted window's rows and the cursor advance commit in ONE
//!   transaction (`ON CONFLICT DO NOTHING` on `(chain_id, block_number,
//!   log_index)`), so a `kill -9` re-reads at most one window and adds no
//!   duplicate; a pass makes at most `CREDITS_USAGE_INDEX_REQUESTS_PER_PASS`
//!   requests and resumes from the durable `last_block` next time.
//! - the fold, deterministic over the log (`settlement::settle`): every
//!   CreditUsed log in `(folded_block, target]` whose salt is a credit this
//!   node signed becomes one ledger effect, keyed by the log
//!   (`credits.usage_settlements`): the first log of a salt debits the
//!   group's charged cents and moves the authorization to `consumed`
//!   whatever it was (`retired`, `released` or `expired` too: a cancel or an
//!   expiry is not proof a signed credit went unused), a later log debits
//!   the remainder it spent at the credit's oracle rate. A replay finds the
//!   settlement row and moves nothing; a debit the balance cannot cover
//!   rolls back and is reported; the reconciler's full pass retries it.
//! - the publish: `folded_block = last_block = target` with the target's
//!   chain time, only when the finalized head moved at most
//!   [`HEAD_LAG_BLOCKS`] during the pass and no stop block holds the index
//!   short. Readers trust it through [`index_ready`]: folded at the last
//!   indexed block, at most [`READY_WINDOW`] ago.
//!
//! Consumers: `POST /credits/authorize` mints a pledge only while the index
//! is ready (503 envelope naming this source otherwise), and the expiry
//! sweep never expires a pledge whose `expires_at` is past the index's
//! `folded_block_time` (the reservation holds until the chain has been read
//! past the credit's expiry).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use alloy_primitives::U256;
use anyhow::{Context, bail};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use super::errors::ApiError;
use super::reconcile::CreditUsageDiff;
use super::store::Credits;
pub use crate::chain_index::rpc::{Rpc, RpcLog};
use crate::chain_index::rpc::{RpcError, hex_to_i64};
use crate::http::Outbound;
use crate::jobs::JobContext;

pub const JOB_NAME: &str = "credits-usage-index";
/// The source a refusal names, and the variable that attaches the job.
pub const SOURCE: &str = "credits-usage-index";
pub const CONFIGURE: &str = "CREDITS_USAGE_INDEX_ENABLED";

/// `keccak256("CreditUsed(address,bytes32,(uint256,uint256,bytes32),uint256)")`.
pub const CREDIT_USED_TOPIC: &str =
    "0xa68d6ae15d7c3b8ca4d13dbccb4762b825753f5bbafab66b88d893af0c1b7179";

pub const CHAIN_POLYGON: i64 = 137;
pub const CHAIN_AMOY: i64 = 80_002;
/// The first block the upstream credits index reads on each chain
/// (decentraland/credits-squid-core `FROM_BLOCK`): before both managers.
pub const START_BLOCK_POLYGON: u64 = 70_459_461;
pub const START_BLOCK_AMOY: u64 = 20_612_932;

/// How old a published fold may be before the authorize lane closes again.
pub const READY_WINDOW: ChronoDuration = ChronoDuration::minutes(10);
/// Publish only when the finalized head moved at most this far during the
/// pass (about five minutes of Polygon blocks).
pub const HEAD_LAG_BLOCKS: u64 = 150;
/// The confirmation depth used when the endpoint does not know `finalized`.
pub const FALLBACK_CONFIRMATIONS: u64 = 256;
/// The first window width: what the free Polygon endpoints accept.
const INITIAL_SPAN: u64 = 10_000;
const MAX_SPAN: u64 = 500_000;
const INSERT_CHUNK: usize = 1_000;

/// The index's configuration, filled by `credits::Settings::parse`.
#[derive(Clone)]
pub struct Settings {
    /// `CREDITS_USAGE_INDEX_ENABLED` (false).
    pub enabled: bool,
    /// `RPC_ENDPOINT_POLYGON`: https to a public host; never logged (gateway keys sit in the path).
    pub rpc_url: Option<String>,
    /// `COLLECTIONS_CHAIN_ID` (137): the cursor row's key, checked against `eth_chainId`.
    pub chain_id: i64,
    /// `CREDITS_MANAGER_CONTRACT`, lowercase.
    pub contract: Option<String>,
    /// `CREDITS_USAGE_START_BLOCK`, else the chain's known start.
    pub start_block: u64,
    /// `CREDITS_USAGE_INDEX_INTERVAL_SECS` (30).
    pub interval: Duration,
    /// `CREDITS_USAGE_INDEX_REQUESTS_PER_PASS` (50).
    pub requests_per_pass: usize,
    /// `CREDITS_USAGE_REPLAY_FROM_BLOCK`: rewind durably once at the first pass.
    pub replay_from_block: Option<u64>,
    /// `CREDITS_USAGE_STOP_BLOCK`: index no further (a development bound; never published).
    pub stop_block: Option<u64>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("usage::Settings")
            .field("enabled", &self.enabled)
            .field("rpc_url", &self.rpc_url.as_ref().map(|_| "<set>"))
            .field("chain_id", &self.chain_id)
            .field("contract", &self.contract)
            .field("start_block", &self.start_block)
            .field("interval", &self.interval)
            .field("requests_per_pass", &self.requests_per_pass)
            .field("replay_from_block", &self.replay_from_block)
            .field("stop_block", &self.stop_block)
            .finish()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            rpc_url: None,
            chain_id: CHAIN_POLYGON,
            contract: None,
            start_block: START_BLOCK_POLYGON,
            interval: Duration::from_secs(30),
            requests_per_pass: 50,
            replay_from_block: None,
            stop_block: None,
        }
    }
}

impl Settings {
    /// Where a full replay starts on `chain_id` when no start block is configured.
    pub fn default_start_block(chain_id: i64) -> u64 {
        match chain_id {
            CHAIN_POLYGON => START_BLOCK_POLYGON,
            CHAIN_AMOY => START_BLOCK_AMOY,
            _ => 0,
        }
    }

    /// The job runs (and the sweep waits for it) only with all three.
    pub fn is_configured(&self) -> bool {
        self.enabled && self.rpc_url.is_some() && self.contract.is_some()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("rpc answer unusable: {0}")]
    Malformed(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("could not decode the log at block {block} index {index}: {detail}")]
    Decode {
        block: i64,
        index: i32,
        detail: String,
    },
}

/// One decoded CreditUsed log, every uint256 as its decimal string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub block_number: i64,
    pub log_index: i32,
    pub contract: String,
    pub tx_hash: String,
    pub block_time: Option<NaiveDateTime>,
    pub sender: String,
    pub credit_id: String,
    pub salt: String,
    pub credit_value: String,
    pub credit_expires_at: String,
    pub value_wei: String,
}

fn hex_u64(s: &str) -> Option<u64> {
    hex_to_i64(s.trim()).and_then(|n| u64::try_from(n).ok())
}

fn is_word(s: &str) -> bool {
    s.len() == 66 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A 0x-prefixed 20-byte address, lowercased; `None` for anything else.
pub fn normalize_address(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    is_address(&lower).then_some(lower)
}

pub fn decode_log(log: &RpcLog) -> Result<Usage, String> {
    let block_number = hex_u64(&log.block_number)
        .ok_or_else(|| format!("bad blockNumber {}", log.block_number))?
        as i64;
    let log_index =
        hex_u64(&log.log_index).ok_or_else(|| format!("bad logIndex {}", log.log_index))? as i32;
    let topics: Vec<String> = log.topics.iter().map(|t| t.to_ascii_lowercase()).collect();
    if topics.len() != 3 || topics[0] != CREDIT_USED_TOPIC {
        return Err(format!("not a CreditUsed log ({} topics)", topics.len()));
    }
    if !is_word(&topics[1]) || !is_word(&topics[2]) || !topics[1][2..26].bytes().all(|b| b == b'0')
    {
        return Err("malformed indexed topics".to_string());
    }
    let data = log
        .data
        .trim()
        .trim_start_matches("0x")
        .to_ascii_lowercase();
    if data.len() != 4 * 64 || !data.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "data carries {} hex digits, want 256 (value, expiresAt, salt, _value)",
            data.len()
        ));
    }
    let word = |i: usize| &data[i * 64..(i + 1) * 64];
    let uint = |i: usize| -> String {
        let bytes = alloy_primitives::hex::decode(word(i)).expect("checked hex");
        U256::from_be_slice(&bytes).to_string()
    };
    let contract = normalize_address(&log.address)
        .ok_or_else(|| format!("bad log address {}", log.address))?;
    let tx_hash = log
        .transaction_hash
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|h| is_word(h))
        .ok_or_else(|| "missing or malformed transactionHash".to_string())?;
    let block_time = log
        .block_timestamp
        .as_deref()
        .and_then(hex_u64)
        .and_then(|t| DateTime::from_timestamp(t as i64, 0))
        .map(|t| t.naive_utc());
    Ok(Usage {
        block_number,
        log_index,
        contract,
        tx_hash,
        block_time,
        sender: format!("0x{}", &topics[1][26..]),
        credit_id: topics[2].clone(),
        salt: format!("0x{}", word(2)),
        credit_value: uint(0),
        credit_expires_at: uint(1),
        value_wei: uint(3),
    })
}

/// A block number and its chain time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub number: u64,
    pub time: NaiveDateTime,
}

fn block_ref(number: u64, timestamp: i64) -> Result<BlockRef, IndexError> {
    DateTime::from_timestamp(timestamp, 0)
        .map(|t| BlockRef {
            number,
            time: t.naive_utc(),
        })
        .ok_or_else(|| IndexError::Malformed(format!("block {number} has timestamp {timestamp}")))
}

/// `eth_chainId` of the endpoint.
pub async fn chain_id(rpc: &Rpc<'_>) -> Result<i64, IndexError> {
    let v = rpc.call("eth_chainId", json!([])).await?;
    v.get("result")
        .and_then(Value::as_str)
        .and_then(hex_to_i64)
        .ok_or_else(|| IndexError::Malformed(format!("eth_chainId returned {v}")))
}

/// The chain's `finalized` block; an endpoint that refuses or does not know
/// the tag gets the head minus [`FALLBACK_CONFIRMATIONS`].
pub async fn finalized(rpc: &Rpc<'_>) -> Result<BlockRef, IndexError> {
    let v = rpc
        .call("eth_getBlockByNumber", json!(["finalized", false]))
        .await?;
    if let Some(block) = v.get("result").filter(|r| !r.is_null()) {
        let number = block
            .get("number")
            .and_then(Value::as_str)
            .and_then(hex_u64);
        let timestamp = block
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(hex_to_i64);
        return match (number, timestamp) {
            (Some(n), Some(t)) => block_ref(n, t),
            _ => Err(IndexError::Malformed(format!(
                "the finalized block has no number/timestamp: {block}"
            ))),
        };
    }
    let n = rpc
        .head_block()
        .await?
        .saturating_sub(FALLBACK_CONFIRMATIONS);
    block_at(rpc, n).await
}

/// One block's number and chain time.
pub async fn block_at(rpc: &Rpc<'_>, number: u64) -> Result<BlockRef, IndexError> {
    let header = rpc.block_header(number).await?;
    block_ref(number, header.timestamp)
}

/// `credits.usage_index_cursor`, one row per chain.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Cursor {
    pub chain_id: i64,
    pub contract: String,
    pub last_block: i64,
    pub folded_block: Option<i64>,
    pub folded_block_time: Option<NaiveDateTime>,
    pub folded_at: Option<NaiveDateTime>,
    pub full_replay_started: bool,
    pub updated_at: NaiveDateTime,
}

const CURSOR_COLUMNS: &str = "chain_id, contract, last_block, folded_block, folded_block_time, folded_at, full_replay_started, updated_at";

pub async fn cursor(pool: &PgPool, chain_id: i64) -> Result<Option<Cursor>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CURSOR_COLUMNS} FROM credits.usage_index_cursor WHERE chain_id = $1"
    )))
    .bind(chain_id)
    .fetch_optional(pool)
    .await
}

/// A fresh database starts at `start - 1` with the full replay recorded; an
/// existing row is kept, and one indexed from another manager is refused.
pub async fn ensure_cursor(
    pool: &PgPool,
    settings: &Settings,
    contract: &str,
) -> anyhow::Result<Cursor> {
    sqlx::query(
        "INSERT INTO credits.usage_index_cursor (chain_id, contract, last_block, full_replay_started) \
         VALUES ($1, $2, $3, true) ON CONFLICT (chain_id) DO NOTHING",
    )
    .bind(settings.chain_id)
    .bind(contract)
    .bind(settings.start_block as i64 - 1)
    .execute(pool)
    .await?;
    let row = cursor(pool, settings.chain_id)
        .await?
        .context("the usage cursor row vanished after its insert")?;
    if row.contract != contract {
        bail!(
            "credits.usage_index_cursor for chain {} was indexed from CreditsManager {} but this node is configured with {}: \
             the log would mix two managers; set CREDITS_USAGE_REPLAY_FROM_BLOCK={} to replay it from the start",
            settings.chain_id,
            row.contract,
            contract,
            settings.start_block
        );
    }
    Ok(row)
}

/// Advances the cursor of `contract`'s log. A cursor rebound to another
/// manager meanwhile is not this pass's: no row matches, and the window's
/// transaction rolls back.
pub async fn set_cursor(
    conn: &mut PgConnection,
    chain_id: i64,
    contract: &str,
    block: i64,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE credits.usage_index_cursor SET last_block = $2, updated_at = timezone('UTC', now()) \
         WHERE chain_id = $1 AND contract = $3",
    )
    .bind(chain_id)
    .bind(block)
    .bind(contract)
    .execute(conn)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Rewinds durably before a repair scan, and clears the publish marker at
/// once (the authorize lane closes until the repaired history is folded).
/// On the same manager `last_block` never moves forward, so no gap is
/// skipped. A rewind to the start block records the full replay
/// [`mark_folded`] requires and may switch managers: the new manager's log
/// then starts exactly at `from`, wherever the old one's cursor stood. A
/// partial rewind never touches another manager's cursor. Returns whether
/// the cursor row was written.
pub async fn begin_replay(
    pool: &PgPool,
    settings: &Settings,
    contract: &str,
    from: u64,
) -> Result<bool, sqlx::Error> {
    let full = from <= settings.start_block;
    let written = sqlx::query(
        "INSERT INTO credits.usage_index_cursor AS c (chain_id, contract, last_block, full_replay_started) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (chain_id) DO UPDATE SET \
             last_block = CASE WHEN c.contract = EXCLUDED.contract \
                 THEN LEAST(c.last_block, EXCLUDED.last_block) ELSE EXCLUDED.last_block END, \
             folded_block = NULL, folded_block_time = NULL, folded_at = NULL, \
             full_replay_started = c.full_replay_started OR EXCLUDED.full_replay_started, \
             contract = EXCLUDED.contract, \
             updated_at = timezone('UTC', now()) \
         WHERE c.contract = EXCLUDED.contract OR EXCLUDED.full_replay_started",
    )
    .bind(settings.chain_id)
    .bind(contract)
    .bind(from as i64 - 1)
    .bind(full)
    .execute(pool)
    .await?;
    Ok(written.rows_affected() == 1)
}

/// Publishes a completed fold on the row whose `last_block` is the block
/// the fold covered and whose history was replayed in full.
pub async fn mark_folded(
    pool: &PgPool,
    chain_id: i64,
    contract: &str,
    through: BlockRef,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE credits.usage_index_cursor SET folded_block = last_block, folded_block_time = $3, folded_at = $4 \
         WHERE chain_id = $1 AND last_block = $2 AND full_replay_started AND contract = $5",
    )
    .bind(chain_id)
    .bind(through.number as i64)
    .bind(through.time)
    .bind(now.naive_utc())
    .bind(contract)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// The readiness rule: the last indexed block is folded, and recently.
pub fn index_ready(cursor: &Cursor, now: DateTime<Utc>) -> bool {
    let Some(folded_at) = cursor.folded_at else {
        return false;
    };
    let age = now.naive_utc().signed_duration_since(folded_at);
    cursor.folded_block == Some(cursor.last_block)
        && age >= ChronoDuration::zero()
        && age <= READY_WINDOW
}

/// Appends `usages` (`ON CONFLICT DO NOTHING`); returns the rows written.
pub async fn insert_usages(
    conn: &mut PgConnection,
    chain_id: i64,
    usages: &[Usage],
) -> Result<usize, sqlx::Error> {
    let mut written = 0usize;
    for chunk in usages.chunks(INSERT_CHUNK) {
        let col = |f: fn(&Usage) -> String| chunk.iter().map(f).collect::<Vec<String>>();
        let blocks: Vec<i64> = chunk.iter().map(|u| u.block_number).collect();
        let indexes: Vec<i32> = chunk.iter().map(|u| u.log_index).collect();
        let times: Vec<Option<NaiveDateTime>> = chunk.iter().map(|u| u.block_time).collect();
        let result = sqlx::query(
            "INSERT INTO credits.usages \
                 (chain_id, block_number, log_index, contract, tx_hash, block_time, sender, credit_id, salt, \
                  credit_value, credit_expires_at, value_wei) \
             SELECT $1, b, i, c, tx, bt, s, cid, salt, cv::numeric, ce::numeric, v::numeric \
             FROM UNNEST($2::bigint[], $3::int[], $4::text[], $5::text[], $6::timestamp[], $7::text[], $8::text[], \
                         $9::text[], $10::text[], $11::text[], $12::text[]) \
                  AS u(b, i, c, tx, bt, s, cid, salt, cv, ce, v) \
             ON CONFLICT (chain_id, block_number, log_index) DO NOTHING",
        )
        .bind(chain_id)
        .bind(&blocks)
        .bind(&indexes)
        .bind(col(|u| u.contract.clone()))
        .bind(col(|u| u.tx_hash.clone()))
        .bind(&times)
        .bind(col(|u| u.sender.clone()))
        .bind(col(|u| u.credit_id.clone()))
        .bind(col(|u| u.salt.clone()))
        .bind(col(|u| u.credit_value.clone()))
        .bind(col(|u| u.credit_expires_at.clone()))
        .bind(col(|u| u.value_wei.clone()))
        .execute(&mut *conn)
        .await?;
        written += result.rows_affected() as usize;
    }
    Ok(written)
}

/// The settlement of `(since, through]`: every CreditUsed log of a credit
/// this node signed becomes a ledger effect exactly once
/// (`settlement::settle`; `since = -1` is the full pass).
pub async fn fold(
    credits: &Credits,
    chain_id: i64,
    contract: &str,
    since: i64,
    through: i64,
) -> Result<Vec<CreditUsageDiff>, ApiError> {
    super::settlement::settle(credits, chain_id, contract, since, through).await
}

/// What one pass did; the job trace and the tests read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassOutcome {
    pub target: u64,
    /// `last_block` when the pass returned.
    pub indexed_to: i64,
    pub requests: usize,
    pub usages_written: usize,
    /// Returned before the target: the request budget or a shutdown.
    pub paused: bool,
    pub consumed: usize,
    pub failed: usize,
    pub published: bool,
}

/// The index state: the ONE pool and client, the credits component the fold
/// debits through, and the process-local window width.
pub struct UsageIndex {
    pub pool: PgPool,
    pub http: Outbound,
    pub settings: Settings,
    pub credits: Credits,
    span: AtomicU64,
    ceiling: AtomicU64,
    replay_requested: AtomicBool,
    chain_checked: AtomicBool,
}

impl UsageIndex {
    pub fn new(credits: Credits, http: Outbound, settings: Settings) -> Self {
        Self {
            pool: credits.pool.clone(),
            http,
            settings,
            credits,
            span: AtomicU64::new(INITIAL_SPAN),
            ceiling: AtomicU64::new(MAX_SPAN),
            replay_requested: AtomicBool::new(false),
            chain_checked: AtomicBool::new(false),
        }
    }

    /// The cursor row when the authorize lane may mint at `now`; a
    /// database error reads as not ready (fail closed).
    pub async fn ready_at(&self, now: DateTime<Utc>) -> Option<Cursor> {
        if !self.settings.is_configured() {
            return None;
        }
        match cursor(&self.pool, self.settings.chain_id).await {
            Ok(Some(row))
                if index_ready(&row, now)
                    && Some(&row.contract) == self.settings.contract.as_ref() =>
            {
                Some(row)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "credits-usage-index: cannot read the cursor; the authorize lane stays closed");
                None
            }
        }
    }

    fn widen(&self) {
        let ceiling = self.ceiling.load(Ordering::Relaxed);
        let span = self.span.load(Ordering::Relaxed);
        self.span.store(
            span.saturating_mul(2).min(ceiling).max(1),
            Ordering::Relaxed,
        );
    }

    /// The endpoint took a narrower window than asked: that width is its ceiling.
    fn learn(&self, width: u64) {
        let width = width.max(1);
        self.ceiling.store(width, Ordering::Relaxed);
        self.span.store(width, Ordering::Relaxed);
    }

    /// Indexes `[from, to]` with at most `budget` requests, each accepted
    /// window committed with its cursor advance.
    #[allow(
        clippy::too_many_arguments,
        reason = "the range, its budget and the job/outcome sinks are independent inputs"
    )]
    pub async fn sync_range(
        &self,
        rpc: &Rpc<'_>,
        contract: &str,
        from: u64,
        to: u64,
        budget: usize,
        ctx: Option<&JobContext>,
        outcome: &mut PassOutcome,
    ) -> Result<(), IndexError> {
        let chain_id = self.settings.chain_id;
        let mut next = from;
        while next <= to {
            if ctx.is_some_and(|c| c.is_cancelled()) || outcome.requests >= budget {
                outcome.paused = true;
                return Ok(());
            }
            let span = self.span.load(Ordering::Relaxed).max(1);
            let upper = next.saturating_add(span - 1).min(to);
            let (logs, reached, used) = rpc
                .get_logs_matching(
                    next,
                    upper,
                    &json!([contract]),
                    &json!([[CREDIT_USED_TOPIC]]),
                )
                .await?;
            outcome.requests += used;
            if reached < upper {
                self.learn(reached - next + 1);
            } else {
                self.widen();
            }
            let upper = reached;
            let mut usages = Vec::with_capacity(logs.len());
            for log in &logs {
                let usage = decode_log(log).map_err(|detail| IndexError::Decode {
                    block: hex_u64(&log.block_number).unwrap_or(0) as i64,
                    index: hex_u64(&log.log_index).unwrap_or(0) as i32,
                    detail,
                })?;
                usages.push(usage);
            }
            let mut tx = self.pool.begin().await?;
            let written = insert_usages(&mut tx, chain_id, &usages).await?;
            set_cursor(&mut tx, chain_id, contract, upper as i64).await?;
            tx.commit().await?;
            outcome.usages_written += written;
            outcome.indexed_to = upper as i64;
            if !usages.is_empty() {
                tracing::info!(
                    chain_id,
                    from = next,
                    to = upper,
                    logs = usages.len(),
                    written,
                    "credits-usage-index: indexed range"
                );
            }
            if let Some(ctx) = ctx {
                ctx.progress(format!(
                    "indexed to block {upper} of {to} ({} requests this pass)",
                    outcome.requests
                ));
            }
            next = upper + 1;
        }
        Ok(())
    }

    /// The `credits-usage-index` job's pass ([`super::State::spawn_jobs`]):
    /// [`Self::run_pass`], folding only when `gate` lets this pass write. The
    /// gate is asked as the pass starts, so `OPS_WRITES_ENABLED` and the
    /// restore stamps the process installed decide each pass; a held pass
    /// reports why in the job's progress (the switch, or which stamp).
    pub(super) async fn gated_pass(
        &self,
        ctx: Option<&JobContext>,
        gate: &crate::shared::writes::PassGate,
        writes_enabled: bool,
    ) -> anyhow::Result<PassOutcome> {
        let refusal = gate.check(writes_enabled);
        let outcome = self.run_pass(ctx, refusal.is_none()).await?;
        if let (Some(refusal), Some(ctx)) = (&refusal, ctx) {
            ctx.progress(format!(
                "indexed to {}; not folded, not published: {}",
                outcome.indexed_to,
                refusal.message(
                    "operator-plane",
                    crate::shared::writes::WRITES_DISABLED_MESSAGE
                )
            ));
        }
        Ok(outcome)
    }

    /// One pass: replay if asked, catch up to the finalized block under the
    /// request budget, fold the new usages, publish when still at the head.
    /// With `fold_enabled` false (`OPS_WRITES_ENABLED` off or held by the
    /// restore stamps: the fold debits the ledger) the pass indexes the log
    /// and stops before the fold, so nothing is published and the authorize
    /// lane stays closed.
    pub(super) async fn run_pass(
        &self,
        ctx: Option<&JobContext>,
        fold_enabled: bool,
    ) -> anyhow::Result<PassOutcome> {
        let settings = &self.settings;
        let (Some(rpc_url), Some(contract)) =
            (settings.rpc_url.as_deref(), settings.contract.as_deref())
        else {
            bail!(
                "the credits usage index needs RPC_ENDPOINT_POLYGON and CREDITS_MANAGER_CONTRACT"
            );
        };
        let rpc = Rpc::at(&self.http, rpc_url);
        if !self.chain_checked.load(Ordering::Relaxed) {
            let chain = chain_id(&rpc).await.context("eth_chainId")?;
            if chain != settings.chain_id {
                bail!(
                    "RPC_ENDPOINT_POLYGON serves chain {chain} but COLLECTIONS_CHAIN_ID is {}: refusing to index another chain's logs",
                    settings.chain_id
                );
            }
            self.chain_checked.store(true, Ordering::Relaxed);
        }
        // The replay counts as done only once its rewind is durable: a failed
        // write leaves it requested, and the next pass tries again.
        let replay = settings
            .replay_from_block
            .filter(|_| !self.replay_requested.load(Ordering::SeqCst));
        // A full replay rebinds the cursor to this manager, so it runs before the
        // check that refuses a cursor indexed from another one; a partial replay
        // never rewinds another manager's cursor.
        let full = replay.filter(|&from| from <= settings.start_block);
        if let Some(from) = full {
            begin_replay(&self.pool, settings, contract, from)
                .await
                .context("beginning the replay")?;
            self.replay_requested.store(true, Ordering::SeqCst);
        }
        let mut row = ensure_cursor(&self.pool, settings, contract).await?;
        if let Some(from) = replay {
            if full.is_none() {
                let written = begin_replay(&self.pool, settings, contract, from)
                    .await
                    .context("beginning the replay")?;
                anyhow::ensure!(
                    written,
                    "the usage cursor was rebound to another manager during the replay"
                );
                self.replay_requested.store(true, Ordering::SeqCst);
            }
            row = cursor(&self.pool, settings.chain_id)
                .await?
                .context("cursor row after the replay rewind")?;
            tracing::warn!(
                chain_id = settings.chain_id,
                from,
                "credits-usage-index: replay started; the authorize lane is closed until it is folded"
            );
        }
        let final_block = finalized(&rpc).await.context("the finalized block")?;
        let target = match settings.stop_block {
            Some(stop) if stop < final_block.number => stop,
            _ => final_block.number,
        };
        let mut outcome = PassOutcome {
            target,
            indexed_to: row.last_block,
            ..Default::default()
        };
        let from = (row.last_block + 1) as u64;
        if from <= target {
            self.sync_range(
                &rpc,
                contract,
                from,
                target,
                settings.requests_per_pass,
                ctx,
                &mut outcome,
            )
            .await
            .context("eth_getLogs")?;
            if outcome.paused {
                return Ok(outcome);
            }
        }
        if !fold_enabled {
            if let Some(ctx) = ctx {
                ctx.progress(format!(
                    "indexed to {target}; operator-plane writes off or held: not folded, not published"
                ));
            }
            return Ok(outcome);
        }
        let since = row.folded_block.unwrap_or(-1).min(target as i64);
        let diffs = fold(
            &self.credits,
            settings.chain_id,
            contract,
            since,
            target as i64,
        )
        .await
        .map_err(|e| anyhow::anyhow!("folding the usages: {e}"))?;
        outcome.consumed = diffs.iter().filter(|d| d.consumed && !d.replayed).count();
        outcome.failed = diffs.iter().filter(|d| !d.consumed).count();
        if settings.stop_block.is_some_and(|s| s < final_block.number) {
            if let Some(ctx) = ctx {
                ctx.progress(format!("indexed to {target}: CREDITS_USAGE_STOP_BLOCK holds the index short, not published"));
            }
            return Ok(outcome);
        }
        let latest = finalized(&rpc)
            .await
            .context("the finalized block after the fold")?;
        if latest.number.saturating_sub(target) > HEAD_LAG_BLOCKS {
            if let Some(ctx) = ctx {
                ctx.progress(format!("folded at {target} but finalized moved to {}; publishing after the next catch-up", latest.number));
            }
            return Ok(outcome);
        }
        let through = if target == final_block.number {
            final_block
        } else {
            block_at(&rpc, target).await.context("the target block")?
        };
        outcome.published =
            mark_folded(&self.pool, settings.chain_id, contract, through, Utc::now())
                .await
                .context("publishing the fold")?;
        if !outcome.published {
            tracing::warn!(
                chain_id = settings.chain_id,
                target,
                "credits-usage-index: fold not published (cursor moved or no full replay recorded)"
            );
        }
        if let Some(ctx) = ctx {
            ctx.progress(format!(
                "folded at block {target}: {} usages written, {} pledges consumed, {} debits refused; published={}",
                outcome.usages_written, outcome.consumed, outcome.failed, outcome.published
            ));
        }
        Ok(outcome)
    }
}

/// The expiry bound of `credits.authorizations`: `$now`, or with the index
/// configured the chain time it has folded through (never a pledge whose
/// expiry the chain has not been read past; `-infinity` before a first
/// fold). Spliced into the sweep statements with the chain as `$chain`.
///
/// The fold only speaks for its own manager's log. A credit signed for
/// another manager (`credits.authorizations.contract`, kept from before a
/// manager switch) is bounded by `-infinity`: no sweep releases it, since
/// nothing reads the chain it could have been used on. It stays held until
/// it is settled against that manager's log or released by hand.
pub(crate) fn expiry_bound_sql(now_param: &str, chain_param: &str) -> String {
    format!(
        "LEAST({now_param}, CASE WHEN {chain_param}::bigint IS NULL THEN {now_param} ELSE \
         COALESCE((SELECT c.folded_block_time FROM credits.usage_index_cursor c WHERE c.chain_id = {chain_param}::bigint \
                   AND (credits.authorizations.contract IS NULL OR c.contract = credits.authorizations.contract)), \
                  '-infinity'::timestamp) END)"
    )
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    /// The CreditUsed log of block 0x5a034c4 on Polygon (manager
    /// 0x8b3a...8cc5), captured 2026-09-25 from rpc.decentraland.org/polygon.
    pub(crate) const CAPTURED_LOG: &str = r#"{
        "address": "0x8b3a40ca1b6f5cafc99d112a4d02e897d1fd8cc5",
        "topics": [
            "0xa68d6ae15d7c3b8ca4d13dbccb4762b825753f5bbafab66b88d893af0c1b7179",
            "0x000000000000000000000000f4ab0b827863352bb551027c385c9e5476fe2cf9",
            "0xd22b50d84aabdb817fb5ca9b6678122f3de586a6749a5a544fd04e97ca51a5fa"
        ],
        "data": "0x000000000000000000000000000000000000000000000001a055690d9db8000000000000000000000000000000000000000000000000000000000000f2a523807230758daa7ee19926df144b3171b155d9fe7e866ac8c56d1d3f53d485b5d65700000000000000000000000000000000000000000000000014d1120d7b160000",
        "blockNumber": "0x5a034c4",
        "logIndex": "0x83",
        "transactionHash": "0xcddf80dc3ecd13c96865beb66f47b317e8e71044b9d57bcdb005fd9be24ae6f1",
        "blockTimestamp": "0x6ab58d9a",
        "removed": false
    }"#;

    #[test]
    fn the_captured_log_decodes_salt_value_and_sender() {
        let log: RpcLog = serde_json::from_str(CAPTURED_LOG).unwrap();
        let u = decode_log(&log).unwrap();
        assert_eq!(u.block_number, 0x5a034c4);
        assert_eq!(u.log_index, 0x83);
        assert_eq!(u.sender, "0xf4ab0b827863352bb551027c385c9e5476fe2cf9");
        assert_eq!(
            u.credit_id,
            "0xd22b50d84aabdb817fb5ca9b6678122f3de586a6749a5a544fd04e97ca51a5fa"
        );
        assert_eq!(
            u.salt, "0x7230758daa7ee19926df144b3171b155d9fe7e866ac8c56d1d3f53d485b5d657",
            "the salt is the struct's, not the indexed creditId"
        );
        assert_eq!(u.credit_value, "30000000000000000000");
        assert_eq!(u.credit_expires_at, "4070908800");
        assert_eq!(
            u.value_wei, "1500000000000000000",
            "a partial use of a 30 MANA credit"
        );
        assert_eq!(u.block_time.unwrap().and_utc().timestamp(), 0x6ab58d9a);
    }

    #[test]
    fn a_foreign_or_short_log_is_refused() {
        let mut log: RpcLog = serde_json::from_str(CAPTURED_LOG).unwrap();
        log.data.truncate(130);
        assert!(decode_log(&log).unwrap_err().contains("256"));
        let mut log: RpcLog = serde_json::from_str(CAPTURED_LOG).unwrap();
        log.topics[0] = "0xbf0c0494baf6ed7e1481b0ec6d3ed75f70442f3ac8d509e54e80251640471373".into();
        assert!(decode_log(&log).is_err(), "CreditsUsed is another event");
        let mut log: RpcLog = serde_json::from_str(CAPTURED_LOG).unwrap();
        log.transaction_hash = None;
        assert!(decode_log(&log).is_err());
    }

    #[test]
    fn index_readiness_requires_a_complete_recent_fold() {
        let now = Utc::now();
        let row = |last: i64, folded: Option<i64>, at: Option<DateTime<Utc>>| Cursor {
            chain_id: CHAIN_POLYGON,
            contract: "0x8b3a40ca1b6f5cafc99d112a4d02e897d1fd8cc5".into(),
            last_block: last,
            folded_block: folded,
            folded_block_time: at.map(|t| t.naive_utc()),
            folded_at: at.map(|t| t.naive_utc()),
            full_replay_started: true,
            updated_at: now.naive_utc(),
        };
        assert!(!index_ready(&row(100, None, None), now));
        assert!(!index_ready(&row(101, Some(100), Some(now)), now));
        assert!(!index_ready(
            &row(100, Some(100), Some(now - ChronoDuration::minutes(11))),
            now
        ));
        assert!(!index_ready(
            &row(100, Some(100), Some(now + ChronoDuration::seconds(1))),
            now
        ));
        assert!(index_ready(
            &row(100, Some(100), Some(now - ChronoDuration::minutes(2))),
            now
        ));
    }

    #[test]
    fn settings_redact_the_rpc_url_and_know_each_chains_start() {
        let s = Settings {
            rpc_url: Some("https://rpc.example/v1/SECRET".into()),
            ..Settings::default()
        };
        assert!(!format!("{s:?}").contains("SECRET"));
        assert_eq!(
            Settings::default_start_block(CHAIN_POLYGON),
            START_BLOCK_POLYGON
        );
        assert_eq!(Settings::default_start_block(CHAIN_AMOY), START_BLOCK_AMOY);
        assert_eq!(Settings::default_start_block(1), 0);
        assert!(
            !s.is_configured(),
            "off until CREDITS_USAGE_INDEX_ENABLED and a contract"
        );
        assert_eq!(
            normalize_address(" 0x8B3A40CA1B6F5CAFC99D112A4D02E897D1FD8CC5 ").as_deref(),
            Some("0x8b3a40ca1b6f5cafc99d112a4d02e897d1fd8cc5")
        );
        assert_eq!(normalize_address("0x1234"), None);
    }
}
