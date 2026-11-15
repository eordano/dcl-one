//! The `rental_started` and `rental_ended` producers (events-notifier's
//! rental-started.ts / rental-ended.ts over the rentals subgraph): the
//! lessor hears that their LAND was rented and, once the period is over,
//! that it ended. The source is the `rentals` index's `rented` rows
//! (`shop.rental_event`), read only while it is published and never past
//! its published block.
//!
//! Every `AssetRented` notifies its start, extensions included, as every one
//! was a new subgraph `Rental`. An extension adds its days to the running
//! rental (Rentals.sol `_rent`), so a rent and its extensions are one period
//! that ends once, at its start plus all their days; it is announced when
//! the published block's time has passed that end, so no later extension
//! can move it. Progress is the `rental_started` (chain position) and
//! `rental_ended` (end time, then the period's first rent) rows of
//! `shop.notification_cursor`; the first pass starts at the published head.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::index::INDEXER_RENTALS;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::land_authz::{ESTATE_REGISTRY_MAINNET, LAND_REGISTRY_MAINNET};
use crate::market::MarketplaceState;
use crate::market::chain_walk::{self, ChainNet};
use crate::market::notice_cursor::{self, advance, lock, seed};
use crate::market::transfer_notices::Pass;
use crate::social::Address;
use crate::social::notifications::store::{NewNotification, insert_notifications};

pub const KIND_STARTED: &str = notice_cursor::RENTAL_STARTED;
pub const KIND_ENDED: &str = notice_cursor::RENTAL_ENDED;
const PAGE_SIZE: i64 = 200;
const POLL_SECS: u64 = 60;
const DAY: i64 = 86_400;

/// Over the rent's [`notice_cursor::tx_log_key`].
pub fn started_id(network: &str, key: &str) -> Uuid {
    notice_cursor::notification_id("rental-started", network, key)
}

/// Over the key of the period's first rent.
pub fn ended_id(network: &str, key: &str) -> Uuid {
    notice_cursor::notification_id("rental-ended", network, key)
}

/// Every published rent of the `tokens` (a select of `(rentals_address,
/// contract_address, token_id)` over published rents `x`) with its period:
/// `head_*` the period's first rent, `days_so_far` its days through this
/// rent, `total_days` all of them. `$1` the published block, `$2`/`$3` the
/// LAND and Estate registries.
fn rents(tokens: &str) -> String {
    format!(
        "
WITH r AS (
    SELECT e.*, {key} AS id_key, count(*) FILTER (WHERE NOT e.is_extension) OVER (
        PARTITION BY e.rentals_address, e.contract_address, e.token_id
        ORDER BY e.block_number, e.log_index) AS period
    FROM shop.rental_event e
    WHERE e.network = 'ethereum' AND e.kind = 'rented' AND e.block_number <= $1
      AND (e.rentals_address, e.contract_address, e.token_id) IN (
          SELECT x.rentals_address, x.contract_address, x.token_id FROM shop.rental_event x
          WHERE x.network = 'ethereum' AND x.kind = 'rented' AND x.block_number <= $1 {tokens})
), p AS (
    SELECT r.*,
        first_value(r.block_number) OVER w AS head_block,
        first_value(r.log_index) OVER w AS head_log,
        first_value(r.block_time) OVER w AS head_time,
        first_value(r.id_key) OVER w AS head_key,
        sum(r.rental_days) OVER w AS days_so_far,
        sum(r.rental_days) OVER whole AS total_days,
        last_value(r.operator) OVER whole AS last_operator
    FROM r
    WINDOW w AS (PARTITION BY r.rentals_address, r.contract_address, r.token_id, r.period
                 ORDER BY r.block_number, r.log_index),
           whole AS (PARTITION BY r.rentals_address, r.contract_address, r.token_id, r.period
                     ORDER BY r.block_number, r.log_index
                     ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)
)",
        key = notice_cursor::tx_log_key("rental_event", "e", "coalesce(e.signature, '')")
    )
}

/// The parcel (`x,y`) a rented token stands for and whether it is an
/// estate of several, over the period rows `p`. An estate's parcel comes
/// from the land-authz estate links, read without its readiness gate since
/// it only describes the notice: while a registry switch replays
/// (`land_authz::indexer::begin_replay` deletes the links, and the new
/// registries' fold rebuilds them), an estate's notice names no parcel.
const PLACE: &str = "
    CASE WHEN p.contract_address = $2
         THEN land_authz.token_x(p.token_id) || ',' || land_authz.token_y(p.token_id)
         ELSE est.land END AS land,
    COALESCE(est.parcels > 1, p.contract_address = $3) AS estate";
const PLACE_JOIN: &str = "
    LEFT JOIN LATERAL (
        SELECT land_authz.token_x(el.land_token_id) || ',' || land_authz.token_y(el.land_token_id) AS land,
               count(*) OVER () AS parcels
        FROM land_authz.estate_land el
        WHERE p.contract_address = $3 AND el.estate_token_id = p.token_id
        ORDER BY el.land_token_id LIMIT 1
    ) est ON true";

#[derive(sqlx::FromRow)]
struct RentRow {
    id_key: String,
    block_number: i64,
    log_index: i32,
    started_at: i64,
    ends_at: i64,
    contract_address: String,
    token_id: String,
    lessor: Option<String>,
    tenant: Option<String>,
    operator: Option<String>,
    land: Option<String>,
    estate: bool,
}

fn metadata(row: &RentRow, base: &str, started: bool) -> Value {
    let what = if row.estate { "ESTATE" } else { "LAND" };
    let at = row
        .land
        .as_deref()
        .map(|l| format!(" at {l}"))
        .unwrap_or_default();
    let tenant = row.tenant.as_deref().unwrap_or_default();
    let (title, description) = if started {
        (
            "LAND Rented",
            format!("Your {what}{at} was rented by {tenant}."),
        )
    } else {
        (
            "Rent Period Ending",
            format!("The rent of your {what}{at} has ended."),
        )
    };
    json!({
        "address": row.lessor,
        "contract": row.contract_address,
        "lessor": row.lessor,
        "tenant": row.tenant,
        "operator": row.operator,
        "startedAt": row.started_at.to_string(),
        "endedAt": row.ends_at.to_string(),
        "tokenId": row.token_id,
        "link": format!("{base}/contracts/{}/tokens/{}/manage", row.contract_address, row.token_id),
        "title": title,
        "land": row.land,
        "description": description,
    })
}

fn notification(
    id: Uuid,
    kind: &str,
    row: &RentRow,
    base: &str,
    at: i64,
) -> Option<NewNotification> {
    Some(NewNotification {
        id,
        address: Address::parse(row.lessor.as_deref()?).ok()?,
        kind: kind.to_string(),
        metadata: metadata(row, base, kind == KIND_STARTED),
        occurred_at: chrono::DateTime::from_timestamp(at, 0)
            .unwrap_or_default()
            .naive_utc(),
    })
}

async fn registries(pool: &PgPool) -> sqlx::Result<(String, String)> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT lower(land_registry), lower(estate_registry) FROM land_authz.index_cursor ORDER BY network LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_else(|| {
        (
            LAND_REGISTRY_MAINNET.to_ascii_lowercase(),
            ESTATE_REGISTRY_MAINNET.to_ascii_lowercase(),
        )
    }))
}

/// The published block and its time, while the rentals index is published.
async fn published(pool: &PgPool) -> Option<(i64, i64)> {
    let c = chain_walk::ready_cursor(pool, INDEXER_RENTALS, ChainNet::Ethereum, Utc::now()).await?;
    Some((c.last_block, c.last_block_time?))
}

const WAITING: &str =
    "the rentals index is not published (chain-1 RPC unset or SHOP_RENTALS_INDEX_ENABLED=false)";

/// One `rental_started` pass: every published rent after the cursor.
pub async fn run_started(pool: &PgPool, base: &str) -> anyhow::Result<Pass> {
    let Some((upto, _)) = published(pool).await else {
        return Ok(Pass::Waiting(WAITING));
    };
    let latest: Option<(i64, i32)> = sqlx::query_as(
        "SELECT block_number, log_index FROM shop.rental_event \
         WHERE network = 'ethereum' AND kind = 'rented' AND block_number <= $1 \
         ORDER BY block_number DESC, log_index DESC LIMIT 1",
    )
    .bind(upto)
    .fetch_optional(pool)
    .await?;
    let (seed_block, seed_log) = latest.unwrap_or((0, -1));
    seed(pool, KIND_STARTED, None, seed_block, seed_log).await?;
    let (land, estate) = registries(pool).await?;
    let network = ChainNet::Ethereum.db_name();
    let mut emitted = 0;
    loop {
        let mut txn = pool.begin().await?;
        let (_, since_block, since_log) = lock(&mut txn, KIND_STARTED).await?;
        // Only the tokens rented after the cursor.
        let rents = rents("AND (x.block_number, x.log_index) > ($4, $5)");
        let rows: Vec<RentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{rents} SELECT p.id_key, p.block_number, p.log_index, p.block_time AS started_at, \
                    p.head_time + p.days_so_far * {DAY} AS ends_at, p.contract_address, p.token_id::text AS token_id, \
                    lower(p.signer) AS lessor, lower(p.tenant) AS tenant, lower(p.operator) AS operator, {PLACE} \
             FROM p {PLACE_JOIN} \
             WHERE (p.block_number, p.log_index) > ($4, $5) \
             ORDER BY p.block_number, p.log_index LIMIT $6"
        )))
        .bind(upto)
        .bind(&land)
        .bind(&estate)
        .bind(since_block)
        .bind(since_log)
        .bind(PAGE_SIZE)
        .fetch_all(&mut *txn)
        .await?;
        let Some(last) = rows.last() else {
            txn.rollback().await?;
            break;
        };
        let (last_block, last_log) = (last.block_number, last.log_index);
        let batch: Vec<NewNotification> = rows
            .iter()
            .filter_map(|r| {
                let id = started_id(network, &r.id_key);
                notification(id, KIND_STARTED, r, base, r.started_at)
            })
            .collect();
        emitted += insert_notifications(&mut *txn, &batch).await?;
        advance(&mut txn, KIND_STARTED, None, last_block, last_log).await?;
        txn.commit().await?;
        if rows.len() < PAGE_SIZE as usize {
            break;
        }
    }
    Ok(Pass::Emitted(emitted))
}

/// One `rental_ended` pass: every period whose end the published block's
/// time has passed, after the cursor, in end order.
pub async fn run_ended(pool: &PgPool, base: &str) -> anyhow::Result<Pass> {
    let Some((upto, upto_time)) = published(pool).await else {
        return Ok(Pass::Waiting(WAITING));
    };
    // Periods that ended before the first pass are history.
    seed(pool, KIND_ENDED, Some(upto_time), upto, i32::MAX).await?;
    let (land, estate) = registries(pool).await?;
    let network = ChainNet::Ethereum.db_name();
    let mut emitted = 0;
    loop {
        let mut txn = pool.begin().await?;
        let (since_time, since_block, since_log) = lock(&mut txn, KIND_ENDED).await?;
        // Only the tokens with a period that may end after the cursor: none
        // ends later than its token's last rent plus all the token's days.
        let rents = rents(&format!(
            "GROUP BY 1, 2, 3 HAVING max(x.block_time) + sum(x.rental_days) * {DAY} >= $4"
        ));
        let rows: Vec<RentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{rents}, ends AS (
                SELECT p.*, p.head_time + p.total_days * {DAY} AS ends_at FROM p
                WHERE (p.block_number, p.log_index) = (p.head_block, p.head_log)
             )
             SELECT p.head_key AS id_key, p.block_number, p.log_index, p.head_time AS started_at, p.ends_at, p.contract_address, \
                    p.token_id::text AS token_id, lower(p.signer) AS lessor, lower(p.tenant) AS tenant, \
                    lower(p.last_operator) AS operator, {PLACE} \
             FROM ends p {PLACE_JOIN} \
             WHERE (p.ends_at, p.block_number, p.log_index) > ($4, $5, $6) AND p.ends_at < $7 \
             ORDER BY p.ends_at, p.block_number, p.log_index LIMIT $8"
        )))
        .bind(upto)
        .bind(&land)
        .bind(&estate)
        .bind(since_time.unwrap_or(upto_time))
        .bind(since_block)
        .bind(since_log)
        .bind(upto_time)
        .bind(PAGE_SIZE)
        .fetch_all(&mut *txn)
        .await?;
        let Some(last) = rows.last() else {
            txn.rollback().await?;
            break;
        };
        let (last_end, last_block, last_log) = (last.ends_at, last.block_number, last.log_index);
        let batch: Vec<NewNotification> = rows
            .iter()
            .filter_map(|r| {
                let id = ended_id(network, &r.id_key);
                notification(id, KIND_ENDED, r, base, r.ends_at)
            })
            .collect();
        emitted += insert_notifications(&mut *txn, &batch).await?;
        advance(&mut txn, KIND_ENDED, Some(last_end), last_block, last_log).await?;
        txn.commit().await?;
        if rows.len() < PAGE_SIZE as usize {
            break;
        }
    }
    Ok(Pass::Emitted(emitted))
}

pub fn spawn(state: &Arc<MarketplaceState>, jobs: &Jobs) {
    tracing::info!(
        "rental notifications run while the rentals index is published; \
         until then their job progress reads `waiting`"
    );
    let pool = state.pool.clone();
    let base = state.settings.market.marketplace_base_url.clone();
    jobs.spawn_periodic_leased(
        "rental-notifications",
        PeriodicCfg::every(Duration::from_secs(POLL_SECS))
            .with_jitter(Duration::from_secs(10))
            .with_error_backoff(Duration::from_secs(600)),
        move |ctx| {
            let (pool, base) = (pool.clone(), base.clone());
            async move {
                let started = run_started(&pool, &base).await?;
                let ended = run_ended(&pool, &base).await?;
                match (started, ended) {
                    (Pass::Emitted(s), Pass::Emitted(e)) => {
                        if s + e > 0 {
                            tracing::info!(started = s, ended = e, "rental notifications emitted");
                        }
                        ctx.progress(format!("{s} started, {e} ended emitted"));
                    }
                    (Pass::Waiting(why), _) | (_, Pass::Waiting(why)) => {
                        ctx.progress(format!("waiting: {why}"))
                    }
                }
                Ok(())
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::transfer_notices::tests::{PrivateDb, publish};

    #[test]
    fn ids_are_stable_and_distinct_per_kind_and_event() {
        assert_eq!(
            started_id("ethereum", "0xa:0:0xs"),
            started_id("ethereum", "0xa:0:0xs")
        );
        assert_ne!(
            started_id("ethereum", "0xa:0:0xs"),
            started_id("ethereum", "0xa:1:0xs")
        );
        assert_ne!(
            started_id("ethereum", "0xa:0:0xs"),
            ended_id("ethereum", "0xa:0:0xs")
        );
    }

    /// Nothing before the index publishes; the first pass announces no past
    /// rent and no period that ended before it; a rent and its extension
    /// each notify the lessor's start, the period ends once at its start
    /// plus all its days, only after the published time passes it; nothing
    /// past the published block; a retried pass inserts nothing twice.
    #[tokio::test]
    async fn rentals_notify_their_start_and_their_end_once() {
        let Some(db) = PrivateDb::create("rentnotif").await else {
            return;
        };
        let pool = db.pool.clone();
        let base = "https://decentraland.org/marketplace";
        let land = LAND_REGISTRY_MAINNET.to_ascii_lowercase();
        let lessor = "0x00000000000000000000000000000000000000a1";
        let tenant = "0x00000000000000000000000000000000000000b0";
        // Parcel 1,2.
        let parcel = "340282366920938463463374607431768211458";
        let t0: i64 = 1_700_000_000;
        let rent = |block: i64, log: i32, time: i64, token: &'static str, days: i32, ext: bool| {
            let (pool, land) = (pool.clone(), land.clone());
            async move {
                sqlx::query(
                    "INSERT INTO shop.rental_event (network, block_number, log_index, block_time, rentals_address, kind, contract_address, token_id, signer, tenant, operator, rental_days, price_per_day, is_extension, sender, signature, tx_hash) \
                     VALUES ('ethereum', $1, $2, $3, '0x3a1469499d0be105d4f77045ca403a5f6dc2f3f5', 'rented', $4, $5::numeric, $6, $7, $7, $8, 1, $9, $7, '0x' || md5($1::text || $2::text), '0xtx')",
                )
                .bind(block)
                .bind(log)
                .bind(time)
                .bind(land)
                .bind(token)
                .bind(lessor)
                .bind(tenant)
                .bind(days)
                .bind(ext)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        // History: one period already over, one still running at boot.
        rent(10, 0, t0, "7", 1, false).await;
        rent(11, 0, t0 + 10, parcel, 1, false).await;
        assert_eq!(
            run_started(&pool, base).await.unwrap(),
            Pass::Waiting(WAITING)
        );
        publish(&pool, INDEXER_RENTALS, "ethereum", 20, t0 + 2 * DAY).await;
        // The parcel's first period was extended before boot (ends t0+10+3d).
        rent(12, 0, t0 + 20, parcel, 2, true).await;
        assert_eq!(
            run_started(&pool, base).await.unwrap(),
            Pass::Emitted(0),
            "no past rent"
        );
        assert_eq!(
            run_ended(&pool, base).await.unwrap(),
            Pass::Emitted(0),
            "no past end"
        );
        // A new period of token 8, extended once; and one past the published block.
        rent(21, 0, t0 + 3 * DAY, "8", 1, false).await;
        rent(22, 0, t0 + 3 * DAY + 5, "8", 2, true).await;
        rent(31, 0, t0 + 4 * DAY, "9", 1, false).await;
        publish(&pool, INDEXER_RENTALS, "ethereum", 30, t0 + 3 * DAY + 100).await;
        assert_eq!(run_started(&pool, base).await.unwrap(), Pass::Emitted(2));
        assert_eq!(
            run_started(&pool, base).await.unwrap(),
            Pass::Emitted(0),
            "nothing twice"
        );
        // The parcel's period (t0+10+3d) has ended; token 8's (t0+3d+3d) has not.
        publish(&pool, INDEXER_RENTALS, "ethereum", 30, t0 + 3 * DAY + 11).await;
        assert_eq!(run_ended(&pool, base).await.unwrap(), Pass::Emitted(1));
        publish(&pool, INDEXER_RENTALS, "ethereum", 30, t0 + 6 * DAY + 1).await;
        assert_eq!(run_ended(&pool, base).await.unwrap(), Pass::Emitted(1));
        assert_eq!(
            run_ended(&pool, base).await.unwrap(),
            Pass::Emitted(0),
            "nothing twice"
        );
        let rows: Vec<(Uuid, String, String, Value)> = sqlx::query_as(
            "SELECT id, address, type, metadata FROM social.notifications ORDER BY occurred_at, type DESC",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
        let key = |block: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT tx_hash || ':0:' || signature FROM shop.rental_event WHERE block_number = $1",
                )
                .bind(block)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(
            ids,
            vec![
                started_id("ethereum", &key(21).await),
                started_id("ethereum", &key(22).await),
                ended_id("ethereum", &key(11).await),
                ended_id("ethereum", &key(21).await),
            ]
        );
        assert!(rows.iter().all(|r| r.1 == lessor));
        assert_eq!(
            rows[1].3,
            json!({
                "address": lessor, "contract": land, "lessor": lessor, "tenant": tenant, "operator": tenant,
                "startedAt": (t0 + 3 * DAY + 5).to_string(),
                "endedAt": (t0 + 6 * DAY).to_string(),
                "tokenId": "8",
                "link": format!("{base}/contracts/{land}/tokens/8/manage"),
                "title": "LAND Rented",
                "land": "0,8",
                "description": format!("Your LAND at 0,8 was rented by {tenant}."),
            })
        );
        assert_eq!(rows[2].3["title"], "Rent Period Ending");
        assert_eq!(rows[2].3["land"], "1,2");
        assert_eq!(rows[2].3["startedAt"], (t0 + 10).to_string());
        assert_eq!(rows[2].3["endedAt"], (t0 + 10 + 3 * DAY).to_string());
        assert_eq!(
            rows[2].3["description"],
            "The rent of your LAND at 1,2 has ended."
        );

        // A rewind to block 20 (planned, rows deleted, then applied) moves
        // both cursors back over the rents after it; the rewritten rows are
        // announced nothing twice.
        let mut tx = pool.begin().await.unwrap();
        let plan = notice_cursor::plan_replay(&mut tx, INDEXER_RENTALS, ChainNet::Ethereum, 20, 30)
            .await
            .unwrap()
            .expect("within the replay window");
        sqlx::query(
            "DELETE FROM shop.rental_event WHERE network = 'ethereum' AND block_number > 20",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        plan.apply(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        let cursors: Vec<(String, Option<i64>, i64, i32)> = sqlx::query_as(
            "SELECT producer, end_time, block_number, log_index FROM shop.notification_cursor ORDER BY producer",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            cursors,
            vec![
                (KIND_ENDED.to_string(), Some(t0 + 3 * DAY), 0, -1),
                (KIND_STARTED.to_string(), None, 20, i32::MAX),
            ]
        );
        rent(21, 0, t0 + 3 * DAY, "8", 1, false).await;
        rent(22, 0, t0 + 3 * DAY + 5, "8", 2, true).await;
        rent(31, 0, t0 + 4 * DAY, "9", 1, false).await;
        assert_eq!(run_started(&pool, base).await.unwrap(), Pass::Emitted(0));
        assert_eq!(run_ended(&pool, base).await.unwrap(), Pass::Emitted(0));
        db.drop().await;
    }
}
