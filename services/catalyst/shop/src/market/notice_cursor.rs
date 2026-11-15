//! What the chain notification producers share: their names (the rows of
//! `shop.notification_cursor`), the cursor helpers, the replay that moves
//! the cursors back over rows an index writes again (a reorg rewind, a late
//! nft backfill), and the reorg-stable notification ids.
//!
//! An id keys on the log's transaction (`tx_hash`, the log's place among the
//! table's logs of that transaction, and its content), or, for `nft_event`,
//! which keeps no hash, on its token, its content and how many identical
//! logs of the token precede it. The same log mined again in another block
//! keeps its id; a different log at the same (block, log_index) gets another.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::chain_walk::{ChainNet, INDEXER_NFT, INDEXER_SALES, INDEXER_TRADE_EVENTS};
use crate::rentals::index::INDEXER_RENTALS;

pub const TRANSFER_RECEIVED: &str = "transfer_received";
pub const ROYALTIES_EARNED: &str = "royalties_earned";
pub const RENTAL_STARTED: &str = "rental_started";
pub const RENTAL_ENDED: &str = "rental_ended";
pub const SALES_TRADED_ETHEREUM: &str = "sales:traded:ethereum";
pub const SALES_TRADED_MATIC: &str = "sales:traded:matic";
pub const SALES_ORDER_SUCCESSFUL_MATIC: &str = "sales:order_successful:matic";
pub const SALES_MINT_MATIC: &str = "sales:mint:matic";
pub const SALES_BID_ACCEPTED_MATIC: &str = "sales:bid_accepted:matic";
pub const ROYALTIES_BID_ACCEPTED_MATIC: &str = "royalties:bid_accepted:matic";

/// Every producer; no other name is ever written to `shop.notification_cursor`
/// (its CHECK lists exactly these).
pub const PRODUCERS: &[&str] = &[
    TRANSFER_RECEIVED,
    ROYALTIES_EARNED,
    RENTAL_STARTED,
    RENTAL_ENDED,
    SALES_TRADED_ETHEREUM,
    SALES_TRADED_MATIC,
    SALES_ORDER_SUCCESSFUL_MATIC,
    SALES_MINT_MATIC,
    SALES_BID_ACCEPTED_MATIC,
    ROYALTIES_BID_ACCEPTED_MATIC,
];

/// How far below an index's head a replay moves the cursors back: deeper
/// rows are not announced again, and a backfill entirely below it replays nothing.
pub const REPLAY_BLOCKS: i64 = 1_000;

/// The network `producer` reads and every index it reads there.
fn reads(producer: &str) -> Option<(ChainNet, &'static [&'static str])> {
    Some(match producer {
        TRANSFER_RECEIVED => (ChainNet::Matic, &[INDEXER_NFT, INDEXER_TRADE_EVENTS]),
        ROYALTIES_EARNED | SALES_ORDER_SUCCESSFUL_MATIC => (ChainNet::Matic, &[INDEXER_NFT]),
        SALES_TRADED_MATIC => (ChainNet::Matic, &[INDEXER_TRADE_EVENTS]),
        SALES_MINT_MATIC | SALES_BID_ACCEPTED_MATIC | ROYALTIES_BID_ACCEPTED_MATIC => {
            (ChainNet::Matic, &[INDEXER_SALES])
        }
        SALES_TRADED_ETHEREUM => (ChainNet::Ethereum, &[INDEXER_TRADE_EVENTS]),
        RENTAL_STARTED | RENTAL_ENDED => (ChainNet::Ethereum, &[INDEXER_RENTALS]),
        _ => return None,
    })
}

/// The producers reading `indexer` on `net`.
pub fn producers_of(indexer: &str, net: ChainNet) -> Vec<&'static str> {
    PRODUCERS
        .iter()
        .copied()
        .filter(|p| reads(p).is_some_and(|(n, idx)| n == net && idx.contains(&indexer)))
        .collect()
}

fn known(producer: &str) -> sqlx::Result<()> {
    if PRODUCERS.contains(&producer) {
        Ok(())
    } else {
        Err(sqlx::Error::InvalidArgument(format!(
            "unknown notification producer {producer}"
        )))
    }
}

/// Seeds the producer's row (and its seed point, the floor of any replay) once.
pub(crate) async fn seed(
    pool: &PgPool,
    producer: &'static str,
    end_time: Option<i64>,
    block_number: i64,
    log_index: i32,
) -> sqlx::Result<()> {
    known(producer)?;
    sqlx::query(
        "INSERT INTO shop.notification_cursor (producer, end_time, block_number, log_index, seed_end_time, seed_block, seed_log_index) \
         VALUES ($1, $2, $3, $4, $2, $3, $4) ON CONFLICT (producer) DO NOTHING",
    )
    .bind(producer)
    .bind(end_time)
    .bind(block_number)
    .bind(log_index)
    .execute(pool)
    .await?;
    Ok(())
}

/// The producer's `end_time`, `block_number` and `log_index`, locked for the page.
pub(crate) async fn lock(
    conn: &mut PgConnection,
    producer: &'static str,
) -> sqlx::Result<(Option<i64>, i64, i32)> {
    known(producer)?;
    sqlx::query_as("SELECT end_time, block_number, log_index FROM shop.notification_cursor WHERE producer = $1 FOR UPDATE")
        .bind(producer)
        .fetch_one(conn)
        .await
}

pub(crate) async fn advance(
    conn: &mut PgConnection,
    producer: &'static str,
    end_time: Option<i64>,
    block_number: i64,
    log_index: i32,
) -> sqlx::Result<()> {
    known(producer)?;
    sqlx::query(
        "UPDATE shop.notification_cursor SET end_time = $2, block_number = $3, log_index = $4, \
         updated_at = timezone('UTC', now()) WHERE producer = $1",
    )
    .bind(producer)
    .bind(end_time)
    .bind(block_number)
    .bind(log_index)
    .execute(conn)
    .await?;
    Ok(())
}

/// A cursor move-back worked out before the index's rows change.
#[derive(Debug, PartialEq, Eq)]
pub struct Replay {
    /// Each producer and the block it moves back to just after.
    moves: Vec<(&'static str, i64)>,
    /// `rental_ended`'s end-time floor: the earliest rent it may have missed.
    rental_since: Option<i64>,
}

/// Plans moving the cursors of the producers reading `indexer` on `net`
/// back to just after `from_block`, for rows written again up to
/// `max_block`. Each producer is clamped by the lowest head of the indexes
/// it reads ([`reads`]), which its cursor never passes: nothing when
/// `max_block` is [`REPLAY_BLOCKS`] or more below it, never further back than that.
/// Reads only, so it runs before the rows are deleted without holding the
/// cursor rows.
pub async fn plan_replay(
    conn: &mut PgConnection,
    indexer: &str,
    net: ChainNet,
    from_block: i64,
    max_block: i64,
) -> sqlx::Result<Option<Replay>> {
    let mut moves = Vec::new();
    for producer in producers_of(indexer, net) {
        let indexes = reads(producer).map_or(&[][..], |(_, idx)| idx);
        let head: Option<i64> = sqlx::query_scalar(
            "SELECT min(last_block) FROM shop.chain_cursor WHERE indexer = ANY($1) AND network = $2",
        )
        .bind(indexes)
        .bind(net.db_name())
        .fetch_one(&mut *conn)
        .await?;
        match head {
            Some(head) if max_block > head - REPLAY_BLOCKS => {
                moves.push((producer, from_block.max(head - REPLAY_BLOCKS)));
            }
            _ => {}
        }
    }
    if moves.is_empty() {
        return Ok(None);
    }
    let rental_since = match moves.iter().find(|(p, _)| *p == RENTAL_ENDED) {
        Some(&(_, from)) => sqlx::query_scalar(
            "SELECT min(block_time) FROM shop.rental_event WHERE network = $1 AND kind = 'rented' AND block_number > $2",
        )
        .bind(net.db_name())
        .bind(from)
        .fetch_one(&mut *conn)
        .await?,
        None => None,
    };
    Ok(Some(Replay {
        moves,
        rental_since,
    }))
}

impl Replay {
    /// Moves the cursors back, never below their seed; the ids keep the
    /// replay from notifying twice. Locks the cursor rows until commit.
    pub async fn apply(self, conn: &mut PgConnection) -> sqlx::Result<()> {
        for (producer, from) in self.moves {
            sqlx::query(
            "UPDATE shop.notification_cursor SET \
                 block_number = CASE WHEN ($2::bigint, 2147483647) > (seed_block, seed_log_index) THEN $2 ELSE seed_block END, \
                 log_index = CASE WHEN ($2::bigint, 2147483647) > (seed_block, seed_log_index) THEN 2147483647 ELSE seed_log_index END, \
                 updated_at = timezone('UTC', now()) \
             WHERE producer = $1 AND end_time IS NULL \
               AND (block_number, log_index) > ($2::bigint, 2147483647) \
               AND (block_number, log_index) > (seed_block, seed_log_index)",
            )
            .bind(producer)
            .bind(from)
            .execute(&mut *conn)
            .await?;
        }
        if let Some(since) = self.rental_since {
            sqlx::query(
                "UPDATE shop.notification_cursor SET \
                     end_time = CASE WHEN ($2::bigint, 0::bigint, -1) > (seed_end_time, seed_block, seed_log_index) THEN $2 ELSE seed_end_time END, \
                     block_number = CASE WHEN ($2::bigint, 0::bigint, -1) > (seed_end_time, seed_block, seed_log_index) THEN 0 ELSE seed_block END, \
                     log_index = CASE WHEN ($2::bigint, 0::bigint, -1) > (seed_end_time, seed_block, seed_log_index) THEN -1 ELSE seed_log_index END, \
                     updated_at = timezone('UTC', now()) \
                 WHERE producer = $1 \
                   AND (end_time, block_number, log_index) > ($2::bigint, 0::bigint, -1) \
                   AND (end_time, block_number, log_index) > (seed_end_time, seed_block, seed_log_index)",
            )
            .bind(RENTAL_ENDED)
            .bind(since)
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
}

/// [`plan_replay`] then [`Replay::apply`], for rows added in place (a backfill).
pub async fn replay(
    conn: &mut PgConnection,
    indexer: &str,
    net: ChainNet,
    from_block: i64,
    max_block: i64,
) -> sqlx::Result<()> {
    if let Some(r) = plan_replay(conn, indexer, net, from_block, max_block).await? {
        r.apply(conn).await?;
    }
    Ok(())
}

/// A chain notification's id: `kind` and `network` over the log's key
/// ([`tx_log_key`] or [`token_log_key`]).
pub fn notification_id(kind: &str, network: &str, key: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("{kind}:{network}:{key}").as_bytes(),
    )
}

/// SQL for the key of a log of `shop.{table}` aliased `a`: its transaction,
/// its place among the table's logs of it, and `content`.
pub fn tx_log_key(table: &str, a: &str, content: &str) -> String {
    format!(
        "({a}.tx_hash || ':' || (SELECT count(*) FROM shop.{table} o \
             WHERE o.network = {a}.network AND o.block_number = {a}.block_number \
               AND o.tx_hash = {a}.tx_hash AND o.log_index < {a}.log_index) || ':' || {content})"
    )
}

/// SQL for the key of a `shop.nft_event` log aliased `a`: its token, its
/// content and how many identical logs of the token precede it.
pub fn token_log_key(a: &str) -> String {
    format!(
        "({a}.contract_address || ':' || {a}.token_id || ':' || {a}.kind \
          || ':' || coalesce(lower({a}.from_address), '') || ':' || coalesce(lower({a}.to_address), '') \
          || ':' || coalesce({a}.amount::text, '') || ':' || (SELECT count(*) FROM shop.nft_event o \
             WHERE o.network = {a}.network AND o.contract_address = {a}.contract_address \
               AND o.token_id = {a}.token_id AND o.kind = {a}.kind \
               AND lower(o.from_address) IS NOT DISTINCT FROM lower({a}.from_address) \
               AND lower(o.to_address) IS NOT DISTINCT FROM lower({a}.to_address) \
               AND o.amount IS NOT DISTINCT FROM {a}.amount \
               AND (o.block_number, o.log_index) < ({a}.block_number, {a}.log_index)))"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::chain_walk::INDEXER_MANA;

    #[test]
    fn the_producers_are_the_listed_names() {
        for p in PRODUCERS {
            assert!(reads(p).is_some(), "{p} lists its indexes");
        }
        assert!(producers_of(INDEXER_MANA, ChainNet::Matic).is_empty());
        assert_eq!(
            producers_of(INDEXER_TRADE_EVENTS, ChainNet::Matic),
            [TRANSFER_RECEIVED, SALES_TRADED_MATIC]
        );
        assert_eq!(
            producers_of(INDEXER_SALES, ChainNet::Matic),
            [
                SALES_MINT_MATIC,
                SALES_BID_ACCEPTED_MATIC,
                ROYALTIES_BID_ACCEPTED_MATIC
            ]
        );
        assert!(producers_of(INDEXER_SALES, ChainNet::Ethereum).is_empty());
        let mut sorted = PRODUCERS.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), PRODUCERS.len());
    }

    #[test]
    fn ids_differ_by_kind_network_and_key() {
        let id = notification_id("traded", "matic", "0xab:0:0x01");
        assert_eq!(id, notification_id("traded", "matic", "0xab:0:0x01"));
        assert_ne!(id, notification_id("traded", "matic", "0xab:1:0x01"));
        assert_ne!(id, notification_id("traded", "ethereum", "0xab:0:0x01"));
        assert_ne!(id, notification_id("royalty", "matic", "0xab:0:0x01"));
    }

    /// Every listed name passes the table's CHECK; the replay floor is the
    /// seed, a deep backfill replays nothing, and planning holds no lock.
    #[tokio::test]
    async fn the_cursor_table_takes_every_producer_and_replays_keep_their_seed() {
        let Some(db) = crate::market::transfer_notices::tests::PrivateDb::create("noticecur").await
        else {
            return;
        };
        let pool = &db.pool;
        for &p in PRODUCERS {
            let end = (p == RENTAL_ENDED).then_some(1_000);
            seed(pool, p, end, 5_000, 3).await.unwrap();
        }
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM shop.notification_cursor")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(rows, PRODUCERS.len() as i64);
        for name in ["Transfer Received", "sales:traded:base"] {
            assert!(
                sqlx::query("INSERT INTO shop.notification_cursor (producer, block_number, log_index, seed_block, seed_log_index) VALUES ($1, 0, 0, 0, 0)")
                    .bind(name)
                    .execute(pool)
                    .await
                    .is_err(),
                "{name}"
            );
        }
        let check: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'notification_cursor_producer_known'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let mut listed: Vec<&str> = check.split('\'').skip(1).step_by(2).collect();
        listed.sort();
        let mut known = PRODUCERS.to_vec();
        known.sort();
        assert_eq!(listed, known, "the CHECK lists exactly PRODUCERS: {check}");
        assert!(seed(pool, "sales:traded:base", None, 0, 0).await.is_err());
        let mut conn = pool.acquire().await.unwrap();
        assert!(lock(&mut conn, "sales:traded:base").await.is_err());
        assert!(
            advance(&mut conn, "sales:traded:base", None, 0, 0)
                .await
                .is_err()
        );
        drop(conn);
        crate::market::transfer_notices::tests::publish(
            pool,
            INDEXER_NFT,
            "matic",
            9_000,
            1_700_000_000,
        )
        .await;
        let cursor = |p: &'static str| async move {
            sqlx::query_as::<_, (i64, i32)>(
                "SELECT block_number, log_index FROM shop.notification_cursor WHERE producer = $1",
            )
            .bind(p)
            .fetch_one(pool)
            .await
            .unwrap()
        };
        advance(
            &mut pool.acquire().await.unwrap(),
            TRANSFER_RECEIVED,
            None,
            8_900,
            0,
        )
        .await
        .unwrap();

        // A backfill chunk entirely at or below head - REPLAY_BLOCKS is history.
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            plan_replay(&mut tx, INDEXER_NFT, ChainNet::Matic, 10, 8_000)
                .await
                .unwrap(),
            None
        );
        tx.rollback().await.unwrap();

        // Planning locks nothing: a producer still takes its row meanwhile.
        let mut tx = pool.begin().await.unwrap();
        let plan = plan_replay(&mut tx, INDEXER_NFT, ChainNet::Matic, 10, 8_500)
            .await
            .unwrap()
            .unwrap();
        let mut other = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM shop.notification_cursor WHERE producer = $1 FOR UPDATE NOWAIT")
            .bind(TRANSFER_RECEIVED)
            .execute(&mut *other)
            .await
            .expect("the planned replay holds no cursor row");
        other.rollback().await.unwrap();
        plan.apply(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            cursor(TRANSFER_RECEIVED).await,
            (8_000, i32::MAX),
            "back to head - REPLAY_BLOCKS"
        );

        // Seeded 500 blocks below the head: the replay stops at the seed.
        sqlx::query(
            "UPDATE shop.notification_cursor SET block_number = 8950, seed_block = 8500, seed_log_index = 4 WHERE producer = $1",
        )
        .bind(ROYALTIES_EARNED)
        .execute(pool)
        .await
        .unwrap();
        replay(
            &mut pool.acquire().await.unwrap(),
            INDEXER_NFT,
            ChainNet::Matic,
            10,
            8_999,
        )
        .await
        .unwrap();
        assert_eq!(cursor(ROYALTIES_EARNED).await, (8_500, 4));
        assert_eq!(
            cursor(SALES_TRADED_MATIC).await,
            (5_000, 3),
            "another index's producer is not moved"
        );
        db.drop().await;
    }

    /// `transfer_received` reads up to the lower of the nft and trade heads,
    /// so a reorg of the lagging trade index replays it by that head.
    #[tokio::test]
    async fn replays_clamp_each_producer_by_the_index_it_tracks() {
        let Some(db) = crate::market::transfer_notices::tests::PrivateDb::create("noticeidx").await
        else {
            return;
        };
        let pool = &db.pool;
        let publish = |indexer: &'static str, block: i64| async move {
            crate::market::transfer_notices::tests::publish(
                pool,
                indexer,
                "matic",
                block,
                1_700_000_000,
            )
            .await;
        };
        let cursor = |p: &'static str| async move {
            sqlx::query_as::<_, (i64, i32)>(
                "SELECT block_number, log_index FROM shop.notification_cursor WHERE producer = $1",
            )
            .bind(p)
            .fetch_one(pool)
            .await
            .unwrap()
        };
        for p in [TRANSFER_RECEIVED, SALES_TRADED_MATIC] {
            seed(pool, p, None, 0, -1).await.unwrap();
        }
        // run_once never passes min(nft head, trade head) = 3000.
        let mut conn = pool.acquire().await.unwrap();
        advance(&mut conn, TRANSFER_RECEIVED, None, 2_999, 0)
            .await
            .unwrap();
        advance(&mut conn, SALES_TRADED_MATIC, None, 2_999, 0)
            .await
            .unwrap();
        drop(conn);
        publish(INDEXER_NFT, 9_000).await;
        publish(INDEXER_TRADE_EVENTS, 3_000).await;

        // Below head - REPLAY_BLOCKS of both indexes: history.
        replay(
            &mut pool.acquire().await.unwrap(),
            INDEXER_TRADE_EVENTS,
            ChainNet::Matic,
            10,
            1_500,
        )
        .await
        .unwrap();
        assert_eq!(cursor(TRANSFER_RECEIVED).await, (2_999, 0));
        assert_eq!(cursor(SALES_TRADED_MATIC).await, (2_999, 0));

        // The trade index lags the nft one by 6000 blocks; its reorg at its
        // head rewinds both producers reading it.
        replay(
            &mut pool.acquire().await.unwrap(),
            INDEXER_TRADE_EVENTS,
            ChainNet::Matic,
            2_990,
            2_999,
        )
        .await
        .unwrap();
        assert_eq!(cursor(TRANSFER_RECEIVED).await, (2_990, i32::MAX));
        assert_eq!(cursor(SALES_TRADED_MATIC).await, (2_990, i32::MAX));

        // An nft reorg near its own head is above the transfer cursor.
        replay(
            &mut pool.acquire().await.unwrap(),
            INDEXER_NFT,
            ChainNet::Matic,
            8_950,
            8_990,
        )
        .await
        .unwrap();
        assert_eq!(cursor(TRANSFER_RECEIVED).await, (2_990, i32::MAX));
        db.drop().await;
    }
}
