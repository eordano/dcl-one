// from: rentals-graph@ddb42672:subgraph.template.yaml and src/handlers/rentals.ts (which events, what each one means for a listing), rentals-contract@b51989f0:contracts/Rentals.sol (_rent, claim, bumpAssetIndex: what emits them), signatures-server (the rentals-subgraph status sync the old crate configured and never ran: open -> executed | cancelled | claimed)
//! The `rentals` chain index: the Rentals contract's logs on Ethereum
//! (`shop.rental_event`), walked by `chain_walk` from its deployment block,
//! and folded into the status of every rentals listing this node stores
//! (`signatures.rentals.status`, `started_at`, `rented_days`,
//! `rental_listings.tenant`), the sync the old signatures-server did from
//! the rentals subgraph:
//!
//! - `executed` when an `AssetRented` carries the listing's own signature
//!   (its tenant, start and days come from that event);
//! - `claimed` when the lessor's `AssetClaimed` follows that rent;
//! - `cancelled` when a signed nonce moved: the contract index
//!   (`ContractIndexUpdated`), the lessor's signer index
//!   (`SignerIndexUpdated`) or the lessor's index over the asset
//!   (`AssetIndexUpdated`, which `bumpAssetIndex` and every rent emit)
//!   above the one the listing signed; a rent of the same listing wins over
//!   the index bump it emits;
//! - `open` otherwise; expiry stays a read-time rule.
//!
//! Readers trust the statuses only while the cursor is published
//! (`chain_walk::is_ready`); before that a listing is `open` as posted, and
//! the rent quote (`shop::mana`) checks the nonces on chain instead.

use crate::market::chain_walk::{topic_address, topic_u256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Address;
use alloy_sol_types::{SolType, sol_data};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};

use crate::chain_index::rpc::RpcLog;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::market::MarketplaceState;
use crate::market::chain_walk::{self, ChainIndex, ChainNet, LogGroup};

pub const INDEXER_RENTALS: &str = "rentals";

/// The Rentals proxy on Ethereum mainnet and the block the pinned subgraph
/// starts from (`rentals-graph@ddb42672:config/mainnet.json`).
pub const RENTALS_MAINNET: &str = "0x3a1469499d0be105d4f77045ca403a5f6dc2f3f5";
pub const RENTALS_MAINNET_START_BLOCK: u64 = 16_070_889;

/// `AssetRented(address indexed _contractAddress, uint256 indexed _tokenId, address _lessor, address _tenant, address _operator, uint256 _rentalDays, uint256 _pricePerDay, bool _isExtension, address _sender, bytes _signature)`.
pub const TOPIC_ASSET_RENTED: &str =
    "0xfe3b2e4cd1e8ad8d90abfd715fe3d39a303e0a6e9adde116d5d10bebfa678d80";
/// `AssetClaimed(address indexed _contractAddress, uint256 indexed _tokenId, address _sender)`.
pub const TOPIC_ASSET_CLAIMED: &str =
    "0x64fcb03e7c10d3b0ae8a1bac00b1e9e9cdf6b593ef0e757797d21977f80183a4";
/// `AssetIndexUpdated(address indexed _signer, address indexed _contractAddress, uint256 indexed _tokenId, uint256 _newIndex, address _sender)`.
pub const TOPIC_ASSET_INDEX_UPDATED: &str =
    "0x03b0084eb3c564c3928723736eeb4c38263fdce62f06c45eaa69df01c0415148";
/// `SignerIndexUpdated(address indexed _signer, uint256 _newIndex, address _sender)`.
pub const TOPIC_SIGNER_INDEX_UPDATED: &str =
    "0xbcdee0760cae1a36f3773c7f12f98aa0afbafa4099c256eeb87d796f2afb7d44";
/// `ContractIndexUpdated(uint256 _newIndex, address _sender)`.
pub const TOPIC_CONTRACT_INDEX_UPDATED: &str =
    "0xa6957d63c6c422962352065c3196f36c268eda93a702712ccf4c3d810da755b3";

pub const ALL_TOPICS: [&str; 5] = [
    TOPIC_ASSET_RENTED,
    TOPIC_ASSET_CLAIMED,
    TOPIC_ASSET_INDEX_UPDATED,
    TOPIC_SIGNER_INDEX_UPDATED,
    TOPIC_CONTRACT_INDEX_UPDATED,
];

pub const KIND_RENTED: &str = "rented";
pub const KIND_CLAIMED: &str = "claimed";
pub const KIND_ASSET_INDEX: &str = "asset_index_updated";
pub const KIND_SIGNER_INDEX: &str = "signer_index_updated";
pub const KIND_CONTRACT_INDEX: &str = "contract_index_updated";

/// One row of `shop.rental_event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RentalEvent {
    pub block_number: i64,
    pub log_index: i32,
    pub block_time: i64,
    pub rentals_address: String,
    pub kind: &'static str,
    pub contract_address: Option<String>,
    pub token_id: Option<String>,
    /// The lessor of a rent, the signer of an index bump.
    pub signer: Option<String>,
    pub tenant: Option<String>,
    pub operator: Option<String>,
    pub rental_days: Option<i32>,
    pub price_per_day: Option<String>,
    pub is_extension: Option<bool>,
    pub sender: Option<String>,
    /// The listing signature a rent consumed, lowercase `0x` hex.
    pub signature: Option<String>,
    pub new_index: Option<String>,
    pub tx_hash: String,
}

#[derive(Debug, thiserror::Error)]
#[error("rentals log at block {block} index {index} ({what}): {why}")]
pub struct DecodeError {
    pub block: i64,
    pub index: i32,
    pub what: &'static str,
    pub why: String,
}

fn lower(a: Address) -> String {
    format!("{a:#x}")
}

/// One log as a row; `Ok(None)` for a topic this index does not model.
pub fn decode_log(log: &RpcLog, block_time: i64) -> Result<Option<RentalEvent>, DecodeError> {
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
    let data =
        hex::decode(log.data.trim_start_matches("0x")).map_err(|e| err("data", e.to_string()))?;
    let topic = |i: usize, what: &'static str| {
        log.topics
            .get(i)
            .ok_or_else(|| err(what, format!("needs topic {i}")))
    };
    let base = |kind: &'static str| RentalEvent {
        block_number: block,
        log_index: index,
        block_time,
        rentals_address: log.address.to_ascii_lowercase(),
        kind,
        contract_address: None,
        token_id: None,
        signer: None,
        tenant: None,
        operator: None,
        rental_days: None,
        price_per_day: None,
        is_extension: None,
        sender: None,
        signature: None,
        new_index: None,
        tx_hash: log
            .transaction_hash
            .clone()
            .unwrap_or_default()
            .to_ascii_lowercase(),
    };
    let event = match topic0.as_str() {
        TOPIC_ASSET_RENTED => {
            let contract = topic_address(topic(1, "AssetRented contract")?)
                .ok_or_else(|| err("AssetRented", "bad contract".into()))?;
            let token = topic_u256(topic(2, "AssetRented tokenId")?)
                .ok_or_else(|| err("AssetRented", "bad token id".into()))?;
            type Data = (
                sol_data::Address,
                sol_data::Address,
                sol_data::Address,
                sol_data::Uint<256>,
                sol_data::Uint<256>,
                sol_data::Bool,
                sol_data::Address,
                sol_data::Bytes,
            );
            let (lessor, tenant, operator, days, price, is_extension, sender, signature) =
                <Data>::abi_decode_params(&data).map_err(|e| err("AssetRented", e.to_string()))?;
            RentalEvent {
                contract_address: Some(contract),
                token_id: Some(token),
                signer: Some(lower(lessor)),
                tenant: Some(lower(tenant)),
                operator: Some(lower(operator)),
                rental_days: Some(i32::try_from(days).unwrap_or(i32::MAX)),
                price_per_day: Some(price.to_string()),
                is_extension: Some(is_extension),
                sender: Some(lower(sender)),
                signature: Some(format!("0x{}", hex::encode(signature))),
                ..base(KIND_RENTED)
            }
        }
        TOPIC_ASSET_CLAIMED => {
            let contract = topic_address(topic(1, "AssetClaimed contract")?)
                .ok_or_else(|| err("AssetClaimed", "bad contract".into()))?;
            let token = topic_u256(topic(2, "AssetClaimed tokenId")?)
                .ok_or_else(|| err("AssetClaimed", "bad token id".into()))?;
            let (sender,) = <(sol_data::Address,)>::abi_decode_params(&data)
                .map_err(|e| err("AssetClaimed", e.to_string()))?;
            RentalEvent {
                contract_address: Some(contract),
                token_id: Some(token),
                sender: Some(lower(sender)),
                ..base(KIND_CLAIMED)
            }
        }
        TOPIC_ASSET_INDEX_UPDATED => {
            let signer = topic_address(topic(1, "AssetIndexUpdated signer")?)
                .ok_or_else(|| err("AssetIndexUpdated", "bad signer".into()))?;
            let contract = topic_address(topic(2, "AssetIndexUpdated contract")?)
                .ok_or_else(|| err("AssetIndexUpdated", "bad contract".into()))?;
            let token = topic_u256(topic(3, "AssetIndexUpdated tokenId")?)
                .ok_or_else(|| err("AssetIndexUpdated", "bad token id".into()))?;
            let (new_index, sender) =
                <(sol_data::Uint<256>, sol_data::Address)>::abi_decode_params(&data)
                    .map_err(|e| err("AssetIndexUpdated", e.to_string()))?;
            RentalEvent {
                contract_address: Some(contract),
                token_id: Some(token),
                signer: Some(signer),
                new_index: Some(new_index.to_string()),
                sender: Some(lower(sender)),
                ..base(KIND_ASSET_INDEX)
            }
        }
        TOPIC_SIGNER_INDEX_UPDATED => {
            let signer = topic_address(topic(1, "SignerIndexUpdated signer")?)
                .ok_or_else(|| err("SignerIndexUpdated", "bad signer".into()))?;
            let (new_index, sender) =
                <(sol_data::Uint<256>, sol_data::Address)>::abi_decode_params(&data)
                    .map_err(|e| err("SignerIndexUpdated", e.to_string()))?;
            RentalEvent {
                signer: Some(signer),
                new_index: Some(new_index.to_string()),
                sender: Some(lower(sender)),
                ..base(KIND_SIGNER_INDEX)
            }
        }
        TOPIC_CONTRACT_INDEX_UPDATED => {
            let (new_index, sender) =
                <(sol_data::Uint<256>, sol_data::Address)>::abi_decode_params(&data)
                    .map_err(|e| err("ContractIndexUpdated", e.to_string()))?;
            RentalEvent {
                new_index: Some(new_index.to_string()),
                sender: Some(lower(sender)),
                ..base(KIND_CONTRACT_INDEX)
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// Appends a window's rows (`ON CONFLICT (network, block_number, log_index) DO NOTHING`).
pub async fn insert_events(
    conn: &mut PgConnection,
    net: ChainNet,
    rows: &[RentalEvent],
) -> Result<usize, sqlx::Error> {
    let mut written = 0;
    for chunk in rows.chunks(1_000) {
        let r = sqlx::query(
            "INSERT INTO shop.rental_event \
                 (network, block_number, log_index, block_time, rentals_address, kind, contract_address, token_id, signer, tenant, operator, \
                  rental_days, price_per_day, is_extension, sender, signature, new_index, tx_hash) \
             SELECT $1, b, i, t, ra, k, c, tok::numeric, s, te, o, d, p::numeric, x, se, sg, n::numeric, h \
             FROM UNNEST($2::bigint[], $3::int[], $4::bigint[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[], $10::text[], $11::text[], \
                         $12::int[], $13::text[], $14::boolean[], $15::text[], $16::text[], $17::text[], $18::text[]) \
                  AS u(b, i, t, ra, k, c, tok, s, te, o, d, p, x, se, sg, n, h) \
             ON CONFLICT (network, block_number, log_index) DO NOTHING",
        )
        .bind(net.db_name())
        .bind(chunk.iter().map(|r| r.block_number).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.log_index).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.block_time).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.rentals_address.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.kind.to_string()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.contract_address.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.token_id.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.signer.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.tenant.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.operator.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.rental_days).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.price_per_day.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.is_extension).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.sender.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.signature.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.new_index.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.tx_hash.clone()).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
        written += r.rows_affected() as usize;
    }
    Ok(written)
}

/// The verdict of every touched listing of the venue: the listings whose
/// asset, lessor or contract index saw an event past `$1` (every listing of
/// the venue when `$1 < 0`, the rebuild, which also revisits closed rows so
/// a reorg's rewind restores what the remaining log says). `$2` is the
/// Rentals address, lowercase; the nonces are `[contract, signer, asset]`.
const VERDICT_SQL: &str = "
WITH listing AS (
    SELECT r.id, r.contract_address, r.token_id, lower(r.signature) AS signature, r.status::text AS status,
           r.nonces[1]::numeric AS contract_nonce, r.nonces[2]::numeric AS signer_nonce, r.nonces[3]::numeric AS asset_nonce,
           l.lessor, l.tenant
    FROM signatures.rentals r JOIN signatures.rental_listings l ON l.id = r.id
    WHERE upper(r.network) = 'ETHEREUM' AND r.rental_contract_address = $2
      AND ($1 < 0 OR r.status IN ('open', 'executed'))
),
touched AS (
    SELECT l.* FROM listing l
    WHERE $1 < 0
       OR EXISTS (SELECT 1 FROM shop.rental_event e WHERE e.network = 'ethereum' AND e.rentals_address = $2 AND e.block_number > $1
                    AND e.contract_address = l.contract_address AND e.token_id = l.token_id)
       OR EXISTS (SELECT 1 FROM shop.rental_event e WHERE e.network = 'ethereum' AND e.rentals_address = $2 AND e.block_number > $1
                    AND e.kind = 'signer_index_updated' AND e.signer = l.lessor)
       OR EXISTS (SELECT 1 FROM shop.rental_event e WHERE e.network = 'ethereum' AND e.rentals_address = $2 AND e.block_number > $1
                    AND e.kind = 'contract_index_updated')
)
SELECT t.id, t.status, rented.block_time AS rented_at, rented.tenant AS rented_tenant, rented.rental_days,
       (rented.block_number IS NOT NULL AND EXISTS (
            SELECT 1 FROM shop.rental_event c WHERE c.network = 'ethereum' AND c.rentals_address = $2 AND c.kind = 'claimed'
              AND c.contract_address = t.contract_address AND c.token_id = t.token_id
              AND (c.block_number, c.log_index) > (rented.block_number, rented.log_index))) AS claimed,
       (COALESCE((SELECT max(e.new_index) FROM shop.rental_event e WHERE e.network = 'ethereum' AND e.rentals_address = $2 AND e.kind = 'contract_index_updated'), 0) > t.contract_nonce
        OR COALESCE((SELECT max(e.new_index) FROM shop.rental_event e WHERE e.network = 'ethereum' AND e.rentals_address = $2 AND e.kind = 'signer_index_updated' AND e.signer = t.lessor), 0) > t.signer_nonce
        OR COALESCE((SELECT max(e.new_index) FROM shop.rental_event e WHERE e.network = 'ethereum' AND e.rentals_address = $2 AND e.kind = 'asset_index_updated'
                       AND e.signer = t.lessor AND e.contract_address = t.contract_address AND e.token_id = t.token_id), 0) > t.asset_nonce) AS invalidated
FROM touched t
LEFT JOIN LATERAL (
    SELECT e.block_number, e.log_index, e.block_time, e.tenant, e.rental_days FROM shop.rental_event e
    WHERE e.network = 'ethereum' AND e.rentals_address = $2 AND e.kind = 'rented' AND e.signature = t.signature
      AND e.contract_address = t.contract_address AND e.token_id = t.token_id
    ORDER BY e.block_number, e.log_index LIMIT 1
) rented ON true";

/// One listing's new state, as the fold writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub id: uuid::Uuid,
    pub status: &'static str,
    pub started_at: Option<i64>,
    pub tenant: Option<String>,
    pub rented_days: Option<i32>,
}

pub fn verdict(rented_at: Option<i64>, claimed: bool, invalidated: bool) -> &'static str {
    match (rented_at, claimed, invalidated) {
        (Some(_), true, _) => "claimed",
        (Some(_), false, _) => "executed",
        (None, _, true) => "cancelled",
        (None, _, false) => "open",
    }
}

/// The verdicts of the touched listings that differ from what is stored.
pub async fn changed_verdicts(
    conn: &mut PgConnection,
    venue: &str,
    since: i64,
) -> Result<Vec<Verdict>, sqlx::Error> {
    let rows = sqlx::query(VERDICT_SQL)
        .bind(since)
        .bind(venue)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let rented_at: Option<i64> = r.get("rented_at");
            let status = verdict(rented_at, r.get("claimed"), r.get("invalidated"));
            let current: String = r.get("status");
            let tenant: Option<String> = r.get("rented_tenant");
            let rented_days: Option<i32> = r.get("rental_days");
            (status != current).then(|| Verdict {
                id: r.get("id"),
                status,
                started_at: rented_at,
                tenant,
                rented_days,
            })
        })
        .collect())
}

/// Writes the verdicts: the rental's status, start and days, the listing's
/// tenant; `now` stamps `updated_at`.
pub async fn apply_verdicts(
    conn: &mut PgConnection,
    verdicts: &[Verdict],
    now: NaiveDateTime,
) -> Result<u64, sqlx::Error> {
    if verdicts.is_empty() {
        return Ok(0);
    }
    let ids: Vec<uuid::Uuid> = verdicts.iter().map(|v| v.id).collect();
    let statuses: Vec<String> = verdicts.iter().map(|v| v.status.to_string()).collect();
    let started: Vec<Option<i64>> = verdicts.iter().map(|v| v.started_at).collect();
    let days: Vec<Option<i32>> = verdicts.iter().map(|v| v.rented_days).collect();
    let tenants: Vec<Option<String>> = verdicts.iter().map(|v| v.tenant.clone()).collect();
    let updated = sqlx::query(
        "UPDATE signatures.rentals r SET status = u.status::signatures.rental_status, \
             started_at = COALESCE(to_timestamp(u.started_at) AT TIME ZONE 'UTC', r.started_at), \
             rented_days = COALESCE(u.days, r.rented_days), updated_at = $6 \
         FROM UNNEST($1::uuid[], $2::text[], $3::bigint[], $4::int[], $5::text[]) AS u(id, status, started_at, days, tenant) \
         WHERE r.id = u.id",
    )
    .bind(&ids)
    .bind(&statuses)
    .bind(&started)
    .bind(&days)
    .bind(&tenants)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE signatures.rental_listings l SET tenant = u.tenant \
         FROM UNNEST($1::uuid[], $2::text[]) AS u(id, tenant) WHERE l.id = u.id AND u.tenant IS NOT NULL",
    )
    .bind(&ids)
    .bind(&tenants)
    .execute(&mut *conn)
    .await?;
    Ok(updated)
}

/// The fold: the touched listings' verdicts written in one transaction.
pub async fn fold(
    pool: &PgPool,
    venue: &str,
    since: Option<i64>,
    now: DateTime<Utc>,
) -> anyhow::Result<u64> {
    let since = since.unwrap_or(-1);
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '600s'")
        .execute(&mut *tx)
        .await?;
    chain_walk::analyze_if_stale(&mut tx, "shop", "rental_event", since < 0).await?;
    let verdicts = changed_verdicts(&mut tx, venue, since).await?;
    let written = apply_verdicts(&mut tx, &verdicts, now.naive_utc()).await?;
    tx.commit().await?;
    Ok(written)
}

/// The walk of one Rentals deployment on Ethereum (the mainnet proxy, or a
/// test deployment: the address and the block it was deployed in).
pub struct RentalsIndex {
    pub venue: String,
    pub start: u64,
}

impl RentalsIndex {
    pub fn mainnet() -> Self {
        Self {
            venue: RENTALS_MAINNET.to_string(),
            start: RENTALS_MAINNET_START_BLOCK,
        }
    }

    pub fn at(venue: &str, start: u64) -> Self {
        Self {
            venue: venue.to_ascii_lowercase(),
            start,
        }
    }
}

#[async_trait::async_trait]
impl ChainIndex for RentalsIndex {
    fn indexer(&self) -> &'static str {
        INDEXER_RENTALS
    }

    fn net(&self) -> ChainNet {
        ChainNet::Ethereum
    }

    fn start_block(&self) -> u64 {
        self.start
    }

    async fn groups(
        &self,
        _pool: &PgPool,
        _from: u64,
        _to: u64,
        _anchor: (u64, i64),
    ) -> anyhow::Result<Vec<LogGroup>> {
        Ok(vec![LogGroup {
            addresses: vec![self.venue.clone()],
            topics: json!([ALL_TOPICS]),
        }])
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
            if let Some(row) = decode_log(log, times.get(&block).copied().unwrap_or(0))? {
                rows.push(row);
            }
        }
        Ok(insert_events(conn, ChainNet::Ethereum, &rows).await?)
    }

    async fn rewind(&self, conn: &mut PgConnection, to: i64) -> anyhow::Result<()> {
        // The cursor's cleared fold marker makes the next fold a rebuild,
        // which revisits every listing of the venue from the remaining log.
        sqlx::query("DELETE FROM shop.rental_event WHERE network = 'ethereum' AND rentals_address = $1 AND block_number > $2")
            .bind(&self.venue)
            .bind(to)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    async fn fold(&self, pool: &PgPool, since: Option<i64>, _upto: i64) -> anyhow::Result<bool> {
        let written = fold(pool, &self.venue, since, Utc::now()).await?;
        tracing::info!(venue = %self.venue, listings = written, "rentals index: folded");
        Ok(true)
    }
}

/// Whether the statuses may be trusted at `now` (the cursor is published).
pub async fn ready(pool: &PgPool, now: DateTime<Utc>) -> bool {
    chain_walk::ready_cursor(pool, INDEXER_RENTALS, ChainNet::Ethereum, now)
        .await
        .is_some()
}

/// The index's state for the status route and the tests.
pub async fn status(pool: &PgPool) -> Value {
    let c = chain_walk::cursor(pool, INDEXER_RENTALS, ChainNet::Ethereum)
        .await
        .ok()
        .flatten();
    json!({
        "ethereum": c.map(|c| json!({
            "lastBlock": c.last_block, "foldedBlock": c.folded_block, "fullHistory": c.full_history,
            "ready": chain_walk::is_ready(&c, Utc::now()), "reorgRewinds": c.reorg_rewinds,
        })),
        "venue": RENTALS_MAINNET,
    })
}

/// Attaches `rentals-index` when `enabled` (`SHOP_RENTALS_INDEX_ENABLED`)
/// and a chain-1 JSON-RPC endpoint is configured (`TRADE_RPC_URLS` 1=, or
/// `ETH_RPC_URL` on mainnet), on the market indexes' cadence and budget.
/// Run by `catalyst index`; a fresh cursor walks from the contract's deployment.
pub fn spawn_job(market: &Arc<MarketplaceState>, jobs: &Jobs, enabled: bool) {
    if !enabled {
        tracing::info!(
            "rentals index off (SHOP_RENTALS_INDEX_ENABLED=false): rental listings keep the status they were posted with"
        );
        return;
    }
    // The venue is mainnet's: off mainnet ETH_RPC_URL serves another chain.
    let Some(url) = market.venue_rpc_url(1) else {
        tracing::warn!(
            "rentals index enabled but no chain-1 JSON-RPC endpoint is configured (TRADE_RPC_URLS 1=, or ETH_RPC_URL on mainnet): it stays off"
        );
        return;
    };
    let settings = market.settings.market.indexes.clone();
    let market = market.clone();
    jobs.spawn_periodic_leased(
        "rentals-index",
        PeriodicCfg::every(settings.interval)
            .with_jitter(Duration::from_secs(20))
            .with_error_backoff(Duration::from_secs(1800)),
        move |ctx| {
            let (market, url, settings) = (market.clone(), url.clone(), settings.clone());
            async move {
                let rpc = crate::chain_index::rpc::Rpc::at(&market.http, &url);
                crate::market::ensure_chain(&rpc, 1).await?;
                let progress = |m: String| ctx.progress(m);
                let outcome = chain_walk::run_pass(
                    &market.pool,
                    &rpc,
                    &RentalsIndex::mainnet(),
                    &settings,
                    Some(&ctx.cancel),
                    &progress,
                )
                .await?;
                tracing::info!(summary = %outcome.summary(), "rentals index pass");
                ctx.progress(outcome.summary());
                Ok(())
            }
        },
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_primitives::{U256, keccak256};
    use alloy_sol_types::SolValue;

    #[test]
    fn the_topics_are_the_keccak_of_the_pinned_abis_events() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures/shop/rentals-graph@ddb426725/source/abis/Rentals.json"
        );
        let abi: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let signature = |name: &str| -> String {
            let e = abi
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["type"] == "event" && e["name"] == name)
                .unwrap();
            let args: Vec<&str> = e["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["type"].as_str().unwrap())
                .collect();
            format!("{name}({})", args.join(","))
        };
        for (name, topic) in [
            ("AssetRented", TOPIC_ASSET_RENTED),
            ("AssetClaimed", TOPIC_ASSET_CLAIMED),
            ("AssetIndexUpdated", TOPIC_ASSET_INDEX_UPDATED),
            ("SignerIndexUpdated", TOPIC_SIGNER_INDEX_UPDATED),
            ("ContractIndexUpdated", TOPIC_CONTRACT_INDEX_UPDATED),
        ] {
            assert_eq!(
                format!("{:#x}", keccak256(signature(name))),
                topic,
                "{name}"
            );
        }
        let mainnet: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../tests/fixtures/shop/rentals-graph@ddb426725/source/config/mainnet.json"
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(mainnet["address"], RENTALS_MAINNET);
        assert_eq!(mainnet["start_block"], RENTALS_MAINNET_START_BLOCK);
    }

    pub(crate) fn topic_word(v: U256) -> String {
        format!("0x{v:064x}")
    }

    pub(crate) fn topic_addr(a: &str) -> String {
        format!("0x{:0>64}", a.trim_start_matches("0x"))
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
            block_timestamp: Some(format!("0x{:x}", 1_700_000_000 + block)),
            block_hash: None,
            transaction_hash: Some(format!("0x{:064x}", block * 1000 + index)),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "fixture: one argument per Rented log field"
    )]
    pub(crate) fn rented_log(
        contract: &str,
        token: U256,
        lessor: &str,
        tenant: &str,
        days: u64,
        price: U256,
        signature: &[u8],
        block: u64,
        index: u64,
    ) -> RpcLog {
        let data = (
            lessor.parse::<Address>().unwrap(),
            tenant.parse::<Address>().unwrap(),
            tenant.parse::<Address>().unwrap(),
            U256::from(days),
            price,
            false,
            tenant.parse::<Address>().unwrap(),
            alloy_primitives::Bytes::copy_from_slice(signature),
        )
            .abi_encode_params();
        log(
            RENTALS_MAINNET,
            vec![
                TOPIC_ASSET_RENTED.into(),
                topic_addr(contract),
                topic_word(token),
            ],
            data,
            block,
            index,
        )
    }

    pub(crate) fn claimed_log(
        contract: &str,
        token: U256,
        sender: &str,
        block: u64,
        index: u64,
    ) -> RpcLog {
        let data = (sender.parse::<Address>().unwrap(),).abi_encode_params();
        log(
            RENTALS_MAINNET,
            vec![
                TOPIC_ASSET_CLAIMED.into(),
                topic_addr(contract),
                topic_word(token),
            ],
            data,
            block,
            index,
        )
    }

    pub(crate) fn asset_index_log(
        signer: &str,
        contract: &str,
        token: U256,
        new_index: u64,
        block: u64,
        index: u64,
    ) -> RpcLog {
        let data = (U256::from(new_index), signer.parse::<Address>().unwrap()).abi_encode_params();
        log(
            RENTALS_MAINNET,
            vec![
                TOPIC_ASSET_INDEX_UPDATED.into(),
                topic_addr(signer),
                topic_addr(contract),
                topic_word(token),
            ],
            data,
            block,
            index,
        )
    }

    pub(crate) fn signer_index_log(signer: &str, new_index: u64, block: u64, index: u64) -> RpcLog {
        let data = (U256::from(new_index), signer.parse::<Address>().unwrap()).abi_encode_params();
        log(
            RENTALS_MAINNET,
            vec![TOPIC_SIGNER_INDEX_UPDATED.into(), topic_addr(signer)],
            data,
            block,
            index,
        )
    }

    pub(crate) fn contract_index_log(new_index: u64, block: u64, index: u64) -> RpcLog {
        let data = (U256::from(new_index), Address::repeat_byte(0x01)).abi_encode_params();
        log(
            RENTALS_MAINNET,
            vec![TOPIC_CONTRACT_INDEX_UPDATED.into()],
            data,
            block,
            index,
        )
    }

    const LAND: &str = "0xf87e31492faf9a91b02ee0deaad50d51d56d5d4d";
    const LESSOR: &str = "0x00000000000000000000000000000000000000a1";
    const TENANT: &str = "0x00000000000000000000000000000000000000b2";

    #[test]
    fn every_event_decodes_and_an_unknown_topic_is_skipped() {
        let sig = [0xabu8; 65];
        let ev = decode_log(
            &rented_log(
                LAND,
                U256::from(42u8),
                LESSOR,
                TENANT,
                30,
                U256::from(5u8),
                &sig,
                10,
                1,
            ),
            77,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                ev.kind,
                ev.contract_address.as_deref(),
                ev.token_id.as_deref()
            ),
            (KIND_RENTED, Some(LAND), Some("42"))
        );
        assert_eq!(
            (
                ev.signer.as_deref(),
                ev.tenant.as_deref(),
                ev.rental_days,
                ev.price_per_day.as_deref()
            ),
            (Some(LESSOR), Some(TENANT), Some(30), Some("5"))
        );
        assert_eq!(
            ev.signature.as_deref(),
            Some(format!("0x{}", "ab".repeat(65)).as_str())
        );
        assert_eq!(
            (ev.is_extension, ev.block_time, ev.rentals_address.as_str()),
            (Some(false), 77, RENTALS_MAINNET)
        );
        let ev = decode_log(&claimed_log(LAND, U256::from(42u8), LESSOR, 11, 0), 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            (ev.kind, ev.sender.as_deref()),
            (KIND_CLAIMED, Some(LESSOR))
        );
        let ev = decode_log(
            &asset_index_log(LESSOR, LAND, U256::from(42u8), 3, 12, 0),
            1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                ev.kind,
                ev.signer.as_deref(),
                ev.new_index.as_deref(),
                ev.token_id.as_deref()
            ),
            (KIND_ASSET_INDEX, Some(LESSOR), Some("3"), Some("42"))
        );
        let ev = decode_log(&signer_index_log(LESSOR, 2, 13, 0), 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            (ev.kind, ev.new_index.as_deref(), ev.contract_address),
            (KIND_SIGNER_INDEX, Some("2"), None)
        );
        let ev = decode_log(&contract_index_log(1, 14, 0), 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            (ev.kind, ev.new_index.as_deref()),
            (KIND_CONTRACT_INDEX, Some("1"))
        );
        assert!(
            decode_log(
                &log(
                    RENTALS_MAINNET,
                    vec![format!("0x{}", "00".repeat(32))],
                    vec![],
                    1,
                    0
                ),
                1
            )
            .unwrap()
            .is_none()
        );
        assert!(
            decode_log(
                &log(
                    RENTALS_MAINNET,
                    vec![TOPIC_ASSET_CLAIMED.into(), topic_addr(LAND)],
                    vec![],
                    1,
                    0
                ),
                1
            )
            .is_err(),
            "a truncated log is an error"
        );
    }

    #[test]
    fn the_verdict_order_is_rent_over_bump_then_claim() {
        assert_eq!(verdict(None, false, false), "open");
        assert_eq!(verdict(None, false, true), "cancelled");
        assert_eq!(
            verdict(Some(1), false, true),
            "executed",
            "the rent's own index bump does not cancel it"
        );
        assert_eq!(verdict(Some(1), true, true), "claimed");
        assert_eq!(
            verdict(None, true, false),
            "open",
            "a claim of an earlier rent says nothing about a new listing"
        );
    }

    async fn seed_listing(
        pool: &PgPool,
        contract: &str,
        token: &str,
        nonces: [&str; 3],
        signature: &str,
        lessor: &str,
        venue: &str,
    ) -> uuid::Uuid {
        sqlx::query("INSERT INTO signatures.rental_assets (contract_address, token_id, nft_id, network, chain_id, category, search_text, nft_created_at, nft_updated_at, fetched_at) VALUES ($1, $2::numeric, $3, 'ETHEREUM', 1, 'parcel', 'p', timezone('UTC', now()), timezone('UTC', now()), timezone('UTC', now())) ON CONFLICT DO NOTHING")
            .bind(contract)
            .bind(token)
            .bind(format!("{contract}-{token}"))
            .execute(pool)
            .await
            .unwrap();
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO signatures.rentals (contract_address, token_id, network, chain_id, expiration, nonces, signature, rental_contract_address, status) \
             VALUES ($1, $2::numeric, 'ETHEREUM', 1, timezone('UTC', now()) + interval '30 days', $3, $4, $5, 'open') RETURNING id",
        )
        .bind(contract)
        .bind(token)
        .bind(nonces.iter().map(|n| n.to_string()).collect::<Vec<_>>())
        .bind(signature)
        .bind(venue)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO signatures.rental_listings (id, lessor) VALUES ($1, $2)")
            .bind(id)
            .bind(lessor)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO signatures.rental_periods (rental_id, min_days, max_days, price_per_day) VALUES ($1, 1, 30, 5)").bind(id).execute(pool).await.unwrap();
        id
    }

    async fn status_of(
        pool: &PgPool,
        id: uuid::Uuid,
    ) -> (String, Option<String>, Option<i32>, bool) {
        let r = sqlx::query("SELECT r.status::text AS status, l.tenant, r.rented_days, r.started_at IS NOT NULL AS started FROM signatures.rentals r JOIN signatures.rental_listings l ON l.id = r.id WHERE r.id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        (
            r.get("status"),
            r.get("tenant"),
            r.get("rented_days"),
            r.get("started"),
        )
    }

    /// Four listings of one venue: rented by its own signature (executed,
    /// then claimed), killed by an asset bump, killed by a signer bump, and
    /// one untouched; an incremental fold only revisits what moved; the
    /// rebuild revisits closed rows too.
    #[tokio::test]
    async fn the_fold_moves_listings_through_executed_claimed_and_cancelled() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let tag = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let land = format!("0x{}{}", &tag[..24], "0000000000000000");
        let sig = |n: u8| format!("0x{}", hex::encode([n; 65]));
        let lessor_a = format!("0x{}a1", &tag[..38]);
        let lessor_b = format!("0x{}b1", &tag[..38]);
        // A venue of its own: the rebuild below revisits every listing of
        // the venue, and sibling tests leave mainnet listings in this database.
        let venue = format!("0x{}", &tag[24..64]);
        let at_venue = |mut e: RentalEvent| {
            e.rentals_address = venue.clone();
            e
        };
        let rented = seed_listing(
            &pool,
            &land,
            "1",
            ["0", "0", "0"],
            &sig(1),
            &lessor_a,
            &venue,
        )
        .await;
        let bumped = seed_listing(
            &pool,
            &land,
            "2",
            ["0", "0", "0"],
            &sig(2),
            &lessor_a,
            &venue,
        )
        .await;
        let signer_bumped = seed_listing(
            &pool,
            &land,
            "3",
            ["0", "0", "0"],
            &sig(3),
            &lessor_b,
            &venue,
        )
        .await;
        let untouched = seed_listing(
            &pool,
            &land,
            "4",
            ["0", "0", "0"],
            &sig(4),
            &lessor_a,
            &venue,
        )
        .await;

        let mut conn = pool.acquire().await.unwrap();
        let rows: Vec<RentalEvent> = [
            rented_log(
                &land,
                U256::from(1u8),
                &lessor_a,
                TENANT,
                7,
                U256::from(5u8),
                &[1u8; 65],
                100,
                0,
            ),
            asset_index_log(&lessor_a, &land, U256::from(1u8), 1, 100, 1),
            asset_index_log(TENANT, &land, U256::from(1u8), 1, 100, 2),
            asset_index_log(&lessor_a, &land, U256::from(2u8), 1, 101, 0),
            signer_index_log(&lessor_b, 1, 102, 0),
        ]
        .iter()
        .map(|l| at_venue(decode_log(l, 1_700_000_000).unwrap().unwrap()))
        .collect();
        insert_events(&mut conn, ChainNet::Ethereum, &rows)
            .await
            .unwrap();
        drop(conn);

        assert_eq!(fold(&pool, &venue, Some(99), Utc::now()).await.unwrap(), 3);
        assert_eq!(
            status_of(&pool, rented).await,
            ("executed".into(), Some(TENANT.into()), Some(7), true)
        );
        assert_eq!(status_of(&pool, bumped).await.0, "cancelled");
        assert_eq!(status_of(&pool, signer_bumped).await.0, "cancelled");
        assert_eq!(status_of(&pool, untouched).await.0, "open");

        let mut conn = pool.acquire().await.unwrap();
        let claim = at_venue(
            decode_log(
                &claimed_log(&land, U256::from(1u8), &lessor_a, 110, 0),
                1_700_000_010,
            )
            .unwrap()
            .unwrap(),
        );
        insert_events(&mut conn, ChainNet::Ethereum, &[claim])
            .await
            .unwrap();
        drop(conn);
        assert_eq!(
            fold(&pool, &venue, Some(102), Utc::now()).await.unwrap(),
            1,
            "only the claimed one moves"
        );
        assert_eq!(status_of(&pool, rented).await.0, "claimed");
        assert_eq!(
            fold(&pool, &venue, Some(110), Utc::now()).await.unwrap(),
            0,
            "nothing new past the fold's block"
        );

        // A reorg rewind: the rent and the claim vanish; the rebuild reopens
        // the listing (the venue's other listings' verdicts still hold).
        sqlx::query("DELETE FROM shop.rental_event WHERE network = 'ethereum' AND contract_address = $1 AND token_id = 1").bind(&land).execute(&pool).await.unwrap();
        assert_eq!(fold(&pool, &venue, None, Utc::now()).await.unwrap(), 1);
        assert_eq!(status_of(&pool, rented).await.0, "open");
        assert_eq!(status_of(&pool, bumped).await.0, "cancelled");

        sqlx::query("DELETE FROM shop.rental_event WHERE network = 'ethereum' AND (contract_address = $1 OR signer IN ($2, $3))").bind(&land).bind(&lessor_a).bind(&lessor_b).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM signatures.rental_assets WHERE contract_address = $1")
            .bind(&land)
            .execute(&pool)
            .await
            .unwrap();
    }
}
