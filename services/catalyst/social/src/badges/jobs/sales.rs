//! The Polygon shop logs → emotionista and fashionista. Upstream counted the
//! collections subgraph's L2 `sales` (primary mints, orders, bids) per
//! transaction hash; here the same sales come from the logs shop indexes on
//! Polygon:
//!
//! - `shop.sale_event` `mint` (a CollectionStore issue, the beneficiary
//!   buys) and `bid_accepted` (ERC721BidV2, the bidder buys);
//! - `shop.trade_event` `traded` (the off-chain marketplace, which the
//!   subgraph did not index: each good's beneficiary buys, else a listing's
//!   caller or a bid's signer);
//! - `shop.nft_event` `order_successful` (the legacy marketplace: its buyer,
//!   or, when the buyer is a relay, the token's last recipient after the
//!   order in its block, as the subgraph resolved it, and nobody when the
//!   relay still holds it; a row indexed without its transaction hash is
//!   counted by its chain position).
//!
//! An item's category is its mirrored `shop.items.item_type`. Each log table
//! is read only up to the block its index has published (`shop.chain_cursor`
//! folded and fresh, shop's own readiness rule), from its own cursor
//! (`sales:<table>:matic`, block and log index), which starts at the
//! published block.
//!
//! A sale of an item the items mirror does not hold yet holds its cursor
//! (the logs are read in chain order, so the sales after it wait too) until
//! the item appears or the mirror completes a pass [`ITEM_WAIT_SECS`] after
//! the sale: a complete pass walks every item and at the defaults takes
//! minutes, so an item still missing then is not a collection item the
//! mirror will ever hold (a bid on a foreign ERC721), and the sale is passed
//! without effect. A mirror that stops completing passes holds the sales,
//! so the job runs only with `MARKETPLACE_MIRRORS_ENABLED`.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;
use sqlx::PgConnection;

use super::super::evaluators as ev;
use super::{Pass, award, completed, init_cursor, lock_cursor};
use crate::identity::Address;

pub(crate) use crate::shared::collections::{ITEM_SHIFT, ITEM_WAIT_SECS};
const ASSET_TYPE_ERC721: i64 = 3;
const ASSET_TYPE_COLLECTION_ITEM: i64 = 4;
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
/// The fiat and cross-chain relays whose legacy orders the subgraph credited
/// to the token's owner after the sale.
const RELAY_BUYERS: &str =
    "'0xed038688ecf1193f8d9717eb3930f0bf0d745cb4', '0xea749fd6ba492dbc14c24fe8a3d08769229b896c'";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Log {
    Sale,
    Trade,
    Order,
}

impl Log {
    pub const ALL: [Log; 3] = [Log::Sale, Log::Trade, Log::Order];

    pub fn source(self) -> &'static str {
        match self {
            Log::Sale => "sales:sale_event:matic",
            Log::Trade => "sales:trade_event:matic",
            Log::Order => "sales:nft_event:matic",
        }
    }

    fn indexer(self) -> &'static str {
        match self {
            Log::Sale => "sales",
            Log::Trade => "trade_events",
            Log::Order => "nft",
        }
    }

    fn rows_sql(self) -> String {
        match self {
            Log::Sale => format!(
                "SELECT e.block_number, e.log_index, e.block_time, e.buyer, e.tx_hash, \
                        NULL::jsonb AS assets, NULL::text AS caller, \
                        CASE WHEN i.item_type IS NULL THEN '{{}}'::text[] ELSE ARRAY[i.item_type] END AS item_types \
                 FROM shop.sale_event e \
                 LEFT JOIN shop.items i ON i.network = e.network AND i.contract_address = e.contract_address \
                   AND i.item_id = CASE WHEN e.kind = 'mint' THEN e.item_id ELSE div(e.token_id, {ITEM_SHIFT}) END \
                 WHERE e.network = 'matic' AND e.kind IN ('mint', 'bid_accepted') \
                   AND (e.block_number, e.log_index) > ($1, $2) AND e.block_number <= $3 \
                 ORDER BY e.block_number, e.log_index LIMIT $4"
            ),
            Log::Trade => {
                "SELECT e.block_number, e.log_index, e.block_time, e.signer AS buyer, e.tx_hash, \
                        e.assets, e.caller, '{}'::text[] AS item_types \
                 FROM shop.trade_event e \
                 WHERE e.network = 'matic' AND e.kind = 'traded' \
                   AND (e.block_number, e.log_index) > ($1, $2) AND e.block_number <= $3 \
                 ORDER BY e.block_number, e.log_index LIMIT $4"
                    .to_string()
            }
            Log::Order => format!(
                "SELECT e.block_number, e.log_index, e.block_time, \
                        CASE WHEN e.to_address IN ({RELAY_BUYERS}) THEN ( \
                            SELECT CASE WHEN t.to_address IN ({RELAY_BUYERS}) THEN NULL ELSE t.to_address END \
                            FROM shop.nft_event t \
                            WHERE t.network = e.network AND t.contract_address = e.contract_address \
                              AND t.token_id = e.token_id AND t.block_number = e.block_number \
                              AND t.log_index > e.log_index AND t.kind = 'transfer' \
                            ORDER BY t.log_index DESC LIMIT 1) \
                        ELSE e.to_address END AS buyer, \
                        coalesce(e.tx_hash, 'nft_event:matic:' || e.block_number || ':' || e.log_index) AS tx_hash, \
                        NULL::jsonb AS assets, NULL::text AS caller, \
                        CASE WHEN i.item_type IS NULL THEN '{{}}'::text[] ELSE ARRAY[i.item_type] END AS item_types \
                 FROM shop.nft_event e \
                 LEFT JOIN shop.items i ON i.network = e.network AND i.contract_address = e.contract_address \
                   AND i.item_id = div(e.token_id, {ITEM_SHIFT}) \
                 WHERE e.network = 'matic' AND e.kind = 'order_successful' \
                   AND (e.block_number, e.log_index) > ($1, $2) AND e.block_number <= $3 \
                 ORDER BY e.block_number, e.log_index LIMIT $4"
            ),
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    block_number: i64,
    log_index: i32,
    block_time: i64,
    buyer: Option<String>,
    tx_hash: String,
    assets: Option<Value>,
    caller: Option<String>,
    item_types: Vec<String>,
}

/// The badge a sold item's type counts for (`item-sold` `category`).
fn badge_of(item_type: &str) -> &'static str {
    if item_type.starts_with("emote") {
        ev::EMOTIONISTA
    } else {
        ev::FASHIONISTA
    }
}

/// (asset type, contract, value) of a Traded good.
type Good = (i64, String, String);

/// The goods (an ERC721 token or a collection item) of one side of a Traded
/// log, each with its non-zero beneficiary.
fn side(assets: &Value, key: &str) -> Vec<(Good, Option<String>)> {
    assets[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let kind = match &a["assetType"] {
                Value::Number(n) => n.as_i64()?,
                Value::String(s) => s.parse().ok()?,
                _ => return None,
            };
            let beneficiary = a["beneficiary"]
                .as_str()
                .map(str::to_ascii_lowercase)
                .filter(|b| b != ZERO_ADDRESS);
            Some((
                (
                    kind,
                    a["contractAddress"].as_str()?.to_ascii_lowercase(),
                    a["value"].as_str()?.to_string(),
                ),
                beneficiary,
            ))
        })
        .filter(|((kind, _, _), _)| {
            *kind == ASSET_TYPE_ERC721 || *kind == ASSET_TYPE_COLLECTION_ITEM
        })
        .collect()
}

/// A Traded log's goods, each with its buyer: the signer sent the goods (a
/// listing) or received them (a bid), and each went to its beneficiary, else
/// to the caller or the signer (`Marketplace._transferAssets`).
fn trade(row: &Row) -> Vec<(String, Good)> {
    let Some(assets) = row.assets.as_ref() else {
        return Vec::new();
    };
    let sent = side(assets, "sent");
    let (goods, to) = if sent.is_empty() {
        (side(assets, "received"), row.buyer.as_deref())
    } else {
        (sent, row.caller.as_deref())
    };
    goods
        .into_iter()
        .filter_map(|(good, beneficiary)| {
            let buyer = beneficiary.or_else(|| to.map(str::to_ascii_lowercase))?;
            Some((buyer, good))
        })
        .collect()
}

/// The mirrored item types of Traded goods, by (contract, value).
async fn trade_item_types(
    conn: &mut PgConnection,
    goods: &[Good],
) -> Result<HashMap<(String, String), String>, sqlx::Error> {
    if goods.is_empty() {
        return Ok(HashMap::new());
    }
    let contracts: Vec<&str> = goods.iter().map(|g| g.1.as_str()).collect();
    let values: Vec<&str> = goods.iter().map(|g| g.2.as_str()).collect();
    let tokens: Vec<bool> = goods.iter().map(|g| g.0 == ASSET_TYPE_ERC721).collect();
    let rows: Vec<(String, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT u.c, u.v, i.item_type \
         FROM unnest($1::text[], $2::text[], $3::bool[]) AS u(c, v, t) \
         JOIN shop.items i ON i.network = 'matic' AND i.contract_address = u.c \
           AND i.item_id = CASE WHEN u.t THEN div(u.v::numeric, {ITEM_SHIFT}) ELSE u.v::numeric END \
         WHERE u.v ~ '^[0-9]{{1,78}}$'"
    )))
    .bind(&contracts)
    .bind(&values)
    .bind(&tokens)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(c, v, t)| ((c, v), t)).collect())
}

/// The block `log`'s index has published, when it has (shop's readiness:
/// full history folded through the last indexed block within 30 minutes).
async fn published(conn: &mut PgConnection, log: Log) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT folded_block FROM shop.chain_cursor \
         WHERE indexer = $1 AND network = 'matic' AND full_history \
           AND folded_block = last_block \
           AND folded_at <= timezone('UTC', now()) \
           AND folded_at >= timezone('UTC', now()) - interval '30 minutes'",
    )
    .bind(log.indexer())
    .fetch_optional(conn)
    .await
    .map(Option::flatten)
}

pub(super) async fn run(pass: &Pass) -> anyhow::Result<u64> {
    let mut passed = 0;
    for log in Log::ALL {
        passed += super::drain(|| batch(pass, log)).await?;
    }
    Ok(passed)
}

pub(crate) async fn batch(pass: &Pass, log: Log) -> Result<(u64, bool), sqlx::Error> {
    let mut tx = pass.pool.begin().await?;
    let Some(until) = published(&mut tx, log).await? else {
        tx.rollback().await?;
        return Ok((0, false));
    };
    let Some((block, log_index)) = lock_cursor(&mut tx, log.source()).await? else {
        init_cursor(&mut tx, log.source(), until, i32::MAX).await?;
        tx.commit().await?;
        return Ok((0, false));
    };
    super::report_lag(log.source(), until - block);
    let limit = pass.batch();
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(log.rows_sql()))
        .bind(block)
        .bind(log_index)
        .bind(until)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
    if rows.is_empty() {
        tx.rollback().await?;
        return Ok((0, false));
    }
    // The last complete items pass, unix seconds.
    let items_pass: Option<i64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM last_complete_pass_at)::bigint FROM shop.mirror_state \
         WHERE mirror = 'items' AND network = 'matic'",
    )
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    // (buyer, tx key, sale time, badges)
    let mut sales: Vec<(String, String, i64, Vec<&'static str>)> = Vec::new();
    let now = crate::shared::now_utc();
    let goods: Vec<Good> = if log == Log::Trade {
        rows.iter().flat_map(trade).map(|(_, g)| g).collect()
    } else {
        Vec::new()
    };
    let traded = trade_item_types(&mut tx, &goods).await?;
    let mut passed = 0;
    for row in &rows {
        let at = chrono::DateTime::from_timestamp(row.block_time, 0)
            .map(|t| t.naive_utc())
            .unwrap_or(now);
        // (buyer, item types) of the row's goods
        let (buys, unmirrored): (Vec<(String, Vec<String>)>, bool) = if log == Log::Trade {
            let goods = trade(row);
            let total = goods.len();
            let mut buys: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let mut found = 0;
            for (buyer, (_, c, v)) in goods {
                let Some(t) = traded.get(&(c, v)) else {
                    continue;
                };
                found += 1;
                buys.entry(buyer).or_default().push(t.clone());
            }
            (buys.into_iter().collect(), found < total)
        } else {
            let missing = row.buyer.is_some() && row.item_types.is_empty();
            let buys = row.buyer.clone().map(|b| (b, row.item_types.clone()));
            (buys.into_iter().collect(), missing)
        };
        if unmirrored && !pass.too_old(at, now) {
            let waited = items_pass
                .is_some_and(|done| done >= row.block_time.saturating_add(ITEM_WAIT_SECS));
            if !waited {
                tracing::debug!(
                    source = log.source(),
                    block = row.block_number,
                    log = row.log_index,
                    "sale of an item not mirrored yet: held"
                );
                break;
            }
            tracing::debug!(
                source = log.source(),
                block = row.block_number,
                log = row.log_index,
                "sale of an item the mirror never held: passed"
            );
        }
        passed += 1;
        if pass.too_old(at, now) {
            continue;
        }
        for (buyer, types) in buys {
            let mut badges: Vec<&'static str> = types.iter().map(|t| badge_of(t)).collect();
            badges.sort();
            badges.dedup();
            if badges.is_empty() {
                continue;
            }
            sales.push((
                buyer.to_ascii_lowercase(),
                row.tx_hash.to_ascii_lowercase(),
                row.block_time.saturating_mul(1000),
                badges,
            ));
        }
    }
    let Some(last) = rows[..passed].last().map(|r| (r.block_number, r.log_index)) else {
        tx.rollback().await?;
        return Ok((0, false));
    };
    let addresses: Vec<String> = sales
        .iter()
        .map(|s| s.0.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let done = completed(&mut tx, &addresses).await?;
    let notify = Some(pass.asset_base.as_str());
    for (buyer, tx_hash, sale_at, badges) in &sales {
        let Ok(address) = Address::parse(buyer) else {
            continue;
        };
        for badge in badges {
            if done.contains(&(buyer.clone(), badge.to_string())) {
                continue;
            }
            award(&mut tx, &address, badge, notify, |p| {
                ev::buyer(p, badge, tx_hash, *sale_at)
            })
            .await?;
        }
    }
    super::advance(&mut tx, log.source(), last.0, last.1).await?;
    tx.commit().await?;
    super::report_lag(log.source(), until - last.0);
    Ok((
        passed as u64,
        passed == rows.len() && rows.len() as i64 == limit,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(signer: &str, caller: &str, assets: Value) -> Row {
        Row {
            block_number: 1,
            log_index: 0,
            block_time: 0,
            buyer: Some(signer.into()),
            tx_hash: "0x".into(),
            assets: Some(assets),
            caller: Some(caller.into()),
            item_types: Vec::new(),
        }
    }

    #[test]
    fn a_listing_is_bought_by_its_caller_a_bid_by_its_signer() {
        let listing = row(
            "0xseller",
            "0xBuyer",
            json!({"sent": [{"assetType": 4, "contractAddress": "0xC", "value": "2",
                             "beneficiary": ZERO_ADDRESS}],
                   "received": [{"assetType": 1, "contractAddress": "0xm", "value": "10"}]}),
        );
        assert_eq!(
            trade(&listing),
            vec![("0xbuyer".into(), (4, "0xc".into(), "2".into()))]
        );
        let bid = row(
            "0xbidder",
            "0xowner",
            json!({"sent": [{"assetType": 1, "contractAddress": "0xm", "value": "10"}],
                   "received": [{"assetType": "3", "contractAddress": "0xc", "value": "77"}]}),
        );
        assert_eq!(
            trade(&bid),
            vec![("0xbidder".into(), (3, "0xc".into(), "77".into()))]
        );
        let currency_only = row(
            "0xa",
            "0xb",
            json!({"sent": [{"assetType": 1, "contractAddress": "0xm", "value": "1"}]}),
        );
        assert_eq!(trade(&currency_only), vec![]);
    }

    #[test]
    fn each_good_is_bought_by_its_beneficiary() {
        // a credits purchase: the CreditsManager calls, the goods go to each beneficiary
        let listing = row(
            "0xseller",
            "0xcredits",
            json!({"sent": [
                       {"assetType": "4", "contractAddress": "0xc", "value": "2", "beneficiary": "0xAlice"},
                       {"assetType": "3", "contractAddress": "0xc", "value": "9", "beneficiary": "0xbob"},
                       {"assetType": "4", "contractAddress": "0xc", "value": "5", "beneficiary": ZERO_ADDRESS}],
                   "received": [{"assetType": 1, "contractAddress": "0xm", "value": "10",
                                 "beneficiary": "0xseller"}]}),
        );
        assert_eq!(
            trade(&listing),
            vec![
                ("0xalice".into(), (4, "0xc".into(), "2".into())),
                ("0xbob".into(), (3, "0xc".into(), "9".into())),
                ("0xcredits".into(), (4, "0xc".into(), "5".into())),
            ]
        );
        let bid = row(
            "0xbidder",
            "0xowner",
            json!({"sent": [{"assetType": 1, "contractAddress": "0xm", "value": "10"}],
                   "received": [{"assetType": "3", "contractAddress": "0xc", "value": "77",
                                 "beneficiary": "0xfriend"}]}),
        );
        assert_eq!(
            trade(&bid),
            vec![("0xfriend".into(), (3, "0xc".into(), "77".into()))]
        );
    }

    #[test]
    fn item_types_pick_the_buyer_badge() {
        assert_eq!(badge_of("emote_v1"), ev::EMOTIONISTA);
        assert_eq!(badge_of("wearable_v2"), ev::FASHIONISTA);
        assert_eq!(badge_of("smart_wearable_v1"), ev::FASHIONISTA);
    }
}
