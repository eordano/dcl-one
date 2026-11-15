//! The LAND authorization indexer as one `jobs::spawn_periodic` pass:
//! `eth_getLogs` over the two registries through the ONE client, appended to
//! `land_authz.authz_event` in `UNNEST` chunks with the cursor advanced in the
//! same transaction (a `kill -9` between passes re-fetches at most one range
//! and `ON CONFLICT DO NOTHING` drops the duplicates), a bounded number of
//! RPC requests per pass (the backpressure: a full replay is many passes,
//! each leaving a durable `last_block`), then the fold of the touched keys
//! into `token_right` / `estate_land` / `account_right` and, only when the
//! chain head is within [`HEAD_LAG_BLOCKS`] of what was indexed, the publish
//! marker `folded_block`. Readers trust the fold through [`index_ready`]:
//! `folded_block = last_block` and `folded_at` at most [`READY_WINDOW`] old,
//! the old server's rule.
//!
//! The fold is deterministic over the log (DATABASE.md rule 5): every leg is
//! decided by the single latest event that touches its key, so a rebuild is
//! the same three statements with `since = -1`; an incremental pass folds
//! only the keys an event past the previous `folded_block` touched, and
//! arrives at the same rows because each key's history is read whole.

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, bail};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use super::events::{ALL_TOPICS, AuthzEvent, DecodeError, decode_log};
use super::{KIND_ADD_LAND, KIND_REMOVE_LAND, LandAuthzState, Settings};
use crate::jobs::JobContext;

use crate::chain_index::rpc::{BlockHeader, HEADER_BATCH, RpcLog, hex_to_i64};
use crate::chain_index::rpc::{Rpc, RpcError};

impl From<RpcError> for IndexError {
    fn from(error: RpcError) -> Self {
        match error {
            RpcError::Budget { upper, requests } => Self::Budget { upper, requests },
            RpcError::Transport(message) => Self::Transport(message),
            RpcError::Rpc(message) => Self::Rpc(message),
        }
    }
}

/// Publish a fold only when the head has not moved further than this past
/// the indexed block: a fold of an old snapshot is not "current rights".
pub const HEAD_LAG_BLOCKS: u64 = 25;
/// How old a published fold may be before readers fall back (a stopped indexer).
pub const READY_WINDOW: ChronoDuration = ChronoDuration::minutes(30);
/// Rows per `INSERT ... UNNEST` statement.
const INSERT_CHUNK: usize = 1_000;
/// Chain/host clocks may differ slightly; authorization still uses strict chain time.
pub const HEADER_CLOCK_SKEW_SECONDS: i64 = 60;
/// Retry batch support after this many fallback fetches, without restarting.
pub const HEADER_BATCH_RETRY_FETCHES: usize = 4;
/// Repair cannot hold up the next live pass after publishing current rights.
pub const LEGACY_REPAIR_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("RPC request budget exhausted at range ending {upper}")]
    Budget { upper: u64, requests: usize },
    #[error("rpc transport: {0}")]
    Transport(String),
    #[error("rpc returned an error that carries no usable block range: {0}")]
    Rpc(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("could not decode a log at block {block} index {index}: {source}")]
    Decode {
        block: i64,
        index: i32,
        #[source]
        source: DecodeError,
    },
}

/// Resolve a bounded set within the remaining request budget. Some RPC
/// endpoints reject batches or omit individual responses. Keep their valid
/// answers, then use single requests for a bounded number of fetches before
/// probing batches again. Two-request passes still make progress between
/// probes, while transient failures cannot permanently disable batching.
async fn timestamp_headers(
    rpc: &Rpc<'_>,
    numbers: &[u64],
    singles: &AtomicUsize,
    budget: usize,
    requests: &mut usize,
) -> std::collections::HashMap<u64, BlockHeader> {
    let mut out = std::collections::HashMap::new();
    if numbers.is_empty() || budget == 0 {
        return out;
    }
    debug_assert!(numbers.len() <= HEADER_BATCH);
    let start = *requests;
    let retry_batch = singles
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
            remaining.checked_sub(1)
        })
        .is_err();
    if retry_batch {
        let body: Vec<Value> = numbers
            .iter()
            .enumerate()
            .map(|(i, n)| {
                crate::http::jsonrpc::request(
                    i as u64,
                    "eth_getBlockByNumber",
                    json!([format!("0x{n:x}"), false]),
                )
            })
            .collect();
        *requests += 1;
        match rpc.batch(&body).await {
            Ok(items) => {
                for item in items {
                    let Some(id) = item["id"]
                        .as_u64()
                        .and_then(|id| usize::try_from(id).ok())
                        .filter(|id| *id < numbers.len())
                    else {
                        continue;
                    };
                    if let Some(header) = timestamp_header(&item["result"], numbers[id]) {
                        out.insert(header.number, header);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "land-authz: timestamp batch unavailable; using budgeted single headers")
            }
        }
        if out.len() < numbers.len() {
            singles.store(HEADER_BATCH_RETRY_FETCHES, Ordering::SeqCst);
        }
    }
    for number in numbers {
        if out.contains_key(number) {
            continue;
        }
        if *requests - start >= budget {
            break;
        }
        *requests += 1;
        match rpc.block_header(*number).await {
            Ok(header) if valid_header_time(header.timestamp) => {
                out.insert(*number, header);
            }
            result => {
                tracing::warn!(
                    block = number,
                    ?result,
                    "land-authz: timestamp header unresolved; retaining verified prefix"
                );
                break;
            }
        }
    }
    out
}

pub fn decode_logs(logs: &[RpcLog]) -> Result<Vec<AuthzEvent>, IndexError> {
    let mut events = Vec::with_capacity(logs.len());
    for log in logs {
        let block = hex_to_i64(&log.block_number)
            .ok_or_else(|| IndexError::Rpc(format!("bad blockNumber {}", log.block_number)))?;
        let index = hex_to_i64(&log.log_index)
            .ok_or_else(|| IndexError::Rpc(format!("bad logIndex {}", log.log_index)))?
            as i32;
        let time = log
            .block_timestamp
            .as_deref()
            .and_then(hex_to_i64)
            .filter(|time| valid_header_time(*time))
            .ok_or_else(|| {
                IndexError::Rpc(format!("missing or invalid block timestamp at {block}"))
            })?;
        match decode_log(&log.address, &log.topics, &log.data, block, index, time) {
            Ok(ev) => events.push(ev),
            Err(DecodeError::UnhandledTopic(t)) => {
                tracing::warn!(topic = %t, block, index, "land-authz: ignoring a log this indexer does not model")
            }
            Err(source) => {
                return Err(IndexError::Decode {
                    block,
                    index,
                    source,
                });
            }
        }
    }
    Ok(events)
}

fn valid_header_time(time: i64) -> bool {
    time > 0 && time <= Utc::now().timestamp() + HEADER_CLOCK_SKEW_SECONDS
}

fn timestamp_header(block: &Value, expected: u64) -> Option<BlockHeader> {
    let number = block["number"]
        .as_str()
        .and_then(hex_to_i64)
        .filter(|n| *n >= 0)? as u64;
    if number != expected {
        return None;
    }
    let hash = block["hash"]
        .as_str()
        .filter(|h| {
            h.len() == 66 && h.starts_with("0x") && h[2..].bytes().all(|c| c.is_ascii_hexdigit())
        })?
        .to_ascii_lowercase();
    let timestamp = block["timestamp"]
        .as_str()
        .and_then(hex_to_i64)
        .filter(|t| valid_header_time(*t))?;
    Some(BlockHeader {
        number,
        hash,
        timestamp,
    })
}

/// Resolve missing times and return the first unresolved block. Only logs
/// strictly before it may advance the durable cursor. Matching the header hash
/// prevents combining logs from one fork with a same-height header from another.
async fn fill_missing_timestamps(
    state: &LandAuthzState,
    rpc: &Rpc<'_>,
    logs: &mut [RpcLog],
    budget: usize,
    requests: &mut usize,
) -> Result<Option<u64>, IndexError> {
    let blocks: std::collections::BTreeSet<u64> = logs
        .iter()
        .filter(|log| {
            log.block_timestamp
                .as_deref()
                .and_then(hex_to_i64)
                .is_none_or(|time| time <= 0)
        })
        .map(|log| {
            hex_to_i64(&log.block_number)
                .filter(|n| *n >= 0)
                .map(|n| n as u64)
                .ok_or_else(|| IndexError::Rpc("invalid block number for timestamp lookup".into()))
        })
        .collect::<Result<_, _>>()?;
    let numbers: Vec<u64> = blocks.into_iter().collect();
    let headers = timestamp_headers(
        rpc,
        &numbers,
        &state.single_timestamp_headers,
        budget,
        requests,
    )
    .await;
    let mut barrier = numbers
        .iter()
        .copied()
        .find(|number| !headers.contains_key(number));
    for log in logs {
        if let Some(header) = hex_to_i64(&log.block_number).and_then(|n| headers.get(&(n as u64))) {
            if !log
                .block_hash
                .as_deref()
                .is_some_and(|hash| hash.eq_ignore_ascii_case(&header.hash))
            {
                tracing::warn!(
                    block = header.number,
                    "land-authz: log/header hash mismatch; retaining verified prefix"
                );
                barrier = Some(barrier.map_or(header.number, |first| first.min(header.number)));
                continue;
            }
            log.block_timestamp = Some(format!("0x{:x}", header.timestamp));
        }
    }
    Ok(barrier)
}

/// Repair only this network's configured registries after live ingest. Apply
/// every resolved block durably, then take another bounded batch while budget
/// remains. A missing/failing header leaves the gate false without starving
/// current logs; an empty final due-set marks completion in the same pass.
async fn repair_timestamps(
    state: &LandAuthzState,
    rpc: &Rpc<'_>,
    budget: usize,
    requests: &mut usize,
) -> Result<(), IndexError> {
    let pool = &state.pool;
    let settings = &state.settings;
    let ready: bool = sqlx::query_scalar(
        "SELECT history_timestamps_ready FROM land_authz.index_cursor WHERE network = $1",
    )
    .bind(&settings.network)
    .fetch_one(pool)
    .await?;
    if ready {
        return Ok(());
    }
    let start = *requests;
    loop {
        let blocks: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT block_number FROM land_authz.authz_event WHERE block_time <= 0 AND token_address IN ($1, $2) ORDER BY block_number LIMIT $3")
            .bind(&settings.land_registry).bind(&settings.estate_registry).bind(HEADER_BATCH as i64).fetch_all(pool).await?;
        if blocks.is_empty() {
            sqlx::query("UPDATE land_authz.index_cursor SET history_timestamps_ready = true WHERE network = $1 AND land_registry = $2 AND estate_registry = $3")
                .bind(&settings.network).bind(&settings.land_registry).bind(&settings.estate_registry).execute(pool).await?;
            return Ok(());
        }
        if *requests - start >= budget {
            return Ok(());
        }
        let numbers = blocks
            .iter()
            .map(|n| {
                u64::try_from(*n).map_err(|_| IndexError::Rpc("negative historical block".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let headers = timestamp_headers(
            rpc,
            &numbers,
            &state.single_timestamp_headers,
            budget - (*requests - start),
            requests,
        )
        .await;
        if headers.is_empty() {
            return Ok(());
        }
        let repaired: Vec<i64> = numbers
            .iter()
            .filter(|n| headers.contains_key(n))
            .map(|n| *n as i64)
            .collect();
        let times: Vec<i64> = repaired
            .iter()
            .map(|n| headers[&(*n as u64)].timestamp)
            .collect();
        sqlx::query("UPDATE land_authz.authz_event e SET block_time = b.time FROM unnest($1::bigint[], $2::bigint[]) b(number, time) WHERE e.block_number = b.number AND e.block_time <= 0 AND e.token_address IN ($3, $4)")
            .bind(&repaired).bind(&times).bind(&settings.land_registry).bind(&settings.estate_registry).execute(pool).await?;
    }
}

/// One `UNNEST` chunk, column by column.
#[derive(Default)]
struct Columns {
    blocks: Vec<i64>,
    indexes: Vec<i32>,
    times: Vec<i64>,
    addrs: Vec<String>,
    kinds: Vec<String>,
    token_ids: Vec<Option<String>>,
    estate_ids: Vec<Option<String>>,
    accounts: Vec<Option<String>>,
    operators: Vec<Option<String>>,
    approvals: Vec<Option<bool>>,
}

/// Appends `events` (`ON CONFLICT (block_number, log_index) DO NOTHING`);
/// returns the rows actually written.
pub async fn insert_events(
    conn: &mut PgConnection,
    events: &[AuthzEvent],
) -> Result<usize, sqlx::Error> {
    let mut written = 0usize;
    for chunk in events.chunks(INSERT_CHUNK) {
        let mut c = Columns::default();
        for e in chunk {
            c.blocks.push(e.block_number);
            c.indexes.push(e.log_index);
            c.times.push(e.block_time);
            c.addrs.push(e.token_address.clone());
            c.kinds.push(e.kind.to_string());
            c.token_ids.push(e.token_id.clone());
            c.estate_ids.push(e.estate_id.clone());
            c.accounts.push(e.account.clone());
            c.operators.push(e.operator.clone());
            c.approvals.push(e.approved);
        }
        let result = sqlx::query(
            "INSERT INTO land_authz.authz_event \
                 (block_number, log_index, block_time, token_address, kind, token_id, estate_id, account, operator, approved) \
             SELECT b, i, t, a, k, tid::numeric, eid::numeric, acct, op, appr \
             FROM UNNEST($1::bigint[], $2::int[], $3::bigint[], $4::text[], $5::text[], \
                         $6::text[], $7::text[], $8::text[], $9::text[], $10::boolean[]) \
                  AS u(b, i, t, a, k, tid, eid, acct, op, appr) \
             ON CONFLICT (block_number, log_index) DO NOTHING",
        )
        .bind(&c.blocks)
        .bind(&c.indexes)
        .bind(&c.times)
        .bind(&c.addrs)
        .bind(&c.kinds)
        .bind(&c.token_ids)
        .bind(&c.estate_ids)
        .bind(&c.accounts)
        .bind(&c.operators)
        .bind(&c.approvals)
        .execute(&mut *conn)
        .await?;
        written += result.rows_affected() as usize;
    }
    Ok(written)
}

/// `land_authz.index_cursor`, one row per network.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Cursor {
    pub network: String,
    pub land_registry: String,
    pub estate_registry: String,
    pub last_block: i64,
    pub folded_block: Option<i64>,
    pub folded_at: Option<NaiveDateTime>,
    pub full_replay_started: bool,
    pub updated_at: NaiveDateTime,
}

const CURSOR_COLUMNS: &str = "network, land_registry, estate_registry, last_block, folded_block, folded_at, full_replay_started, updated_at";

pub async fn cursor(pool: &PgPool, network: &str) -> Result<Option<Cursor>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CURSOR_COLUMNS} FROM land_authz.index_cursor WHERE network = $1"
    )))
    .bind(network)
    .fetch_optional(pool)
    .await
}

/// Readiness observations share the database clock that writes folded_at.
/// Keeping both values in one query also makes the status age consistent with
/// its ready flag, without depending on an HTTP worker's local clock.
#[derive(Debug, sqlx::FromRow)]
pub struct CursorObservation {
    #[sqlx(flatten)]
    pub cursor: Cursor,
    pub observed_at: NaiveDateTime,
}

pub async fn cursor_observed<'e, E>(
    executor: E,
    network: &str,
) -> Result<Option<CursorObservation>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CURSOR_COLUMNS}, timezone('UTC', now()) AS observed_at FROM land_authz.index_cursor WHERE network = $1"
    ))).bind(network).fetch_optional(executor).await
}

/// A read of the local index at one snapshot: a `REPEATABLE READ, READ
/// ONLY` transaction whose first statement found the cursor ready for the
/// reader's registries ([`observation_ready`]). Every read run on `tx` sees
/// the database exactly as that check did. A full replay that rebinds the
/// cursor meanwhile, and the links its fold writes afterwards, stay
/// invisible to it, so a reader that checked before the rebind answers from
/// its registries' coherent state and never joins its rights to another
/// registry's estate links. It takes no lock: neither the rebind nor the
/// indexer waits on a reader.
pub struct ReadySnapshot {
    pub cursor: Cursor,
    pub tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

/// `Some` when the local index of these registries may answer, with the
/// snapshot every dependent read must run on ([`ReadySnapshot`]); `None`
/// when it may not (the transaction is rolled back).
pub async fn ready_snapshot(
    pool: &PgPool,
    network: &str,
    land_registry: &str,
    estate_registry: &str,
) -> Result<Option<ReadySnapshot>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(match cursor_observed(&mut *tx, network).await? {
        Some(observation) if observation_ready(&observation, land_registry, estate_registry) => {
            Some(ReadySnapshot {
                cursor: observation.cursor,
                tx,
            })
        }
        _ => None,
    })
}

/// The row a fresh database starts from: `last_block = start - 1` so the
/// first pass begins at the LAND registry's deployment block, and this IS
/// the full replay (`full_replay_started`). An existing row is left alone;
/// a new one is refused beside another network's cursor.
pub async fn ensure_cursor(pool: &PgPool, settings: &Settings) -> Result<Cursor, anyhow::Error> {
    if cursor(pool, &settings.network).await?.is_none() {
        // A new network's cursor is admitted only to a database holding no
        // other network's ([`admit_network`]).
        let mut tx = pool.begin().await?;
        if let Some(other) = admit_network(&mut tx, &settings.network)
            .await
            .context("admitting the land-authz cursor")?
        {
            return Err(shared_database(&settings.network, &other));
        }
        sqlx::query(
            "INSERT INTO land_authz.index_cursor (network, land_registry, estate_registry, last_block, full_replay_started) \
             VALUES ($1, $2, $3, $4, true) ON CONFLICT (network) DO NOTHING",
        )
        .bind(&settings.network)
        .bind(&settings.land_registry)
        .bind(&settings.estate_registry)
        .bind(settings.start_block as i64 - 1)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    let row = cursor(pool, &settings.network)
        .await?
        .context("the cursor row vanished after its insert")?;
    if row.land_registry != settings.land_registry
        || row.estate_registry != settings.estate_registry
    {
        bail!(
            "land_authz.index_cursor for {} was indexed from registries {}/{} but this node is configured with {}/{}: \
             the log would mix two contracts; set LAND_AUTHZ_REPLAY_FROM_BLOCK={} to replay it from the start, \
             and unset it once the replay has published (while it is set, every restart replays again)",
            settings.network,
            row.land_registry,
            row.estate_registry,
            settings.land_registry,
            settings.estate_registry,
            settings.start_block
        );
    }
    Ok(row)
}

/// Advances the cursor of the configured registries' log. A cursor rebound
/// to other registries meanwhile is not this pass's: no row matches, and the
/// range's transaction rolls back.
pub async fn set_cursor(
    conn: &mut PgConnection,
    settings: &Settings,
    block: i64,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE land_authz.index_cursor SET last_block = $2, updated_at = timezone('UTC', now()) \
         WHERE network = $1 AND land_registry = $3 AND estate_registry = $4",
    )
    .bind(&settings.network)
    .bind(block)
    .bind(&settings.land_registry)
    .bind(&settings.estate_registry)
    .execute(conn)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Rewinds durably before a repair scan, and clears the publish marker at
/// once (readers fall back to the subgraph until the repaired history is
/// folded at chain head). On the same registries `last_block` never moves
/// forward, so a later rewind cannot skip an earlier gap. A rewind to the
/// start block records the full replay [`mark_folded`] requires and may
/// switch registries: the new registries' log then starts exactly at `from`,
/// wherever the old cursor stood, and the history timestamps are owed again.
/// The old registries' events and rights are kept, since every reader names
/// its registry and the readiness gate requires the cursor to be of the
/// reader's registries ([`observation_ready`]), so a node still configured
/// with the old ones falls back instead of answering; the estate links are
/// not (a link does not record its
/// registry, so an old one would join a new estate of the same id), and the
/// replay's fold rebuilds them from the new estate registry alone. A partial
/// rewind never touches a cursor indexed from other registries.
///
/// Only a switch locks the cursor row explicitly (`FOR UPDATE`), the lock a
/// fold of the old registries holds while it writes their links
/// ([`fold_in_tx`]), so the links are deleted after that fold commits. The
/// switch waits at most [`REBIND_LOCK_TIMEOUT`] for it, and otherwise defers
/// to the next pass: a fold left running by a lost lease holds the row for
/// up to its own statement timeout, and failing the pass would pause the
/// indexer for its error backoff. A rewind on the same registries takes no
/// explicit lock and writes only columns no key covers, so it does not wait
/// on a fold's `FOR KEY SHARE` (its write still waits on another writer of
/// the row).
///
/// The database holds one network's index: `estate_land` is keyed by the
/// LAND token alone. Creating the cursor or switching it therefore happens
/// under [`admit_network`], as [`ensure_cursor`]'s creation does, and a
/// switch refuses to run beside another network's cursor, whose links it
/// would delete.
pub async fn begin_replay(
    pool: &PgPool,
    settings: &Settings,
    from: u64,
) -> Result<ReplayStart, sqlx::Error> {
    // A lock timeout anywhere in the replay's transaction (the admission,
    // the cursor row, or a write behind another writer) defers it: nothing
    // was committed, and the next pass tries again without the error backoff.
    match rewind_for_replay(pool, settings, from).await {
        Err(e) if lock_not_available(&e) => Ok(ReplayStart::Deferred),
        started => started,
    }
}

/// [`begin_replay`]'s transaction; a lock timeout comes back as the error.
async fn rewind_for_replay(
    pool: &PgPool,
    settings: &Settings,
    from: u64,
) -> Result<ReplayStart, sqlx::Error> {
    let before = from as i64 - 1;
    let full = from <= settings.start_block;
    let mut tx = pool.begin().await?;
    let previous: Option<(String, String)> = sqlx::query_as(
        "SELECT land_registry, estate_registry FROM land_authz.index_cursor WHERE network = $1",
    )
    .bind(&settings.network)
    .fetch_optional(&mut *tx)
    .await?;
    let switch = previous.as_ref().is_some_and(|(land, estate)| {
        *land != settings.land_registry || *estate != settings.estate_registry
    });
    if switch && !full {
        return Ok(ReplayStart::Refused);
    }
    let admitting = previous.is_none() || switch;
    if admitting && let Some(other) = admit_network(&mut tx, &settings.network).await? {
        return Ok(ReplayStart::SharedDatabase { other });
    }
    if switch {
        let locked: Option<(String, String)> = sqlx::query_as(
            "SELECT land_registry, estate_registry FROM land_authz.index_cursor WHERE network = $1 FOR UPDATE",
        )
        .bind(&settings.network)
        .fetch_optional(&mut *tx)
        .await?;
        // Another pass made this same switch while this one waited (two
        // nodes overlapping after a lost lease): the cursor is already of
        // these registries, rewound to their start, and its links are theirs,
        // so the replay is begun and this one writes nothing.
        if locked.is_some_and(|(land, estate)| {
            land == settings.land_registry && estate == settings.estate_registry
        }) {
            return Ok(ReplayStart::Begun);
        }
    }
    // Unadmitted, the row was read without a lock: the rewind writes it only
    // while it is still of these registries, so a switch committed since the
    // read is never undone without its links deleted, and it never creates
    // a cursor outside `admit_network`.
    let sql = if admitting {
        format!(
            "INSERT INTO land_authz.index_cursor AS c \
                 (network, land_registry, estate_registry, last_block, folded_block, folded_at, full_replay_started) \
             VALUES ($1, $2, $3, $4, NULL, NULL, $5) \
             ON CONFLICT (network) DO UPDATE SET {REWIND_SET} \
             WHERE $6 OR (c.land_registry = $2 AND c.estate_registry = $3)"
        )
    } else {
        format!(
            "UPDATE land_authz.index_cursor AS c SET {REWIND_SET} \
             WHERE c.network = $1 AND c.land_registry = $2 AND c.estate_registry = $3"
        )
    };
    let written = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&settings.network)
        .bind(&settings.land_registry)
        .bind(&settings.estate_registry)
        .bind(before)
        .bind(full)
        .bind(switch)
        .execute(&mut *tx)
        .await?;
    if written.rows_affected() != 1 {
        return Ok(ReplayStart::Deferred);
    }
    if switch {
        sqlx::query("DELETE FROM land_authz.estate_land")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(ReplayStart::Begun)
}

/// The rewind [`begin_replay`] writes over a cursor row `c`: `$4` the block
/// before `from`, `$5` whether it is a full replay, `$6` whether it switches
/// registries (`$2`, `$3`).
const REWIND_SET: &str = "last_block = CASE WHEN $6 THEN $4 ELSE LEAST(c.last_block, $4) END, \
     folded_block = NULL, folded_at = NULL, \
     full_replay_started = c.full_replay_started OR $5, \
     history_timestamps_ready = c.history_timestamps_ready AND NOT $6, \
     land_registry = $2, estate_registry = $3, \
     updated_at = timezone('UTC', now())";

/// What [`begin_replay`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayStart {
    /// The cursor was rewound, and on a switch rebound.
    Begun,
    /// A partial rewind over a cursor indexed from other registries: nothing
    /// was written.
    Refused,
    /// Nothing was written, and the next pass tries again: a lock was not
    /// granted within [`REBIND_LOCK_TIMEOUT`] (another pass held
    /// [`admit_network`]'s lock, a fold of the old registries held their
    /// cursor row, or another writer held the row being written), or the row
    /// was switched to other registries under an unadmitted rewind.
    Deferred,
    /// The database also holds `other`'s cursor, so a switch would delete
    /// that network's estate links too: nothing was written.
    SharedDatabase { other: String },
}

/// How long a cursor's creation or switch waits for [`admit_network`]'s lock,
/// and a switch for a fold of the old registries to commit (a PostgreSQL
/// interval, set as the transaction's `lock_timeout`).
pub const REBIND_LOCK_TIMEOUT: &str = "5s";

/// PostgreSQL's `lock_not_available`, raised when `lock_timeout` elapses.
const LOCK_NOT_AVAILABLE: &str = "55P03";

fn lock_not_available(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(e) if e.code().as_deref() == Some(LOCK_NOT_AVAILABLE))
}

/// The advisory lock that makes "one network's index per database" atomic.
const CURSOR_NETWORKS_LOCK: &str = "land-authz:cursor-networks";

/// Takes, until the transaction ends, the lock under which a network's
/// cursor is created ([`ensure_cursor`]) or rebound to other registries
/// ([`begin_replay`], which deletes the estate links), and answers the other
/// network whose cursor this database already holds, if any. Taken before
/// any cursor row lock, and waited for at most [`REBIND_LOCK_TIMEOUT`] (the
/// `lock_timeout` it sets stays for the rest of the transaction). A row lock
/// alone cannot do this: it protects a cursor that exists, not the absence
/// of another network's. Readers and folds never take it.
pub(crate) async fn admit_network(
    conn: &mut PgConnection,
    network: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(REBIND_LOCK_TIMEOUT)
        .execute(&mut *conn)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(CURSOR_NETWORKS_LOCK)
        .execute(&mut *conn)
        .await?;
    sqlx::query_scalar(
        "SELECT network FROM land_authz.index_cursor WHERE network <> $1 ORDER BY network LIMIT 1",
    )
    .bind(network)
    .fetch_optional(conn)
    .await
}

/// The error for a cursor that would join, or a switch that would delete the
/// links of, another network's index in the same database.
pub(crate) fn shared_database(network: &str, other: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the land-authz index of {other} shares this database: {network}'s index cannot be created or switched to other registries beside it (land_authz.estate_land holds one network's estate links); give each network its own database"
    )
}

/// Publishes a completed fold: only on the configured registries' row whose
/// `last_block` is the block the fold covered and whose history was
/// replayed in full. A failed or interrupted fold leaves the previous marker
/// (or none) behind.
pub async fn mark_folded(
    pool: &PgPool,
    settings: &Settings,
    head: u64,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE land_authz.index_cursor SET folded_block = last_block, folded_at = timezone('UTC', now()) \
         WHERE network = $1 AND last_block = $2 AND full_replay_started \
           AND land_registry = $3 AND estate_registry = $4",
    )
    .bind(&settings.network)
    .bind(head as i64)
    .bind(&settings.land_registry)
    .bind(&settings.estate_registry)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// The readiness rule: the latest event cursor has been folded and the fold
/// is recent. Production passes the database observation time; unit tests
/// can inject a clock without affecting production readiness authority.
pub fn index_ready(cursor: &Cursor, now: DateTime<Utc>) -> bool {
    let Some(folded_at) = cursor.folded_at else {
        return false;
    };
    let age = now.naive_utc().signed_duration_since(folded_at);
    cursor.folded_block == Some(cursor.last_block)
        && age >= ChronoDuration::zero()
        && age <= READY_WINDOW
}

/// Whether a reader of `land_registry` and `estate_registry` may answer from
/// the local index: the readiness rule ([`index_ready`]) on a cursor indexed
/// from exactly those registries. A full replay can rebind the network's
/// cursor to other registries ([`begin_replay`]); for a node still
/// configured with the old ones, that published cursor is not its index.
/// Its rights rows are of its own registries, while `estate_land` (which
/// names no registry) now follows the cursor's estate registry, so an answer
/// from both would mix two contracts.
pub fn observation_ready(
    observation: &CursorObservation,
    land_registry: &str,
    estate_registry: &str,
) -> bool {
    observation.cursor.land_registry == land_registry
        && observation.cursor.estate_registry == estate_registry
        && index_ready(&observation.cursor, observation.observed_at.and_utc())
}

/// `Some(cursor)` when the local index of these registries may answer on the
/// database clock ([`observation_ready`]). A database error reads as not
/// ready (the fallback, never a grant).
pub async fn ready_cursor(
    pool: &PgPool,
    network: &str,
    land_registry: &str,
    estate_registry: &str,
) -> Option<Cursor> {
    match cursor_observed(pool, network).await {
        Ok(Some(observation))
            if observation_ready(&observation, land_registry, estate_registry) =>
        {
            Some(observation.cursor)
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "land-authz: cannot read index_cursor; using the fallback reader");
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldCounts {
    pub tokens: u64,
    pub accounts: u64,
    pub estate_lands: u64,
}

const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

const FOLD_TOKEN_SQL: &str = "
WITH touched AS (
    SELECT DISTINCT token_address, token_id FROM land_authz.authz_event
    WHERE token_id IS NOT NULL AND kind IN ('approval', 'transfer', 'update_operator') AND block_number > $2
),
latest_touch AS (
    SELECT DISTINCT ON (e.token_address, e.token_id) e.token_address, e.token_id, e.block_number, e.log_index
    FROM land_authz.authz_event e JOIN touched t ON (t.token_address, t.token_id) = (e.token_address, e.token_id)
    WHERE e.token_id IS NOT NULL AND e.kind IN ('approval', 'transfer', 'update_operator')
    ORDER BY e.token_address, e.token_id, e.block_number DESC, e.log_index DESC
),
latest_operator AS (
    SELECT DISTINCT ON (e.token_address, e.token_id) e.token_address, e.token_id,
           CASE WHEN e.kind = 'approval' THEN e.operator END AS operator
    FROM land_authz.authz_event e JOIN touched t ON (t.token_address, t.token_id) = (e.token_address, e.token_id)
    WHERE e.token_id IS NOT NULL AND e.kind IN ('approval', 'transfer')
    ORDER BY e.token_address, e.token_id, e.block_number DESC, e.log_index DESC
),
latest_update_operator AS (
    SELECT DISTINCT ON (e.token_address, e.token_id) e.token_address, e.token_id,
           CASE WHEN e.kind = 'update_operator' THEN e.operator END AS update_operator
    FROM land_authz.authz_event e JOIN touched t ON (t.token_address, t.token_id) = (e.token_address, e.token_id)
    WHERE e.token_id IS NOT NULL AND e.kind IN ('update_operator', 'transfer')
    ORDER BY e.token_address, e.token_id, e.block_number DESC, e.log_index DESC
),
latest_owner AS (
    SELECT DISTINCT ON (e.token_address, e.token_id) e.token_address, e.token_id, e.account AS owner
    FROM land_authz.authz_event e JOIN touched t ON (t.token_address, t.token_id) = (e.token_address, e.token_id)
    WHERE e.token_id IS NOT NULL AND e.kind = 'transfer'
    ORDER BY e.token_address, e.token_id, e.block_number DESC, e.log_index DESC
),
merged AS (
    SELECT token_address, token_id,
           NULLIF(w.owner, $3) AS owner,
           o.operator, u.update_operator,
           lt.block_number AS updated_block, lt.log_index AS updated_log
    FROM latest_touch lt
    LEFT JOIN latest_operator o USING (token_address, token_id)
    LEFT JOIN latest_update_operator u USING (token_address, token_id)
    LEFT JOIN latest_owner w USING (token_address, token_id)
)
INSERT INTO land_authz.token_right
    (token_address, token_id, x, y, owner, operator, update_operator, updated_block, updated_log)
SELECT token_address, token_id,
       CASE WHEN token_address = $1 THEN land_authz.token_x(token_id) END,
       CASE WHEN token_address = $1 THEN land_authz.token_y(token_id) END,
       owner, operator, update_operator, updated_block, updated_log
FROM merged
ON CONFLICT (token_address, token_id) DO UPDATE SET
    owner           = EXCLUDED.owner,
    operator        = EXCLUDED.operator,
    update_operator = EXCLUDED.update_operator,
    x               = EXCLUDED.x,
    y               = EXCLUDED.y,
    updated_block   = EXCLUDED.updated_block,
    updated_log     = EXCLUDED.updated_log";

const FOLD_ACCOUNT_SQL: &str = "
WITH touched AS (
    SELECT DISTINCT token_address, account, operator, kind FROM land_authz.authz_event
    WHERE kind IN ('update_manager', 'approved_for_all') AND account IS NOT NULL AND operator IS NOT NULL
      AND block_number > $1
),
latest AS (
    SELECT DISTINCT ON (e.token_address, e.account, e.operator, e.kind)
           e.token_address, e.account, e.operator, e.kind, e.approved, e.block_number, e.log_index
    FROM land_authz.authz_event e
    JOIN touched t ON (t.token_address, t.account, t.operator, t.kind) = (e.token_address, e.account, e.operator, e.kind)
    WHERE e.account IS NOT NULL AND e.operator IS NOT NULL
    ORDER BY e.token_address, e.account, e.operator, e.kind, e.block_number DESC, e.log_index DESC
)
INSERT INTO land_authz.account_right
    (token_address, account, operator, kind, is_approved, updated_block, updated_log)
SELECT token_address, account, operator, kind, COALESCE(approved, false), block_number, log_index
FROM latest
ON CONFLICT (token_address, account, operator, kind) DO UPDATE SET
    is_approved   = EXCLUDED.is_approved,
    updated_block = EXCLUDED.updated_block,
    updated_log   = EXCLUDED.updated_log";

const FOLD_ESTATE_SQL: &str = "
WITH touched AS (
    SELECT DISTINCT token_id FROM land_authz.authz_event
    WHERE token_address = $2 AND kind IN ('add_land', 'remove_land') AND block_number > $1
),
latest AS (
    SELECT DISTINCT ON (e.token_id) e.token_id, e.estate_id, e.kind, e.block_number, e.log_index
    FROM land_authz.authz_event e JOIN touched t ON t.token_id = e.token_id
    WHERE e.token_address = $2 AND e.kind IN ('add_land', 'remove_land')
    ORDER BY e.token_id, e.block_number DESC, e.log_index DESC
),
removed AS (
    DELETE FROM land_authz.estate_land el USING latest l
    WHERE el.land_token_id = l.token_id AND l.kind = 'remove_land'
    RETURNING el.land_token_id
)
INSERT INTO land_authz.estate_land (land_token_id, estate_token_id, updated_block, updated_log)
SELECT token_id, estate_id, block_number, log_index FROM latest WHERE kind = 'add_land'
ON CONFLICT (land_token_id) DO UPDATE SET
    estate_token_id = EXCLUDED.estate_token_id,
    updated_block   = EXCLUDED.updated_block,
    updated_log     = EXCLUDED.updated_log";

/// Folds the log into current rights, in one transaction: the keys an event
/// with `block_number > since` touched (`since = None`: every key, the
/// rebuild). Each per-token leg is decided by the single latest event that
/// touches it, so "granted then revoked" and "granted then transferred
/// away" both end denied: a transfer stores NULL, a grant to the zero
/// address stores the zero address (the land-manager subgraph's cleared
/// leg, which `deploy_flags_for` reads as no grant); `owner` is the latest
/// `Transfer`'s `to`, NULL for the zero address; a LAND's estate is its latest
/// `AddLand` unless a later `RemoveLand` deleted the row.
pub async fn fold(
    pool: &PgPool,
    settings: &Settings,
    since: Option<i64>,
) -> Result<FoldCounts, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let counts = fold_in_tx(&mut tx, settings, since).await?;
    tx.commit().await?;
    Ok(counts)
}

/// [`fold`] inside the caller's transaction. It writes only while the
/// cursor still names `settings`' registries, and holds that row until the
/// transaction ends. The estate links carry no registry of their own, so a
/// pass bound to registries a full replay has since replaced must not write
/// them. A fold already running when the replay rebinds the cursor makes
/// [`begin_replay`] (`FOR UPDATE` on the same row) wait, so the replay's
/// deletion of the old links lands after it. A fold that starts after the
/// rebind finds no row and writes nothing: `RowNotFound`, as
/// [`set_cursor`] answers such a pass. The lock is `FOR KEY SHARE`, the
/// weakest that `FOR UPDATE` still waits on: the cursor's ordinary writers
/// (`set_cursor`, `mark_folded`, the timestamp repair) update non-key
/// columns and never wait on a fold, even one left running by a lost lease.
/// So a rebind must lock the row `FOR UPDATE` before it changes the
/// registries, as [`begin_replay`] does.
pub(crate) async fn fold_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    settings: &Settings,
    since: Option<i64>,
) -> Result<FoldCounts, sqlx::Error> {
    let since = since.unwrap_or(-1);
    let bound: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM land_authz.index_cursor \
         WHERE network = $1 AND land_registry = $2 AND estate_registry = $3 FOR KEY SHARE",
    )
    .bind(&settings.network)
    .bind(&settings.land_registry)
    .bind(&settings.estate_registry)
    .fetch_optional(&mut **tx)
    .await?;
    if bound.is_none() {
        return Err(sqlx::Error::RowNotFound);
    }
    // A job, not a request: the pool's 60 s statement_timeout is for the
    // routes. The rebuild fold (since = -1) over the whole registry history
    // is one statement per leg and may outlive it (the first live fold of
    // 34,034 events, 0.5 s warm, was cancelled at 60 s while the killed
    // node's orphaned fold still held the rows, 2026-09-25); a failed fold
    // retries with backoff, so the bound stays generous but finite.
    sqlx::query("SET LOCAL statement_timeout = '1800s'")
        .execute(&mut **tx)
        .await?;
    // The fold follows the bulk insert of the same pass, before autoanalyze
    // has seen the rows: without column statistics the planner sizes the
    // `touched` CTE at 68 rows and nests loops over it (34,397 events: the
    // live fold ran 169 s until the connection was lost; with statistics
    // the same statement hash-joins in 0.5 s, 2026-09-25). One ANALYZE per
    // fold is the price of a plan that matches the data.
    sqlx::query("ANALYZE land_authz.authz_event")
        .execute(&mut **tx)
        .await?;
    let tokens = sqlx::query(FOLD_TOKEN_SQL)
        .bind(&settings.land_registry)
        .bind(since)
        .bind(ZERO_ADDRESS)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    let accounts = sqlx::query(FOLD_ACCOUNT_SQL)
        .bind(since)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    let estate_lands = sqlx::query(FOLD_ESTATE_SQL)
        .bind(since)
        .bind(&settings.estate_registry)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    Ok(FoldCounts {
        tokens,
        accounts,
        estate_lands,
    })
}

/// What one pass did; the job trace and the tests read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassOutcome {
    pub head: u64,
    /// The block `last_block` reached when the pass returned.
    pub indexed_to: i64,
    pub requests: usize,
    pub events_written: usize,
    /// The pass returned before reaching the target: the request budget or a shutdown.
    pub paused: bool,
    pub folded: Option<FoldCounts>,
    pub published: bool,
}

/// Indexes `[from, to]` with at most `max(request_budget, 2)` RPC requests, each
/// answered range committed with its cursor advance; returns where it
/// stopped and whether it ran out of budget (or was cancelled) first.
pub async fn sync_range(
    state: &LandAuthzState,
    rpc: &Rpc<'_>,
    from: u64,
    to: u64,
    request_budget: usize,
    ctx: Option<&JobContext>,
    outcome: &mut PassOutcome,
) -> Result<(), IndexError> {
    let network = &state.settings.network;
    // Two calls are the smallest useful range: getLogs plus omitted timestamps.
    let request_budget = request_budget.max(2);
    let mut next = from;
    while next <= to {
        if ctx.is_some_and(|c| c.is_cancelled()) || outcome.requests >= request_budget {
            outcome.paused = true;
            return Ok(());
        }
        let retry_upper = state.retry_upper.load(Ordering::SeqCst);
        let upper = if retry_upper >= next {
            to.min(retry_upper)
        } else {
            to
        };
        let (mut logs, mut reached, used) = match rpc
            .get_logs_bounded(
                next,
                upper,
                &json!([state.settings.land_registry, state.settings.estate_registry]),
                &json!([ALL_TOPICS]),
                request_budget - outcome.requests,
            )
            .await
            .map_err(IndexError::from)
        {
            Ok(result) => result,
            Err(IndexError::Budget { upper, requests }) => {
                state.retry_upper.store(upper, Ordering::SeqCst);
                outcome.requests += requests;
                outcome.paused = true;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        // Cap hydration at one batch and commit only that verified prefix.
        // If log retries used the budget, keep their narrowed hint for the
        // next pass; no cursor advance can outrun its mandatory timestamps.
        let missing: std::collections::BTreeSet<i64> = logs
            .iter()
            .filter(|log| {
                log.block_timestamp
                    .as_deref()
                    .and_then(hex_to_i64)
                    .is_none_or(|time| time <= 0)
            })
            .filter_map(|log| hex_to_i64(&log.block_number))
            .collect();
        let needs_headers = !missing.is_empty();
        if let Some(first_uncovered) = missing.into_iter().nth(HEADER_BATCH) {
            reached = (first_uncovered - 1) as u64;
            logs.retain(|log| hex_to_i64(&log.block_number).is_some_and(|n| n < first_uncovered));
        }
        outcome.requests += used;
        if needs_headers && outcome.requests >= request_budget {
            state.retry_upper.store(reached, Ordering::SeqCst);
            outcome.paused = true;
            return Ok(());
        }
        if let Some(unresolved) = fill_missing_timestamps(
            state,
            rpc,
            &mut logs,
            request_budget - outcome.requests,
            &mut outcome.requests,
        )
        .await?
        {
            reached = unresolved.saturating_sub(1);
            state.retry_upper.store(reached.max(next), Ordering::SeqCst);
            if reached < next {
                outcome.paused = true;
                return Ok(());
            }
            logs.retain(|log| hex_to_i64(&log.block_number).is_some_and(|n| n < unresolved as i64));
        }
        let events = decode_logs(&logs)?;
        let mut tx = state.pool.begin().await?;
        let written = insert_events(&mut tx, &events).await?;
        set_cursor(&mut tx, &state.settings, reached as i64).await?;
        tx.commit().await?;
        outcome.events_written += written;
        outcome.indexed_to = reached as i64;
        tracing::info!(network = %network, from = next, to = reached, logs = logs.len(), written, "land-authz: indexed range");
        if let Some(ctx) = ctx {
            ctx.progress(format!(
                "indexed to block {reached} of {to} ({} requests this pass)",
                outcome.requests
            ));
        }
        state.retry_upper.store(u64::MAX, Ordering::SeqCst);
        next = reached + 1;
    }
    Ok(())
}

/// Whether the requested replay is now durable, so the pass goes on; it is
/// then consumed. A deferred one stays requested and ends the pass without
/// an error, so the next pass comes at the normal interval rather than
/// after the error backoff.
fn replay_begun(state: &LandAuthzState, started: ReplayStart) -> anyhow::Result<bool> {
    let settings = &state.settings;
    match started {
        ReplayStart::Begun => {
            state.replay_requested.store(true, Ordering::SeqCst);
            Ok(true)
        }
        ReplayStart::Deferred => {
            // Warned, not just noted: a fold left open (a connection idle in
            // its transaction) defers the switch on every pass until it ends.
            tracing::warn!(network = %settings.network, "land-authz: the replay was deferred: a lock was not granted in time (another pass admitting a cursor, a fold of the old registries holding theirs, or another writer of the cursor row), or the cursor was switched under the rewind; retrying next pass");
            Ok(false)
        }
        ReplayStart::Refused => {
            bail!("the land-authz cursor was rebound to other registries during the replay")
        }
        ReplayStart::SharedDatabase { other } => Err(shared_database(&settings.network, &other)),
    }
}

/// One periodic pass: replay if requested, catch up under the request
/// budget, and when caught up fold and (within [`HEAD_LAG_BLOCKS`] of the
/// head, with no `stop_block` holding the index short) publish.
pub async fn run_pass(state: &LandAuthzState, ctx: &JobContext) -> anyhow::Result<PassOutcome> {
    let settings = &state.settings;
    let Some(rpc_url) = settings.rpc_url.as_deref() else {
        bail!("ETH_RPC_URL is unset: the land-authz indexer has no log source");
    };
    let pool = &state.pool;
    // The replay counts as done only once its rewind is durable: a failed
    // write leaves it requested, and the next pass tries again.
    let replay = settings
        .replay_from_block
        .filter(|_| !state.replay_requested.load(Ordering::SeqCst));
    // A full replay rebinds the cursor to the configured registries, so it
    // runs before the check that refuses a cursor indexed from other ones
    // (the remedy that check names); a partial replay never rewinds another
    // registries' cursor.
    let full = replay.filter(|&from| from <= settings.start_block);
    if let Some(from) = full {
        let started = begin_replay(pool, settings, from)
            .await
            .context("beginning the replay")?;
        if !replay_begun(state, started)? {
            return Ok(PassOutcome::default());
        }
    }
    let mut row = match ensure_cursor(pool, settings).await {
        // A new cursor waits on another pass's admission past the lock
        // timeout: the pass ends as a deferred replay does, without the
        // error backoff.
        Err(e)
            if e.downcast_ref::<sqlx::Error>()
                .is_some_and(lock_not_available) =>
        {
            tracing::warn!(network = %settings.network, "land-authz: the cursor waits on another pass admitting one; retrying next pass");
            return Ok(PassOutcome::default());
        }
        row => row?,
    };
    if let Some(from) = replay {
        if full.is_none() {
            let started = begin_replay(pool, settings, from)
                .await
                .context("beginning the replay")?;
            if !replay_begun(state, started)? {
                return Ok(PassOutcome::default());
            }
        }
        row = cursor(pool, &settings.network)
            .await?
            .context("cursor row after the replay rewind")?;
        tracing::warn!(network = %settings.network, from, "land-authz: replay started; readers use the subgraph until it is folded at head. Unset LAND_AUTHZ_REPLAY_FROM_BLOCK once it has published: while it is set, every restart replays again");
    }
    let rpc = Rpc::at(&state.http, rpc_url);
    let head = rpc.head_block().await.context("eth_blockNumber")?;
    let target = settings.stop_block.map_or(head, |s| s.min(head));
    let mut outcome = PassOutcome {
        head,
        indexed_to: row.last_block,
        ..Default::default()
    };
    let from = (row.last_block + 1) as u64;
    if from <= target {
        sync_range(
            state,
            &rpc,
            from,
            target,
            settings.requests_per_pass,
            Some(ctx),
            &mut outcome,
        )
        .await
        .context("eth_getLogs")?;
        if outcome.paused {
            return Ok(outcome);
        }
    }
    let since = row.folded_block;
    let counts = fold(pool, settings, since)
        .await
        .context("folding the log")?;
    outcome.folded = Some(counts);
    let latest = rpc
        .head_block()
        .await
        .context("eth_blockNumber after the fold")?;
    let held_short = settings.stop_block.is_some_and(|s| s < latest);
    if held_short {
        ctx.progress(format!("indexed to {target}, head {latest}: LAND_AUTHZ_STOP_BLOCK holds the index short, not published"));
        return Ok(outcome);
    }
    if latest.saturating_sub(target) > HEAD_LAG_BLOCKS {
        ctx.progress(format!(
            "folded at {target} but the head moved to {latest}; publishing after the next catch-up"
        ));
        return Ok(outcome);
    }
    outcome.published = mark_folded(pool, settings, target)
        .await
        .context("publishing the fold")?;
    if !outcome.published {
        tracing::warn!(network = %settings.network, target, "land-authz: fold not published (cursor moved or no full replay recorded)");
    }
    // Publish ordinary ownership before potentially slow historical repair.
    // A cancelled/timed-out repair retains committed chunks and leaves its
    // collaborator gate false until a subsequent pass observes an empty set.
    let remaining = settings
        .requests_per_pass
        .max(2)
        .saturating_sub(outcome.requests);
    tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => { outcome.paused = true; }
        _ = tokio::time::sleep(LEGACY_REPAIR_TIME_LIMIT) => {
            tracing::warn!("land-authz: historical timestamp repair time budget exhausted");
        }
        result = repair_timestamps(state, &rpc, remaining, &mut outcome.requests) => {
            if let Err(error) = result {
                tracing::warn!(%error, "land-authz: legacy timestamp repair deferred");
            }
        }
    }
    ctx.progress(format!(
        "folded at block {target} (head {latest}): {} tokens, {} account rights, {} estate lands; published={}",
        counts.tokens, counts.accounts, counts.estate_lands, outcome.published
    ));
    Ok(outcome)
}

/// The estate-membership kinds, named here so the fold SQL and the schema
/// CHECK are read next to each other.
pub const ESTATE_KINDS: [&str; 2] = [KIND_ADD_LAND, KIND_REMOVE_LAND];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_readiness_requires_a_complete_recent_fold() {
        let now = Utc::now();
        let row = |last: i64, folded: Option<i64>, at: Option<DateTime<Utc>>| Cursor {
            network: "ethereum-mainnet".into(),
            land_registry: String::new(),
            estate_registry: String::new(),
            last_block: last,
            folded_block: folded,
            folded_at: at.map(|t| t.naive_utc()),
            full_replay_started: true,
            updated_at: now.naive_utc(),
        };
        assert!(!index_ready(&row(100, None, None), now));
        assert!(!index_ready(&row(101, Some(100), Some(now)), now));
        assert!(!index_ready(
            &row(100, Some(100), Some(now - ChronoDuration::minutes(31))),
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
}
