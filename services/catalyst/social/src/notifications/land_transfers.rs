//! The `land_transfer_received` producer: every Genesis LAND or Estate
//! transfer tells its recipient "Your wallet received Genesis parcel x,y" /
//! "estate #id". The source is the land_authz chain index (its `transfer`
//! events of the LAND and Estate registries), where the old service read the
//! squid's `transfer` table; the producer waits until that index is
//! published (`index_cursor.folded_block`, a completed replay) and never
//! reads past the published block, so a replay or a repair rewind is never
//! announced. The first pass starts at the newest published transfer: the
//! history is never replayed as notifications. Progress is
//! `social.land_transfer_cursor` (one row, the last `(block_number,
//! log_index)` handled), advanced with the notifications of each page in
//! one transaction; the notification id is a uuid v5 of the event position,
//! so a retried page inserts nothing twice.

use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

pub const KIND: &str = "land_transfer_received";
const PAGE_SIZE: i64 = 200;
const POLL_SECS: u64 = 60;

pub fn notification_id(block_number: i64, log_index: i32) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("genesis-land-transfer:{block_number}:{log_index}").as_bytes(),
    )
}

#[derive(sqlx::FromRow)]
struct Registries {
    land_registry: String,
    estate_registry: String,
    folded_block: i64,
}

#[derive(sqlx::FromRow)]
struct TransferRow {
    block_number: i64,
    log_index: i32,
    block_time: i64,
    token_address: String,
    token_id: String,
    account: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
}

fn is_recipient(address: &str, registries: &Registries) -> bool {
    address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|b| b.is_ascii_hexdigit())
        && address != crate::shared::identity::ZERO_ADDRESS
        && address != registries.land_registry
        && address != registries.estate_registry
}

fn metadata(row: &TransferRow, parcel: bool) -> serde_json::Value {
    let x = row.x.map(|v| v.to_string());
    let y = row.y.map(|v| v.to_string());
    let place = if parcel {
        match (&x, &y) {
            (Some(x), Some(y)) => format!("parcel {x},{y}"),
            _ => format!("parcel #{}", row.token_id),
        }
    } else {
        format!("estate #{}", row.token_id)
    };
    json!({
        "title": "LAND received",
        "description": format!("Your wallet received Genesis {place}."),
        "link": "/creator-hub/land",
        "category": if parcel { "parcel" } else { "estate" },
        "tokenId": row.token_id,
        "x": x,
        "y": y,
    })
}

/// One pass: every published transfer after the cursor, a page at a time.
/// Returns the notifications inserted.
pub async fn run_once(pool: &PgPool) -> anyhow::Result<u64> {
    let registries: Option<Registries> = sqlx::query_as(
        "SELECT land_registry, estate_registry, folded_block FROM land_authz.index_cursor \
         WHERE folded_block IS NOT NULL ORDER BY network LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let Some(registries) = registries else {
        return Ok(0);
    };
    sqlx::query(
        "INSERT INTO social.land_transfer_cursor (id, block_number, log_index) \
         SELECT 1, coalesce(max(block_number), 0), coalesce((array_agg(log_index ORDER BY block_number DESC, log_index DESC))[1], -1) \
         FROM (SELECT block_number, log_index FROM land_authz.authz_event \
               WHERE kind = 'transfer' AND block_number <= $1 \
               ORDER BY block_number DESC, log_index DESC LIMIT 1) latest \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(registries.folded_block)
    .execute(pool)
    .await?;
    let mut emitted = 0;
    loop {
        let mut txn = pool.begin().await?;
        let (since_block, since_log): (i64, i32) =
            sqlx::query_as("SELECT block_number, log_index FROM social.land_transfer_cursor WHERE id = 1 FOR UPDATE").fetch_one(&mut *txn).await?;
        let rows: Vec<TransferRow> = sqlx::query_as(
            "SELECT e.block_number, e.log_index, e.block_time, e.token_address, e.token_id::text AS token_id, lower(e.account) AS account, \
                    CASE WHEN e.token_address = $4 THEN land_authz.token_x(e.token_id) END AS x, \
                    CASE WHEN e.token_address = $4 THEN land_authz.token_y(e.token_id) END AS y \
             FROM land_authz.authz_event e \
             WHERE (e.block_number, e.log_index) > ($1, $2) AND e.block_number <= $3 \
               AND e.kind = 'transfer' AND e.token_address IN ($4, $5) \
             ORDER BY e.block_number, e.log_index LIMIT $6",
        )
        .bind(since_block)
        .bind(since_log)
        .bind(registries.folded_block)
        .bind(&registries.land_registry)
        .bind(&registries.estate_registry)
        .bind(PAGE_SIZE)
        .fetch_all(&mut *txn)
        .await?;
        let Some(last) = rows.last() else {
            txn.rollback().await?;
            break;
        };
        let (last_block, last_log) = (last.block_number, last.log_index);
        for row in &rows {
            let Some(recipient) = row
                .account
                .as_deref()
                .filter(|a| is_recipient(a, &registries))
            else {
                continue;
            };
            let parcel = row.token_address == registries.land_registry;
            let occurred_at = chrono::DateTime::from_timestamp(row.block_time, 0)
                .unwrap_or_default()
                .naive_utc();
            let inserted = sqlx::query(
                "INSERT INTO social.notifications (id, address, type, metadata, occurred_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, timezone('UTC', now())) ON CONFLICT (id) DO NOTHING",
            )
            .bind(notification_id(row.block_number, row.log_index))
            .bind(recipient)
            .bind(KIND)
            .bind(metadata(row, parcel))
            .bind(occurred_at)
            .execute(&mut *txn)
            .await?;
            emitted += inserted.rows_affected();
        }
        sqlx::query("UPDATE social.land_transfer_cursor SET block_number = $1, log_index = $2, updated_at = timezone('UTC', now()) WHERE id = 1")
            .bind(last_block)
            .bind(last_log)
            .execute(&mut *txn)
            .await?;
        txn.commit().await?;
        if rows.len() < PAGE_SIZE as usize {
            break;
        }
    }
    Ok(emitted)
}

pub fn spawn(pool: PgPool, jobs: &crate::jobs::Jobs) {
    jobs.spawn_periodic_leased(
        "land-transfer-notifications",
        crate::jobs::PeriodicCfg::every(Duration::from_secs(POLL_SECS))
            .with_jitter(Duration::from_secs(10))
            .with_error_backoff(Duration::from_secs(600)),
        move |ctx| {
            let pool = pool.clone();
            async move {
                let emitted = run_once(&pool).await?;
                if emitted > 0 {
                    tracing::info!(emitted, "LAND transfer notifications emitted");
                    ctx.progress(format!("{emitted} emitted"));
                }
                Ok(())
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_id_is_stable_and_unique_per_event() {
        assert_eq!(notification_id(10, 1), notification_id(10, 1));
        assert_ne!(notification_id(10, 1), notification_id(10, 2));
        assert_ne!(notification_id(10, 1), notification_id(11, 1));
    }

    /// On a database of its own (the land_authz cursor row is global):
    /// nothing before the index is published; the first pass starts at the
    /// newest published transfer; later transfers notify their recipient, a
    /// parcel with its coordinates and an estate by id; nothing past the
    /// published block; transfers into the registries and burns notify
    /// nobody; a retried pass inserts nothing twice.
    #[tokio::test]
    async fn published_transfers_notify_their_recipients_once() {
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
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
        let land = "0xf87e31492faf9a91b02ee0deaad50d51d56d5d4d";
        let estate = "0x959e104e1a4db6317fa58f8295f586e1a978c297";
        let wallet = "0x00000000000000000000000000000000000000b0".to_string();
        let parcel_token = "340282366920938463463374607431768211457";
        let base: i64 = 1_000;
        let event = |block: i64,
                     log: i32,
                     token_address: &'static str,
                     token_id: &'static str,
                     to: String| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO land_authz.authz_event (block_number, log_index, block_time, token_address, kind, token_id, account) \
                     VALUES ($1, $2, 1700000000, $3, 'transfer', $4::numeric, $5)",
                )
                .bind(block)
                .bind(log)
                .bind(token_address)
                .bind(token_id)
                .bind(to)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        event(
            base,
            0,
            land,
            parcel_token,
            "0x00000000000000000000000000000000000000aa".into(),
        )
        .await;
        assert_eq!(
            run_once(&pool).await.unwrap(),
            0,
            "an unpublished index is not read"
        );
        sqlx::query("INSERT INTO land_authz.index_cursor (network, land_registry, estate_registry, last_block, folded_block) VALUES ('ethereum-sepolia', $1, $2, $3, $3)")
            .bind(land)
            .bind(estate)
            .bind(base + 5)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            run_once(&pool).await.unwrap(),
            0,
            "the first pass starts at the newest published transfer"
        );
        event(base + 1, 0, land, parcel_token, wallet.clone()).await;
        event(base + 1, 1, estate, "42", wallet.clone()).await;
        event(base + 2, 0, land, parcel_token, estate.to_string()).await;
        event(
            base + 3,
            0,
            estate,
            "42",
            crate::shared::identity::ZERO_ADDRESS.to_string(),
        )
        .await;
        event(base + 9, 0, land, parcel_token, wallet.clone()).await;
        assert_eq!(run_once(&pool).await.unwrap(), 2);
        assert_eq!(run_once(&pool).await.unwrap(), 0, "nothing twice");
        let rows: Vec<(serde_json::Value,)> =
            sqlx::query_as("SELECT metadata FROM social.notifications WHERE address = $1 AND type = $2 ORDER BY occurred_at, metadata->>'category' DESC")
                .bind(&wallet)
                .bind(KIND)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].0["description"],
            "Your wallet received Genesis parcel 1,1."
        );
        assert_eq!(
            (rows[0].0["x"].as_str(), rows[0].0["y"].as_str()),
            (Some("1"), Some("1"))
        );
        assert_eq!(
            rows[1].0["description"],
            "Your wallet received Genesis estate #42."
        );
        let cursor: (i64, i32) =
            sqlx::query_as("SELECT block_number, log_index FROM social.land_transfer_cursor")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            cursor,
            (base + 3, 0),
            "the cursor stops at the published block"
        );
        pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
    }
}
