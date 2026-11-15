//! The `royalties_earned` producer: the creator (item beneficiary, else
//! creator) of a resold item hears of the royalty its sale paid. The old
//! source was the collections subgraph's `Sale` with `royaltiesCut != 0`,
//! which only the Polygon MarketplaceV2 `OrderSuccessful` (and the legacy
//! ERC721BidV2 `BidAccepted`, no sale since 2025-01-01) ever set: the cut is
//! `price * royaltiesCutPerMillion / 1e6` and the collector the item's
//! beneficiary or creator, the same split the contract pays in MANA.
//! Here the sale is the nft index's `order_successful` row of that
//! marketplace, read only up to the published block while the index is
//! ready; the first pass starts at the published block (no history is
//! replayed). Progress is `shop.notification_cursor` (`royalties_earned`), advanced with the
//! page's notifications in one transaction; the id is reorg-stable
//! (`notice_cursor::token_log_key`), so a retried page inserts nothing twice.

// TODO: on-chain ERC721Bid `BidAccepted` sales are not indexed, so they pay royalties without a `royalties_earned`.

use std::time::Duration;

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::chain_walk::{self, ChainNet, INDEXER_NFT};
use super::notice_cursor;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::shared::identity::Address;
use crate::social::notifications::store::{NewNotification, insert_notifications};
use crate::social::notifications::templates::format_mana;

pub const KIND: &str = notice_cursor::ROYALTIES_EARNED;
/// MarketplaceV2 on Polygon, the one emitter whose sales carry royalties.
pub const MARKETPLACE_V2: &str = "0x480a0f4e360e8964e68858dd231c2922f1df45ef";
/// `royaltiesCutPerMillion()` of MarketplaceV2: 25000 on chain (2026-11-05) and on every royalty sale the subgraph recorded since its deployment.
pub const ROYALTIES_CUT_PER_MILLION: i64 = 25_000;
const NET: ChainNet = ChainNet::Matic;
const PAGE_SIZE: i64 = 200;
const POLL_SECS: u64 = 60;

/// Over the sale's [`notice_cursor::token_log_key`].
pub fn notification_id(network: &str, key: &str) -> Uuid {
    notice_cursor::notification_id("royalty", network, key)
}

#[derive(sqlx::FromRow)]
struct SaleRow {
    id_key: String,
    block_number: i64,
    log_index: i32,
    block_time: i64,
    contract_address: String,
    token_id: String,
    cut: Option<String>,
    collector: Option<String>,
    seller: Option<String>,
    image: Option<String>,
    name: Option<String>,
    rarity: Option<String>,
    item_type: Option<String>,
}

fn metadata(row: &SaleRow, cut: &str, collector: &str, base: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("image".into(), json!(row.image.clone().unwrap_or_default()));
    let category = if row.item_type.as_deref() == Some("emote_v1") {
        "emote"
    } else {
        "wearable"
    };
    m.insert("category".into(), json!(category));
    if let Some(r) = &row.rarity {
        m.insert("rarity".into(), json!(r));
    }
    m.insert(
        "link".into(),
        json!(format!(
            "{base}/contracts/{}/tokens/{}",
            row.contract_address, row.token_id
        )),
    );
    if let Some(n) = &row.name {
        m.insert("nftName".into(), json!(n));
    }
    m.insert("title".into(), json!("Royalties Earned"));
    m.insert(
        "description".into(),
        json!(format!(
            "You earned {} MANA for this {}.",
            format_mana(&json!(cut)),
            row.name.as_deref().unwrap_or("undefined")
        )),
    );
    m.insert("royaltiesCut".into(), json!(cut));
    m.insert("royaltiesCollector".into(), json!(collector));
    m.insert("network".into(), json!("polygon"));
    Value::Object(m)
}

/// What one pass did.
#[derive(Debug, PartialEq, Eq)]
pub enum Pass {
    /// The nft index of Polygon is not published and fresh.
    NotReady,
    Emitted(u64),
}

/// One pass: every published MarketplaceV2 sale after the cursor, a page at a time.
pub async fn run_once(pool: &PgPool, base: &str) -> anyhow::Result<Pass> {
    let Some(published) = chain_walk::ready_cursor(pool, INDEXER_NFT, NET, chrono::Utc::now())
        .await
        .and_then(|c| c.folded_block)
    else {
        return Ok(Pass::NotReady);
    };
    notice_cursor::seed(pool, KIND, None, published, i32::MAX).await?;
    let mut emitted = 0;
    loop {
        let mut txn = pool.begin().await?;
        let (_, since_block, since_log) = notice_cursor::lock(&mut txn, KIND).await?;
        let rows: Vec<SaleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {key} AS id_key, e.block_number, e.log_index, e.block_time, e.contract_address, e.token_id::text AS token_id, \
                    div(e.amount * $6, 1000000)::text AS cut, \
                    lower(CASE WHEN i.beneficiary IS NOT NULL AND i.beneficiary <> $7 THEN i.beneficiary ELSE i.creator END) AS collector, \
                    lower(e.from_address) AS seller, \
                    i.image, i.name, i.rarity, i.item_type \
             FROM shop.nft_event e \
             LEFT JOIN shop.nft n ON n.network = e.network AND n.contract_address = e.contract_address AND n.token_id = e.token_id \
             LEFT JOIN shop.items i ON i.network = n.network AND i.contract_address = n.contract_address AND i.item_id = n.item_id \
             WHERE e.network = $1 AND (e.block_number, e.log_index) > ($2, $3) AND e.block_number <= $4 \
               AND e.kind = 'order_successful' AND e.emitter = $5 \
             ORDER BY e.block_number, e.log_index LIMIT $8",
            key = notice_cursor::token_log_key("e"),
        )))
        .bind(NET.db_name())
        .bind(since_block)
        .bind(since_log)
        .bind(published)
        .bind(MARKETPLACE_V2)
        .bind(ROYALTIES_CUT_PER_MILLION)
        .bind(super::nft_index::ZERO_ADDRESS)
        .bind(PAGE_SIZE)
        .fetch_all(&mut *txn)
        .await?;
        let batch: Vec<NewNotification> = rows
            .iter()
            .filter_map(|row| {
                let cut = row.cut.as_deref().filter(|c| *c != "0")?;
                let collector = row
                    .collector
                    .as_deref()
                    .filter(|c| *c != super::nft_index::ZERO_ADDRESS)
                    // A creator reselling their own item hears item_sold only.
                    .filter(|c| row.seller.as_deref() != Some(*c))
                    .and_then(|c| Address::parse(c).ok())?;
                Some(NewNotification {
                    id: notification_id(NET.db_name(), &row.id_key),
                    metadata: metadata(row, cut, collector.as_str(), base),
                    address: collector,
                    kind: KIND.to_string(),
                    occurred_at: chrono::DateTime::from_timestamp(row.block_time, 0)
                        .unwrap_or_default()
                        .naive_utc(),
                })
            })
            .collect();
        emitted += insert_notifications(&mut *txn, &batch).await?;
        let full = rows.len() == PAGE_SIZE as usize;
        let (to_block, to_log) = match rows.last() {
            Some(last) if full => (last.block_number, last.log_index),
            _ => (published, i32::MAX),
        };
        if (to_block, to_log) > (since_block, since_log) {
            notice_cursor::advance(&mut txn, KIND, None, to_block, to_log).await?;
        }
        txn.commit().await?;
        if !full {
            break;
        }
    }
    Ok(Pass::Emitted(emitted))
}

pub fn spawn(pool: PgPool, base: String, nft_index_enabled: bool, jobs: &Jobs) {
    tracing::info!(
        nft_index_enabled,
        "royalties_earned producer up: reads the published Polygon nft index"
    );
    jobs.spawn_periodic_leased(
        "royalty-notifications",
        PeriodicCfg::every(Duration::from_secs(POLL_SECS))
            .with_jitter(Duration::from_secs(10))
            .with_error_backoff(Duration::from_secs(600)),
        move |ctx| {
            let (pool, base) = (pool.clone(), base.clone());
            async move {
                match run_once(&pool, &base).await? {
                    Pass::NotReady => ctx.progress(format!(
                        "waiting for the matic nft index to publish (MARKETPLACE nft index enabled: {nft_index_enabled})"
                    )),
                    Pass::Emitted(n) if n > 0 => {
                        tracing::info!(emitted = n, "royalties_earned notifications emitted");
                        ctx.progress(format!("{n} emitted"));
                    }
                    Pass::Emitted(_) => {}
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

    const BASE: &str = "https://market.example";
    const COLLECTION: &str = "0x00000000000000000000000000000000000000c0";
    const CREATOR: &str = "0x00000000000000000000000000000000000000c1";
    const BENEFICIARY: &str = "0x00000000000000000000000000000000000000b1";

    async fn sale(pool: &PgPool, block: i64, log: i32, emitter: &str, token: &str, price: &str) {
        sqlx::query(
            "INSERT INTO shop.nft_event (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, from_address, to_address, amount) \
             VALUES ('matic', $1, $2, 1700000000, 'order_successful', $3, $4, $5::numeric, '0x00000000000000000000000000000000000000aa', '0x00000000000000000000000000000000000000bb', $6::numeric)",
        )
        .bind(block)
        .bind(log)
        .bind(emitter)
        .bind(COLLECTION)
        .bind(token)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn item(pool: &PgPool, item_id: i64, token: &str, beneficiary: Option<&str>) {
        sqlx::query(
            "INSERT INTO shop.items (network, contract_address, item_id, chain_id, item_type, urn, creator, beneficiary, rarity, total_supply, max_supply, available, price, image, name, category, created_at, updated_at, reviewed_at, mirrored_at) \
             VALUES ('matic', $1, $2, 137, 'wearable_v2', $3, $4, $5, 'rare', 1, 10, 9, 0, 'https://img.example/x.png', $6, 'hat', 0, 0, 0, timezone('UTC', now()))",
        )
        .bind(COLLECTION)
        .bind(item_id)
        .bind(format!("urn:decentraland:matic:collections-v2:{COLLECTION}:{item_id}"))
        .bind(CREATOR)
        .bind(beneficiary)
        .bind(format!("Hat {item_id}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO shop.nft (network, contract_address, token_id, owner, item_id, minted_block, minted_at, transferred_block, transferred_log, transferred_at) \
             VALUES ('matic', $1, $2::numeric, '0x00000000000000000000000000000000000000bb', $3, 1, 0, 1, 0, 0)",
        )
        .bind(COLLECTION)
        .bind(token)
        .bind(item_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn publish(pool: &PgPool, block: i64) {
        sqlx::query(
            "INSERT INTO shop.chain_cursor (indexer, network, start_block, last_block, last_block_hash, fold_through, folded_block, folded_at, full_history) \
             VALUES ('nft', 'matic', 0, $1, $2, $1, $1, timezone('UTC', now()), true) \
             ON CONFLICT (indexer, network) DO UPDATE SET last_block = $1, fold_through = $1, folded_block = $1, folded_at = timezone('UTC', now())",
        )
        .bind(block)
        .bind(format!("0x{:064x}", block))
        .execute(pool)
        .await
        .unwrap();
    }

    /// On a database of its own (the nft cursor row is global): nothing
    /// before the index is published; the first pass replays no history;
    /// a MarketplaceV2 sale notifies the item's beneficiary (else creator)
    /// with the old metadata, unless they sold it themselves; the V1
    /// marketplace and unpublished blocks are not read; a retried pass
    /// inserts nothing twice.
    #[tokio::test]
    async fn published_v2_sales_notify_their_royalty_collector_once() {
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

        item(&pool, 1, "1", Some(BENEFICIARY)).await;
        item(&pool, 2, "2", None).await;
        // The seller of every test sale collects this item's royalties.
        item(
            &pool,
            3,
            "3",
            Some("0x00000000000000000000000000000000000000aa"),
        )
        .await;
        sale(&pool, 100, 0, MARKETPLACE_V2, "1", "20000000000000000000").await;
        assert_eq!(run_once(&pool, BASE).await.unwrap(), Pass::NotReady);
        publish(&pool, 100).await;
        assert_eq!(
            run_once(&pool, BASE).await.unwrap(),
            Pass::Emitted(0),
            "the first pass replays no history"
        );

        sale(&pool, 101, 3, MARKETPLACE_V2, "1", "20000000000000000000").await;
        sale(&pool, 101, 7, MARKETPLACE_V2, "2", "1000000000000000000").await;
        sale(&pool, 101, 9, MARKETPLACE_V2, "3", "1000000000000000000").await;
        let v1 = super::super::nft_index::legacy_marketplaces(ChainNet::Matic)[0].0;
        sale(&pool, 102, 0, v1, "1", "5000000000000000000").await;
        sale(&pool, 102, 1, MARKETPLACE_V2, "9", "5000000000000000000").await;
        sale(&pool, 105, 0, MARKETPLACE_V2, "2", "1000000000000000000").await;
        publish(&pool, 103).await;
        assert_eq!(run_once(&pool, BASE).await.unwrap(), Pass::Emitted(2));
        assert_eq!(
            run_once(&pool, BASE).await.unwrap(),
            Pass::Emitted(0),
            "nothing twice"
        );

        let rows: Vec<(Uuid, String, Value)> = sqlx::query_as(
            "SELECT id, address, metadata FROM social.notifications WHERE type = $1 ORDER BY occurred_at, address",
        )
        .bind(KIND)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        let by = |a: &str| rows.iter().find(|r| r.1 == a).unwrap();
        let (id, _, m) = by(BENEFICIARY);
        assert_eq!(
            *id,
            notification_id(
                "matic",
                &format!(
                    "{COLLECTION}:1:order_successful:0x00000000000000000000000000000000000000aa:0x00000000000000000000000000000000000000bb:20000000000000000000:1"
                )
            ),
            "the second identical sale of the token"
        );
        assert_eq!(
            *m,
            json!({
                "image": "https://img.example/x.png",
                "category": "wearable",
                "rarity": "rare",
                "link": format!("{BASE}/contracts/{COLLECTION}/tokens/1"),
                "nftName": "Hat 1",
                "title": "Royalties Earned",
                "description": "You earned 0.50 MANA for this Hat 1.",
                "royaltiesCut": "500000000000000000",
                "royaltiesCollector": BENEFICIARY,
                "network": "polygon",
            })
        );
        let (_, _, m) = by(CREATOR);
        assert_eq!(
            m["royaltiesCut"], "25000000000000000",
            "no beneficiary: the creator"
        );

        let cursor: (i64, i32) = sqlx::query_as(
            "SELECT block_number, log_index FROM shop.notification_cursor WHERE producer = 'royalties_earned'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            cursor,
            (103, i32::MAX),
            "the cursor stops at the published block"
        );

        sqlx::query(
            "UPDATE shop.chain_cursor SET folded_at = timezone('UTC', now()) - interval '2 hours'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            run_once(&pool, BASE).await.unwrap(),
            Pass::NotReady,
            "a stale index is not read"
        );
        publish(&pool, 101).await;
        assert_eq!(
            run_once(&pool, BASE).await.unwrap(),
            Pass::Emitted(0),
            "a rewound index moves the cursor back never"
        );
        let cursor: (i64, i32) = sqlx::query_as(
            "SELECT block_number, log_index FROM shop.notification_cursor WHERE producer = 'royalties_earned'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor, (103, i32::MAX));
        publish(&pool, 105).await;
        assert_eq!(run_once(&pool, BASE).await.unwrap(), Pass::Emitted(1));

        pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
    }
}
