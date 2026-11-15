//! The `sales` chain index on Polygon: the primary mints the CollectionStore
//! issues (a mirrored collection's `Issue` whose `_caller` is the store) and
//! the ERC721BidV2 bid lifecycle (`BidCreated`, `BidAccepted`,
//! `BidCancelled`), kept in `shop.sale_event` and folded into `shop.nft_bid`
//! (one row per bidder and token, as the contract keeps them: a bidder's new
//! bid on the same token replaces the old one without a cancellation; the
//! seller is the token's owner when the bid was placed, read from the nft
//! walk's transfers, or the acceptance's seller). `shop.sale_venue` pins the
//! venues and the royalties cut each charges; `check_venues` reads
//! `royaltiesCutPerMillion()` once at the job's start and warns on a
//! difference.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy_primitives::U256;
use alloy_sol_types::{SolType, sol_data};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use super::MarketplaceState;
use super::chain_walk::{self, ChainIndex, ChainNet, INDEXER_SALES, LogGroup};
use super::chain_walk::{topic_address, topic_u256};
use super::nft_index::{self, DecodeError};
use crate::chain_index::rpc::{Rpc, RpcLog};
use crate::jobs::{Jobs, PeriodicCfg};

/// `Issue(address indexed _beneficiary, uint256 indexed _tokenId, uint256 indexed _itemId, uint256 _issuedId, address _caller)` (v2 collections).
pub const TOPIC_ISSUE: &str = "0x57e2fe3f4d57a597e655a893f43c395d7dc18cb9c9c1898eda75cc0ad7608525";
/// `BidCreated(bytes32 _id, address indexed _tokenAddress, uint256 indexed _tokenId, address indexed _bidder, uint256 _price, uint256 _expiresAt, bytes _fingerprint)`.
pub const TOPIC_BID_CREATED: &str =
    "0xe45b7709f1eed66d6e28f43b32f2003da0f0c40b92f75a8994370516e048620f";
/// `BidAccepted(bytes32 _id, address indexed _tokenAddress, uint256 indexed _tokenId, address _bidder, address indexed _seller, uint256 _price, uint256 _fee)`.
pub const TOPIC_BID_ACCEPTED: &str =
    "0x4e5ca6c6c06fa8d62f2930830e0d370de70f108bd89213de0b51141775e695bd";
/// `BidCancelled(bytes32 _id, address indexed _tokenAddress, uint256 indexed _tokenId, address indexed _bidder)`.
pub const TOPIC_BID_CANCELLED: &str =
    "0xc43098075c34b8b92567bd49f08f55e397ebf82dd82072e3eb1be525c4506f5f";

pub const KIND_MINT: &str = "mint";
pub const KIND_BID_CREATED: &str = "bid_created";
pub const KIND_BID_ACCEPTED: &str = "bid_accepted";
pub const KIND_BID_CANCELLED: &str = "bid_cancelled";

/// The CollectionStore on Polygon: an Issue it called is a primary sale.
pub const STORE_MATIC: &str = "0x214ffc0f0103735728dc66b61a22e4f163e275ae";
/// ERC721BidV2 on Polygon and the block the pinned subgraph starts it from.
pub const BID_V2_MATIC: &str = "0xb96697fa4a3361ba35b774a42c58daccaad1b8e1";
pub const BID_V2_MATIC_START_BLOCK: u64 = 22_514_900;
/// The selector of `royaltiesCutPerMillion()`.
pub const ROYALTIES_CUT_SELECTOR: &str = "0xce2243b4";

/// One row of `shop.sale_event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaleEvent {
    pub block_number: i64,
    pub log_index: i32,
    pub block_time: i64,
    pub kind: &'static str,
    pub emitter: String,
    pub contract_address: String,
    pub token_id: String,
    pub item_id: Option<String>,
    pub bid_id: Option<String>,
    /// The mint's beneficiary or the bid's bidder.
    pub buyer: String,
    pub seller: Option<String>,
    pub price: Option<String>,
    /// The bid's expiry, unix seconds.
    pub expires_at: Option<i64>,
    pub tx_hash: String,
}

fn seconds(v: U256) -> i64 {
    let v: u128 = v.try_into().unwrap_or(u128::MAX);
    v.min(i64::MAX as u128) as i64
}

/// One log as a row; `Ok(None)` for a topic this index does not model and
/// for an Issue the store did not call (a creator's own mint).
pub fn decode_log(log: &RpcLog, block_time: i64) -> Result<Option<SaleEvent>, DecodeError> {
    let (block, index) = chain_walk::log_position(log);
    let index = index as i32;
    let err = |what: &'static str, why: String| DecodeError {
        block,
        index,
        what,
        why,
    };
    let Some(topic0) = log.topics.first().map(|t| t.to_ascii_lowercase()) else {
        return Ok(None);
    };
    let kind = match topic0.as_str() {
        TOPIC_ISSUE => KIND_MINT,
        TOPIC_BID_CREATED => KIND_BID_CREATED,
        TOPIC_BID_ACCEPTED => KIND_BID_ACCEPTED,
        TOPIC_BID_CANCELLED => KIND_BID_CANCELLED,
        _ => return Ok(None),
    };
    let data =
        hex::decode(log.data.trim_start_matches("0x")).map_err(|e| err("data", e.to_string()))?;
    let topic = |i: usize| {
        log.topics
            .get(i)
            .ok_or_else(|| err(kind, format!("needs topic {i}")))
    };
    let addr = |i: usize| {
        topic_address(topic(i)?).ok_or_else(|| err(kind, format!("bad address in topic {i}")))
    };
    let word =
        |i: usize| topic_u256(topic(i)?).ok_or_else(|| err(kind, format!("bad uint in topic {i}")));
    let tx_hash = log
        .transaction_hash
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|h| {
            h.len() == 66 && h.starts_with("0x") && h[2..].bytes().all(|c| c.is_ascii_hexdigit())
        })
        .ok_or_else(|| err(kind, "missing transaction hash".into()))?;
    let base = |contract_address: String, token_id: String, buyer: String| SaleEvent {
        block_number: block,
        log_index: index,
        block_time,
        kind,
        emitter: log.address.to_ascii_lowercase(),
        contract_address,
        token_id,
        item_id: None,
        bid_id: None,
        buyer,
        seller: None,
        price: None,
        expires_at: None,
        tx_hash: tx_hash.clone(),
    };
    let event = match kind {
        KIND_MINT => {
            let (_issued, caller) =
                <(sol_data::Uint<256>, sol_data::Address)>::abi_decode_params(&data)
                    .map_err(|e| err(kind, e.to_string()))?;
            if format!("{caller:#x}") != STORE_MATIC {
                return Ok(None);
            }
            SaleEvent {
                item_id: Some(word(3)?),
                ..base(log.address.to_ascii_lowercase(), word(2)?, addr(1)?)
            }
        }
        KIND_BID_CREATED => {
            let (id, price, expires, _fingerprint) = <(
                sol_data::FixedBytes<32>,
                sol_data::Uint<256>,
                sol_data::Uint<256>,
                sol_data::Bytes,
            )>::abi_decode_params(&data)
            .map_err(|e| err(kind, e.to_string()))?;
            SaleEvent {
                bid_id: Some(format!("{id:#x}")),
                price: Some(price.to_string()),
                expires_at: Some(seconds(expires)),
                ..base(addr(1)?, word(2)?, addr(3)?)
            }
        }
        KIND_BID_ACCEPTED => {
            let (id, bidder, price, _fee) = <(
                sol_data::FixedBytes<32>,
                sol_data::Address,
                sol_data::Uint<256>,
                sol_data::Uint<256>,
            )>::abi_decode_params(&data)
            .map_err(|e| err(kind, e.to_string()))?;
            SaleEvent {
                bid_id: Some(format!("{id:#x}")),
                seller: Some(addr(3)?),
                price: Some(price.to_string()),
                ..base(addr(1)?, word(2)?, format!("{bidder:#x}"))
            }
        }
        _ => {
            let id = <sol_data::FixedBytes<32>>::abi_decode(&data)
                .map_err(|e| err(kind, e.to_string()))?;
            SaleEvent {
                bid_id: Some(format!("{id:#x}")),
                ..base(addr(1)?, word(2)?, addr(3)?)
            }
        }
    };
    Ok(Some(event))
}

pub async fn insert_events(
    conn: &mut PgConnection,
    rows: &[SaleEvent],
) -> Result<usize, sqlx::Error> {
    let mut written = 0;
    for chunk in rows.chunks(1_000) {
        let r = sqlx::query(
            "INSERT INTO shop.sale_event \
                 (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, item_id, bid_id, buyer, seller, price, expires_at, tx_hash) \
             SELECT $1, b, i, t, k, em, c, tok::numeric, it::numeric, bid, by, se, p::numeric, x, h \
             FROM UNNEST($2::bigint[], $3::int[], $4::bigint[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[], $10::text[], $11::text[], $12::text[], $13::text[], $14::bigint[], $15::text[]) \
                  AS u(b, i, t, k, em, c, tok, it, bid, by, se, p, x, h) \
             ON CONFLICT (network, block_number, log_index) DO NOTHING",
        )
        .bind(ChainNet::Matic.db_name())
        .bind(chunk.iter().map(|r| r.block_number).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.log_index).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.block_time).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.kind.to_string()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.emitter.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.contract_address.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.token_id.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.item_id.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.bid_id.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.buyer.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.seller.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.price.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.expires_at).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.tx_hash.clone()).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
        written += r.rows_affected() as usize;
    }
    Ok(written)
}

/// The bids fold of the touched keys: every (token, bidder) a bid event
/// past `$2` touched, the open bids on a token with an acceptance after
/// them (whatever fold saw the acceptance, so a database folded before the
/// rule existed converges without a cursor reset), plus the rows still
/// missing their seller that the nft walk (`$3`, its last indexed block) has
/// passed since. The
/// latest `bid_created` of a key defines the bid; the latest event at or
/// after it, its status, `invalidated` when another bid on the token was
/// accepted after it (the contract drops the token's other bids without an
/// event); the seller is the acceptance's or the owner before the bid as the
/// nft walk's transfers tell it, which a bid past the walk's last block or on
/// a contract still owed a backfill does not read (the walk has not seen every
/// transfer before the bid: the row waits with no seller and is revisited once
/// the walk passes it and the backfill is done). An
/// unchanged row is left alone.
const FOLD_BIDS_SQL: &str = "
WITH touched AS (
    SELECT contract_address, token_id, buyer AS bidder FROM shop.sale_event
    WHERE network = $1 AND block_number > $2 AND kind <> 'mint'
    UNION
    SELECT b.contract_address, b.token_id, b.bidder FROM shop.nft_bid b
    WHERE b.network = $1 AND b.status = 'open' AND EXISTS (
        SELECT 1 FROM shop.sale_event a
        WHERE a.network = $1 AND a.contract_address = b.contract_address AND a.token_id = b.token_id
          AND a.kind = 'bid_accepted' AND (a.block_number, a.log_index) > (b.created_block, b.created_log_index))
    UNION
    SELECT b.contract_address, b.token_id, b.bidder FROM shop.nft_bid b
    WHERE b.network = $1 AND b.seller IS NULL AND b.created_block <= $3 AND EXISTS (
        SELECT 1 FROM shop.nft_event n
        WHERE n.network = $1 AND n.contract_address = b.contract_address AND n.token_id = b.token_id
          AND n.kind = 'transfer' AND (n.block_number, n.log_index) < (b.created_block, b.created_log_index))
), created AS (
    SELECT DISTINCT ON (e.contract_address, e.token_id, e.buyer)
        e.contract_address, e.token_id, e.buyer AS bidder, e.bid_id, e.price, e.expires_at,
        e.block_number, e.log_index, e.block_time
    FROM touched t
    JOIN shop.sale_event e ON e.network = $1 AND e.contract_address = t.contract_address
        AND e.token_id = t.token_id AND e.buyer = t.bidder AND e.kind = 'bid_created'
    ORDER BY e.contract_address, e.token_id, e.buyer, e.block_number DESC, e.log_index DESC
), latest AS (
    SELECT DISTINCT ON (c.contract_address, c.token_id, c.bidder)
        c.contract_address, c.token_id, c.bidder, e.kind, e.seller, e.block_number, e.log_index, e.block_time
    FROM created c
    JOIN shop.sale_event e ON e.network = $1 AND e.contract_address = c.contract_address
        AND e.token_id = c.token_id AND e.buyer = c.bidder AND e.kind <> 'mint'
        AND (e.block_number, e.log_index) >= (c.block_number, c.log_index)
    ORDER BY c.contract_address, c.token_id, c.bidder, e.block_number DESC, e.log_index DESC
)
INSERT INTO shop.nft_bid (network, contract_address, token_id, bidder, bid_id, seller, price, expires_at, status,
    created_block, created_log_index, created_at, updated_block, updated_log_index, updated_at)
SELECT $1, c.contract_address, c.token_id, c.bidder, c.bid_id,
    COALESCE(l.seller, (
        SELECT n.to_address FROM shop.nft_event n
        WHERE n.network = $1 AND n.contract_address = c.contract_address AND n.token_id = c.token_id
          AND n.kind = 'transfer' AND (n.block_number, n.log_index) < (c.block_number, c.log_index)
          AND c.block_number <= $3
          AND NOT EXISTS (
            SELECT 1 FROM shop.nft_contract k
            WHERE k.network = $1 AND k.contract_address = c.contract_address AND k.backfill_from_block IS NOT NULL
              AND COALESCE(k.backfilled_to_block, k.backfill_from_block - 1) < k.walked_from_block - 1)
        ORDER BY n.block_number DESC, n.log_index DESC LIMIT 1)),
    c.price, c.expires_at,
    CASE WHEN l.kind = 'bid_accepted' THEN 'accepted' WHEN l.kind = 'bid_cancelled' THEN 'cancelled'
         WHEN EXISTS (
            SELECT 1 FROM shop.sale_event a
            WHERE a.network = $1 AND a.contract_address = c.contract_address AND a.token_id = c.token_id
              AND a.kind = 'bid_accepted' AND (a.block_number, a.log_index) > (c.block_number, c.log_index)) THEN 'invalidated'
         ELSE 'open' END,
    c.block_number, c.log_index, c.block_time, l.block_number, l.log_index, l.block_time
FROM created c JOIN latest l USING (contract_address, token_id, bidder)
ON CONFLICT (network, contract_address, token_id, bidder) DO UPDATE SET
    bid_id = EXCLUDED.bid_id, seller = EXCLUDED.seller, price = EXCLUDED.price, expires_at = EXCLUDED.expires_at,
    status = EXCLUDED.status, created_block = EXCLUDED.created_block, created_log_index = EXCLUDED.created_log_index,
    created_at = EXCLUDED.created_at, updated_block = EXCLUDED.updated_block,
    updated_log_index = EXCLUDED.updated_log_index, updated_at = EXCLUDED.updated_at
WHERE (nft_bid.bid_id, nft_bid.seller, nft_bid.status, nft_bid.updated_block, nft_bid.updated_log_index)
    IS DISTINCT FROM (EXCLUDED.bid_id, EXCLUDED.seller, EXCLUDED.status, EXCLUDED.updated_block, EXCLUDED.updated_log_index)";

/// The fold in one transaction: the keys a bid event past `since` touched
/// (`None`: every key), each over its whole history; the rows written.
pub async fn fold(pool: &PgPool, since: Option<i64>) -> anyhow::Result<u64> {
    let since = since.unwrap_or(-1);
    let nft_indexed = chain_walk::cursor(pool, chain_walk::INDEXER_NFT, ChainNet::Matic)
        .await?
        .map_or(-1, |cursor| cursor.last_block);
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '3600s'")
        .execute(&mut *tx)
        .await?;
    chain_walk::analyze_if_stale(&mut tx, "shop", "sale_event", since < 0).await?;
    let bids = sqlx::query(FOLD_BIDS_SQL)
        .bind(ChainNet::Matic.db_name())
        .bind(since)
        .bind(nft_indexed)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(bids)
}

/// Polygon's side of the walk: the mirrored collections' Issue logs (the
/// address set is time-bounded like the nft walk's; a collection the mirror
/// reports late has no store sale before its approval, so no backfill lane)
/// and the bid contract's three events from its deployment.
pub struct SalesIndex;

#[async_trait::async_trait]
impl ChainIndex for SalesIndex {
    fn indexer(&self) -> &'static str {
        INDEXER_SALES
    }

    fn net(&self) -> ChainNet {
        ChainNet::Matic
    }

    fn start_block(&self) -> u64 {
        nft_index::start_block(ChainNet::Matic)
    }

    async fn groups(
        &self,
        pool: &PgPool,
        _from: u64,
        to: u64,
        anchor: (u64, i64),
    ) -> anyhow::Result<Vec<LogGroup>> {
        let bound = chain_walk::time_at_most(ChainNet::Matic, anchor, to);
        let collections: Vec<String> = sqlx::query_scalar(
            "SELECT contract_address FROM shop.collections WHERE network = $1 AND created_at <= $2 ORDER BY contract_address",
        )
        .bind(ChainNet::Matic.db_name())
        .bind(bound)
        .fetch_all(pool)
        .await?;
        let mut groups = Vec::new();
        if !collections.is_empty() {
            groups.push(LogGroup {
                addresses: collections,
                topics: json!([[TOPIC_ISSUE]]),
            });
        }
        if to >= BID_V2_MATIC_START_BLOCK {
            groups.push(LogGroup {
                addresses: vec![BID_V2_MATIC.to_string()],
                topics: json!([[TOPIC_BID_CREATED, TOPIC_BID_ACCEPTED, TOPIC_BID_CANCELLED]]),
            });
        }
        Ok(groups)
    }

    async fn write(
        &self,
        conn: &mut PgConnection,
        logs: &[RpcLog],
        times: &HashMap<i64, i64>,
    ) -> anyhow::Result<usize> {
        let mut rows = Vec::with_capacity(logs.len());
        for log in logs {
            let block = chain_walk::log_position(log).0;
            match decode_log(log, times.get(&block).copied().unwrap_or(0)) {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => {}
                // A malformed log is logged and skipped, never a stalled walk.
                Err(e) => {
                    tracing::warn!(error = %e, "sales index: skipping a log it cannot decode")
                }
            }
        }
        Ok(insert_events(conn, &rows).await?)
    }

    async fn rewind(&self, conn: &mut PgConnection, to: i64) -> anyhow::Result<()> {
        // The deleted rows' bids are dropped (a mint keys no bid, so none
        // comes back); the cleared fold marker makes the next fold a rebuild,
        // which restores the ones with a remaining creation from the log that
        // is left.
        let gone: Vec<(String, String, String)> = sqlx::query_as(
            "WITH gone AS (DELETE FROM shop.sale_event WHERE network = $1 AND block_number > $2 RETURNING contract_address, token_id, buyer, kind) \
             SELECT DISTINCT contract_address, token_id::text, buyer FROM gone WHERE kind <> 'mint'",
        )
        .bind(ChainNet::Matic.db_name())
        .bind(to)
        .fetch_all(&mut *conn)
        .await?;
        let (contracts, tokens, bidders): (Vec<String>, Vec<String>, Vec<String>) = gone
            .into_iter()
            .fold((vec![], vec![], vec![]), |mut acc, (c, t, b)| {
                acc.0.push(c);
                acc.1.push(t);
                acc.2.push(b);
                acc
            });
        sqlx::query(
            "DELETE FROM shop.nft_bid b USING UNNEST($2::text[], $3::text[], $4::text[]) AS g(c, t, bd) \
             WHERE b.network = $1 AND b.contract_address = g.c AND b.token_id = g.t::numeric AND b.bidder = g.bd",
        )
        .bind(ChainNet::Matic.db_name())
        .bind(&contracts)
        .bind(&tokens)
        .bind(&bidders)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn fold(&self, pool: &PgPool, since: Option<i64>, _upto: i64) -> anyhow::Result<bool> {
        let bids = fold(pool, since).await?;
        tracing::info!(bids, "sales index: folded");
        Ok(true)
    }
}

/// One venue's pinned royalties cut beside the chain's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenueCheck {
    pub address: String,
    pub kind: String,
    pub pinned: i64,
    pub live: Option<U256>,
}

impl VenueCheck {
    pub fn matches(&self) -> bool {
        self.live == Some(U256::from(self.pinned))
    }
}

/// `royaltiesCutPerMillion()` of every Polygon venue that charges one
/// (`marketplace_v2`, `bid_v2`), compared with `shop.sale_venue`: a
/// difference or an unreadable answer is a warning, never a rewrite.
pub async fn check_venues(rpc: &Rpc<'_>, pool: &PgPool) -> anyhow::Result<Vec<VenueCheck>> {
    let venues: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT address, kind, royalties_cut_per_million FROM shop.sale_venue \
         WHERE network = $1 AND kind IN ('marketplace_v2', 'bid_v2') ORDER BY address",
    )
    .bind(ChainNet::Matic.db_name())
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(venues.len());
    for (address, kind, pinned) in venues {
        let reply = rpc
            .call(
                "eth_call",
                json!([{ "to": address, "data": ROYALTIES_CUT_SELECTOR }, "latest"]),
            )
            .await?;
        let live = reply
            .get("result")
            .and_then(Value::as_str)
            .and_then(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        let check = VenueCheck {
            address,
            kind,
            pinned: i64::from(pinned),
            live,
        };
        match check.live {
            Some(live) if check.matches() => tracing::info!(
                venue = %check.address, kind = %check.kind, cut = %live,
                "sales index: the venue's royalties cut matches the pinned value"
            ),
            Some(live) => tracing::warn!(
                venue = %check.address, kind = %check.kind, pinned = check.pinned, live = %live,
                "sales index: the venue's royalties cut differs from shop.sale_venue"
            ),
            None => tracing::warn!(
                venue = %check.address, kind = %check.kind, reply = %reply,
                "sales index: royaltiesCutPerMillion() is unreadable"
            ),
        }
        out.push(check);
    }
    Ok(out)
}

/// Whether the bids may be trusted at `now` (the cursor is published).
pub async fn ready(pool: &PgPool, now: DateTime<Utc>) -> bool {
    chain_walk::ready_cursor(pool, INDEXER_SALES, ChainNet::Matic, now)
        .await
        .is_some()
}

/// The index's state for the status route and the tests.
pub async fn status(pool: &PgPool) -> Value {
    let c = chain_walk::cursor(pool, INDEXER_SALES, ChainNet::Matic)
        .await
        .ok()
        .flatten();
    json!({
        "matic": c.map(|c| json!({
            "lastBlock": c.last_block, "foldedBlock": c.folded_block, "fullHistory": c.full_history,
            "ready": chain_walk::is_ready(&c, Utc::now()), "reorgRewinds": c.reorg_rewinds,
        })),
    })
}

/// Attaches `sales-index` when `MARKETPLACE_SALES_INDEX_ENABLED` and a
/// Polygon JSON-RPC endpoint (`TRADE_RPC_URLS` 137=) are configured, on the
/// market indexes' cadence and budget: the venue check on its first tick,
/// then one pass per tick once the collections mirror has completed a pass
/// (the address set is only as complete as it).
pub fn spawn_job(market: &Arc<MarketplaceState>, jobs: &Jobs) {
    let settings = market.settings.market.indexes.clone();
    if !settings.sales_enabled {
        tracing::info!(
            job = "sales-index",
            "marketplace chain index off: its routes answer the unavailable envelope"
        );
        return;
    }
    let Some(url) = market.chain_rpc_url(ChainNet::Matic) else {
        tracing::warn!(
            job = "sales-index",
            "marketplace chain index enabled but no Polygon JSON-RPC endpoint is configured (TRADE_RPC_URLS 137=): it stays off"
        );
        return;
    };
    let market = market.clone();
    let venues_checked = Arc::new(AtomicBool::new(false));
    jobs.spawn_periodic_leased(
        "sales-index",
        PeriodicCfg::every(settings.interval)
            .with_jitter(Duration::from_secs(1))
            .with_error_backoff(Duration::from_secs(30)),
        move |ctx| {
            let (market, url, settings, venues_checked) = (
                market.clone(),
                url.clone(),
                settings.clone(),
                venues_checked.clone(),
            );
            async move {
                let rpc = Rpc::at(&market.http, &url);
                if !venues_checked.load(Ordering::SeqCst) {
                    check_venues(&rpc, &market.pool).await?;
                    venues_checked.store(true, Ordering::SeqCst);
                }
                if !nft_index::mirror_complete(&market.pool, ChainNet::Matic).await {
                    ctx.progress(
                        "sales matic: waiting for the collections mirror's first complete pass"
                            .to_string(),
                    );
                    return Ok(());
                }
                let progress = |m: String| ctx.progress(m);
                let outcome = chain_walk::run_pass(
                    &market.pool,
                    &rpc,
                    &SalesIndex,
                    &settings,
                    Some(&ctx.cancel),
                    &progress,
                )
                .await?;
                tracing::info!(indexer = INDEXER_SALES, summary = %outcome.summary(), "marketplace chain index pass");
                ctx.progress(outcome.summary());
                Ok(())
            }
        },
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::evm::contracts_addrs::DclContracts;
    use alloy_primitives::{Bytes, FixedBytes, keccak256};
    use alloy_sol_types::SolValue;
    use std::str::FromStr;

    const COLLECTION: &str = "0x00000000000000000000000000000000000000c1";
    const ALICE: &str = "0xaaaa000000000000000000000000000000000001";
    const BOB: &str = "0xbbbb000000000000000000000000000000000002";

    #[test]
    fn the_topics_are_the_keccak_of_their_signatures_and_the_venues_the_pinned_book() {
        for (sig, topic) in [
            (
                "Issue(address,uint256,uint256,uint256,address)",
                TOPIC_ISSUE,
            ),
            (
                "BidCreated(bytes32,address,uint256,address,uint256,uint256,bytes)",
                TOPIC_BID_CREATED,
            ),
            (
                "BidAccepted(bytes32,address,uint256,address,address,uint256,uint256)",
                TOPIC_BID_ACCEPTED,
            ),
            (
                "BidCancelled(bytes32,address,uint256,address)",
                TOPIC_BID_CANCELLED,
            ),
        ] {
            assert_eq!(format!("{:#x}", keccak256(sig)), topic, "{sig}");
        }
        assert_eq!(
            &format!("{:#x}", keccak256("royaltiesCutPerMillion()"))[..10],
            ROYALTIES_CUT_SELECTOR
        );
        let book = DclContracts::for_chain(137).unwrap();
        assert_eq!(format!("{:#x}", book.collection_store), STORE_MATIC);
        assert_eq!(format!("{:#x}", book.bid_v2), BID_V2_MATIC);
    }

    pub(crate) fn log(
        address: &str,
        topics: Vec<String>,
        data: Vec<u8>,
        block: u64,
        index: u64,
    ) -> RpcLog {
        RpcLog {
            address: address.to_string(),
            topics,
            data: format!("0x{}", hex::encode(data)),
            block_number: format!("0x{block:x}"),
            log_index: format!("0x{index:x}"),
            block_timestamp: Some(format!("0x{:x}", 1_600_000_000 + block)),
            block_hash: None,
            transaction_hash: Some(format!("0x{:064x}", block * 100 + index)),
        }
    }

    pub(crate) fn issue_log(
        collection: &str,
        beneficiary: &str,
        token: U256,
        item: u64,
        caller: &str,
        block: u64,
        index: u64,
    ) -> RpcLog {
        log(
            collection,
            vec![
                TOPIC_ISSUE.into(),
                nft_index::tests::topic_addr(beneficiary),
                nft_index::tests::topic_word(token),
                nft_index::tests::topic_word(U256::from(item)),
            ],
            (
                U256::from(1u8),
                alloy_primitives::Address::from_str(caller).unwrap(),
            )
                .abi_encode_params(),
            block,
            index,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bid_created_log(
        collection: &str,
        token: U256,
        bidder: &str,
        id: u8,
        price: u64,
        expires: u64,
        block: u64,
        index: u64,
    ) -> RpcLog {
        log(
            BID_V2_MATIC,
            vec![
                TOPIC_BID_CREATED.into(),
                nft_index::tests::topic_addr(collection),
                nft_index::tests::topic_word(token),
                nft_index::tests::topic_addr(bidder),
            ],
            (
                FixedBytes::<32>::repeat_byte(id),
                U256::from(price),
                U256::from(expires),
                Bytes::new(),
            )
                .abi_encode_params(),
            block,
            index,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bid_accepted_log(
        collection: &str,
        token: U256,
        bidder: &str,
        seller: &str,
        id: u8,
        price: u64,
        block: u64,
        index: u64,
    ) -> RpcLog {
        log(
            BID_V2_MATIC,
            vec![
                TOPIC_BID_ACCEPTED.into(),
                nft_index::tests::topic_addr(collection),
                nft_index::tests::topic_word(token),
                nft_index::tests::topic_addr(seller),
            ],
            (
                FixedBytes::<32>::repeat_byte(id),
                alloy_primitives::Address::from_str(bidder).unwrap(),
                U256::from(price),
                U256::from(price / 40),
            )
                .abi_encode_params(),
            block,
            index,
        )
    }

    pub(crate) fn bid_cancelled_log(
        collection: &str,
        token: U256,
        bidder: &str,
        id: u8,
        block: u64,
        index: u64,
    ) -> RpcLog {
        log(
            BID_V2_MATIC,
            vec![
                TOPIC_BID_CANCELLED.into(),
                nft_index::tests::topic_addr(collection),
                nft_index::tests::topic_word(token),
                nft_index::tests::topic_addr(bidder),
            ],
            FixedBytes::<32>::repeat_byte(id).abi_encode(),
            block,
            index,
        )
    }

    #[test]
    fn a_store_issue_is_a_mint_and_a_creators_own_issue_is_not() {
        let token = (U256::from(3u8) << 216) | U256::from(7u8);
        let kept = decode_log(
            &issue_log(COLLECTION, ALICE, token, 3, STORE_MATIC, 100, 1),
            5,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            kept,
            SaleEvent {
                block_number: 100,
                log_index: 1,
                block_time: 5,
                kind: KIND_MINT,
                emitter: COLLECTION.into(),
                contract_address: COLLECTION.into(),
                token_id: token.to_string(),
                item_id: Some("3".into()),
                bid_id: None,
                buyer: ALICE.into(),
                seller: None,
                price: None,
                expires_at: None,
                tx_hash: format!("0x{:064x}", 10_001),
            }
        );
        assert_eq!(
            decode_log(&issue_log(COLLECTION, ALICE, token, 3, BOB, 100, 2), 5).unwrap(),
            None,
            "an Issue the store did not call is a creator's mint, not a sale"
        );
        assert_eq!(
            decode_log(
                &nft_index::tests::transfer_log(COLLECTION, ALICE, BOB, token, 100, 3),
                5
            )
            .unwrap(),
            None
        );
        let mut no_hash = issue_log(COLLECTION, ALICE, token, 3, STORE_MATIC, 100, 1);
        no_hash.transaction_hash = None;
        assert!(decode_log(&no_hash, 5).is_err());
    }

    #[test]
    fn the_bid_lifecycle_decodes() {
        let token = U256::from(9u8);
        let created = decode_log(
            &bid_created_log(COLLECTION, token, BOB, 0x11, 700, 1_700_000_000, 200, 0),
            6,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                created.kind,
                created.emitter.as_str(),
                created.contract_address.as_str(),
                created.buyer.as_str(),
                created.bid_id.as_deref(),
                created.price.as_deref(),
                created.expires_at,
                created.seller.as_deref(),
            ),
            (
                KIND_BID_CREATED,
                BID_V2_MATIC,
                COLLECTION,
                BOB,
                Some(format!("0x{}", "11".repeat(32)).as_str()),
                Some("700"),
                Some(1_700_000_000),
                None,
            )
        );
        let accepted = decode_log(
            &bid_accepted_log(COLLECTION, token, BOB, ALICE, 0x11, 700, 201, 0),
            7,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                accepted.kind,
                accepted.buyer.as_str(),
                accepted.seller.as_deref(),
                accepted.price.as_deref(),
                accepted.bid_id.as_deref(),
            ),
            (
                KIND_BID_ACCEPTED,
                BOB,
                Some(ALICE),
                Some("700"),
                Some(format!("0x{}", "11".repeat(32)).as_str()),
            )
        );
        let cancelled = decode_log(&bid_cancelled_log(COLLECTION, token, BOB, 0x11, 202, 0), 8)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                cancelled.kind,
                cancelled.buyer.as_str(),
                cancelled.token_id.as_str(),
                cancelled.bid_id.as_deref(),
                cancelled.price,
            ),
            (
                KIND_BID_CANCELLED,
                BOB,
                "9",
                Some(format!("0x{}", "11".repeat(32)).as_str()),
                None,
            )
        );
    }
}
