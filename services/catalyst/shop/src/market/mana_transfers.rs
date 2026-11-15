use super::{
    SharedState,
    chain_walk::{self, ChainIndex, ChainNet, INDEXER_MANA, LogGroup},
};
use crate::chain_index::rpc::{RpcLog, hex_to_i64};
use alloy_primitives::{Address, U256};
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use std::{collections::HashMap, str::FromStr};

pub const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

pub fn contract(net: ChainNet) -> String {
    format!(
        "{:#x}",
        crate::mana::venues::Book::production(|_| None)
            .chain_of_net(net)
            .expect("production network")
            .mana
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Transfer {
    block: i64,
    index: i32,
    transaction: String,
    sender: String,
    recipient: String,
    amount: String,
    timestamp: i64,
}

fn decode(log: &RpcLog, emitter: &str, timestamp: i64) -> anyhow::Result<Transfer> {
    ensure!(
        log.address.eq_ignore_ascii_case(emitter),
        "wrong MANA emitter"
    );
    ensure!(
        log.topics.len() == 3 && log.topics[0].eq_ignore_ascii_case(TRANSFER),
        "invalid MANA Transfer topics"
    );
    let address = |s: &str| -> anyhow::Result<String> {
        let bytes = hex::decode(s.strip_prefix("0x").context("topic prefix")?)?;
        ensure!(
            bytes.len() == 32 && bytes[..12].iter().all(|b| *b == 0),
            "invalid address topic"
        );
        Ok(format!("0x{}", hex::encode(&bytes[12..])))
    };
    let bytes = hex::decode(log.data.strip_prefix("0x").context("data prefix")?)?;
    ensure!(bytes.len() == 32, "invalid MANA amount width");
    let transaction = log
        .transaction_hash
        .as_deref()
        .context("missing transaction hash")?;
    ensure!(
        transaction.len() == 66
            && transaction.starts_with("0x")
            && transaction[2..].bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid transaction hash"
    );
    let block = hex_to_i64(&log.block_number)
        .filter(|n| *n >= 0)
        .context("invalid block")?;
    let index = hex_to_i64(&log.log_index)
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| *n >= 0)
        .context("invalid log index")?;
    ensure!(timestamp >= 0, "invalid block timestamp");
    Ok(Transfer {
        block,
        index,
        transaction: transaction.to_ascii_lowercase(),
        sender: address(&log.topics[1])?,
        recipient: address(&log.topics[2])?,
        amount: U256::from_be_slice(&bytes).to_string(),
        timestamp,
    })
}

pub struct ManaTransfersIndex {
    pub net: ChainNet,
}

#[async_trait::async_trait]
impl ChainIndex for ManaTransfersIndex {
    fn indexer(&self) -> &'static str {
        INDEXER_MANA
    }
    fn net(&self) -> ChainNet {
        self.net
    }
    fn start_block(&self) -> u64 {
        0
    }
    async fn groups(
        &self,
        _: &PgPool,
        _: u64,
        _: u64,
        _: (u64, i64),
    ) -> anyhow::Result<Vec<LogGroup>> {
        Ok(vec![LogGroup {
            addresses: vec![contract(self.net)],
            topics: json!([TRANSFER]),
        }])
    }
    async fn write(
        &self,
        conn: &mut PgConnection,
        logs: &[RpcLog],
        times: &HashMap<i64, i64>,
    ) -> anyhow::Result<usize> {
        let emitter = contract(self.net);
        let rows = logs
            .iter()
            .map(|log| {
                let block = hex_to_i64(&log.block_number).context("invalid block")?;
                decode(
                    log,
                    &emitter,
                    *times.get(&block).context("missing block timestamp")?,
                )
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut written = 0;
        for chunk in rows.chunks(1000) {
            written += sqlx::query("INSERT INTO shop.mana_transfer (network, block_number, log_index, transaction_hash, sender, recipient, amount, timestamp) SELECT $1,b,i,x,s,r,a::numeric,t FROM UNNEST($2::bigint[],$3::int[],$4::text[],$5::text[],$6::text[],$7::text[],$8::bigint[]) AS u(b,i,x,s,r,a,t) ON CONFLICT (network,block_number,log_index) DO NOTHING")
                .bind(self.net.db_name())
                .bind(chunk.iter().map(|r| r.block).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.index).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.transaction.as_str()).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.sender.as_str()).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.recipient.as_str()).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.amount.as_str()).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.timestamp).collect::<Vec<_>>())
                .execute(&mut *conn).await?.rows_affected() as usize;
        }
        Ok(written)
    }
    async fn rewind(&self, conn: &mut PgConnection, to: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM shop.mana_transfer WHERE network = $1 AND block_number > $2")
            .bind(self.net.db_name())
            .bind(to)
            .execute(conn)
            .await?;
        Ok(())
    }
    async fn fold(&self, _: &PgPool, _: Option<i64>, _: i64) -> anyhow::Result<bool> {
        Ok(true)
    }
}

pub async fn status(pool: &PgPool) -> Value {
    let mut rows = Vec::new();
    for net in ChainNet::ALL {
        let c = chain_walk::cursor(pool, INDEXER_MANA, net)
            .await
            .ok()
            .flatten();
        rows.push(json!({ "network": net.db_name(), "ready": c.as_ref().is_some_and(|c| chain_walk::is_ready(c, chrono::Utc::now())),
            "lastBlock": c.as_ref().map(|c| c.last_block), "lastBlockTime": c.as_ref().and_then(|c| c.last_block_time) }));
    }
    json!(rows)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    network: String,
    address: String,
    before_block: Option<i64>,
    before_log: Option<i32>,
    limit: Option<i64>,
}

pub async fn history(State(state): State<SharedState>, Query(q): Query<HistoryQuery>) -> Response {
    let Some(net) = ChainNet::parse(&q.network) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(address) = Address::from_str(&q.address) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if q.before_block.is_some() != q.before_log.is_some()
        || q.before_block.is_some_and(|n| n < 0)
        || q.before_log.is_some_and(|n| n < 0)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(mut tx) = state.pool.begin().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let cursor: Option<i64> = match sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT last_block FROM shop.chain_cursor c WHERE indexer = $1 AND network = $2 AND {}",
        chain_walk::ready_sql("c", "$3")
    )))
    .bind(INDEXER_MANA)
    .bind(net.db_name())
    .bind(chrono::Utc::now().naive_utc())
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(c) => c,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some(cursor) = cursor else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"MANA transfer history is synchronizing"})),
        )
            .into_response();
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = sqlx::query("SELECT block_number, log_index, transaction_hash, sender, recipient, amount::text AS amount, timestamp FROM ((SELECT * FROM shop.mana_transfer WHERE network=$1 AND sender=$2 AND (block_number,log_index)<($3,$4) AND block_number <= $6 ORDER BY block_number DESC,log_index DESC LIMIT $5) UNION (SELECT * FROM shop.mana_transfer WHERE network=$1 AND recipient=$2 AND (block_number,log_index)<($3,$4) AND block_number <= $6 ORDER BY block_number DESC,log_index DESC LIMIT $5)) t ORDER BY block_number DESC,log_index DESC LIMIT $5")
        .bind(net.db_name()).bind(format!("{address:#x}"))
        .bind(q.before_block.unwrap_or(i64::MAX)).bind(q.before_log.unwrap_or(i32::MAX))
        .bind(limit + 1).bind(cursor).fetch_all(&mut *tx).await;
    match rows {
        Ok(rows) => {
            let more = rows.len() > limit as usize;
            let data: Vec<Value> = rows.iter().take(limit as usize).map(|r| json!({
                "blockNumber":r.get::<i64,_>("block_number"), "logIndex":r.get::<i32,_>("log_index"),
                "transactionHash":r.get::<String,_>("transaction_hash"), "from":r.get::<String,_>("sender"),
                "to":r.get::<String,_>("recipient"), "amount":r.get::<String,_>("amount"), "timestamp":r.get::<i64,_>("timestamp")
            })).collect();
            let next = if more {
                data.last()
                    .map(|r| json!({"beforeBlock":r["blockNumber"], "beforeLog":r["logIndex"]}))
            } else {
                None
            };
            Json(json!({"network":net.db_name(), "decimals":18, "indexedThrough":cursor, "data":data, "next":next})).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
