//! The `item_sold` and `bid_accepted` producer. An off-chain `Traded` log
//! (`shop.trade_event`) of a listing tells its seller (the signer) "Item
//! Sold", of a bid tells its bidder (the signer) "Bid Accepted"; a legacy
//! Polygon `OrderSuccessful` (`shop.nft_event`) tells its seller. One
//! notification per log, so every use of a multi-use trade notifies. The
//! metadata is notifications-workers' parse of the old events:
//! marketplace-server's accept events for trades (the `trade_events`
//! builders), events-notifier's sales poller for legacy orders.
//!
//! The `sales` index (`shop.sale_event`, Polygon) adds the rest of that
//! poller's L2 sales: a CollectionStore mint tells the item's creator
//! (once per transaction, as the old `txHash` key did; priced by the
//! store's `Bought`), an ERC721BidV2
//! `BidAccepted` tells its bidder "Bid Accepted" (events-notifier's bids
//! poller) and its seller "Item Sold". A zero-price sale is no sale there,
//! so it sells nothing here. A sale of an item the items mirror lacks holds
//! its cursor until the item appears or a complete pass `ITEM_WAIT_SECS`
//! after the sale, as the badge processor's do. The Ethereum ERC721Bid was
//! never polled.
//!
//! Each (source, network) reads only up to its index's `folded_block` while
//! `chain_walk::is_ready`; its first pass starts at that published block, so
//! history is never replayed. Progress is `shop.notification_cursor` (`sales:{source}:{network}`), advanced
//! with each page's notifications in one transaction; the id is reorg-stable
//! (`notice_cursor`), so a retried page inserts nothing twice.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::MarketplaceState;
use super::chain_walk::{self, ChainNet, INDEXER_NFT, INDEXER_SALES, INDEXER_TRADE_EVENTS};
use super::notice_cursor;
use super::trade_events::{self, AssetMeta, StoredTrade};
use crate::shared::identity::Address;
use crate::social::notifications::templates::format_mana;
use crate::social::notifications::{NewNotification, insert_notifications};
use crate::types::{
    ASSET_TYPE_COLLECTION_ITEM, ASSET_TYPE_ERC20, ASSET_TYPE_ERC721, ASSET_TYPE_USD_PEGGED_MANA,
};

pub const KIND_ITEM_SOLD: &str = "item_sold";
pub const KIND_BID_ACCEPTED: &str = "bid_accepted";
const PAGE_SIZE: i64 = 200;
const POLL_SECS: u64 = 60;

/// The metadata notifications-workers kept of an `item-sold` event.
const ITEM_SOLD_FIELDS: &[&str] = &[
    "image",
    "seller",
    "category",
    "rarity",
    "link",
    "nftName",
    "title",
    "description",
    "network",
];
/// The metadata notifications-workers kept of a `bid-accepted` event.
const BID_ACCEPTED_FIELDS: &[&str] = &[
    "image",
    "seller",
    "category",
    "rarity",
    "link",
    "nftName",
    "price",
    "title",
    "description",
    "network",
];

use crate::shared::collections::{ITEM_SHIFT, item_awaited};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Traded,
    OrderSuccessful,
    Mint,
    BidAccepted,
}

impl Source {
    const ALL: [Source; 4] = [
        Source::Traded,
        Source::OrderSuccessful,
        Source::Mint,
        Source::BidAccepted,
    ];

    fn db_name(self) -> &'static str {
        match self {
            Source::Traded => "traded",
            Source::OrderSuccessful => "order_successful",
            Source::Mint => "mint",
            Source::BidAccepted => "bid_accepted",
        }
    }

    fn indexer(self) -> &'static str {
        match self {
            Source::Traded => INDEXER_TRADE_EVENTS,
            Source::OrderSuccessful => INDEXER_NFT,
            Source::Mint | Source::BidAccepted => INDEXER_SALES,
        }
    }

    /// Only Traded on Ethereum: the old poller read the L2 sales only.
    fn networks(self) -> &'static [ChainNet] {
        match self {
            Source::Traded => &ChainNet::ALL,
            _ => &[ChainNet::Matic],
        }
    }

    fn producer(self, net: ChainNet) -> &'static str {
        match (self, net) {
            (Source::Traded, ChainNet::Ethereum) => notice_cursor::SALES_TRADED_ETHEREUM,
            (Source::Traded, ChainNet::Matic) => notice_cursor::SALES_TRADED_MATIC,
            (Source::OrderSuccessful, _) => notice_cursor::SALES_ORDER_SUCCESSFUL_MATIC,
            (Source::Mint, _) => notice_cursor::SALES_MINT_MATIC,
            (Source::BidAccepted, _) => notice_cursor::SALES_BID_ACCEPTED_MATIC,
        }
    }

    fn rows_sql(self) -> String {
        match self {
            Source::Traded => format!(
                "SELECT {key} AS id_key, e.block_number, e.log_index, e.block_time, e.signer AS recipient, e.caller AS counterparty, e.assets, \
                        NULL::text AS contract_address, NULL::text AS token_id, NULL::text AS item_id, NULL::text AS price, NULL::text AS tx_hash \
                 FROM shop.trade_event e \
                 WHERE e.network = $1 AND e.kind = 'traded' AND (e.block_number, e.log_index) > ($2, $3) AND e.block_number <= $4 \
                 ORDER BY e.block_number, e.log_index LIMIT $5",
                key = notice_cursor::tx_log_key(
                    "trade_event",
                    "e",
                    "coalesce(e.hashed_signature, '')"
                )
            ),
            Source::OrderSuccessful => format!(
                "SELECT {key} AS id_key, e.block_number, e.log_index, e.block_time, e.from_address AS recipient, e.to_address AS counterparty, NULL::jsonb AS assets, \
                        e.contract_address::text AS contract_address, e.token_id::text AS token_id, NULL::text AS item_id, NULL::text AS price, NULL::text AS tx_hash \
                 FROM shop.nft_event e \
                 WHERE e.network = $1 AND e.kind = 'order_successful' AND (e.block_number, e.log_index) > ($2, $3) AND e.block_number <= $4 \
                 ORDER BY e.block_number, e.log_index LIMIT $5",
                key = notice_cursor::token_log_key("e")
            ),
            // The mint's price is the store's `Bought` of it.
            Source::Mint => format!(
                "SELECT {key} AS id_key, e.block_number, e.log_index, e.block_time, NULL::text AS recipient, e.buyer AS counterparty, NULL::jsonb AS assets, \
                        e.contract_address::text AS contract_address, e.token_id::text AS token_id, e.item_id::text AS item_id, e.price::text AS price, e.tx_hash::text AS tx_hash \
                 FROM shop.sale_event e \
                 WHERE e.network = $1 AND e.kind = 'mint' AND (e.block_number, e.log_index) > ($2, $3) AND e.block_number <= $4 \
                 ORDER BY e.block_number, e.log_index LIMIT $5",
                key = notice_cursor::tx_log_key("sale_event", "e", SALE_CONTENT)
            ),
            Source::BidAccepted => format!(
                "SELECT {key} AS id_key, e.block_number, e.log_index, e.block_time, e.buyer AS recipient, e.seller AS counterparty, NULL::jsonb AS assets, \
                        e.contract_address::text AS contract_address, e.token_id::text AS token_id, div(e.token_id, {ITEM_SHIFT})::text AS item_id, e.price::text AS price, e.tx_hash::text AS tx_hash \
                 FROM shop.sale_event e \
                 WHERE e.network = $1 AND e.kind = 'bid_accepted' AND (e.block_number, e.log_index) > ($2, $3) AND e.block_number <= $4 \
                 ORDER BY e.block_number, e.log_index LIMIT $5",
                key = notice_cursor::tx_log_key("sale_event", "e", SALE_CONTENT)
            ),
        }
    }
}

/// What a `shop.sale_event` id keys on beside its transaction.
const SALE_CONTENT: &str =
    "(e.kind || ':' || e.contract_address || ':' || e.token_id || ':' || coalesce(e.bid_id, ''))";

/// Over the log's [`notice_cursor::tx_log_key`] (Traded, the sales
/// index) or [`notice_cursor::token_log_key`] (OrderSuccessful); a mint's
/// key is its transaction and creator.
pub fn notification_id(source: &str, network: &str, key: &str) -> Uuid {
    let prefix = match source {
        "traded" => "traded",
        "mint" => "mint-sold",
        "bid_accepted" => "bid-accepted",
        "bid_sold" => "bid-sold",
        _ => "order-successful",
    };
    notice_cursor::notification_id(prefix, network, key)
}

/// The site bases the metadata links and images point at.
#[derive(Clone, Debug)]
pub struct Links {
    pub marketplace: String,
    pub land_images: String,
    /// Whether the items mirror runs: a sale waits for its item only then.
    pub items_mirror_enabled: bool,
}

#[derive(sqlx::FromRow)]
struct LogRow {
    id_key: String,
    block_number: i64,
    log_index: i32,
    block_time: i64,
    recipient: Option<String>,
    counterparty: Option<String>,
    assets: Option<Value>,
    contract_address: Option<String>,
    token_id: Option<String>,
    item_id: Option<String>,
    price: Option<String>,
    tx_hash: Option<String>,
}

/// (assetType, contract, value) of one side of a Traded log's trade.
fn side(assets: &Value, key: &str) -> Vec<(i32, String, String)> {
    assets[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some((
                a["assetType"].as_str()?.parse().ok()?,
                a["contractAddress"].as_str()?.to_ascii_lowercase(),
                a["value"].as_str()?.to_string(),
            ))
        })
        .collect()
}

fn is_good(asset_type: i32) -> bool {
    asset_type == ASSET_TYPE_ERC721 || asset_type == ASSET_TYPE_COLLECTION_ITEM
}

fn project(event: &Value, fields: &[&str]) -> Value {
    let meta = &event["metadata"];
    Value::Object(
        fields
            .iter()
            .filter_map(|f| meta.get(*f).map(|v| (f.to_string(), v.clone())))
            .collect(),
    )
}

/// marketplace-server's `trade.network` (@dcl/schemas `Network`).
fn trade_network(net: ChainNet) -> &'static str {
    match net {
        ChainNet::Ethereum => "ETHEREUM",
        ChainNet::Matic => "MATIC",
    }
}

/// The single good a log names: (is an nft, contract, token or item id).
/// A sales-index log is described by its item, linked to its token.
fn wanted(source: Source, row: &LogRow) -> Option<(bool, String, String)> {
    match source {
        Source::OrderSuccessful => {
            return Some((true, row.contract_address.clone()?, row.token_id.clone()?));
        }
        Source::Mint | Source::BidAccepted => {
            return Some((false, row.contract_address.clone()?, row.item_id.clone()?));
        }
        Source::Traded => {}
    }
    let assets = row.assets.as_ref()?;
    let goods: Vec<_> = side(assets, "sent")
        .into_iter()
        .chain(side(assets, "received"))
        .filter(|a| is_good(a.0))
        .collect();
    let [(asset_type, contract, value)] = goods.as_slice() else {
        return None;
    };
    Some((
        *asset_type == ASSET_TYPE_ERC721,
        contract.clone(),
        value.clone(),
    ))
}

/// The page's asset descriptions, fetched together.
#[derive(Default)]
struct Metas {
    nfts: HashMap<(String, String), AssetMeta>,
    items: HashMap<(String, String), AssetMeta>,
}

impl Metas {
    async fn fetch(
        pool: &PgPool,
        links: &Links,
        source: Source,
        net: ChainNet,
        rows: &[LogRow],
    ) -> Result<Self, sqlx::Error> {
        let (mut nfts, mut items) = (Vec::new(), Vec::new());
        for (nft, contract, value) in rows.iter().filter_map(|r| wanted(source, r)) {
            if nft {
                nfts.push((contract, value));
            } else {
                items.push((contract, value));
            }
        }
        let network = net.db_name();
        let mut metas = Metas::default();
        if !nfts.is_empty() {
            metas.nfts = trade_events::nft_metas(pool, &links.land_images, network, &nfts).await?;
        }
        if !items.is_empty() {
            metas.items = trade_events::item_metas(pool, network, &items).await?;
        }
        Ok(metas)
    }

    fn get(&self, nft: bool, contract: String, value: String) -> Option<AssetMeta> {
        let map = if nft { &self.nfts } else { &self.items };
        map.get(&(contract, value)).cloned()
    }
}

/// events-notifier's L2 sale for its seller: its own wording, network `polygon`.
fn polygon_item_sold(links: &Links, seller: &str, buyer: &str, asset: &AssetMeta) -> Value {
    let trade = StoredTrade {
        id: String::new(),
        network: "polygon".into(),
        signer: seller.to_string(),
        trade_type: String::new(),
        contract: String::new(),
        sent_amount: None,
        assets: Vec::new(),
    };
    let asset = AssetMeta {
        seller: seller.to_string(),
        ..asset.clone()
    };
    let mut event = trade_events::item_sold_event(&trade, &asset, buyer, &links.marketplace);
    event["metadata"]["description"] = json!(format!(
        "You just sold this {}.",
        asset.name.as_deref().unwrap_or_default()
    ));
    project(&event, ITEM_SOLD_FIELDS)
}

/// A positive integer amount (a sale the old index recorded).
fn is_paid(price: Option<&str>) -> bool {
    price.is_some_and(|p| {
        p.bytes().all(|b| b.is_ascii_digit()) && !p.trim_start_matches('0').is_empty()
    })
}

/// One notification: recipient, kind, metadata and id.
type Notice = (String, &'static str, Value, Uuid);

/// The notifications of one log; `None` when it names no single described asset.
fn notices(
    links: &Links,
    source: Source,
    net: ChainNet,
    row: &LogRow,
    metas: &Metas,
) -> Option<Vec<Notice>> {
    let id = |prefix: &str, key: &str| notification_id(prefix, net.db_name(), key);
    let counterparty = row
        .counterparty
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let (nft, contract, value) = wanted(source, row)?;
    let asset = metas.get(nft, contract, value)?;
    match source {
        Source::Mint => {
            if !is_paid(row.price.as_deref()) {
                return None;
            }
            // The store pays the creator; one notice per (transaction, creator).
            let creator = asset.seller.to_ascii_lowercase();
            let asset = AssetMeta {
                token_id: row.token_id.clone(),
                ..asset
            };
            let key = format!("{}:{creator}", row.tx_hash.as_deref()?);
            let metadata = polygon_item_sold(links, &creator, &counterparty, &asset);
            return Some(vec![(creator, KIND_ITEM_SOLD, metadata, id("mint", &key))]);
        }
        Source::BidAccepted => {
            let bidder = row.recipient.as_deref()?.to_ascii_lowercase();
            let price = row.price.clone()?;
            let asset = AssetMeta {
                seller: counterparty.clone(),
                token_id: row.token_id.clone(),
                ..asset
            };
            let trade = StoredTrade {
                id: String::new(),
                network: "polygon".into(),
                signer: bidder.clone(),
                trade_type: String::new(),
                contract: String::new(),
                sent_amount: Some(price.clone()),
                assets: Vec::new(),
            };
            let mut event = trade_events::bid_accepted_event(&trade, &asset, &links.marketplace);
            event["metadata"]["description"] = json!(format!(
                "Your bid for {} MANA for this {} was accepted.",
                format_mana(&json!(price)),
                asset.name.as_deref().unwrap_or_default()
            ));
            let mut out = vec![(
                bidder.clone(),
                KIND_BID_ACCEPTED,
                project(&event, BID_ACCEPTED_FIELDS),
                id("bid_accepted", &row.id_key),
            )];
            if is_paid(Some(&price)) {
                out.push((
                    counterparty.clone(),
                    KIND_ITEM_SOLD,
                    polygon_item_sold(links, &counterparty, &bidder, &asset),
                    id("bid_sold", &row.id_key),
                ));
            }
            return Some(out);
        }
        Source::OrderSuccessful => {
            let seller = row.recipient.as_deref()?.to_ascii_lowercase();
            let metadata = polygon_item_sold(links, &seller, &counterparty, &asset);
            return Some(vec![(
                seller,
                KIND_ITEM_SOLD,
                metadata,
                id(source.db_name(), &row.id_key),
            )]);
        }
        Source::Traded => {}
    }
    let recipient = row.recipient.as_deref()?.to_ascii_lowercase();
    let mut trade = StoredTrade {
        id: String::new(),
        network: trade_network(net).to_string(),
        signer: recipient.clone(),
        trade_type: String::new(),
        contract: String::new(),
        sent_amount: None,
        assets: Vec::new(),
    };
    let sent = side(row.assets.as_ref()?, "sent");
    let bid = !sent.iter().any(|a| is_good(a.0));
    trade.sent_amount = sent
        .iter()
        .find(|a| a.0 == ASSET_TYPE_ERC20 || a.0 == ASSET_TYPE_USD_PEGGED_MANA)
        .map(|a| a.2.clone());
    // The seller is who parted with the asset: the listing's signer, the bid's acceptor.
    let asset = AssetMeta {
        seller: if bid {
            counterparty.clone()
        } else {
            recipient.clone()
        },
        ..asset
    };
    let id = id(source.db_name(), &row.id_key);
    Some(vec![if bid {
        let event = trade_events::bid_accepted_event(&trade, &asset, &links.marketplace);
        (
            recipient,
            KIND_BID_ACCEPTED,
            project(&event, BID_ACCEPTED_FIELDS),
            id,
        )
    } else {
        let event =
            trade_events::item_sold_event(&trade, &asset, &counterparty, &links.marketplace);
        (
            recipient,
            KIND_ITEM_SOLD,
            project(&event, ITEM_SOLD_FIELDS),
            id,
        )
    }])
}

/// Every published log of one (source, network) after its cursor, a page
/// per transaction. Returns the notifications inserted.
async fn drain(
    pool: &PgPool,
    links: &Links,
    source: Source,
    net: ChainNet,
    published: i64,
    now: i64,
) -> anyhow::Result<u64> {
    let producer = source.producer(net);
    notice_cursor::seed(pool, producer, None, published, i32::MAX).await?;
    let mut emitted = 0;
    loop {
        let mut txn = pool.begin().await?;
        let (_, since_block, since_log) = notice_cursor::lock(&mut txn, producer).await?;
        let rows: Vec<LogRow> = sqlx::query_as(sqlx::AssertSqlSafe(source.rows_sql()))
            .bind(net.db_name())
            .bind(since_block)
            .bind(since_log)
            .bind(published)
            .bind(PAGE_SIZE)
            .fetch_all(&mut *txn)
            .await?;
        if rows.is_empty() {
            txn.rollback().await?;
            break;
        }
        let metas = Metas::fetch(pool, links, source, net, &rows).await?;
        let items_pass = match source {
            Source::Mint | Source::BidAccepted => items_pass(&mut txn, net).await?,
            _ => None,
        };
        let mut batch: Vec<NewNotification> = Vec::with_capacity(rows.len());
        let mut done: Option<(i64, i32)> = None;
        let mut held = false;
        for row in &rows {
            if waits_for_item(source, row, &metas, items_pass, links, now) {
                tracing::debug!(
                    source = source.db_name(),
                    block = row.block_number,
                    log = row.log_index,
                    "sale of an item not mirrored yet: held"
                );
                held = true;
                break;
            }
            done = Some((row.block_number, row.log_index));
            let found = notices(links, source, net, row, &metas).unwrap_or_default();
            if found.is_empty() {
                tracing::debug!(
                    source = source.db_name(),
                    network = net.db_name(),
                    block = row.block_number,
                    log = row.log_index,
                    "sale without a single described, paid asset: no notification"
                );
            }
            for (recipient, kind, metadata, id) in found {
                let Ok(address) = Address::parse(&recipient) else {
                    continue;
                };
                // A mint's later item in the same transaction: the first wins.
                if batch.iter().any(|n| n.id == id) {
                    continue;
                }
                batch.push(NewNotification {
                    id,
                    address,
                    kind: kind.to_string(),
                    metadata,
                    occurred_at: DateTime::from_timestamp(row.block_time, 0)
                        .unwrap_or_default()
                        .naive_utc(),
                });
            }
        }
        let Some((done_block, done_log)) = done else {
            txn.rollback().await?;
            break;
        };
        emitted += insert_notifications(&mut *txn, &batch).await?;
        notice_cursor::advance(&mut txn, producer, None, done_block, done_log).await?;
        txn.commit().await?;
        if held || rows.len() < PAGE_SIZE as usize {
            break;
        }
    }
    Ok(emitted)
}

/// The items mirror's last complete pass on `net`, unix seconds.
async fn items_pass(conn: &mut sqlx::PgConnection, net: ChainNet) -> sqlx::Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT extract(epoch FROM last_complete_pass_at)::bigint FROM shop.mirror_state \
         WHERE mirror = 'items' AND network = $1",
    )
    .bind(net.db_name())
    .fetch_optional(conn)
    .await?
    .flatten())
}

/// A sales-index row whose item the mirror lacks waits, as the badge
/// processor's do, while [`item_awaited`].
fn waits_for_item(
    source: Source,
    row: &LogRow,
    metas: &Metas,
    items_pass: Option<i64>,
    links: &Links,
    now: i64,
) -> bool {
    if !matches!(source, Source::Mint | Source::BidAccepted) {
        return false;
    }
    let Some((nft, contract, value)) = wanted(source, row) else {
        return false;
    };
    metas.get(nft, contract, value).is_none()
        && item_awaited(links.items_mirror_enabled, items_pass, row.block_time, now)
}

/// One pass's result: notifications inserted and the indexes not published yet.
#[derive(Debug, Default)]
pub struct Pass {
    pub emitted: u64,
    pub waiting: Vec<String>,
}

/// One pass over every (source, network) whose index is published at `now`.
pub async fn run_once(pool: &PgPool, links: &Links, now: DateTime<Utc>) -> anyhow::Result<Pass> {
    let mut pass = Pass::default();
    for source in Source::ALL {
        for net in source.networks() {
            match chain_walk::ready_cursor(pool, source.indexer(), *net, now).await {
                Some(c) => {
                    let published = c.folded_block.unwrap_or(c.last_block);
                    pass.emitted +=
                        drain(pool, links, source, *net, published, now.timestamp()).await?;
                }
                None => pass
                    .waiting
                    .push(format!("{} {}", source.indexer(), net.db_name())),
            }
        }
    }
    Ok(pass)
}

pub fn spawn(state: &Arc<MarketplaceState>, jobs: &crate::jobs::Jobs) {
    let pool = state.pool.clone();
    let links = Links {
        marketplace: state.settings.market.marketplace_base_url.clone(),
        land_images: state.settings.land_image_base_url.clone(),
        items_mirror_enabled: state.settings.market.mirrors_enabled,
    };
    let ix = &state.settings.market.indexes;
    tracing::info!(
        trade_events = ix.trade_events_enabled,
        nft = ix.nft_enabled,
        sales = ix.sales_enabled,
        "item_sold/bid_accepted producer up: it reads the trade_events, nft and sales indexes once published"
    );
    jobs.spawn_periodic_leased(
        "sale-notifications",
        crate::jobs::PeriodicCfg::every(Duration::from_secs(POLL_SECS))
            .with_jitter(Duration::from_secs(10))
            .with_error_backoff(Duration::from_secs(600)),
        move |ctx| {
            let (pool, links) = (pool.clone(), links.clone());
            async move {
                let pass = run_once(&pool, &links, Utc::now()).await?;
                if pass.emitted > 0 {
                    tracing::info!(emitted = pass.emitted, "sale notifications emitted");
                }
                if pass.emitted > 0 || !pass.waiting.is_empty() {
                    let waiting = if pass.waiting.is_empty() {
                        String::new()
                    } else {
                        format!("; waiting for {} to publish", pass.waiting.join(", "))
                    };
                    ctx.progress(format!("{} emitted{waiting}", pass.emitted));
                }
                Ok(())
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    const SELLER: &str = "0x00000000000000000000000000000000000000a1";
    const BUYER: &str = "0x00000000000000000000000000000000000000b2";
    const CREATOR: &str = "0x00000000000000000000000000000000000000c3";
    const COLLECTION: &str = "0x00000000000000000000000000000000000000d4";
    const MARKET: &str = "0xe38ef22abe871513555cba89adfe45ab4f548ada";
    const LEGACY: &str = "0x480a0f4e360e8964e68858dd231c2922f1df45ef";

    #[test]
    fn ids_are_stable_and_unique_per_log() {
        let id = notification_id("traded", "matic", "0xa:0:0x01");
        assert_eq!(id, notification_id("traded", "matic", "0xa:0:0x01"));
        assert_ne!(id, notification_id("traded", "matic", "0xa:1:0x01"));
        assert_ne!(id, notification_id("traded", "ethereum", "0xa:0:0x01"));
        assert_ne!(
            id,
            notification_id("order_successful", "matic", "0xa:0:0x01")
        );
    }

    fn asset(asset_type: i32, contract: &str, value: &str) -> Value {
        json!({ "assetType": asset_type.to_string(), "contractAddress": contract, "value": value, "beneficiary": "0x0000000000000000000000000000000000000000" })
    }

    async fn traded(
        pool: &PgPool,
        block: i64,
        log: i32,
        signer: &str,
        caller: &str,
        sent: Value,
        received: Value,
    ) {
        sqlx::query(
            "INSERT INTO shop.trade_event (network, block_number, log_index, block_time, marketplace_address, kind, caller, hashed_signature, signer, assets, tx_hash) \
             VALUES ('matic', $1, $2, 1700000000, $3, 'traded', $4, '0x01', $5, $6, $7)",
        )
        .bind(block)
        .bind(log)
        .bind(MARKET)
        .bind(caller)
        .bind(signer)
        .bind(json!({ "sent": sent, "received": received, "uses": "1" }))
        .bind(format!("0xtx{block}"))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn order_successful(pool: &PgPool, network: &str, block: i64, token: &str) {
        sqlx::query(
            "INSERT INTO shop.nft_event (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, from_address, to_address, amount, detail) \
             VALUES ($1, $2, 0, 1700000000, 'order_successful', $3, $4, $5::numeric, $6, $7, 5, '0x03')",
        )
        .bind(network)
        .bind(block)
        .bind(LEGACY)
        .bind(COLLECTION)
        .bind(token)
        .bind(SELLER)
        .bind(BUYER)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn publish(pool: &PgPool, indexer: &str, block: i64) {
        sqlx::query(
            "INSERT INTO shop.chain_cursor (indexer, network, start_block, last_block, fold_through, folded_block, folded_at, full_history) \
             VALUES ($1, 'matic', 1, $2, $2, $2, timezone('UTC', now()), true) \
             ON CONFLICT (indexer, network) DO UPDATE SET last_block = $2, fold_through = $2, folded_block = $2, folded_at = timezone('UTC', now())",
        )
        .bind(indexer)
        .bind(block)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_assets(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO shop.collections (network, contract_address, chain_id, name, creator, owner, urn, is_completed, is_approved, is_editable, created_at, updated_at, reviewed_at, mirrored_at) \
             VALUES ('matic', $1, 137, 'Coll', $2, $2, 'urn:coll', true, true, false, 1, 1, 1, timezone('UTC', now()))",
        )
        .bind(COLLECTION)
        .bind(CREATOR)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO shop.items (network, contract_address, item_id, chain_id, item_type, urn, creator, rarity, total_supply, max_supply, available, price, image, name, category, body_shapes, search_is_collection_approved, created_at, updated_at, reviewed_at, mirrored_at) \
             VALUES ('matic', $1, 0, 137, 'wearable_v2', 'urn:coll:0', $2, 'epic', 1, 100, 99, 10, 'https://img/0', 'Shoes', 'feet', ARRAY['BaseMale'], true, 1, 1, 1, timezone('UTC', now()))",
        )
        .bind(COLLECTION)
        .bind(CREATOR)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO shop.nft (network, contract_address, token_id, owner, item_id, issued_id, minted_block, minted_at, transferred_block, transferred_log, transferred_at) \
             VALUES ('matic', $1, 7, $2, 0, 7, 1, 1, 1, 0, 1)",
        )
        .bind(COLLECTION)
        .bind(BUYER)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn stored(pool: &PgPool, kind: &str) -> Vec<(String, Value)> {
        sqlx::query_as("SELECT address, metadata FROM social.notifications WHERE type = $1 ORDER BY occurred_at, id")
            .bind(kind)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// On a database of its own (the chain cursors are global): nothing
    /// before the indexes publish; the first pass starts at the published
    /// block; later listing trades, bids and legacy orders notify their
    /// signer or seller once, never past the published block, never for an
    /// undescribed asset; a rewound cursor inserts nothing twice.
    #[tokio::test]
    async fn published_sales_notify_once_without_replaying_history() {
        let Ok(base_url) = std::env::var("CATALYST_TEST_DATABASE_URL") else {
            return;
        };
        let opts: PgConnectOptions = base_url.parse().unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.database("postgres"))
            .await
            .unwrap();
        let name = format!(
            "catalyst_test_salenotif_{}",
            &Uuid::new_v4().simple().to_string()[..12]
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&base_url).unwrap();
        url.set_path(&format!("/{name}"));
        let pool = crate::db::connect_pool(
            url.as_str(),
            &crate::db::PoolSettings {
                max_connections: 4,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        crate::db::apply_schema(&pool).await.unwrap();
        seed_assets(&pool).await;
        let links = Links {
            marketplace: "https://market.example".into(),
            land_images: "https://api.example".into(),
            items_mirror_enabled: true,
        };
        let mana = "0x0000000000000000000000000000000000000ffa";
        let item = || json!([asset(ASSET_TYPE_COLLECTION_ITEM, COLLECTION, "0")]);
        let price = || json!([asset(ASSET_TYPE_ERC20, mana, "1500000000000000000")]);

        traded(&pool, 50, 0, CREATOR, BUYER, item(), price()).await;
        order_successful(&pool, "matic", 50, "7").await;
        let pass = run_once(&pool, &links, Utc::now()).await.unwrap();
        assert_eq!(pass.emitted, 0, "an unpublished index is not read");
        assert_eq!(pass.waiting.len(), 5, "traded x2, orders, mints, bids");

        publish(&pool, INDEXER_TRADE_EVENTS, 100).await;
        publish(&pool, INDEXER_NFT, 100).await;
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0,
            "the first pass starts at the published block"
        );

        // A listing of an item sold twice (multi-use), a bid on it, a legacy order.
        traded(&pool, 101, 0, CREATOR, BUYER, item(), price()).await;
        traded(&pool, 101, 1, CREATOR, SELLER, item(), price()).await;
        traded(&pool, 102, 0, BUYER, CREATOR, price(), item()).await;
        order_successful(&pool, "matic", 103, "7").await;
        order_successful(&pool, "ethereum", 103, "7").await;
        // Undescribed asset, then a sale past the published block.
        traded(
            &pool,
            104,
            0,
            CREATOR,
            BUYER,
            json!([asset(ASSET_TYPE_COLLECTION_ITEM, COLLECTION, "9")]),
            price(),
        )
        .await;
        traded(&pool, 150, 0, CREATOR, BUYER, item(), price()).await;
        publish(&pool, INDEXER_TRADE_EVENTS, 120).await;
        publish(&pool, INDEXER_NFT, 120).await;

        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            4
        );
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0,
            "nothing twice"
        );

        let sold = stored(&pool, KIND_ITEM_SOLD).await;
        assert_eq!(sold.len(), 3);
        let listing = sold
            .iter()
            .find(|(a, m)| a == CREATOR && m["network"] == "MATIC")
            .unwrap();
        assert_eq!(
            listing.1,
            json!({
                "image": "https://img/0", "seller": CREATOR, "category": "wearable", "rarity": "epic",
                "link": format!("https://market.example/contracts/{COLLECTION}/items/0"), "nftName": "Shoes",
                "title": "Item Sold", "description": "Someone just bought your Shoes", "network": "MATIC",
            })
        );
        let legacy = sold.iter().find(|(a, _)| a == SELLER).unwrap();
        assert_eq!(legacy.1["seller"], SELLER);
        assert_eq!(legacy.1["network"], "polygon");
        assert_eq!(legacy.1["description"], "You just sold this Shoes.");
        assert_eq!(
            legacy.1["link"],
            format!("https://market.example/contracts/{COLLECTION}/tokens/7")
        );

        let bids = stored(&pool, KIND_BID_ACCEPTED).await;
        assert_eq!(bids.len(), 1);
        assert_eq!(bids[0].0, BUYER);
        assert_eq!(bids[0].1["seller"], CREATOR);
        assert_eq!(bids[0].1["price"], "1500000000000000000");
        assert_eq!(
            bids[0].1["description"],
            "Your bid for 1.5 MANA for this Shoes was accepted."
        );

        sqlx::query(
            "UPDATE shop.notification_cursor SET block_number = 100, log_index = 2147483647 WHERE producer LIKE 'sales:%'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0,
            "a replayed page inserts nothing twice"
        );
        let cursor: (i64, i32) = sqlx::query_as(
            "SELECT block_number, log_index FROM shop.notification_cursor WHERE producer = 'sales:traded:matic'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor, (104, 0), "the cursor stops at the published block");

        // A reorg mines transaction 101 again in block 105 and puts another
        // trade where its first log was: only that one is new.
        sqlx::query("UPDATE shop.trade_event SET block_number = 105, log_index = log_index + 3 WHERE block_number = 101")
            .execute(&pool)
            .await
            .unwrap();
        traded(&pool, 101, 0, CREATOR, BUYER, item(), price()).await;
        sqlx::query("UPDATE shop.trade_event SET tx_hash = '0xother' WHERE block_number = 101")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE shop.notification_cursor SET block_number = 100, log_index = 2147483647 WHERE producer LIKE 'sales:%'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            1,
            "the moved transaction is not announced twice"
        );
        let names: Vec<String> =
            sqlx::query_scalar("SELECT producer FROM shop.notification_cursor")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(names.len(), 2);
        assert!(
            names
                .iter()
                .all(|n| notice_cursor::PRODUCERS.contains(&n.as_str()))
        );

        pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
    }

    /// One `shop.sale_event` row; a bid's id is its position.
    #[derive(Clone, Copy)]
    struct Ev<'a> {
        block: i64,
        log: i32,
        kind: &'a str,
        contract: &'a str,
        token: &'a str,
        item: Option<&'a str>,
        seller: Option<&'a str>,
        price: Option<&'a str>,
        tx: u8,
        time: i64,
    }

    impl Default for Ev<'_> {
        fn default() -> Self {
            Ev {
                block: 0,
                log: 0,
                kind: "mint",
                contract: COLLECTION,
                token: "7",
                item: Some("0"),
                seller: None,
                price: None,
                tx: 0,
                time: Utc::now().timestamp() - 600,
            }
        }
    }

    fn bid<'a>(block: i64, token: &'a str, price: &'a str, tx: u8) -> Ev<'a> {
        Ev {
            block,
            kind: "bid_accepted",
            token,
            item: None,
            seller: Some(SELLER),
            price: Some(price),
            tx,
            ..Ev::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mint<'a>(
        block: i64,
        log: i32,
        contract: &'a str,
        token: &'a str,
        item: &'a str,
        price: Option<&'a str>,
        tx: u8,
    ) -> Ev<'a> {
        Ev {
            block,
            log,
            contract,
            token,
            item: Some(item),
            price,
            tx,
            ..Ev::default()
        }
    }

    async fn sale_event(pool: &PgPool, e: Ev<'_>) {
        let bid =
            (e.kind != "mint").then(|| format!("0x{:064x}", e.block * 100 + i64::from(e.log)));
        let emitter = if e.kind == "mint" {
            e.contract
        } else {
            super::super::sales_index::BID_V2_MATIC
        };
        sqlx::query(
            "INSERT INTO shop.sale_event (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, item_id, bid_id, buyer, seller, price, tx_hash) \
             VALUES ('matic', $1, $2, $13, $3, $4, $5, $6::numeric, $7::numeric, $8, $9, $10, $11::numeric, $12)",
        )
        .bind(e.block)
        .bind(e.log)
        .bind(e.kind)
        .bind(emitter)
        .bind(e.contract)
        .bind(e.token)
        .bind(e.item)
        .bind(bid)
        .bind(BUYER)
        .bind(e.seller)
        .bind(e.price)
        .bind(format!("0x{:064x}", e.tx))
        .bind(e.time)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn publish_sales(pool: &PgPool, block: i64) {
        sqlx::query(
            "INSERT INTO shop.chain_cursor (indexer, network, start_block, last_block, fold_through, folded_block, folded_at, full_history) \
             VALUES ('sales', 'matic', 1, $1, $1, $1, timezone('UTC', now()), true) \
             ON CONFLICT (indexer, network) DO UPDATE SET last_block = $1, fold_through = $1, folded_block = $1, folded_at = timezone('UTC', now())",
        )
        .bind(block)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_item(
        pool: &PgPool,
        contract: &str,
        item: i64,
        creator: &str,
        kind: &str,
        price: &str,
        name: &str,
    ) {
        sqlx::query(
            "INSERT INTO shop.items (network, contract_address, item_id, chain_id, item_type, urn, creator, rarity, total_supply, max_supply, available, price, image, name, category, body_shapes, search_is_collection_approved, created_at, updated_at, reviewed_at, mirrored_at) \
             VALUES ('matic', $1, $2, 137, $3, $4, $5, 'rare', 1, 100, 99, $6::numeric, 'https://img/x', $7, 'hat', ARRAY['BaseMale'], true, 1, 1, 1, timezone('UTC', now()))",
        )
        .bind(contract)
        .bind(item)
        .bind(kind)
        .bind(format!("urn:{contract}:{item}"))
        .bind(creator)
        .bind(price)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn sales_cursor(pool: &PgPool, producer: &str) -> (i64, i32) {
        sqlx::query_as(
            "SELECT block_number, log_index FROM shop.notification_cursor WHERE producer = $1",
        )
        .bind(producer)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The sales index's store mints tell each creator once per transaction
    /// at the price the store charged (none for a free or unpriced mint); a
    /// BidV2 acceptance tells its bidder "Bid Accepted" and, when paid, its
    /// seller "Item Sold", with events-notifier's L2 metadata. A sale of an
    /// item the mirror lacks holds the cursor until the item appears or a
    /// complete items pass an hour after it; history, replays and a reorg
    /// that moves a transaction notify nothing.
    #[tokio::test]
    async fn store_mints_and_bid_acceptances_notify_like_the_l2_pollers() {
        let Ok(base_url) = std::env::var("CATALYST_TEST_DATABASE_URL") else {
            return;
        };
        let opts: PgConnectOptions = base_url.parse().unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.database("postgres"))
            .await
            .unwrap();
        let Some(pool) = crate::db::scratch::isolated_pool(2).await else {
            return;
        };
        let name = pool.connect_options().get_database().unwrap().to_string();
        seed_assets(&pool).await;
        const OTHER: &str = "0x00000000000000000000000000000000000000e5";
        const OTHER_CREATOR: &str = "0x00000000000000000000000000000000000000c4";
        add_item(&pool, COLLECTION, 1, CREATOR, "emote_v1", "0", "Wave").await;
        add_item(&pool, OTHER, 0, OTHER_CREATOR, "wearable_v2", "5", "Hat").await;
        let links = Links {
            marketplace: "https://market.example".into(),
            land_images: "https://api.example".into(),
            items_mirror_enabled: true,
        };
        let token = |item: u32, issued: u32| {
            ((alloy_primitives::U256::from(item) << 216usize)
                | alloy_primitives::U256::from(issued))
            .to_string()
        };
        let (wave, unmirrored, later) = (token(1, 3), token(5, 1), token(6, 1));
        let ten = "10000000000000000000";

        sale_event(&pool, mint(90, 0, COLLECTION, "7", "0", Some(ten), 1)).await;
        publish_sales(&pool, 100).await;
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0,
            "the first pass starts at the published block"
        );

        // One transaction: two of CREATOR's item and one of OTHER_CREATOR's.
        sale_event(&pool, mint(101, 0, COLLECTION, "8", "0", Some(ten), 2)).await;
        sale_event(&pool, mint(101, 1, COLLECTION, "9", "0", Some(ten), 2)).await;
        sale_event(&pool, mint(101, 2, OTHER, "1", "0", Some("5"), 2)).await;
        // A free mint and one indexed before its price was read.
        sale_event(&pool, mint(102, 0, COLLECTION, &wave, "1", Some("0"), 3)).await;
        sale_event(&pool, mint(102, 1, COLLECTION, "10", "0", None, 3)).await;
        // A paid bid and a zero-price bid.
        sale_event(&pool, bid(103, "7", "1500000000000000000", 4)).await;
        sale_event(&pool, bid(104, &wave, "0", 5)).await;
        // A mint of an item not mirrored yet, then a later bid and a mint past the head.
        sale_event(
            &pool,
            mint(105, 0, COLLECTION, &unmirrored, "5", Some("7"), 6),
        )
        .await;
        sale_event(&pool, bid(106, "7", "2000000000000000000", 7)).await;
        sale_event(&pool, mint(150, 0, COLLECTION, "11", "0", Some(ten), 8)).await;
        publish_sales(&pool, 120).await;

        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            7,
            "two mint notices, then the bids: 2 + 1 + 2"
        );
        assert_eq!(
            sales_cursor(&pool, notice_cursor::SALES_MINT_MATIC).await,
            (102, 1),
            "the unmirrored item holds the mints"
        );
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0
        );

        add_item(
            &pool,
            COLLECTION,
            5,
            OTHER_CREATOR,
            "wearable_v2",
            "7",
            "Cap",
        )
        .await;
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            1,
            "the mirrored item is announced"
        );
        assert_eq!(
            sales_cursor(&pool, notice_cursor::SALES_MINT_MATIC).await,
            (105, 0)
        );

        let sold = stored(&pool, KIND_ITEM_SOLD).await;
        assert_eq!(sold.len(), 5);
        let mint_of = |a: &str| {
            sold.iter()
                .filter(|(r, m)| r == a && m["link"].as_str().unwrap().contains("/tokens/"))
                .count()
        };
        assert_eq!(mint_of(CREATOR), 1, "once per transaction and creator");
        assert_eq!(
            mint_of(OTHER_CREATOR),
            2,
            "its item in the shared transaction, then the held one"
        );
        let creators = sold.iter().find(|(a, _)| a == CREATOR).unwrap();
        assert_eq!(
            creators.1,
            json!({
                "image": "https://img/0", "seller": CREATOR, "category": "wearable", "rarity": "epic",
                "link": format!("https://market.example/contracts/{COLLECTION}/tokens/8"), "nftName": "Shoes",
                "title": "Item Sold", "description": "You just sold this Shoes.", "network": "polygon",
            })
        );
        let bid_sales: Vec<_> = sold.iter().filter(|(a, _)| a == SELLER).collect();
        assert_eq!(bid_sales.len(), 2, "the paid bids only");
        assert_eq!(bid_sales[0].1["description"], "You just sold this Shoes.");
        assert_eq!(
            bid_sales[0].1["link"],
            format!("https://market.example/contracts/{COLLECTION}/tokens/7")
        );

        let bids = stored(&pool, KIND_BID_ACCEPTED).await;
        assert_eq!(bids.len(), 3);
        assert!(bids.iter().all(|(a, _)| a == BUYER));
        let paid = bids
            .iter()
            .find(|(_, m)| m["price"] == "1500000000000000000")
            .unwrap();
        let free = bids.iter().find(|(_, m)| m["price"] == "0").unwrap();
        assert_eq!(
            paid.1,
            json!({
                "image": "https://img/0", "seller": SELLER, "category": "wearable", "rarity": "epic",
                "link": format!("https://market.example/contracts/{COLLECTION}/tokens/7"), "nftName": "Shoes",
                "price": "1500000000000000000", "title": "Bid Accepted",
                "description": "Your bid for 1.50 MANA for this Shoes was accepted.", "network": "polygon",
            })
        );
        assert_eq!(free.1["category"], "emote");

        // An item the mirror never holds waits until a complete pass an hour after the sale.
        sale_event(&pool, mint(115, 0, COLLECTION, &later, "6", Some("7"), 9)).await;
        sqlx::query("UPDATE shop.notification_cursor SET block_number = 110, log_index = 0 WHERE producer = $1")
            .bind(notice_cursor::SALES_MINT_MATIC)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0
        );
        assert_eq!(
            sales_cursor(&pool, notice_cursor::SALES_MINT_MATIC).await,
            (110, 0)
        );
        let held_time = Ev::default().time;
        sqlx::query(
            "INSERT INTO shop.mirror_state (mirror, network, source_url, last_complete_pass_at) \
             VALUES ('items', 'matic', 'https://items.example', to_timestamp($1::bigint + 3600) AT TIME ZONE 'UTC')",
        )
        .bind(held_time)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0
        );
        assert_eq!(
            sales_cursor(&pool, notice_cursor::SALES_MINT_MATIC).await,
            (115, 0),
            "passed without effect"
        );

        // A reorg mines transactions 2 and 4 again in block 118: nothing is new.
        sqlx::query("UPDATE shop.sale_event SET block_number = 118, log_index = log_index + CASE block_number WHEN 101 THEN 5 ELSE 10 END WHERE block_number IN (101, 103)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE shop.notification_cursor SET block_number = 100, log_index = 2147483647 WHERE producer LIKE 'sales:%'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0,
            "a replayed or reorged page inserts nothing twice"
        );

        // A stalled mirror: a sale older than ITEM_HOLD_MAX_SECS is passed.
        let (stale, off) = (token(7, 1), token(8, 1));
        publish_sales(&pool, 130).await;
        let old = Utc::now().timestamp() - crate::shared::collections::ITEM_HOLD_MAX_SECS - 60;
        sale_event(
            &pool,
            Ev {
                time: old,
                ..mint(121, 0, COLLECTION, &stale, "7", Some("7"), 10)
            },
        )
        .await;
        // A recent sale (newer than the last complete pass allows) waits.
        let recent = held_time + 100;
        sale_event(
            &pool,
            Ev {
                time: recent,
                ..mint(122, 0, COLLECTION, &off, "8", Some("7"), 11)
            },
        )
        .await;
        assert_eq!(
            run_once(&pool, &links, Utc::now()).await.unwrap().emitted,
            0
        );
        assert_eq!(
            sales_cursor(&pool, notice_cursor::SALES_MINT_MATIC).await,
            (121, 0),
            "the stale sale is passed, the recent one held"
        );
        // With the items mirror off nothing waits.
        let off_links = Links {
            items_mirror_enabled: false,
            ..links.clone()
        };
        assert_eq!(
            run_once(&pool, &off_links, Utc::now())
                .await
                .unwrap()
                .emitted,
            0
        );
        assert_eq!(
            sales_cursor(&pool, notice_cursor::SALES_MINT_MATIC).await,
            (122, 0),
            "no hold without the mirror"
        );

        pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
    }
}
