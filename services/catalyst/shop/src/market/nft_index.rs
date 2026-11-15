//! The `nft` index: the ERC-721 `Transfer` logs of every collection the
//! collections mirror knows (Polygon v2 collections, Ethereum v1 ones and
//! their v1 `Issue` logs, which carry the wearable id a v1 token id does
//! not), the DCLRegistrar's names and `NameRegistered` subdomains, the
//! legacy Marketplace orders (`OrderCreated`/`OrderSuccessful`/
//! `OrderCancelled`, both chains) and the LAND and Estate registries'
//! `Update` data (names and descriptions). LAND and estate ownership and
//! estate membership are not walked again: they are land_authz's facts
//! (`land_authz.token_right`, `estate_land`) and the readers join them.
//!
//! Identity is (network, contract, token) throughout; a v2 token id decodes
//! to its item as `token >> 216` (the collection's `blockchainId`) and its
//! issue as the low 216 bits. The log (`shop.nft_event`) is keyed by
//! the chain's position, appended with its cursor advance
//! (`chain_walk::run_pass`), and folded into `shop.nft` (owner = the
//! latest transfer's `to`, the mint the first), `nft_order` (the latest
//! legacy order event per token) and `land_data` (the latest Update per
//! token), each a deterministic function of the log: a touched key is
//! refolded over its whole history, `since = None` rebuilds.
//!
//! The address set grows with the mirror. A collection enters the walk at
//! the first window whose end its `created_at` may precede (an upper bound
//! on the window's time from an exact anchor, `chain_walk::time_at_most`);
//! one the mirror reports only after the walk passed its creation gets a
//! backfill lane (`nft_contract.backfill_from_block`), walked before the
//! main range and refolded whole when it completes. A network publishes only
//! with no backfill owed.

use crate::market::chain_walk::{topic_address, topic_u256};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use alloy_sol_types::{SolType, sol_data};
use anyhow::Context;
use serde_json::json;
use sqlx::{PgConnection, PgPool, Row};

use super::chain_walk::{self, ChainIndex, ChainNet, INDEXER_NFT, LogGroup};
use crate::chain_index::rpc::{Rpc, RpcLog};
use crate::land_authz::{ESTATE_REGISTRY_MAINNET, LAND_REGISTRY_MAINNET, LAND_START_BLOCK_MAINNET};

pub const TOPIC_TRANSFER: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
/// `Issue(address indexed _beneficiary, uint256 indexed _tokenId, bytes32 indexed _wearableIdKey, string _wearableId, uint256 _issuedId)` (v1 collections).
pub const TOPIC_ISSUE_V1: &str =
    "0xdb78fb3a605c85b40cf64f4ddff503d984ed442e5ae01282bd60137db494f80b";
/// `NameRegistered(address indexed _caller, address indexed _beneficiary, bytes32 indexed _labelHash, string _subdomain, uint256 _createdDate)`.
pub const TOPIC_NAME_REGISTERED: &str =
    "0x570313dae523ecb48b1176a4b60272e5ea7ec637f5b2d09983cbc4bf25e7e9e3";
/// `OrderCreated(bytes32 id, uint256 indexed assetId, address indexed seller, address nftAddress, uint256 priceInWei, uint256 expiresAt)`.
pub const TOPIC_ORDER_CREATED: &str =
    "0x84c66c3f7ba4b390e20e8e8233e2a516f3ce34a72749e4f12bd010dfba238039";
/// `OrderSuccessful(bytes32 id, uint256 indexed assetId, address indexed seller, address nftAddress, uint256 totalPrice, address indexed buyer)`.
pub const TOPIC_ORDER_SUCCESSFUL: &str =
    "0x695ec315e8a642a74d450a4505eeea53df699b47a7378c7d752e97d5b16eb9bb";
/// `OrderCancelled(bytes32 id, uint256 indexed assetId, address indexed seller, address nftAddress)`.
pub const TOPIC_ORDER_CANCELLED: &str =
    "0x0325426328de5b91ae4ad8462ad4076de4bcaf4551e81556185cacde5a425c6b";
/// `Update(uint256 indexed assetId, address indexed holder, address indexed operator, string data)` (LAND and Estate registries).
pub const TOPIC_LAND_UPDATE: &str =
    "0x47c705b9219229ad762fca605f08fb024a3415d0ae78af5d319820c72e510414";

pub const KIND_TRANSFER: &str = "transfer";
pub const KIND_ISSUE_V1: &str = "issue_v1";
pub const KIND_NAME_REGISTERED: &str = "name_registered";
pub const KIND_ORDER_CREATED: &str = "order_created";
pub const KIND_ORDER_SUCCESSFUL: &str = "order_successful";
pub const KIND_ORDER_CANCELLED: &str = "order_cancelled";
pub const KIND_LAND_DATA: &str = "land_data";

pub const DCL_REGISTRAR: &str = "0x2a187453064356c898cae034eaed119e1663acb8";
pub const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// The legacy Marketplace deployments per network with their deployment
/// block (`eth_getCode` bisection, 2026-09-25; the squid's address books).
pub fn legacy_marketplaces(net: ChainNet) -> &'static [(&'static str, u64)] {
    match net {
        ChainNet::Ethereum => &[("0x8e5660b4ab70168b5a6feea0e0315cb49c8cd539", 6_496_012)],
        ChainNet::Matic => &[
            ("0x02080031b45a3c67d338dd4a2cc309d28756a160", 15_202_565),
            ("0x480a0f4e360e8964e68858dd231c2922f1df45ef", 22_513_570),
        ],
    }
}

/// The contracts walked from their deployment regardless of the mirror.
pub fn fixed_contracts(net: ChainNet) -> Vec<(&'static str, u64)> {
    let mut out: Vec<(&'static str, u64)> = legacy_marketplaces(net).to_vec();
    if net == ChainNet::Ethereum {
        out.push((DCL_REGISTRAR, 9_412_979));
    }
    out
}

/// Where a full history starts: the LAND registry on Ethereum (its Update
/// data), the squid's collection-factory block on Polygon.
pub fn start_block(net: ChainNet) -> u64 {
    match net {
        ChainNet::Ethereum => LAND_START_BLOCK_MAINNET,
        ChainNet::Matic => 15_202_000,
    }
}

fn collection_topics(net: ChainNet) -> serde_json::Value {
    match net {
        ChainNet::Ethereum => json!([[
            TOPIC_TRANSFER,
            TOPIC_ISSUE_V1,
            TOPIC_NAME_REGISTERED,
            TOPIC_ORDER_CREATED,
            TOPIC_ORDER_SUCCESSFUL,
            TOPIC_ORDER_CANCELLED
        ]]),
        ChainNet::Matic => json!([[
            TOPIC_TRANSFER,
            TOPIC_ORDER_CREATED,
            TOPIC_ORDER_SUCCESSFUL,
            TOPIC_ORDER_CANCELLED
        ]]),
    }
}

/// One row of `shop.nft_event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NftEvent {
    pub block_number: i64,
    pub log_index: i32,
    pub block_time: i64,
    pub kind: &'static str,
    pub emitter: String,
    pub contract_address: String,
    pub token_id: String,
    pub from_address: Option<String>,
    pub to_address: Option<String>,
    pub amount: Option<String>,
    pub expires_at: Option<i64>,
    pub detail: Option<String>,
    /// The transaction of an `order_successful` (NULL on other kinds and on
    /// rows indexed before 0040).
    pub tx_hash: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("log at block {block} index {index} ({what}): {why}")]
pub struct DecodeError {
    pub block: i64,
    pub index: i32,
    pub what: &'static str,
    pub why: String,
}

/// A legacy order's `expiresAt`: the 2018 Ethereum contract took seconds
/// from some clients and milliseconds from others; below 10^11 is seconds.
pub fn expires_ms(raw: u128) -> i64 {
    let v = raw.min(i64::MAX as u128) as i64;
    if v < 100_000_000_000 {
        v.saturating_mul(1000)
    } else {
        v
    }
}

/// One log as a row; `Ok(None)` for a topic this index does not model.
pub fn decode_log(log: &RpcLog, block_time: i64) -> Result<Option<NftEvent>, DecodeError> {
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
    let emitter = log.address.to_ascii_lowercase();
    let data =
        hex::decode(log.data.trim_start_matches("0x")).map_err(|e| err("data", e.to_string()))?;
    let topic = |i: usize, what: &'static str| {
        log.topics
            .get(i)
            .ok_or_else(|| err(what, format!("needs topic {i}")))
    };
    let base = |kind: &'static str, contract: String, token: String| NftEvent {
        block_number: block,
        log_index: index,
        block_time,
        kind,
        emitter: emitter.clone(),
        contract_address: contract,
        token_id: token,
        from_address: None,
        to_address: None,
        amount: None,
        expires_at: None,
        detail: None,
        tx_hash: None,
    };
    let event = match topic0.as_str() {
        TOPIC_TRANSFER => {
            let from = topic_address(topic(1, "Transfer from")?)
                .ok_or_else(|| err("Transfer", "bad from".into()))?;
            let to = topic_address(topic(2, "Transfer to")?)
                .ok_or_else(|| err("Transfer", "bad to".into()))?;
            // The standard form indexes the token id; some v1 collections
            // (and ERC-20s, never in this address set) carry it in the data.
            let token = match log.topics.get(3) {
                Some(t) => topic_u256(t),
                None => <sol_data::Uint<256>>::abi_decode(&data)
                    .ok()
                    .map(|v| v.to_string()),
            }
            .ok_or_else(|| err("Transfer", "no token id".into()))?;
            NftEvent {
                from_address: Some(from),
                to_address: Some(to),
                ..base(KIND_TRANSFER, emitter.clone(), token)
            }
        }
        TOPIC_ISSUE_V1 => {
            let token = topic_u256(topic(2, "Issue tokenId")?)
                .ok_or_else(|| err("Issue", "bad token id".into()))?;
            let (wearable, issued) =
                <(sol_data::String, sol_data::Uint<256>)>::abi_decode_params(&data)
                    .map_err(|e| err("Issue", e.to_string()))?;
            NftEvent {
                to_address: topic_address(topic(1, "Issue beneficiary")?),
                amount: Some(issued.to_string()),
                detail: Some(wearable.trim_end_matches('\0').to_string()),
                ..base(KIND_ISSUE_V1, emitter.clone(), token)
            }
        }
        TOPIC_NAME_REGISTERED => {
            let token = topic_u256(topic(3, "NameRegistered labelHash")?)
                .ok_or_else(|| err("NameRegistered", "bad label".into()))?;
            let (subdomain, _created) =
                <(sol_data::String, sol_data::Uint<256>)>::abi_decode_params(&data)
                    .map_err(|e| err("NameRegistered", e.to_string()))?;
            NftEvent {
                to_address: topic_address(topic(2, "NameRegistered beneficiary")?),
                detail: Some(subdomain.trim_end_matches('\0').to_string()),
                ..base(KIND_NAME_REGISTERED, emitter.clone(), token)
            }
        }
        TOPIC_ORDER_CREATED => {
            let token = topic_u256(topic(1, "OrderCreated assetId")?)
                .ok_or_else(|| err("OrderCreated", "bad asset".into()))?;
            let seller = topic_address(topic(2, "OrderCreated seller")?);
            let (id, nft, price, expires) = <(
                sol_data::FixedBytes<32>,
                sol_data::Address,
                sol_data::Uint<256>,
                sol_data::Uint<256>,
            )>::abi_decode_params(&data)
            .map_err(|e| err("OrderCreated", e.to_string()))?;
            NftEvent {
                from_address: seller,
                amount: Some(price.to_string()),
                expires_at: Some(expires_ms(expires.try_into().unwrap_or(u128::MAX))),
                detail: Some(format!("{id:#x}")),
                ..base(KIND_ORDER_CREATED, format!("{nft:#x}"), token)
            }
        }
        TOPIC_ORDER_SUCCESSFUL => {
            let token = topic_u256(topic(1, "OrderSuccessful assetId")?)
                .ok_or_else(|| err("OrderSuccessful", "bad asset".into()))?;
            let seller = topic_address(topic(2, "OrderSuccessful seller")?);
            let buyer = topic_address(topic(3, "OrderSuccessful buyer")?);
            let (id, nft, price) = <(
                sol_data::FixedBytes<32>,
                sol_data::Address,
                sol_data::Uint<256>,
            )>::abi_decode_params(&data)
            .map_err(|e| err("OrderSuccessful", e.to_string()))?;
            NftEvent {
                from_address: seller,
                to_address: buyer,
                amount: Some(price.to_string()),
                detail: Some(format!("{id:#x}")),
                tx_hash: log
                    .transaction_hash
                    .as_ref()
                    .map(|h| h.to_ascii_lowercase()),
                ..base(KIND_ORDER_SUCCESSFUL, format!("{nft:#x}"), token)
            }
        }
        TOPIC_ORDER_CANCELLED => {
            let token = topic_u256(topic(1, "OrderCancelled assetId")?)
                .ok_or_else(|| err("OrderCancelled", "bad asset".into()))?;
            let seller = topic_address(topic(2, "OrderCancelled seller")?);
            let (id, nft) =
                <(sol_data::FixedBytes<32>, sol_data::Address)>::abi_decode_params(&data)
                    .map_err(|e| err("OrderCancelled", e.to_string()))?;
            NftEvent {
                from_address: seller,
                detail: Some(format!("{id:#x}")),
                ..base(KIND_ORDER_CANCELLED, format!("{nft:#x}"), token)
            }
        }
        TOPIC_LAND_UPDATE => {
            let token = topic_u256(topic(1, "Update assetId")?)
                .ok_or_else(|| err("Update", "bad asset".into()))?;
            let (raw,) = <(sol_data::String,)>::abi_decode_params(&data)
                .map_err(|e| err("Update", e.to_string()))?;
            NftEvent {
                detail: Some(raw.replace('\0', "")),
                ..base(KIND_LAND_DATA, emitter.clone(), token)
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// The registries' data CSV (`0,"name","description",...`): the version
/// must be `0` (squid `buildData`), fields may be quoted with `""` escapes.
pub fn parse_land_data(raw: &str) -> Option<(Option<String>, Option<String>)> {
    let csv = raw.replace('\0', "");
    if !csv.starts_with('0') {
        return None;
    }
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = csv.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut cur)),
            (c, _) => cur.push(c),
        }
    }
    fields.push(cur);
    if fields.first().map(|f| f.trim()) != Some("0") {
        return None;
    }
    let field = |i: usize| {
        fields
            .get(i)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Some((field(1), field(2)))
}

/// Appends a window's rows; a row already kept only gains a missing `tx_hash`.
pub async fn insert_events(
    conn: &mut PgConnection,
    net: ChainNet,
    rows: &[NftEvent],
) -> Result<usize, sqlx::Error> {
    let mut written = 0;
    for chunk in rows.chunks(1_000) {
        let r = sqlx::query(
            "INSERT INTO shop.nft_event \
                 (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, from_address, to_address, amount, expires_at, detail, tx_hash) \
             SELECT $1, b, i, t, k, em, c, tok::numeric, f, tt, a::numeric, x, d, h \
             FROM UNNEST($2::bigint[], $3::int[], $4::bigint[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[], $10::text[], $11::text[], $12::bigint[], $13::text[], $14::text[]) \
                  AS u(b, i, t, k, em, c, tok, f, tt, a, x, d, h) \
             ON CONFLICT (network, block_number, log_index) DO UPDATE SET tx_hash = EXCLUDED.tx_hash \
             WHERE shop.nft_event.tx_hash IS NULL AND EXCLUDED.tx_hash IS NOT NULL",
        )
        .bind(net.db_name())
        .bind(chunk.iter().map(|r| r.block_number).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.log_index).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.block_time).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.kind.to_string()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.emitter.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.contract_address.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.token_id.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.from_address.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.to_address.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.amount.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.expires_at).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.detail.clone()).collect::<Vec<_>>())
        .bind(chunk.iter().map(|r| r.tx_hash.clone()).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
        written += r.rows_affected() as usize;
    }
    Ok(written)
}

const TWO_POW_216: &str = "105312291668557186697918027683670432318895095400549111254310977536";

/// The keys a fold past `$2` touches, counted before the fold decides its
/// join strategy (`NESTED_LOOP_TOUCHED_MAX`); the same two arms as the
/// fold's `touched`.
const COUNT_TOUCHED_SQL: &str = "
SELECT count(*) FROM (
    SELECT contract_address, token_id FROM shop.nft_event
    WHERE network = $1 AND block_number > $2
      AND kind IN ('transfer', 'issue_v1', 'name_registered', 'order_successful')
    UNION
    SELECT contract_address, token_id FROM shop.nft_event
    WHERE network = $1 AND contract_address = ANY($3)
      AND kind IN ('transfer', 'issue_v1', 'name_registered', 'order_successful')
) touched";

/// Up to this many touched keys a fold reads each key's history through
/// `nft_event_token_idx` (nested loops, hash and merge joins off for the
/// transaction). The planner cannot see that: `block_number > $2` is
/// estimated from one histogram over both chains' block numbers, so a pass
/// touching four ethereum keys is estimated at ~200 k and merge-joined
/// against the network's whole token index (1.0-1.4 s per pass on the deployed host;
/// 3 ms with nested loops). Past the threshold the estimate is roughly
/// right and the merge plan wins (2.8 k keys: 1.2 s against 1.7 s), and a
/// rebuild (`since` -1) never counts: it folds everything.
const NESTED_LOOP_TOUCHED_MAX: i64 = 1000;

/// The ownership fold of the touched keys: the transfer, issue, name and
/// sale legs of every token an event past `$2` (or of a contract in `$3`)
/// touched, each read over the token's whole history. The two arms of
/// `touched` are a UNION, not one `OR`: an `OR` across the two indexed
/// columns leaves the primary key only `network = $1` to seek on and filters
/// the network's whole history for every pass (514 k rows and 175 ms on
/// the deployed host for a pass touching four keys); the arms seek `(network,
/// block_number)` and `(network, contract_address)` respectively.
const FOLD_NFT_SQL: &str = "
WITH touched AS (
    SELECT contract_address, token_id FROM shop.nft_event
    WHERE network = $1 AND block_number > $2
      AND kind IN ('transfer', 'issue_v1', 'name_registered', 'order_successful')
    UNION
    SELECT contract_address, token_id FROM shop.nft_event
    WHERE network = $1 AND contract_address = ANY($3)
      AND kind IN ('transfer', 'issue_v1', 'name_registered', 'order_successful')
),
latest_transfer AS (
    SELECT DISTINCT ON (e.contract_address, e.token_id) e.contract_address, e.token_id, e.to_address, e.block_number, e.log_index, e.block_time
    FROM shop.nft_event e JOIN touched t ON (t.contract_address, t.token_id) = (e.contract_address, e.token_id)
    WHERE e.network = $1 AND e.kind = 'transfer'
    ORDER BY e.contract_address, e.token_id, e.block_number DESC, e.log_index DESC
),
first_transfer AS (
    SELECT DISTINCT ON (e.contract_address, e.token_id) e.contract_address, e.token_id, e.block_number, e.block_time
    FROM shop.nft_event e JOIN touched t ON (t.contract_address, t.token_id) = (e.contract_address, e.token_id)
    WHERE e.network = $1 AND e.kind = 'transfer'
    ORDER BY e.contract_address, e.token_id, e.block_number ASC, e.log_index ASC
),
issue AS (
    SELECT DISTINCT ON (e.contract_address, e.token_id) e.contract_address, e.token_id, e.detail AS wearable_id, e.amount AS issued_id
    FROM shop.nft_event e JOIN touched t ON (t.contract_address, t.token_id) = (e.contract_address, e.token_id)
    WHERE e.network = $1 AND e.kind = 'issue_v1'
    ORDER BY e.contract_address, e.token_id, e.block_number DESC, e.log_index DESC
),
name AS (
    SELECT DISTINCT ON (e.contract_address, e.token_id) e.contract_address, e.token_id, e.detail AS subdomain
    FROM shop.nft_event e JOIN touched t ON (t.contract_address, t.token_id) = (e.contract_address, e.token_id)
    WHERE e.network = $1 AND e.kind = 'name_registered'
    ORDER BY e.contract_address, e.token_id, e.block_number DESC, e.log_index DESC
),
sold AS (
    SELECT e.contract_address, e.token_id, max(e.block_time) AS sold_at
    FROM shop.nft_event e JOIN touched t ON (t.contract_address, t.token_id) = (e.contract_address, e.token_id)
    WHERE e.network = $1 AND e.kind = 'order_successful'
    GROUP BY e.contract_address, e.token_id
)
INSERT INTO shop.nft
    (network, contract_address, token_id, owner, item_id, issued_id, wearable_id, ens_subdomain,
     minted_block, minted_at, transferred_block, transferred_log, transferred_at, sold_at)
SELECT $1, lt.contract_address, lt.token_id, NULLIF(lt.to_address, $4),
       CASE WHEN $1 = 'matic' THEN div(lt.token_id, $5::numeric) END,
       CASE WHEN $1 = 'matic' THEN mod(lt.token_id, $5::numeric) ELSE i.issued_id END,
       i.wearable_id, n.subdomain, ft.block_number, ft.block_time, lt.block_number, lt.log_index, lt.block_time, s.sold_at
FROM latest_transfer lt
JOIN first_transfer ft ON (ft.contract_address, ft.token_id) = (lt.contract_address, lt.token_id)
LEFT JOIN issue i ON (i.contract_address, i.token_id) = (lt.contract_address, lt.token_id)
LEFT JOIN name n ON (n.contract_address, n.token_id) = (lt.contract_address, lt.token_id)
LEFT JOIN sold s ON (s.contract_address, s.token_id) = (lt.contract_address, lt.token_id)
ON CONFLICT (network, contract_address, token_id) DO UPDATE SET
    owner = EXCLUDED.owner, item_id = EXCLUDED.item_id, issued_id = EXCLUDED.issued_id,
    wearable_id = EXCLUDED.wearable_id, ens_subdomain = EXCLUDED.ens_subdomain,
    minted_block = EXCLUDED.minted_block, minted_at = EXCLUDED.minted_at,
    transferred_block = EXCLUDED.transferred_block, transferred_log = EXCLUDED.transferred_log,
    transferred_at = EXCLUDED.transferred_at, sold_at = EXCLUDED.sold_at";

/// The legacy order fold: the latest order event per touched token decides
/// the status; the order it names supplies the listing's price, expiry and
/// creation time.
const FOLD_ORDER_SQL: &str = "
WITH touched AS (
    SELECT DISTINCT contract_address, token_id FROM shop.nft_event
    WHERE network = $1 AND block_number > $2 AND kind IN ('order_created', 'order_successful', 'order_cancelled')
),
latest AS (
    SELECT DISTINCT ON (e.contract_address, e.token_id) e.*
    FROM shop.nft_event e JOIN touched t ON (t.contract_address, t.token_id) = (e.contract_address, e.token_id)
    WHERE e.network = $1 AND e.kind IN ('order_created', 'order_successful', 'order_cancelled')
    ORDER BY e.contract_address, e.token_id, e.block_number DESC, e.log_index DESC
),
created AS (
    SELECT DISTINCT ON (e.contract_address, e.token_id, e.detail) e.contract_address, e.token_id, e.detail, e.amount, e.expires_at, e.block_time
    FROM shop.nft_event e JOIN touched t ON (t.contract_address, t.token_id) = (e.contract_address, e.token_id)
    WHERE e.network = $1 AND e.kind = 'order_created'
    ORDER BY e.contract_address, e.token_id, e.detail, e.block_number DESC, e.log_index DESC
)
INSERT INTO shop.nft_order
    (network, contract_address, token_id, order_id, marketplace_address, status, seller, buyer, price, expires_at,
     created_at, updated_at, updated_block, updated_log)
SELECT $1, l.contract_address, l.token_id, l.detail, l.emitter,
       CASE l.kind WHEN 'order_created' THEN 'open' WHEN 'order_successful' THEN 'sold' ELSE 'cancelled' END,
       COALESCE(l.from_address, $3), CASE WHEN l.kind = 'order_successful' THEN l.to_address END,
       COALESCE(c.amount, l.amount, 0), COALESCE(c.expires_at, l.expires_at, 0),
       COALESCE(c.block_time, l.block_time), l.block_time, l.block_number, l.log_index
FROM latest l
LEFT JOIN created c ON (c.contract_address, c.token_id, c.detail) = (l.contract_address, l.token_id, l.detail)
ON CONFLICT (network, contract_address, token_id) DO UPDATE SET
    order_id = EXCLUDED.order_id, marketplace_address = EXCLUDED.marketplace_address, status = EXCLUDED.status,
    seller = EXCLUDED.seller, buyer = EXCLUDED.buyer, price = EXCLUDED.price, expires_at = EXCLUDED.expires_at,
    created_at = EXCLUDED.created_at, updated_at = EXCLUDED.updated_at,
    updated_block = EXCLUDED.updated_block, updated_log = EXCLUDED.updated_log";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldCounts {
    pub nfts: u64,
    pub orders: u64,
    pub land_data: u64,
}

/// The three folds of `net` in one transaction: the keys an event past
/// `since` touched (`None`: every key) and every token of a contract whose
/// backfill completed since (`refold_owed`, cleared here), each over its
/// whole history.
pub async fn fold(pool: &PgPool, net: ChainNet, since: Option<i64>) -> anyhow::Result<FoldCounts> {
    let since = since.unwrap_or(-1);
    let mut tx = pool.begin().await?;
    // A job, not a request (the land_authz fold's reasons): the rebuild
    // after a catch-up folds millions of rows, and the statistics the bulk
    // insert has not produced yet decide the plan; a steady-state fold keeps
    // the ones autovacuum maintains (`chain_walk::analyze_if_stale`).
    sqlx::query("SET LOCAL statement_timeout = '3600s'")
        .execute(&mut *tx)
        .await?;
    super::chain_walk::analyze_if_stale(&mut tx, "shop", "nft_event", since < 0).await?;
    let contracts: Vec<String> = sqlx::query_scalar("SELECT contract_address FROM shop.nft_contract WHERE network = $1 AND refold_owed FOR UPDATE")
        .bind(net.db_name())
        .fetch_all(&mut *tx)
        .await?;
    if since >= 0 {
        let touched: i64 = sqlx::query_scalar(COUNT_TOUCHED_SQL)
            .bind(net.db_name())
            .bind(since)
            .bind(&contracts)
            .fetch_one(&mut *tx)
            .await?;
        if touched <= NESTED_LOOP_TOUCHED_MAX {
            sqlx::query("SET LOCAL enable_mergejoin = off")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL enable_hashjoin = off")
                .execute(&mut *tx)
                .await?;
        }
    }
    let nfts = sqlx::query(FOLD_NFT_SQL)
        .bind(net.db_name())
        .bind(since)
        .bind(&contracts)
        .bind(ZERO_ADDRESS)
        .bind(TWO_POW_216)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let orders = sqlx::query(FOLD_ORDER_SQL)
        .bind(net.db_name())
        .bind(since)
        .bind(ZERO_ADDRESS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let land_data = if net == ChainNet::Ethereum {
        fold_land_data(&mut tx, since).await?
    } else {
        0
    };
    if !contracts.is_empty() {
        sqlx::query("UPDATE shop.nft_contract SET refold_owed = false WHERE network = $1 AND contract_address = ANY($2)")
            .bind(net.db_name())
            .bind(&contracts)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(FoldCounts {
        nfts,
        orders,
        land_data,
    })
}

/// The latest Update per touched LAND/estate token, parsed in Rust (the CSV
/// has quoting SQL would re-implement badly) and upserted.
async fn fold_land_data(conn: &mut PgConnection, since: i64) -> anyhow::Result<u64> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (e.contract_address, e.token_id) e.contract_address, e.token_id::text AS token_id, e.detail, e.block_number, e.log_index \
         FROM shop.nft_event e \
         WHERE e.network = 'ethereum' AND e.kind = 'land_data' \
           AND (e.contract_address, e.token_id) IN (SELECT contract_address, token_id FROM shop.nft_event \
                                                     WHERE network = 'ethereum' AND kind = 'land_data' AND block_number > $1) \
         ORDER BY e.contract_address, e.token_id, e.block_number DESC, e.log_index DESC",
    )
    .bind(since)
    .fetch_all(&mut *conn)
    .await?;
    type Columns = (
        Vec<String>,
        Vec<String>,
        Vec<Option<String>>,
        Vec<Option<String>>,
        Vec<i64>,
        Vec<i32>,
    );
    let mut cols: Columns = Default::default();
    for r in &rows {
        let (name, description) = r
            .get::<Option<String>, _>("detail")
            .as_deref()
            .and_then(parse_land_data)
            .unwrap_or((None, None));
        cols.0.push(r.get("contract_address"));
        cols.1.push(r.get("token_id"));
        cols.2.push(name);
        cols.3.push(description);
        cols.4.push(r.get("block_number"));
        cols.5.push(r.get("log_index"));
    }
    let mut written = 0;
    for start in (0..cols.0.len()).step_by(1_000) {
        let end = (start + 1_000).min(cols.0.len());
        written += sqlx::query(
            "INSERT INTO shop.land_data (token_address, token_id, name, description, updated_block, updated_log) \
             SELECT a, t::numeric, n, d, b, l FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[], $6::int[]) AS u(a, t, n, d, b, l) \
             ON CONFLICT (token_address, token_id) DO UPDATE SET name = EXCLUDED.name, description = EXCLUDED.description, \
                 updated_block = EXCLUDED.updated_block, updated_log = EXCLUDED.updated_log",
        )
        .bind(&cols.0[start..end])
        .bind(&cols.1[start..end])
        .bind(&cols.2[start..end])
        .bind(&cols.3[start..end])
        .bind(&cols.4[start..end])
        .bind(&cols.5[start..end])
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(written)
}

/// A mirrored collection the walk may take in.
#[derive(Clone, Debug)]
struct Collection {
    address: String,
    created_at: i64,
}

/// A walked contract (`shop.nft_contract`).
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct WalkedContract {
    pub contract_address: String,
    pub walked_from_block: i64,
    pub backfill_from_block: Option<i64>,
    pub backfilled_to_block: Option<i64>,
}

impl WalkedContract {
    /// The range the backfill lane still owes, if any.
    pub fn owed(&self) -> Option<(i64, i64)> {
        let from = self.backfill_from_block?;
        let next = self.backfilled_to_block.map_or(from, |b| b + 1);
        (next < self.walked_from_block).then_some((next, self.walked_from_block - 1))
    }
}

/// One network's side of the walk. `pending` carries the contracts a
/// window's groups took in for the first time to the `write` of the same
/// window (the same transaction records them).
pub struct NftIndex {
    pub net: ChainNet,
    pending: Mutex<Vec<(String, i64, Option<i64>)>>,
    collections: Mutex<Option<Vec<Collection>>>,
    walked: Mutex<Option<HashMap<String, i64>>>,
    /// The cursor's start: nothing before it is the walk's (a bounded
    /// window's history, or none at all for a full one).
    walk_start: Mutex<Option<u64>>,
}

impl NftIndex {
    pub fn new(net: ChainNet) -> Self {
        Self {
            net,
            pending: Mutex::new(Vec::new()),
            collections: Mutex::new(None),
            walked: Mutex::new(None),
            walk_start: Mutex::new(None),
        }
    }

    /// The mirrored collections of this network with their creation time,
    /// read once per pass (the address set's source).
    async fn load(&self, pool: &PgPool) -> anyhow::Result<(Vec<Collection>, HashMap<String, i64>)> {
        if let (Some(c), Some(w)) = (
            self.collections.lock().unwrap().clone(),
            self.walked.lock().unwrap().clone(),
        ) {
            return Ok((c, w));
        }
        let rows = sqlx::query("SELECT contract_address, created_at FROM shop.collections WHERE network = $1 ORDER BY created_at, contract_address")
            .bind(self.net.db_name())
            .fetch_all(pool)
            .await?;
        let collections: Vec<Collection> = rows
            .iter()
            .map(|r| Collection {
                address: r.get("contract_address"),
                created_at: r.get("created_at"),
            })
            .collect();
        let walked: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
            "SELECT contract_address, walked_from_block FROM shop.nft_contract WHERE network = $1",
        )
        .bind(self.net.db_name())
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
        let start = chain_walk::cursor(pool, INDEXER_NFT, self.net)
            .await?
            .map_or(start_block(self.net), |c| c.start_block.max(0) as u64);
        *self.walk_start.lock().unwrap() = Some(start);
        *self.collections.lock().unwrap() = Some(collections.clone());
        *self.walked.lock().unwrap() = Some(walked.clone());
        Ok((collections, walked))
    }

    /// Forget the per-pass caches (a new pass rereads the mirror).
    pub fn reset(&self) {
        *self.collections.lock().unwrap() = None;
        *self.walked.lock().unwrap() = None;
        *self.walk_start.lock().unwrap() = None;
        self.pending.lock().unwrap().clear();
    }
}

/// Whether the collections mirror has completed a pass for `net`: the
/// address set is only as complete as it.
pub async fn mirror_complete(pool: &PgPool, net: ChainNet) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT last_complete_pass_at IS NOT NULL FROM shop.mirror_state WHERE mirror = 'collections' AND network = $1")
        .bind(net.db_name())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl ChainIndex for NftIndex {
    fn indexer(&self) -> &'static str {
        INDEXER_NFT
    }

    fn net(&self) -> ChainNet {
        self.net
    }

    fn start_block(&self) -> u64 {
        start_block(self.net)
    }

    async fn groups(
        &self,
        pool: &PgPool,
        from: u64,
        to: u64,
        anchor: (u64, i64),
    ) -> anyhow::Result<Vec<LogGroup>> {
        let (collections, walked) = self.load(pool).await?;
        let walk_start = self
            .walk_start
            .lock()
            .unwrap()
            .unwrap_or(start_block(self.net));
        let bound = chain_walk::time_at_most(self.net, anchor, to);
        let before_from = chain_walk::time_at_most(self.net, anchor, from.saturating_sub(1));
        let mut addresses: Vec<String> = fixed_contracts(self.net)
            .into_iter()
            .filter(|(_, deployed)| *deployed <= to)
            .map(|(a, _)| a.to_string())
            .collect();
        let mut pending = Vec::new();
        for c in &collections {
            if walked.contains_key(&c.address) {
                addresses.push(c.address.clone());
            } else if c.created_at <= bound {
                addresses.push(c.address.clone());
                // Taken in only now: when it may predate this window, the
                // blocks since the walk's start before it are owed by the
                // backfill lane (the first window of a walk owes nothing).
                let backfill = (c.created_at <= before_from && from > walk_start).then(|| {
                    chain_walk::block_at_or_before(self.net, anchor, c.created_at)
                        .max(walk_start)
                        .min(from - 1) as i64
                });
                pending.push((c.address.clone(), from as i64, backfill));
            }
        }
        addresses.sort();
        addresses.dedup();
        *self.pending.lock().unwrap() = pending;
        let mut groups = vec![LogGroup {
            addresses,
            topics: collection_topics(self.net),
        }];
        if self.net == ChainNet::Ethereum {
            groups.push(LogGroup {
                addresses: vec![
                    LAND_REGISTRY_MAINNET.to_string(),
                    ESTATE_REGISTRY_MAINNET.to_string(),
                ],
                topics: json!([[TOPIC_LAND_UPDATE]]),
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
                // A contract of the set emitting a shape outside the model
                // (a non-standard token) is logged and skipped, never a
                // stalled walk; a malformed standard log is the same data.
                Err(e) => {
                    tracing::warn!(error = %e, network = self.net.db_name(), "nft index: skipping a log it cannot decode")
                }
            }
        }
        let written = insert_events(conn, self.net, &rows).await?;
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        if !pending.is_empty() {
            let (addrs, froms, backfills): (Vec<String>, Vec<i64>, Vec<Option<i64>>) = pending
                .iter()
                .cloned()
                .fold((vec![], vec![], vec![]), |mut acc, (a, f, b)| {
                    acc.0.push(a);
                    acc.1.push(f);
                    acc.2.push(b);
                    acc
                });
            sqlx::query(
                "INSERT INTO shop.nft_contract (network, contract_address, walked_from_block, backfill_from_block) \
                 SELECT $1, a, f, b FROM UNNEST($2::text[], $3::bigint[], $4::bigint[]) AS u(a, f, b) \
                 ON CONFLICT (network, contract_address) DO NOTHING",
            )
            .bind(self.net.db_name())
            .bind(&addrs)
            .bind(&froms)
            .bind(&backfills)
            .execute(&mut *conn)
            .await?;
            if let Some(w) = self.walked.lock().unwrap().as_mut() {
                for (a, f, _) in pending {
                    w.entry(a).or_insert(f);
                }
            }
        }
        Ok(written)
    }

    async fn rewind(&self, conn: &mut PgConnection, to: i64) -> anyhow::Result<()> {
        // The deleted rows' tokens are refolded from what remains; a token
        // whose only transfers were reorged out loses its row.
        let gone: Vec<(String, String)> = sqlx::query_as(
            "DELETE FROM shop.nft_event WHERE network = $1 AND block_number > $2 RETURNING contract_address, token_id::text",
        )
        .bind(self.net.db_name())
        .bind(to)
        .fetch_all(&mut *conn)
        .await?;
        let (contracts, tokens): (Vec<String>, Vec<String>) = gone.into_iter().unzip();
        sqlx::query(
            "DELETE FROM shop.nft n USING UNNEST($2::text[], $3::text[]) AS g(c, t) \
             WHERE n.network = $1 AND n.contract_address = g.c AND n.token_id = g.t::numeric",
        )
        .bind(self.net.db_name())
        .bind(&contracts)
        .bind(&tokens)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "DELETE FROM shop.nft_order o USING UNNEST($2::text[], $3::text[]) AS g(c, t) \
             WHERE o.network = $1 AND o.contract_address = g.c AND o.token_id = g.t::numeric",
        )
        .bind(self.net.db_name())
        .bind(&contracts)
        .bind(&tokens)
        .execute(&mut *conn)
        .await?;
        // LAND data is keyed by (token_address, token_id) and folded only for
        // keys with a remaining Update: a key whose Updates were all reorged
        // out would otherwise keep a row past the cursor. The rebuild below
        // restores the rows of every key that still has one.
        if self.net == ChainNet::Ethereum {
            sqlx::query(
                "DELETE FROM shop.land_data l USING UNNEST($1::text[], $2::text[]) AS g(c, t) \
                 WHERE l.token_address = g.c AND l.token_id = g.t::numeric",
            )
            .bind(&contracts)
            .bind(&tokens)
            .execute(&mut *conn)
            .await?;
        }
        // Contracts taken in past the rewind are taken in again.
        sqlx::query("DELETE FROM shop.nft_contract WHERE network = $1 AND walked_from_block > $2")
            .bind(self.net.db_name())
            .bind(to)
            .execute(&mut *conn)
            .await?;
        // The cleared marker makes the next fold a rebuild, which also
        // restores the deleted keys' rows from their remaining history.
        Ok(())
    }

    async fn fold(&self, pool: &PgPool, since: Option<i64>, _upto: i64) -> anyhow::Result<bool> {
        let counts = fold(pool, self.net, since).await?;
        tracing::info!(
            network = self.net.db_name(),
            nfts = counts.nfts,
            orders = counts.orders,
            land_data = counts.land_data,
            "nft index: folded"
        );
        let owed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM shop.nft_contract WHERE network = $1 AND backfill_from_block IS NOT NULL \
               AND COALESCE(backfilled_to_block, backfill_from_block - 1) < walked_from_block - 1",
        )
        .bind(self.net.db_name())
        .fetch_one(pool)
        .await?;
        Ok(owed == 0)
    }

    async fn lanes(
        &self,
        pool: &PgPool,
        rpc: &Rpc<'_>,
        _anchor: (u64, i64),
        budget: usize,
        chunk: &mut usize,
    ) -> anyhow::Result<usize> {
        self.reset();
        let owed: Vec<WalkedContract> = sqlx::query_as(
            "SELECT contract_address, walked_from_block, backfill_from_block, backfilled_to_block FROM shop.nft_contract \
             WHERE network = $1 AND backfill_from_block IS NOT NULL \
               AND COALESCE(backfilled_to_block, backfill_from_block - 1) < walked_from_block - 1 \
             ORDER BY walked_from_block, contract_address",
        )
        .bind(self.net.db_name())
        .fetch_all(pool)
        .await?;
        let mut used = 0usize;
        for contract in owed {
            let Some((mut next, last)) = contract.owed() else {
                continue;
            };
            let group = [LogGroup {
                addresses: vec![contract.contract_address.clone()],
                topics: collection_topics(self.net),
            }];
            while next <= last {
                if used >= budget {
                    return Ok(used);
                }
                let to = (next + 10_000 - 1).min(last);
                let w = chain_walk::fetch_window(rpc, next as u64, to as u64, &group, chunk)
                    .await
                    .with_context(|| {
                        format!("backfill of {} [{next}, {to}]", contract.contract_address)
                    })?;
                used += w.requests;
                let (times, extra) = chain_walk::block_times(rpc, &w.logs).await?;
                used += extra;
                let mut rows = Vec::new();
                for log in &w.logs {
                    if let Ok(Some(row)) = decode_log(
                        log,
                        times
                            .get(&chain_walk::log_position(log).0)
                            .copied()
                            .unwrap_or(0),
                    ) {
                        rows.push(row);
                    }
                }
                let mut tx = pool.begin().await?;
                insert_events(&mut tx, self.net, &rows).await?;
                // Late rows near the head below the producers' cursors are read again.
                let blocks = rows.iter().map(|r| r.block_number);
                if let (Some(first), Some(last)) = (blocks.clone().min(), blocks.max()) {
                    super::notice_cursor::replay(&mut tx, INDEXER_NFT, self.net, first - 1, last)
                        .await?;
                }
                sqlx::query(
                    "UPDATE shop.nft_contract SET backfilled_to_block = $3, refold_owed = ($3 >= walked_from_block - 1) \
                     WHERE network = $1 AND contract_address = $2",
                )
                .bind(self.net.db_name())
                .bind(&contract.contract_address)
                .bind(w.reached as i64)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                next = w.reached as i64 + 1;
            }
            tracing::info!(network = self.net.db_name(), contract = %contract.contract_address, "nft index: backfill complete; its tokens are refolded at the next fold");
        }
        Ok(used)
    }
}

/// The index's state for the operator status route and the tests.
pub async fn status(pool: &PgPool) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for net in ChainNet::ALL {
        let c = chain_walk::cursor(pool, INDEXER_NFT, net)
            .await
            .ok()
            .flatten();
        let owed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM shop.nft_contract WHERE network = $1 AND backfill_from_block IS NOT NULL \
               AND COALESCE(backfilled_to_block, backfill_from_block - 1) < walked_from_block - 1",
        )
        .bind(net.db_name())
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        out.insert(net.db_name().to_string(), json!(c.map(|c| json!({
            "lastBlock": c.last_block, "foldedBlock": c.folded_block, "fullHistory": c.full_history,
            "ready": chain_walk::is_ready(&c, chrono::Utc::now()), "reorgRewinds": c.reorg_rewinds, "backfillsOwed": owed,
        }))));
    }
    serde_json::Value::Object(out)
}

/// The set of networks the nft index may answer for at `now`.
pub async fn ready_networks(
    pool: &PgPool,
    now: chrono::DateTime<chrono::Utc>,
) -> HashSet<ChainNet> {
    let mut out = HashSet::new();
    for net in ChainNet::ALL {
        if chain_walk::ready_cursor(pool, INDEXER_NFT, net, now)
            .await
            .is_some()
        {
            out.insert(net);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_primitives::{Address, FixedBytes, U256, keccak256};
    use alloy_sol_types::SolValue;
    use std::str::FromStr;

    #[test]
    fn the_topics_are_the_keccak_of_their_signatures() {
        for (sig, topic) in [
            ("Transfer(address,address,uint256)", TOPIC_TRANSFER),
            (
                "Issue(address,uint256,bytes32,string,uint256)",
                TOPIC_ISSUE_V1,
            ),
            (
                "NameRegistered(address,address,bytes32,string,uint256)",
                TOPIC_NAME_REGISTERED,
            ),
            (
                "OrderCreated(bytes32,uint256,address,address,uint256,uint256)",
                TOPIC_ORDER_CREATED,
            ),
            (
                "OrderSuccessful(bytes32,uint256,address,address,uint256,address)",
                TOPIC_ORDER_SUCCESSFUL,
            ),
            (
                "OrderCancelled(bytes32,uint256,address,address)",
                TOPIC_ORDER_CANCELLED,
            ),
            ("Update(uint256,address,address,string)", TOPIC_LAND_UPDATE),
        ] {
            assert_eq!(format!("{:#x}", keccak256(sig)), topic, "{sig}");
        }
        assert_eq!(U256::from(1u8) << 216, U256::from_str(TWO_POW_216).unwrap());
    }

    pub(crate) fn topic_word(v: U256) -> String {
        format!("0x{:064x}", v)
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
            block_timestamp: Some(format!("0x{:x}", 1_600_000_000 + block)),
            block_hash: None,
            transaction_hash: None,
        }
    }

    pub(crate) fn transfer_log(
        contract: &str,
        from: &str,
        to: &str,
        token: U256,
        block: u64,
        index: u64,
    ) -> RpcLog {
        log(
            contract,
            vec![
                TOPIC_TRANSFER.into(),
                topic_addr(from),
                topic_addr(to),
                topic_word(token),
            ],
            vec![],
            block,
            index,
        )
    }

    #[test]
    fn a_transfer_decodes_with_the_token_in_a_topic_or_in_the_data() {
        let c = "0x00000000000000000000000000000000000000c1";
        let a = "0x00000000000000000000000000000000000000a1";
        let ev = decode_log(&transfer_log(c, ZERO_ADDRESS, a, U256::from(5u8), 10, 1), 7)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                ev.kind,
                ev.token_id.as_str(),
                ev.to_address.as_deref(),
                ev.from_address.as_deref()
            ),
            (KIND_TRANSFER, "5", Some(a), Some(ZERO_ADDRESS))
        );
        let legacy = log(
            c,
            vec![
                TOPIC_TRANSFER.into(),
                topic_addr(a),
                topic_addr(ZERO_ADDRESS),
            ],
            U256::from(9u8).abi_encode(),
            11,
            0,
        );
        assert_eq!(decode_log(&legacy, 7).unwrap().unwrap().token_id, "9");
    }

    #[test]
    fn issue_name_orders_and_land_data_decode() {
        let c = "0x00000000000000000000000000000000000000c1";
        let a = "0x00000000000000000000000000000000000000a1";
        let issue = log(
            c,
            vec![
                TOPIC_ISSUE_V1.into(),
                topic_addr(a),
                topic_word(U256::ZERO),
                topic_word(U256::from(3u8)),
            ],
            ("razor_blade_upper_body".to_string(), U256::from(1u8)).abi_encode_params(),
            20,
            0,
        );
        let ev = decode_log(&issue, 1).unwrap().unwrap();
        assert_eq!(
            (
                ev.kind,
                ev.token_id.as_str(),
                ev.detail.as_deref(),
                ev.amount.as_deref()
            ),
            (
                KIND_ISSUE_V1,
                "0",
                Some("razor_blade_upper_body"),
                Some("1")
            )
        );
        let label = keccak256("pixelgolf");
        let name = log(
            DCL_REGISTRAR,
            vec![
                TOPIC_NAME_REGISTERED.into(),
                topic_addr(a),
                topic_addr(a),
                format!("{label:#x}"),
            ],
            ("pixelgolf".to_string(), U256::from(1u8)).abi_encode_params(),
            21,
            0,
        );
        let ev = decode_log(&name, 1).unwrap().unwrap();
        assert_eq!(
            (
                ev.contract_address.as_str(),
                ev.token_id.clone(),
                ev.detail.as_deref()
            ),
            (
                DCL_REGISTRAR,
                U256::from_be_bytes(label.0).to_string(),
                Some("pixelgolf")
            )
        );
        let market = legacy_marketplaces(ChainNet::Ethereum)[0].0;
        let nft = Address::from_str(LAND_REGISTRY_MAINNET).unwrap();
        let id = FixedBytes::<32>::repeat_byte(0x42);
        let created = log(
            market,
            vec![
                TOPIC_ORDER_CREATED.into(),
                topic_word(U256::from(77u8)),
                topic_addr(a),
            ],
            (id, nft, U256::from(10u8), U256::from(1_600_000_000u64)).abi_encode_params(),
            22,
            3,
        );
        let ev = decode_log(&created, 1).unwrap().unwrap();
        assert_eq!(
            (
                ev.kind,
                ev.emitter.as_str(),
                ev.contract_address.as_str(),
                ev.token_id.as_str()
            ),
            (KIND_ORDER_CREATED, market, LAND_REGISTRY_MAINNET, "77")
        );
        assert_eq!(
            (
                ev.amount.as_deref(),
                ev.expires_at,
                ev.from_address.as_deref()
            ),
            (Some("10"), Some(1_600_000_000_000), Some(a))
        );
        assert_eq!(
            ev.detail.as_deref(),
            Some(format!("0x{}", "42".repeat(32)).as_str())
        );
        let mut sold = log(
            market,
            vec![
                TOPIC_ORDER_SUCCESSFUL.into(),
                topic_word(U256::from(77u8)),
                topic_addr(a),
                topic_addr(c),
            ],
            (id, nft, U256::from(10u8)).abi_encode_params(),
            23,
            0,
        );
        sold.transaction_hash = Some(format!("0x{}", "AB".repeat(32)));
        let ev = decode_log(&sold, 1).unwrap().unwrap();
        assert_eq!(
            (ev.kind, ev.to_address.as_deref(), ev.tx_hash),
            (
                KIND_ORDER_SUCCESSFUL,
                Some(c),
                Some(format!("0x{}", "ab".repeat(32)))
            )
        );
        let cancelled = log(
            market,
            vec![
                TOPIC_ORDER_CANCELLED.into(),
                topic_word(U256::from(77u8)),
                topic_addr(a),
            ],
            (id, nft).abi_encode_params(),
            24,
            0,
        );
        assert_eq!(
            decode_log(&cancelled, 1).unwrap().unwrap().kind,
            KIND_ORDER_CANCELLED
        );
        let update = log(
            ESTATE_REGISTRY_MAINNET,
            vec![
                TOPIC_LAND_UPDATE.into(),
                topic_word(U256::from(5750u32)),
                topic_addr(a),
                topic_addr(a),
            ],
            ("0,\"1x2 Estate\",\"\"".to_string(),).abi_encode_params(),
            25,
            0,
        );
        let ev = decode_log(&update, 1).unwrap().unwrap();
        assert_eq!(
            (ev.kind, ev.contract_address.as_str(), ev.token_id.as_str()),
            (KIND_LAND_DATA, ESTATE_REGISTRY_MAINNET, "5750")
        );
        assert_eq!(
            parse_land_data(ev.detail.as_deref().unwrap()),
            Some((Some("1x2 Estate".to_string()), None))
        );
    }

    #[test]
    fn land_data_is_the_version_zero_csv() {
        assert_eq!(
            parse_land_data("0,\"Fashion Street, Estate\",\"say \"\"hi\"\"\""),
            Some((
                Some("Fashion Street, Estate".into()),
                Some("say \"hi\"".into())
            ))
        );
        assert_eq!(
            parse_land_data("0,P3P3V3RS3"),
            Some((Some("P3P3V3RS3".into()), None))
        );
        assert_eq!(parse_land_data("1,\"x\""), None);
        assert_eq!(parse_land_data("0"), Some((None, None)));
    }

    #[test]
    fn legacy_expiries_normalize_to_milliseconds() {
        assert_eq!(expires_ms(1_600_000_000), 1_600_000_000_000);
        assert_eq!(expires_ms(1_600_000_000_000), 1_600_000_000_000);
        assert_eq!(expires_ms(u128::MAX), i64::MAX);
    }

    #[test]
    fn an_owed_backfill_is_the_range_below_the_walk() {
        let w = |b: Option<i64>, done: Option<i64>| WalkedContract {
            contract_address: "0x".into(),
            walked_from_block: 100,
            backfill_from_block: b,
            backfilled_to_block: done,
        };
        assert_eq!(w(None, None).owed(), None);
        assert_eq!(w(Some(40), None).owed(), Some((40, 99)));
        assert_eq!(w(Some(40), Some(60)).owed(), Some((61, 99)));
        assert_eq!(w(Some(40), Some(99)).owed(), None);
    }
}
