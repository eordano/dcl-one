//! The marketplace chain indexes on a scratch database per test (created on
//! the `CATALYST_TEST_DATABASE_URL` cluster with the real schema, dropped at
//! the end; skipped without the variable): the walk against a fake chain
//! (window and address limits, the request budget, a kill between passes,
//! the confirmation depth and the publish rule, a reorg rewind, a late
//! collection's backfill), the folds (trade verdicts, nft owners, legacy
//! orders, names, LAND data), and the routes that read them before and
//! after readiness (the envelope, then the answer), plus the neighbours
//! build and the coupon reads, writes and state refresh.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_primitives::{Address, FixedBytes, U256, keccak256};
use alloy_sol_types::SolValue;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;

use super::chain_walk::{
    self, ChainIndex, ChainNet, INDEXER_NFT, INDEXER_SALES, INDEXER_TRADE_EVENTS, IndexSettings,
    PassOutcome,
};
use super::listings::{self, RpcEndpoints};
use super::nft_index::{self, NftIndex};
use super::sales_index::{self, SalesIndex};
use super::trade_events::{self, TradeEventsIndex};
use super::{MarketSettings, MarketplaceState, Settings, SharedState};
use crate::chain_index::rpc::Rpc;

struct ScratchDb {
    name: String,
    pool: PgPool,
    admin: PgPool,
}

impl ScratchDb {
    async fn create() -> Option<Self> {
        let Ok(base) = std::env::var("CATALYST_TEST_DATABASE_URL") else {
            eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
            return None;
        };
        let opts: PgConnectOptions = base
            .parse()
            .expect("CATALYST_TEST_DATABASE_URL must be a postgres DSN");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.clone().database("postgres"))
            .await
            .unwrap();
        let name = format!(
            "catalyst_z_mkt_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&base).unwrap();
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
        Some(Self { name, pool, admin })
    }

    async fn drop(self) {
        self.pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
    }
}

/// A JSON-RPC chain with a fixed log set, the two public backends' limits
/// (a window wider than `max_span` refused with "exceeds limit of N", more
/// than `max_addresses` refused without a range) and a block hash per block
/// that a test can move (a reorg).
struct FakeChain {
    head: Mutex<u64>,
    finalized_lag: std::sync::atomic::AtomicU64,
    finalized_available: std::sync::atomic::AtomicBool,
    reorg_on_logs: std::sync::atomic::AtomicBool,
    logs: Mutex<Vec<Value>>,
    max_span: u64,
    max_addresses: usize,
    forks: Mutex<HashMap<u64, String>>,
    requests: AtomicUsize,
    get_logs: AtomicUsize,
    /// Answer eth_getLogs without `blockTimestamp` (the Polygon backends that do).
    strip_timestamps: std::sync::atomic::AtomicBool,
    batches: AtomicUsize,
    drop_in_batch: Mutex<Vec<u64>>,
}

impl FakeChain {
    fn new(
        head: u64,
        logs: Vec<crate::chain_index::rpc::RpcLog>,
        max_span: u64,
        max_addresses: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            head: Mutex::new(head),
            finalized_lag: std::sync::atomic::AtomicU64::new(64),
            finalized_available: std::sync::atomic::AtomicBool::new(true),
            reorg_on_logs: std::sync::atomic::AtomicBool::new(false),
            logs: Mutex::new(logs.iter().map(log_json).collect()),
            max_span,
            max_addresses,
            forks: Mutex::new(HashMap::new()),
            requests: AtomicUsize::new(0),
            get_logs: AtomicUsize::new(0),
            strip_timestamps: std::sync::atomic::AtomicBool::new(false),
            batches: AtomicUsize::new(0),
            drop_in_batch: Mutex::new(Vec::new()),
        })
    }

    fn hash_of(&self, block: u64) -> String {
        self.forks
            .lock()
            .get(&block)
            .cloned()
            .unwrap_or_else(|| format!("0x{:064x}", block))
    }
}

fn log_json(l: &crate::chain_index::rpc::RpcLog) -> Value {
    json!({
        "address": l.address, "topics": l.topics, "data": l.data, "blockNumber": l.block_number, "logIndex": l.log_index,
        "blockTimestamp": l.block_timestamp, "transactionHash": l.transaction_hash.clone().unwrap_or_else(|| format!("0x{:064x}", 7)),
    })
}

fn hex_u64(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

/// The fake chain's block time: two seconds a block from 1,600,000,000.
fn time_of(block: u64) -> i64 {
    1_600_000_000 + 2 * block as i64
}

async fn rpc(
    State(chain): State<Arc<FakeChain>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    chain.requests.fetch_add(1, Ordering::SeqCst);
    match body {
        Value::Array(calls) => {
            chain.batches.fetch_add(1, Ordering::SeqCst);
            // A batch: every element answered, except the one a flaky
            // backend drops (the client asks it again alone).
            let answers: Vec<Value> = calls
                .iter()
                .filter(|c| {
                    !chain
                        .drop_in_batch
                        .lock()
                        .contains(&hex_u64(&c["params"][0]))
                })
                .map(|c| answer(&chain, c))
                .collect();
            axum::Json(Value::Array(answers)).into_response()
        }
        one => axum::Json(answer(&chain, &one)).into_response(),
    }
}

fn answer(chain: &FakeChain, body: &Value) -> Value {
    let id = body["id"].clone();
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let refuse = |message: String| json!({ "jsonrpc": "2.0", "id": body["id"], "error": { "code": -32602, "message": message } });
    match body["method"].as_str() {
        Some("eth_blockNumber") => reply(json!(format!("0x{:x}", *chain.head.lock()))),
        Some("eth_chainId") => reply(json!("0x89")),
        // `royaltiesCutPerMillion()` of any venue: the pinned 2.5 %.
        Some("eth_call") if body["params"][0]["data"] == sales_index::ROYALTIES_CUT_SELECTOR => {
            reply(json!(format!("0x{:064x}", 25_000)))
        }
        Some("eth_getBlockByNumber") => {
            let n = if body["params"][0] == "finalized" {
                if !chain.finalized_available.load(Ordering::SeqCst) {
                    return reply(Value::Null);
                }
                chain
                    .head
                    .lock()
                    .saturating_sub(chain.finalized_lag.load(Ordering::SeqCst))
            } else {
                hex_u64(&body["params"][0])
            };
            reply(
                json!({ "number": format!("0x{n:x}"), "hash": chain.hash_of(n), "timestamp": format!("0x{:x}", time_of(n)) }),
            )
        }
        Some("eth_getLogs") => {
            chain.get_logs.fetch_add(1, Ordering::SeqCst);
            let f = &body["params"][0];
            let (from, to) = (hex_u64(&f["fromBlock"]), hex_u64(&f["toBlock"]));
            if chain.reorg_on_logs.swap(false, Ordering::SeqCst) {
                chain
                    .forks
                    .lock()
                    .insert(to, format!("0x{}", "ee".repeat(32)));
            }
            if to - from + 1 > chain.max_span {
                return refuse(format!(
                    "range {} exceeds limit of {}",
                    to - from + 1,
                    chain.max_span
                ));
            }
            let addresses: Vec<String> = match &f["address"] {
                Value::String(a) => vec![a.to_ascii_lowercase()],
                Value::Array(a) => a
                    .iter()
                    .map(|v| v.as_str().unwrap().to_ascii_lowercase())
                    .collect(),
                _ => vec![],
            };
            if addresses.len() > chain.max_addresses {
                return refuse("exceed max addresses or topics per search position".into());
            }
            let topics: Vec<String> = f["topics"][0]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|t| t.as_str().unwrap().to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            let logs = chain.logs.lock();
            let hits: Vec<Value> = logs
                .iter()
                .filter(|l| (from..=to).contains(&hex_u64(&l["blockNumber"])))
                .filter(|l| {
                    addresses.contains(&l["address"].as_str().unwrap().to_ascii_lowercase())
                })
                .filter(|l| {
                    topics.is_empty()
                        || topics.contains(&l["topics"][0].as_str().unwrap().to_ascii_lowercase())
                })
                .cloned()
                .collect();
            if chain.strip_timestamps.load(Ordering::SeqCst) {
                let stripped: Vec<Value> = hits
                    .into_iter()
                    .map(|mut l| {
                        l.as_object_mut().unwrap().remove("blockTimestamp");
                        l
                    })
                    .collect();
                return reply(Value::Array(stripped));
            }
            reply(Value::Array(hits))
        }
        other => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unexpected method {other:?}") } })
        }
    }
}

async fn serve(chain: Arc<FakeChain>) -> String {
    let app = Router::new().route("/", post(rpc)).with_state(chain);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/")
}

fn index_settings(budget: usize, window: u64) -> IndexSettings {
    IndexSettings {
        nft_enabled: true,
        trade_events_enabled: true,
        requests_per_pass: budget,
        window_blocks: window,
        ..IndexSettings::default()
    }
}

fn state_over(pool: PgPool, url: &str, indexes: IndexSettings, writes: bool) -> SharedState {
    let market = MarketSettings {
        trade_rpc_urls: RpcEndpoints::from_env(Some(&format!("137={url},1={url}"))),
        ..MarketSettings::default()
    }
    .with_indexes(indexes);
    let settings = Settings {
        data_writes_enabled: writes,
        matic_collections_subgraph_url: Some("https://subgraph.example/matic".into()),
        ..Settings::for_tests()
    }
    .with_market(market);
    Arc::new(MarketplaceState::new(
        pool,
        crate::http::Outbound::for_tests(),
        settings,
    ))
}

/// `st` with its trades mirrored from an upstream (no pass recorded yet).
fn trades_from_upstream(mut st: SharedState) -> SharedState {
    Arc::get_mut(&mut st)
        .expect("an unshared state")
        .settings
        .trades_sync_upstream_url = Some("https://marketplace-api.example/v1/trades".into());
    st
}

/// The trades mirror's last pass completed `ago` before now.
async fn trades_pass_ended(st: &MarketplaceState, ago: std::time::Duration) {
    sqlx::query(
        "INSERT INTO shop.trades_mirror (id, upstream_url, pass, completed_at) \
         VALUES (1, $1, 1, timezone('UTC', now()) - make_interval(secs => $2)) \
         ON CONFLICT (id) DO UPDATE SET upstream_url = EXCLUDED.upstream_url, completed_at = EXCLUDED.completed_at",
    )
    .bind(st.settings.trades_sync_upstream_url.as_deref())
    .bind(ago.as_secs_f64())
    .execute(&st.pool)
    .await
    .unwrap();
    st.trades_ready.clear();
}

async fn pass(state: &MarketplaceState, url: &str, ix: &dyn ChainIndex) -> PassOutcome {
    let rpc = Rpc::at(&state.http, url);
    chain_walk::run_pass(
        &state.pool,
        &rpc,
        ix,
        &state.settings.market.indexes,
        None,
        &|_| {},
    )
    .await
    .unwrap()
}

async fn get_json(app: &Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn topic_addr(a: &str) -> String {
    format!("0x{:0>64}", a.trim_start_matches("0x"))
}

fn topic_word(v: U256) -> String {
    format!("0x{v:064x}")
}

fn log_at(
    address: &str,
    topics: Vec<String>,
    data: Vec<u8>,
    block: u64,
    index: u64,
) -> crate::chain_index::rpc::RpcLog {
    crate::chain_index::rpc::RpcLog {
        address: address.to_string(),
        topics,
        data: format!("0x{}", hex::encode(data)),
        block_number: format!("0x{block:x}"),
        log_index: format!("0x{index:x}"),
        block_timestamp: Some(format!("0x{:x}", time_of(block))),
        block_hash: None,
        transaction_hash: Some(format!("0x{:064x}", block * 100 + index)),
    }
}

const ALICE: &str = "0xaaaa000000000000000000000000000000000001";
const BOB: &str = "0xbbbb000000000000000000000000000000000002";
const CARL: &str = "0xcccc000000000000000000000000000000000003";
const DAVE: &str = "0xdddd000000000000000000000000000000000004";
const ZERO: &str = "0x0000000000000000000000000000000000000000";

/// A stored trade with one sent and one received asset; returns (id, hashed signature).
#[allow(
    clippy::too_many_arguments,
    reason = "fixture: one argument per seeded trade field"
)]
async fn seed_trade(
    pool: &PgPool,
    kind: &str,
    signer: &str,
    contract: &str,
    uses: u64,
    signer_index: u64,
    sent: (i32, &str, &str),
    received: (i32, &str, &str),
) -> (String, String) {
    let signature = format!(
        "0x{}1b",
        hex::encode(uuid::Uuid::new_v4().as_bytes()).repeat(4)
    );
    let hashed = listings::hashed_signature(&signature);
    let id: String = sqlx::query_scalar(
        "INSERT INTO shop.trades (network, chain_id, signature, hashed_signature, checks, signer, type, expires_at, effective_since, contract) \
         VALUES ('MATIC', 137, $1, $2, $3, $4, $5::shop.trade_type, timezone('UTC', now()) + interval '30 days', timezone('UTC', now()) - interval '1 day', $6) \
         RETURNING id::text",
    )
    .bind(&signature)
    .bind(&hashed)
    .bind(json!({ "uses": uses, "expiration": 4_000_000_000_000i64, "effective": 0, "salt": "0x01", "contractSignatureIndex": 0, "signerSignatureIndex": signer_index, "allowedRoot": "0x", "externalChecks": [] }))
    .bind(signer)
    .bind(kind)
    .bind(contract)
    .fetch_one(pool)
    .await
    .unwrap();
    for (direction, (asset_type, addr, value)) in [("sent", sent), ("received", received)] {
        let asset: String = sqlx::query_scalar(
            "INSERT INTO shop.trade_assets (trade_id, direction, asset_type, contract_address, beneficiary, extra) \
             VALUES ($1::uuid, $2::shop.asset_direction_type, $3, $4, $5, '0x') RETURNING id::text",
        )
        .bind(&id)
        .bind(direction)
        .bind(asset_type as i16)
        .bind(addr)
        .bind(if direction == "received" { Some(signer) } else { None })
        .fetch_one(pool)
        .await
        .unwrap();
        let (table, column) = match asset_type {
            1 | 2 => ("trade_assets_erc20", "amount"),
            3 => ("trade_assets_erc721", "token_id"),
            _ => ("trade_assets_item", "item_id"),
        };
        let cast = if column == "amount" { "::numeric" } else { "" };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO shop.{table} (asset_id, {column}) VALUES ($1::uuid, $2{cast})"
        )))
        .bind(&asset)
        .bind(value)
        .execute(pool)
        .await
        .unwrap();
    }
    (id, hashed)
}

fn traded(
    block: u64,
    index: u64,
    market: &str,
    caller: &str,
    signer: &str,
    hashed: &str,
) -> crate::chain_index::rpc::RpcLog {
    let mut log =
        trade_events::tests::traded_log(block, index, market, caller, signer, "0x01", 1, false);
    log.topics[2] = hashed.to_string();
    log.block_timestamp = Some(format!("0x{:x}", time_of(block)));
    log
}

fn cancelled(
    block: u64,
    market: &str,
    caller: &str,
    hashed: &str,
) -> crate::chain_index::rpc::RpcLog {
    log_at(
        market,
        vec![
            trade_events::TOPIC_SIGNATURE_CANCELLED.into(),
            topic_addr(caller),
            hashed.to_string(),
        ],
        vec![],
        block,
        0,
    )
}

fn signer_index(
    block: u64,
    market: &str,
    signer: &str,
    value: u64,
) -> crate::chain_index::rpc::RpcLog {
    log_at(
        market,
        vec![
            trade_events::TOPIC_SIGNER_INDEX_INCREASED.into(),
            topic_addr(signer),
            topic_word(U256::from(value)),
        ],
        vec![],
        block,
        0,
    )
}

async fn status_of(pool: &PgPool, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT status FROM shop.trade_chain_state WHERE trade_id = $1::uuid")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_trade_events_walk_refuses_an_endpoint_of_another_chain() {
    let url = serve(FakeChain::new(1_000, vec![], 10_000, 1_000)).await;
    let http = crate::http::Outbound::for_tests();
    let rpc = Rpc::at(&http, &url);
    super::ensure_chain(&rpc, 137).await.unwrap();
    let refused = super::ensure_chain(&rpc, 80_002)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("serves chain 137, not 80002"), "{refused}");
}

#[tokio::test]
async fn trade_events_walk_under_a_budget_resume_after_a_kill_fold_the_verdicts_and_answer_accept()
{
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let market = trade_events::marketplaces(ChainNet::Matic)[1].0;
    let start = TradeEventsIndex::mainnet(ChainNet::Matic).start_block();
    let item = "0x00000000000000000000000000000000000000c1";
    let mana = "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4";
    let sold = seed_trade(
        &db.pool,
        "public_item_order",
        ALICE,
        market,
        1,
        0,
        (4, item, "0"),
        (1, mana, "1000"),
    )
    .await;
    let by_signer = seed_trade(
        &db.pool,
        "public_item_order",
        ALICE,
        market,
        1,
        0,
        (4, item, "1"),
        (1, mana, "1000"),
    )
    .await;
    let by_stranger = seed_trade(
        &db.pool,
        "public_item_order",
        ALICE,
        market,
        1,
        0,
        (4, item, "2"),
        (1, mana, "1000"),
    )
    .await;
    let index_moved = seed_trade(
        &db.pool,
        "public_item_order",
        BOB,
        market,
        1,
        0,
        (4, item, "3"),
        (1, mana, "1000"),
    )
    .await;
    let open = seed_trade(
        &db.pool,
        "public_item_order",
        ALICE,
        market,
        2,
        0,
        (4, item, "4"),
        (1, mana, "1000"),
    )
    .await;
    let bid = seed_trade(
        &db.pool,
        "bid",
        CARL,
        market,
        1,
        0,
        (1, mana, "5"),
        (4, item, "0"),
    )
    .await;
    sqlx::query(
        "INSERT INTO shop.items (network, contract_address, item_id, chain_id, item_type, urn, creator, rarity, total_supply, max_supply, available, price, image, name, category, created_at, updated_at, reviewed_at, mirrored_at) \
         VALUES ('matic', $1, 0, 137, 'wearable_v2', 'urn:decentraland:matic:collections-v2:0xc1:0', $2, 'epic', 1, 10, 9, 1000, 'https://img/0', 'Hat', 'hat', 1, 1, 1, timezone('UTC', now()))",
    )
    .bind(item)
    .bind(ALICE)
    .execute(&db.pool)
    .await
    .unwrap();
    // Each item order is its creator's primary sale, so every verdict is read.
    for (item_id, creator) in [(1, ALICE), (2, ALICE), (3, BOB), (4, ALICE)] {
        sqlx::query(
            "INSERT INTO shop.items (network, contract_address, item_id, chain_id, item_type, urn, creator, rarity, total_supply, max_supply, available, price, image, name, category, created_at, updated_at, reviewed_at, mirrored_at) \
             VALUES ('matic', $1, $2::int4, 137, 'wearable_v2', 'urn:decentraland:matic:collections-v2:0xc1:' || $2::int4::text, $3, 'epic', 1, 10, 9, 1000, 'https://img/0', 'Hat', 'hat', 1, 1, 1, timezone('UTC', now()))",
        )
        .bind(item)
        .bind(item_id)
        .bind(creator)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    let s = start;
    let logs = vec![
        traded(s + 100, 0, market, CARL, ALICE, &sold.1),
        traded(s + 150, 1, market, BOB, ALICE, &open.1),
        cancelled(s + 2_000, market, ALICE, &by_signer.1),
        cancelled(s + 3_000, market, CARL, &by_stranger.1),
        signer_index(s + 4_000, market, BOB, 1),
        traded(s + 5_000, 0, market, ALICE, CARL, &bid.1),
    ];
    let head = s + 6_000 + 64;
    let chain = FakeChain::new(head, logs, 1_000, 10);
    let url = serve(chain.clone()).await;

    // Pass 1, budget 3: the refused 10,000-block window narrows to 1,000, a
    // few windows land, the cursor is durable, nothing is published and the
    // accept route answers the envelope.
    let st = state_over(db.pool.clone(), &url, index_settings(4, 10_000), false);
    let app = super::market_router(st.clone());
    let first = pass(&st, &url, &TradeEventsIndex::mainnet(ChainNet::Matic)).await;
    assert!(first.paused && !first.published, "{first:?}");
    assert!(first.indexed_to >= s as i64 && first.indexed_to < head as i64);
    let (code, body) = get_json(&app, &format!("/v1/trades/{}/accept", sold.1)).await;
    assert_eq!(
        (code, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("trade-events-index")),
        "{body}"
    );
    assert_eq!(
        get_json(&app, &format!("/v1/trades/0x{}/accept", "ab".repeat(32)))
            .await
            .0,
        StatusCode::NOT_FOUND,
        "an unknown trade is the 404 even before readiness"
    );

    // A kill: a new state and index resume from the durable cursor.
    let st = state_over(db.pool.clone(), &url, index_settings(4, 10_000), false);
    let mut last = PassOutcome::default();
    for _ in 0..20 {
        last = pass(&st, &url, &TradeEventsIndex::mainnet(ChainNet::Matic)).await;
        if last.published {
            break;
        }
    }
    assert!(last.published, "{last:?}");
    let (rows, distinct): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT (block_number, log_index)) FROM shop.trade_event",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((rows, distinct), (6, 6), "every log once across the kill");
    assert_eq!(
        last.indexed_to,
        (head - 64) as i64,
        "the walk stays the confirmation depth behind the head"
    );

    assert_eq!(status_of(&db.pool, &sold.0).await.as_deref(), Some("sold"));
    assert_eq!(
        status_of(&db.pool, &by_signer.0).await.as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        status_of(&db.pool, &by_stranger.0).await.as_deref(),
        Some("open"),
        "only the signer's cancellation cancels"
    );
    assert_eq!(
        status_of(&db.pool, &index_moved.0).await.as_deref(),
        Some("cancelled"),
        "the signer's index moved past the signed one"
    );
    assert_eq!(
        status_of(&db.pool, &open.0).await.as_deref(),
        Some("open"),
        "one of two uses"
    );
    assert_eq!(status_of(&db.pool, &bid.0).await.as_deref(), Some("sold"));

    // The routes over the published index.
    let app = super::market_router(st.clone());
    let (code, body) = get_json(
        &app,
        &format!("/v1/trades/{}/accept?timestamp=1234&caller={BOB}", sold.1),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        (
            body["ok"].as_bool(),
            body["data"]["subType"].as_str(),
            body["data"]["timestamp"].as_i64()
        ),
        (Some(true), Some("item-sold"), Some(1234))
    );
    assert_eq!(body["data"]["metadata"]["nftName"], "Hat");
    assert_eq!(body["data"]["metadata"]["buyer"], BOB);
    let (code, body) = get_json(&app, &format!("/v1/trades/{}/accept", by_stranger.1)).await;
    assert_eq!(
        (code, body["ok"].as_bool()),
        (StatusCode::NOT_FOUND, Some(false)),
        "a trade the chain never executed: {body}"
    );
    let open_ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "WITH {} SELECT id::text FROM unified_trades",
        super::catalog::unified_trades_cte("$1", "$2")
    )))
    .bind(chrono::Utc::now().naive_utc())
    .bind(None::<chrono::NaiveDateTime>)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert!(
        open_ids.contains(&open.0) && open_ids.contains(&by_stranger.0),
        "{open_ids:?}"
    );
    assert!(
        !open_ids.contains(&sold.0)
            && !open_ids.contains(&by_signer.0)
            && !open_ids.contains(&index_moved.0),
        "{open_ids:?}"
    );
    let bids: Vec<(String, String)> =
        sqlx::query_as(sqlx::AssertSqlSafe(super::bids::test_trades_bids_sql()))
            .bind(chrono::Utc::now().naive_utc())
            .bind(None::<chrono::NaiveDateTime>)
            .fetch_all(&db.pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert!(
        bids.contains(&(bid.0.clone(), "sold".to_string())),
        "{bids:?}"
    );

    // A stale publish is not trusted: the verdicts fall back to open.
    sqlx::query("UPDATE shop.chain_cursor SET folded_at = folded_at - interval '31 minutes' WHERE indexer = 'trade_events'").execute(&db.pool).await.unwrap();
    let open_ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "WITH {} SELECT id::text FROM unified_trades",
        super::catalog::unified_trades_cte("$1", "$2")
    )))
    .bind(chrono::Utc::now().naive_utc())
    .bind(None::<chrono::NaiveDateTime>)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert!(
        open_ids.contains(&sold.0) && open_ids.contains(&index_moved.0),
        "an unpublished verdict is ignored: {open_ids:?}"
    );
    db.drop().await;
}

#[tokio::test]
async fn a_v3_cancellation_names_the_digest_the_fold_recomputes_from_the_stored_trade() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let v3 = listings::offchain_marketplace_v3(137).unwrap();
    let mut trade = listings::test_support::signed_trade_on(137, &v3);
    listings::test_support::sign(&mut trade, &v3);
    let signer = listings::test_support::signer()
        .address()
        .to_string()
        .to_ascii_lowercase();
    let digest = format!("{:#x}", listings::signing_hash(&trade, &v3).unwrap());
    let id = match listings::insert_trade(&db.pool, &trade, "MATIC", v3.address).await {
        Ok(id) => id,
        Err(e) => panic!("the fixture trade is stored: {e}"),
    };
    let start = TradeEventsIndex::mainnet(ChainNet::Matic).start_block();
    let head = 93_600_000u64.max(start + 200);
    let chain = FakeChain::new(
        head,
        vec![cancelled(93_590_000, v3.address, &signer, &digest)],
        1_000_000_000,
        10,
    );
    let url = serve(chain).await;
    let st = state_over(
        db.pool.clone(),
        &url,
        IndexSettings {
            window_blocks: 20_000_000,
            requests_per_pass: 50,
            ..index_settings(50, 20_000_000)
        },
        false,
    );
    let mut last = PassOutcome::default();
    for _ in 0..10 {
        last = pass(&st, &url, &TradeEventsIndex::mainnet(ChainNet::Matic)).await;
        if last.published {
            break;
        }
    }
    assert!(last.published, "{last:?}");
    let (stored_digest, status): (Option<String>, String) = sqlx::query_as(
        "SELECT digest, status FROM shop.trade_chain_state WHERE trade_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(stored_digest.as_deref(), Some(digest.as_str()));
    assert_eq!(status, "cancelled");
    db.drop().await;
}

fn transfer(
    contract: &str,
    from: &str,
    to: &str,
    token: U256,
    block: u64,
    index: u64,
) -> crate::chain_index::rpc::RpcLog {
    log_at(
        contract,
        vec![
            nft_index::TOPIC_TRANSFER.into(),
            topic_addr(from),
            topic_addr(to),
            topic_word(token),
        ],
        vec![],
        block,
        index,
    )
}

fn v2_token(item: u64, issued: u64) -> U256 {
    (U256::from(item) << 216) | U256::from(issued)
}

async fn seed_collection(
    pool: &PgPool,
    network: &str,
    contract: &str,
    created_block: u64,
    urn: &str,
    name: &str,
) {
    sqlx::query(
        "INSERT INTO shop.collections (network, contract_address, chain_id, name, creator, owner, urn, is_completed, is_approved, is_editable, created_at, updated_at, reviewed_at, mirrored_at) \
         VALUES ($1, $2, $3, $4, $5, $5, $6, true, true, false, $7, $7, $7, timezone('UTC', now()))",
    )
    .bind(network)
    .bind(contract)
    .bind(if network == "matic" { 137 } else { 1 })
    .bind(name)
    .bind(CARL)
    .bind(urn)
    .bind(time_of(created_block))
    .execute(pool)
    .await
    .unwrap();
}

#[allow(
    clippy::too_many_arguments,
    reason = "fixture: one argument per seeded shop.items column"
)]
async fn seed_item(
    pool: &PgPool,
    network: &str,
    contract: &str,
    item: u64,
    urn: &str,
    name: &str,
    item_type: &str,
    category: &str,
) {
    sqlx::query(
        "INSERT INTO shop.items (network, contract_address, item_id, chain_id, item_type, urn, creator, rarity, total_supply, max_supply, available, price, image, name, category, body_shapes, search_is_collection_approved, created_at, updated_at, reviewed_at, mirrored_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'rare', 1, 100, 99, 10, 'https://img/' || $6, $8, $9, ARRAY['BaseMale'], true, 1, 1, 1, timezone('UTC', now()))",
    )
    .bind(network)
    .bind(contract)
    .bind(item as i64)
    .bind(if network == "matic" { 137 } else { 1 })
    .bind(item_type)
    .bind(urn)
    .bind(CARL)
    .bind(name)
    .bind(category)
    .execute(pool)
    .await
    .unwrap();
}

async fn owner_of(
    pool: &PgPool,
    network: &str,
    contract: &str,
    token: U256,
) -> Option<Option<String>> {
    sqlx::query_scalar("SELECT owner FROM shop.nft WHERE network = $1 AND contract_address = $2 AND token_id = $3::numeric")
        .bind(network)
        .bind(contract)
        .bind(token.to_string())
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn publish_nft(st: &MarketplaceState, url: &str, ix: &NftIndex) -> PassOutcome {
    publish(st, url, ix).await
}

async fn publish(st: &MarketplaceState, url: &str, ix: &dyn ChainIndex) -> PassOutcome {
    let mut last = PassOutcome::default();
    for _ in 0..40 {
        last = pass(st, url, ix).await;
        if last.published {
            break;
        }
    }
    last
}

#[tokio::test]
async fn the_nft_walk_folds_owners_orders_and_resumes_a_kill_backfills_a_late_collection_and_serves_the_deep_shapes()
 {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let s = nft_index::start_block(ChainNet::Matic);
    let c1 = "0x00000000000000000000000000000000000000c1";
    let c2 = "0x00000000000000000000000000000000000000c2";
    let late = "0x00000000000000000000000000000000000000c3";
    let market = nft_index::legacy_marketplaces(ChainNet::Matic)[0].0;
    seed_collection(
        &db.pool,
        "matic",
        c1,
        s + 100,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Pirates",
    )
    .await;
    seed_collection(
        &db.pool,
        "matic",
        c2,
        s + 20_000,
        "urn:decentraland:matic:collections-v2:0xc2",
        "Dances",
    )
    .await;
    seed_item(
        &db.pool,
        "matic",
        c1,
        0,
        "urn:decentraland:matic:collections-v2:0xc1:0",
        "Pirate Hat",
        "wearable_v2",
        "hat",
    )
    .await;
    seed_item(
        &db.pool,
        "matic",
        c1,
        1,
        "urn:decentraland:matic:collections-v2:0xc1:1",
        "Parrot",
        "wearable_v2",
        "earring",
    )
    .await;
    seed_item(
        &db.pool,
        "matic",
        c2,
        0,
        "urn:decentraland:matic:collections-v2:0xc2:0",
        "Moonwalk",
        "emote_v1",
        "dance",
    )
    .await;
    let t1 = v2_token(0, 1);
    let t2 = v2_token(1, 1);
    let t3 = v2_token(0, 1);
    let order_id = FixedBytes::<32>::repeat_byte(0x42);
    let order_data = (
        order_id,
        Address::from_str(c1).unwrap(),
        U256::from(5_000u64),
        U256::from(4_000_000_000_000u64),
    )
        .abi_encode_params();
    let logs = vec![
        transfer(c1, ZERO, ALICE, t1, s + 200, 0),
        transfer(c1, ALICE, BOB, t1, s + 300, 0),
        transfer(c1, ZERO, ALICE, t2, s + 310, 0),
        log_at(
            market,
            vec![
                nft_index::TOPIC_ORDER_CREATED.into(),
                topic_word(t1),
                topic_addr(BOB),
            ],
            order_data.clone(),
            s + 400,
            1,
        ),
        transfer(c2, ZERO, CARL, t3, s + 20_100, 0),
        transfer(late, ZERO, ALICE, v2_token(0, 7), s + 1_500, 0),
    ];
    let head = s + 30_000;
    let chain = FakeChain::new(head, logs, 10_000, 2);
    let url = serve(chain.clone()).await;
    let st = trades_from_upstream(state_over(
        db.pool.clone(),
        &url,
        index_settings(3, 10_000),
        false,
    ));
    trades_pass_ended(&st, std::time::Duration::ZERO).await;
    let app = super::market_router(st.clone());
    let (code, body) = get_json(
        &app,
        "/marketplace-api/v1/nfts?search=pirate&network=MATIC"
            .trim_start_matches("/marketplace-api"),
    )
    .await;
    assert_eq!(
        (code, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("nft-index")),
        "before readiness the envelope: {body}"
    );

    // Pass 1 under a budget of 3; then a kill (new index state) and the rest.
    let first = pass(&st, &url, &NftIndex::new(ChainNet::Matic)).await;
    assert!(first.paused && !first.published, "{first:?}");
    let last = publish_nft(&st, &url, &NftIndex::new(ChainNet::Matic)).await;
    assert!(last.published, "{last:?}");
    let (rows, distinct): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT (network, block_number, log_index)) FROM shop.nft_event",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (rows, distinct),
        (5, 5),
        "c3 is not in the mirror: its log is not walked"
    );
    assert_eq!(
        owner_of(&db.pool, "matic", c1, t1).await,
        Some(Some(BOB.to_string()))
    );
    let (item, issued, minted): (Option<String>, Option<String>, i64) =
        sqlx::query_as("SELECT item_id::text, issued_id::text, minted_at FROM shop.nft WHERE contract_address = $1 AND token_id = $2::numeric").bind(c1).bind(t2.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(
        (item.as_deref(), issued.as_deref(), minted),
        (Some("1"), Some("1"), time_of(s + 310))
    );
    let (status, seller, price): (String, String, String) = sqlx::query_as("SELECT status, seller, price::text FROM shop.nft_order WHERE contract_address = $1 AND token_id = $2::numeric").bind(c1).bind(t1.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(
        (status.as_str(), seller.as_str(), price.as_str()),
        ("open", BOB, "5000")
    );
    let walked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shop.nft_contract WHERE network = 'matic'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(walked, 2);
    assert!(
        chain.get_logs.load(Ordering::SeqCst) > 3,
        "the 2-address limit split the list"
    );

    // The index-served shapes.
    let (code, body) = get_json(&app, "/v1/nfts?search=hat&network=MATIC").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["data"][0]["nft"]["name"], "Pirate Hat");
    assert_eq!(body["data"][0]["nft"]["owner"], BOB);
    assert_eq!(body["data"][0]["nft"]["itemId"], "0");
    assert_eq!(
        body["data"][0]["order"]["price"], "5000",
        "the legacy order of the owner: {body}"
    );
    assert_eq!(
        body["data"][0]["nft"]["activeOrderId"],
        format!("0x{}", "42".repeat(32))
    );
    let (code, body) = get_json(&app, "/v1/nfts?search=pirates&network=MATIC").await;
    assert_eq!(
        (code, body["total"].as_i64()),
        (StatusCode::OK, Some(2)),
        "the collection name matches both of its items: {body}"
    );
    let (code, body) = get_json(&app, "/v1/nfts?search=pirates&network=MATIC&isOnSale=true").await;
    assert_eq!(
        (code, body["total"].as_i64()),
        (StatusCode::OK, Some(1)),
        "only the listed one is on sale: {body}"
    );
    let (code, body) = get_json(&app, "/v1/nfts?first=10&skip=6000&network=MATIC").await;
    assert_eq!(
        (
            code,
            body["total"].as_i64(),
            body["data"].as_array().map(Vec::len)
        ),
        (StatusCode::OK, Some(3), Some(0)),
        "a page past the reader window reads the index: {body}"
    );
    let (code, body) = get_json(
        &app,
        "/v1/prices?assetType=nft&category=wearable&network=MATIC",
    )
    .await;
    assert_eq!(
        (code, body["data"].clone()),
        (StatusCode::OK, json!({ "5000": 1 })),
        "{body}"
    );

    // A late collection: the mirror learns c3 after the walk passed its
    // creation; the backfill lane reads it before the network republishes.
    seed_collection(
        &db.pool,
        "matic",
        late,
        s + 1_000,
        "urn:decentraland:matic:collections-v2:0xc3",
        "Late",
    )
    .await;
    *chain.head.lock() = head + 100;
    let last = publish_nft(&st, &url, &NftIndex::new(ChainNet::Matic)).await;
    assert!(last.published, "{last:?}");
    let (from, backfill, done, owed): (i64, Option<i64>, Option<i64>, bool) =
        sqlx::query_as("SELECT walked_from_block, backfill_from_block, backfilled_to_block, refold_owed FROM shop.nft_contract WHERE contract_address = $1").bind(late).fetch_one(&db.pool).await.unwrap();
    assert!(
        backfill.is_some_and(|b| b <= (s + 1_000) as i64) && done == Some(from - 1) && !owed,
        "{from} {backfill:?} {done:?} {owed}"
    );
    assert_eq!(
        owner_of(&db.pool, "matic", late, v2_token(0, 7)).await,
        Some(Some(ALICE.to_string())),
        "the backfilled token is folded"
    );
    db.drop().await;
}

#[tokio::test]
async fn a_reorg_past_the_confirmation_depth_rewinds_and_the_refold_follows_the_new_chain() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let s = nft_index::start_block(ChainNet::Matic);
    let c1 = "0x00000000000000000000000000000000000000c1";
    seed_collection(
        &db.pool,
        "matic",
        c1,
        s + 10,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Pirates",
    )
    .await;
    let t1 = v2_token(0, 1);
    let head = s + 2_000;
    let chain = FakeChain::new(
        head,
        vec![
            transfer(c1, ZERO, ALICE, t1, s + 100, 0),
            transfer(c1, ALICE, BOB, t1, s + 1_900, 0),
        ],
        1_000_000,
        100,
    );
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(50, 100_000), false);
    assert!(
        publish_nft(&st, &url, &NftIndex::new(ChainNet::Matic))
            .await
            .published
    );
    assert_eq!(
        owner_of(&db.pool, "matic", c1, t1).await,
        Some(Some(BOB.to_string()))
    );
    let cursor = chain_walk::cursor(&db.pool, INDEXER_NFT, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    // The chain reorganizes: the block the cursor stands on has another
    // hash, the transfer to BOB is gone and one to CARL replaces it.
    chain
        .forks
        .lock()
        .insert(cursor.last_block as u64, format!("0x{}", "ee".repeat(32)));
    {
        let mut logs = chain.logs.lock();
        logs.retain(|l| l["blockNumber"] != json!(format!("0x{:x}", s + 1_900)));
        logs.push(log_json(&transfer(c1, ALICE, CARL, t1, s + 1_880, 0)));
    }
    let rewound = pass(&st, &url, &NftIndex::new(ChainNet::Matic)).await;
    assert_eq!(
        rewound.reorg_rewound_to,
        Some(cursor.start_block - 1),
        "{rewound:?}"
    );
    assert!(
        chain_walk::ready_cursor(&db.pool, INDEXER_NFT, ChainNet::Matic, chrono::Utc::now())
            .await
            .is_none(),
        "a rewind unpublishes"
    );
    chain.forks.lock().clear();
    assert!(
        publish_nft(&st, &url, &NftIndex::new(ChainNet::Matic))
            .await
            .published
    );
    assert_eq!(
        owner_of(&db.pool, "matic", c1, t1).await,
        Some(Some(CARL.to_string()))
    );
    let orphans: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shop.nft_event WHERE block_number = $1")
            .bind((s + 1_900) as i64)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(orphans, 0);
    db.drop().await;
}

/// After a reorg rewind and the refold it owes, `land_data` equals a
/// truncate-and-rebuild of the remaining log: a token whose only Update was
/// reorged out loses its row, a token with an older Update falls back to it.
#[tokio::test]
async fn a_rewind_leaves_land_data_as_a_rebuild_of_the_remaining_log() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let land = crate::land_authz::LAND_REGISTRY_MAINNET;
    let update = |block: i64, token: i64, name: &str| {
        format!(
            "('ethereum', {block}, 0, {block}, 'land_data', '{land}', '{land}', {token}, NULL, NULL, NULL, NULL, '0,\"{name}\",\"\"')"
        )
    };
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO shop.nft_event (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, from_address, to_address, amount, expires_at, detail) VALUES {}, {}, {}",
        update(100, 1, "A-old"),
        update(200, 1, "A-new"),
        update(201, 2, "B-only-late"),
    )))
    .execute(&db.pool)
    .await
    .unwrap();
    nft_index::fold(&db.pool, ChainNet::Ethereum, None)
        .await
        .unwrap();
    let rows = |pool: sqlx::PgPool| async move {
        sqlx::query_as::<_, (String, Option<String>, i64)>(
            "SELECT token_id::text, name, updated_block FROM shop.land_data ORDER BY token_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    assert_eq!(
        rows(db.pool.clone()).await,
        vec![
            ("1".into(), Some("A-new".into()), 200),
            ("2".into(), Some("B-only-late".into()), 201)
        ]
    );

    let mut conn = db.pool.acquire().await.unwrap();
    NftIndex::new(ChainNet::Ethereum)
        .rewind(&mut conn, 150)
        .await
        .unwrap();
    drop(conn);
    nft_index::fold(&db.pool, ChainNet::Ethereum, None)
        .await
        .unwrap();
    let after_rewind = rows(db.pool.clone()).await;
    assert_eq!(
        after_rewind,
        vec![("1".into(), Some("A-old".into()), 100)],
        "token 2 had no Update left at or before the cursor"
    );

    sqlx::query("TRUNCATE shop.land_data")
        .execute(&db.pool)
        .await
        .unwrap();
    nft_index::fold(&db.pool, ChainNet::Ethereum, None)
        .await
        .unwrap();
    assert_eq!(
        rows(db.pool.clone()).await,
        after_rewind,
        "the rewind's result is the truncate-and-rebuild's"
    );
    db.drop().await;
}

#[tokio::test]
async fn a_reindexed_order_only_gains_its_missing_transaction_hash() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let order = |to: &str, tx_hash: Option<&str>| nft_index::NftEvent {
        block_number: 100,
        log_index: 2,
        block_time: 1,
        kind: "order_successful",
        emitter: "0xmarket".into(),
        contract_address: "0xnft".into(),
        token_id: "7".into(),
        from_address: None,
        to_address: Some(to.into()),
        amount: Some("5".into()),
        expires_at: None,
        detail: None,
        tx_hash: tx_hash.map(str::to_string),
    };
    let (a, b) = (
        format!("0x{}", "a".repeat(64)),
        format!("0x{}", "b".repeat(64)),
    );
    let mut conn = db.pool.acquire().await.unwrap();
    let mut write = async |row| {
        nft_index::insert_events(&mut conn, ChainNet::Matic, &[row])
            .await
            .unwrap()
    };
    assert_eq!(write(order("0xbuyer", None)).await, 1);
    assert_eq!(write(order("0xbuyer", None)).await, 0, "nothing to fill");
    assert_eq!(
        write(order("0xother", Some(&a))).await,
        1,
        "the hash is filled"
    );
    assert_eq!(write(order("0xbuyer", Some(&a))).await, 0, "already filled");
    assert_eq!(
        write(order("0xbuyer", Some(&b))).await,
        0,
        "a kept hash stays"
    );
    drop(conn);
    let kept: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT to_address, tx_hash FROM shop.nft_event WHERE network = 'matic' AND block_number = 100",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(kept, (Some("0xbuyer".into()), Some(a)));
    db.drop().await;
}

#[tokio::test]
async fn ethereum_names_v1_wearables_land_data_and_the_land_routes() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    // Past the registrar's deployment (9,412,979), the walk's first
    // fixed contracts, so every log below is in the address set.
    let s = 9_420_000u64;
    let v1 = "0x00000000000000000000000000000000000000e1";
    let market = nft_index::legacy_marketplaces(ChainNet::Ethereum)[0].0;
    let (land, estate) = (
        crate::land_authz::LAND_REGISTRY_MAINNET,
        crate::land_authz::ESTATE_REGISTRY_MAINNET,
    );
    seed_collection(
        &db.pool,
        "ethereum",
        v1,
        s + 10,
        "urn:decentraland:ethereum:collections-v1:halloween_2019",
        "Halloween",
    )
    .await;
    seed_item(
        &db.pool,
        "ethereum",
        v1,
        6,
        "urn:decentraland:ethereum:collections-v1:halloween_2019:jester_feet",
        "Jester Feet",
        "wearable_v1",
        "feet",
    )
    .await;
    let label = keccak256("pixelgolf");
    let name_token = U256::from_be_bytes(label.0);
    let parcel = U256::from_str("12590447576074723148144860474975423823927").unwrap();
    let order = (
        FixedBytes::<32>::repeat_byte(0x07),
        Address::from_str(land).unwrap(),
        U256::from(9_000u64),
        U256::from(4_000_000_000u64),
    )
        .abi_encode_params();
    let reg = nft_index::DCL_REGISTRAR;
    // PixelGolf on sale for 5,000 wei, under the parcel's 9,000: the NAME browse's price range.
    let name_order = (
        FixedBytes::<32>::repeat_byte(0x08),
        Address::from_str(reg).unwrap(),
        U256::from(5_000u64),
        U256::from(4_000_000_000u64),
    )
        .abi_encode_params();
    let logs = vec![
        transfer(v1, ZERO, ALICE, U256::from(0u8), s + 100, 0),
        log_at(
            v1,
            vec![
                nft_index::TOPIC_ISSUE_V1.into(),
                topic_addr(ALICE),
                topic_word(U256::ZERO),
                format!("{:#x}", keccak256("jester_feet")),
            ],
            ("jester_feet".to_string(), U256::from(1u8)).abi_encode_params(),
            s + 100,
            1,
        ),
        transfer(reg, ZERO, BOB, name_token, s + 200, 0),
        log_at(
            reg,
            vec![
                nft_index::TOPIC_NAME_REGISTERED.into(),
                topic_addr(BOB),
                topic_addr(BOB),
                format!("{label:#x}"),
            ],
            ("PixelGolf".to_string(), U256::from(1u8)).abi_encode_params(),
            s + 200,
            1,
        ),
        log_at(
            land,
            vec![
                nft_index::TOPIC_LAND_UPDATE.into(),
                topic_word(parcel),
                topic_addr(CARL),
                topic_addr(CARL),
            ],
            ("0,\"Sea View\",\"by the road\"".to_string(),).abi_encode_params(),
            s + 300,
            0,
        ),
        log_at(
            estate,
            vec![
                nft_index::TOPIC_LAND_UPDATE.into(),
                topic_word(U256::from(500u32)),
                topic_addr(CARL),
                topic_addr(CARL),
            ],
            ("0,\"Fashion Street\",\"\"".to_string(),).abi_encode_params(),
            s + 301,
            0,
        ),
        log_at(
            market,
            vec![
                nft_index::TOPIC_ORDER_CREATED.into(),
                topic_word(parcel),
                topic_addr(CARL),
            ],
            order,
            s + 400,
            0,
        ),
        log_at(
            market,
            vec![
                nft_index::TOPIC_ORDER_CREATED.into(),
                topic_word(name_token),
                topic_addr(BOB),
            ],
            name_order,
            s + 401,
            0,
        ),
    ];
    let head = s + 1_000;
    let chain = FakeChain::new(head, logs, 1_000_000, 100);
    let url = serve(chain).await;
    let st = trades_from_upstream(state_over(
        db.pool.clone(),
        &url,
        index_settings(60, 500_000),
        false,
    ));
    trades_pass_ended(&st, std::time::Duration::ZERO).await;
    assert!(
        publish_nft(&st, &url, &NftIndex::new(ChainNet::Ethereum))
            .await
            .published
    );
    // The LAND facts are land_authz's: a published cursor and its rows.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO land_authz.index_cursor (network, land_registry, estate_registry, last_block, folded_block, folded_at, full_replay_started) \
         VALUES ('ethereum-mainnet', '{land}', '{estate}', 10, 10, timezone('UTC', now()), true); \
         INSERT INTO land_authz.token_right (token_address, token_id, x, y, owner, updated_block, updated_log) VALUES \
            ('{land}', {parcel}, 37, 55, '{CARL}', 1, 0), ('{estate}', 500, NULL, NULL, '{ALICE}', 1, 1), ('{land}', 1, 0, 1, '{ESTATE_OWNER}', 1, 2), ('{land}', 2, 0, 2, '{ESTATE_OWNER}', 1, 3); \
         INSERT INTO land_authz.estate_land VALUES (1, 500, 1, 0), (2, 500, 1, 1); \
         INSERT INTO land_authz.authz_event (block_number, log_index, block_time, token_address, kind, token_id, account) VALUES \
            (1, 0, 1600000000, '{land}', 'transfer', {parcel}, '{CARL}'), (1, 1, 1600000100, '{estate}', 'transfer', 500, '{ALICE}')",
        ESTATE_OWNER = estate,
    )))
    .execute(&db.pool)
    .await
    .unwrap();
    let app = super::market_router(st.clone());
    let (code, body) = get_json(&app, "/v1/nfts?category=ens&search=pixel").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        (
            body["data"][0]["nft"]["name"].as_str(),
            body["data"][0]["nft"]["data"]["ens"]["subdomain"].as_str(),
            body["data"][0]["nft"]["owner"].as_str()
        ),
        (Some("PixelGolf"), Some("PixelGolf"), Some(BOB))
    );
    // The NAME browse's price range is in wei and bounds the open listing; `cheapest` sorts by it.
    // A search reads this index (without one the subgraph serves, its `where` checked in nfts.rs).
    for (range, listed) in [
        ("minPrice=5000&maxPrice=5000", true),
        ("minPrice=1&maxPrice=9000", true),
        ("minPrice=5001", false),
        ("maxPrice=4999", false),
    ] {
        let (code, body) = get_json(
            &app,
            &format!("/v1/nfts?category=ens&search=pixel&isOnSale=true&sortBy=cheapest&{range}"),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{range}: {body}");
        let names: Vec<_> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["nft"]["name"].as_str())
            .collect();
        assert_eq!(
            names,
            if listed {
                vec![Some("PixelGolf")]
            } else {
                vec![]
            },
            "{range}: {body}"
        );
        if listed {
            assert_eq!(body["data"][0]["order"]["price"], "5000", "{range}: {body}");
        }
    }
    let (code, body) = get_json(&app, "/v1/nfts?category=ens&isOnSale=true&minPrice=abc").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    let (code, body) = get_json(&app, "/v1/nfts?search=jester&network=ETHEREUM").await;
    assert_eq!(
        (
            code,
            body["data"][0]["nft"]["name"].as_str(),
            body["data"][0]["nft"]["issuedId"].as_str()
        ),
        (StatusCode::OK, Some("Jester Feet"), Some("1")),
        "{body}"
    );
    let (code, body) = get_json(&app, "/v1/nfts?category=parcel&search=37,55").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        (
            body["data"][0]["nft"]["name"].as_str(),
            body["data"][0]["nft"]["data"]["parcel"]["x"].as_str()
        ),
        (Some("Sea View"), Some("37"))
    );
    assert_eq!(
        body["data"][0]["order"]["price"], "9000",
        "the LAND's legacy order, its seller the owner"
    );
    assert_eq!(
        body["data"][0]["order"]["expiresAt"], 4_000_000_000_000i64,
        "seconds scaled to ms"
    );
    let (code, body) = get_json(&app, "/v1/nfts?category=estate&search=fashion").await;
    assert_eq!(
        (
            code,
            body["data"][0]["nft"]["data"]["estate"]["size"].as_i64()
        ),
        (StatusCode::OK, Some(2)),
        "{body}"
    );
    assert_eq!(
        body["data"][0]["nft"]["data"]["estate"]["parcels"],
        json!([{ "x": 0, "y": 1 }, { "x": 0, "y": 2 }])
    );
    let (code, body) = get_json(&app, "/v1/prices?category=parcel").await;
    assert_eq!(
        (code, body["data"].clone()),
        (StatusCode::OK, json!({ "9000": 1 })),
        "{body}"
    );
    let (code, body) = get_json(&app, "/v1/stats/estate/size").await;
    assert_eq!(
        (code, body["data"].clone()),
        (StatusCode::OK, json!({ "2": 1 })),
        "{body}"
    );
    // The geography filters read the squid's static LAND map (none of these
    // parcels is in it: distance -1, no road), with the squid's semantics:
    // `<= max` keeps a -1, `>= min` and `adjacentToRoad` drop it.
    let (code, body) = get_json(
        &app,
        "/v1/nfts?category=parcel&search=37,55&adjacentToRoad=true",
    )
    .await;
    assert_eq!(
        (code, body["total"].as_i64()),
        (StatusCode::OK, Some(0)),
        "{body}"
    );
    let (code, body) = get_json(
        &app,
        "/v1/nfts?category=parcel&search=37,55&maxDistanceToPlaza=3",
    )
    .await;
    assert_eq!(
        (code, body["data"][0]["nft"]["name"].as_str()),
        (StatusCode::OK, Some("Sea View")),
        "{body}"
    );
    let (code, body) = get_json(
        &app,
        "/v1/nfts?category=parcel&search=37,55&minDistanceToPlaza=1",
    )
    .await;
    assert_eq!(
        (code, body["total"].as_i64()),
        (StatusCode::OK, Some(0)),
        "{body}"
    );
    let (code, body) = get_json(&app, "/v1/stats/estate/size?adjacentToRoad=true").await;
    assert_eq!(
        (code, body["data"].clone()),
        (StatusCode::OK, json!({})),
        "{body}"
    );
    let (code, body) = get_json(&app, "/v1/prices?category=parcel&maxDistanceToPlaza=3").await;
    assert_eq!(
        (code, body["data"].clone()),
        (StatusCode::OK, json!({ "9000": 1 })),
        "{body}"
    );
    // The trades mirror past its budget: every read over the order legs
    // answers its envelope (the prices memo above included); the rest serve.
    trades_pass_ended(
        &st,
        super::trades::max_age(st.settings.trades_sync_interval)
            + std::time::Duration::from_secs(60),
    )
    .await;
    for uri in [
        "/v1/prices?category=parcel",
        "/v1/prices?category=estate",
        "/v1/prices?category=ens",
        "/v1/nfts?category=parcel&search=37,55&isOnSale=true",
        "/v1/nfts?category=parcel&isOnSale=true&skip=30000",
        "/v1/stats/estate/size?isOnSale=true",
    ] {
        let (code, body) = get_json(&app, uri).await;
        assert_eq!(
            (code, body["source"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("trades-mirror")),
            "{uri}: {body}"
        );
    }
    let (code, body) = get_json(&app, "/v1/nfts?category=parcel&search=37,55").await;
    assert_eq!(
        (code, body["data"][0]["nft"]["name"].as_str()),
        (StatusCode::OK, Some("Sea View")),
        "no order filter, no trades read: {body}"
    );
    let (code, body) = get_json(&app, "/v1/stats/estate/size").await;
    assert_eq!(
        (code, body["data"].clone()),
        (StatusCode::OK, json!({ "2": 1 })),
        "{body}"
    );
    // land_authz unpublished: the LAND shapes answer its envelope.
    sqlx::query("UPDATE land_authz.index_cursor SET folded_at = NULL")
        .execute(&db.pool)
        .await
        .unwrap();
    let (code, body) = get_json(&app, "/v1/stats/estate/size").await;
    assert_eq!(
        (code, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("land-authz-index")),
        "{body}"
    );
    db.drop().await;
}

#[tokio::test]
async fn block_times_missing_from_the_logs_come_from_batched_headers() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let s = nft_index::start_block(ChainNet::Matic);
    let c1 = "0x00000000000000000000000000000000000000c1";
    seed_collection(
        &db.pool,
        "matic",
        c1,
        s + 10,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Pirates",
    )
    .await;
    let logs: Vec<_> = (0..5u64)
        .map(|i| transfer(c1, ZERO, ALICE, v2_token(0, i + 1), s + 100 + i * 7, 0))
        .collect();
    let chain = FakeChain::new(s + 1_000, logs, 1_000_000, 100);
    chain.strip_timestamps.store(true, Ordering::SeqCst);
    chain.drop_in_batch.lock().push(s + 114);
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(50, 100_000), false);
    assert!(
        publish_nft(&st, &url, &NftIndex::new(ChainNet::Matic))
            .await
            .published
    );
    let times: Vec<(i64, i64)> =
        sqlx::query_as("SELECT block_number, block_time FROM shop.nft_event ORDER BY block_number")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(times.len(), 5);
    assert!(
        times.iter().all(|(b, t)| *t == time_of(*b as u64)),
        "{times:?}"
    );
    assert!(
        chain.batches.load(Ordering::SeqCst) >= 1,
        "the five headers came in a batch"
    );
    db.drop().await;
}

#[tokio::test]
async fn a_bounded_window_start_is_never_published() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let s = nft_index::start_block(ChainNet::Matic);
    seed_collection(
        &db.pool,
        "matic",
        "0x00000000000000000000000000000000000000c1",
        s + 10,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Old",
    )
    .await;
    let chain = FakeChain::new(s + 5_000, vec![], 1_000_000, 100);
    let url = serve(chain).await;
    let st = state_over(
        db.pool.clone(),
        &url,
        IndexSettings {
            start_block_matic: Some(s + 4_000),
            ..index_settings(50, 100_000)
        },
        false,
    );
    let out = pass(&st, &url, &NftIndex::new(ChainNet::Matic)).await;
    assert!(out.folded && !out.published, "{out:?}");
    let c = chain_walk::cursor(&db.pool, INDEXER_NFT, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    assert!(!c.full_history && c.start_block == (s + 4_000) as i64);
    assert_eq!(
        c.fold_through,
        Some(c.last_block),
        "the fold is recorded without a publish"
    );
    // Collections older than a bounded window owe no backfill: the window
    // is the walk.
    let owed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM shop.nft_contract WHERE backfill_from_block IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let walked: i64 = sqlx::query_scalar("SELECT count(*) FROM shop.nft_contract")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!((walked, owed), (1, 0));
    db.drop().await;
}

#[tokio::test]
async fn the_neighbours_build_from_holdings_and_personalise_the_rail() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let c = "0x00000000000000000000000000000000000000c1";
    seed_collection(
        &db.pool,
        "matic",
        c,
        1,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Pirates",
    )
    .await;
    for i in 0..3u64 {
        seed_item(
            &db.pool,
            "matic",
            c,
            i,
            &format!("urn:decentraland:matic:collections-v2:0xc1:{i}"),
            &format!("Item {i}"),
            "wearable_v2",
            "hat",
        )
        .await;
    }
    // Four wallets hold items 0 and 1; two hold 0 and 2.
    let mut rows = Vec::new();
    for w in 0..6u64 {
        let wallet = format!("0x{:040x}", 0x1000 + w);
        let other = if w < 4 { 1 } else { 2 };
        for item in [0u64, other] {
            rows.push((wallet.clone(), item, v2_token(item, w + 1)));
        }
    }
    for (wallet, item, token) in &rows {
        sqlx::query(
            "INSERT INTO shop.nft (network, contract_address, token_id, owner, item_id, issued_id, minted_block, minted_at, transferred_block, transferred_log, transferred_at) \
             VALUES ('matic', $1, $2::numeric, $3, $4, 1, 1, 1, 1, 0, 1)",
        )
        .bind(c)
        .bind(token.to_string())
        .bind(wallet)
        .bind(*item as i64)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    let built = super::neighbours::rebuild_from(&db.pool, &["matic".to_string()], Some(1), None)
        .await
        .unwrap();
    assert_eq!((built.wallets, built.items), (6, 3), "{built:?}");
    let n = super::neighbours::neighbours_of(
        &db.pool,
        &[("matic".into(), c.into(), "0".into())],
        &[],
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        n.first().map(|s| s.id.as_str()),
        Some(format!("{c}-1").as_str()),
        "four co-owners beat two: {n:?}"
    );
    // A seed's weight scales what it contributes (an equipped item outranks a holding).
    let weighted = super::neighbours::weighted_neighbours_of(
        &db.pool,
        &[("matic".into(), c.into(), "0".into(), 1.5)],
        &[],
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        weighted
            .iter()
            .map(|s| (&s.id, &s.seed))
            .collect::<Vec<_>>(),
        n.iter().map(|s| (&s.id, &s.seed)).collect::<Vec<_>>()
    );
    for (w, u) in weighted.iter().zip(&n) {
        assert!((w.score - 1.5 * u.score).abs() < 1e-6, "{w:?} vs {u:?}");
    }
    let held = super::neighbours::held_items(&db.pool, &format!("0x{:040x}", 0x1000), 10)
        .await
        .unwrap();
    assert_eq!(held.len(), 2);
    db.drop().await;
}

#[tokio::test]
async fn the_coupon_routes_read_the_store_and_refuse_writes_while_disabled() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let st = state_over(
        db.pool.clone(),
        "http://127.0.0.1:9/",
        IndexSettings::default(),
        false,
    );
    let app = super::market_router(st.clone());
    assert_eq!(
        get_json(&app, "/v1/coupons").await.0,
        StatusCode::BAD_REQUEST,
        "a signer is required"
    );
    let (code, body) = get_json(&app, &format!("/v1/coupons?signer={ALICE}")).await;
    assert_eq!(
        (code, body),
        (StatusCode::OK, json!({ "ok": true, "data": [] }))
    );
    assert_eq!(
        get_json(&app, "/v1/coupons/not-a-uuid").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_json(&app, &format!("/v1/coupons/{}", uuid::Uuid::new_v4()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/coupons")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "DATA_WRITES_ENABLED=false refuses the write"
    );
    db.drop().await;
}

struct FakeManager {
    uses: i64,
    reads: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::coupons::CouponChainReader for FakeManager {
    async fn read_coupon_allowed(
        &self,
        _chain: i64,
        _manager: &str,
        _coupon: &str,
    ) -> Result<bool, crate::coupons::ChainReadError> {
        Ok(true)
    }
    async fn read_indexes(
        &self,
        _chain: i64,
        _manager: &str,
        _signer: &str,
    ) -> Result<crate::coupons::types::CouponChainIndexes, crate::coupons::ChainReadError> {
        Ok(crate::coupons::types::CouponChainIndexes {
            contract_signature_index: 0,
            signer_signature_index: 0,
        })
    }
    async fn read_state(
        &self,
        _chain: i64,
        _manager: &str,
        _keys: &[String],
    ) -> Result<crate::coupons::types::CouponChainState, crate::coupons::ChainReadError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(crate::coupons::types::CouponChainState {
            uses: self.uses,
            cancelled: false,
        })
    }
}

#[tokio::test]
async fn a_coupon_is_stored_once_for_its_collections_creator_and_its_state_refreshes() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let (coupon, signer) = crate::coupons::component_tests::fixture_for_db();
    let collection = coupon.collections[0].clone();
    sqlx::query(
        "INSERT INTO shop.collections (network, contract_address, chain_id, name, creator, owner, urn, is_completed, is_approved, is_editable, created_at, updated_at, reviewed_at, mirrored_at) \
         VALUES ('matic', $1, 137, 'Mine', $2, $2, 'urn:x', true, true, false, 1, 1, 1, timezone('UTC', now()))",
    )
    .bind(&collection)
    .bind(&signer)
    .execute(&db.pool)
    .await
    .unwrap();
    let manager = Arc::new(FakeManager {
        uses: 0,
        reads: AtomicUsize::new(0),
    });
    let component = crate::coupons::CouponsComponent::new(db.pool.clone(), manager.clone());
    let now = crate::coupons::component_tests::NOW_FOR_DB;
    let (coupon2, _) = crate::coupons::component_tests::fixture_for_db();
    let stored = component
        .add_coupon(coupon, &signer, now)
        .await
        .unwrap_or_else(|e| panic!("stored: {e}"));
    assert_eq!(stored.signer, signer);
    assert!(matches!(
        component.add_coupon(coupon2, &signer, now).await,
        Err(crate::coupons::CouponError::Duplicate)
    ));
    let listed = component
        .get_coupons_by_signer(&signer, crate::coupons::CouponPagination::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    // The state reader: a used-up coupon reads exhausted after a refresh.
    let exhausted = Arc::new(FakeManager {
        uses: 10,
        reads: AtomicUsize::new(0),
    });
    sqlx::query("UPDATE shop.coupons SET effective_since = timezone('UTC', now()) - interval '1 hour', expires_at = timezone('UTC', now()) + interval '1 day'").execute(&db.pool).await.unwrap();
    let refreshed = crate::coupons::CouponsComponent::new(db.pool.clone(), exhausted.clone())
        .refresh_state()
        .await
        .unwrap();
    assert_eq!(refreshed, 1);
    let one = component.get_coupon(&stored.id).await.unwrap();
    assert_eq!(
        (one.state.map(|s| s.uses), one.status),
        (Some(10), crate::coupons::CouponStatus::Exhausted)
    );
    db.drop().await;
}

/// One bid row per bidder and token after the walk: `(token, bidder, bid_id, seller, price, status)`.
async fn bids_of(pool: &PgPool) -> Vec<(String, String, String, Option<String>, String, String)> {
    sqlx::query_as(
        "SELECT token_id::text, bidder, bid_id, seller, price::text, status FROM shop.nft_bid \
         WHERE network = 'matic' ORDER BY token_id, bidder",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn bid_id(byte: u8) -> String {
    format!("0x{}", format!("{byte:02x}").repeat(32))
}

#[tokio::test]
async fn the_sales_walk_keeps_store_mints_folds_the_bids_and_checks_the_venues() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let s = nft_index::start_block(ChainNet::Matic);
    let b = sales_index::BID_V2_MATIC_START_BLOCK + 100;
    let c1 = "0x00000000000000000000000000000000000000c1";
    seed_collection(
        &db.pool,
        "matic",
        c1,
        s + 10,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Pirates",
    )
    .await;
    let (t1, t2) = (v2_token(0, 1), v2_token(0, 2));
    let (c9, t9) = ("0x00000000000000000000000000000000000000c9", v2_token(0, 9));
    let chain = FakeChain::new(
        b + 1_000,
        vec![
            // The store sells t1 to ALICE; the creator mints t2 to CARL.
            sales_index::tests::issue_log(c1, ALICE, t1, 0, sales_index::STORE_MATIC, s + 100, 0),
            transfer(c1, ZERO, ALICE, t1, s + 100, 1),
            sales_index::tests::issue_log(c1, CARL, t2, 0, CARL, s + 100, 2),
            transfer(c1, ZERO, CARL, t2, s + 100, 3),
            // BOB, CARL and DAVE bid on t1; ALICE accepts CARL's, which
            // drops DAVE's without an event; BOB cancels.
            sales_index::tests::bid_created_log(c1, t1, BOB, 0x11, 7, 1_700_000_000, b, 0),
            sales_index::tests::bid_created_log(c1, t1, CARL, 0x22, 8, 1_700_000_000, b + 1, 0),
            sales_index::tests::bid_created_log(c1, t1, DAVE, 0x55, 9, 1_700_000_000, b + 1, 1),
            sales_index::tests::bid_accepted_log(c1, t1, CARL, ALICE, 0x22, 8, b + 2, 0),
            transfer(c1, ALICE, CARL, t1, b + 2, 1),
            sales_index::tests::bid_cancelled_log(c1, t1, BOB, 0x11, b + 3, 0),
            // BOB bids twice on t2: the second replaces the first.
            sales_index::tests::bid_created_log(c1, t2, BOB, 0x33, 1, 1_700_000_000, b + 4, 0),
            sales_index::tests::bid_created_log(c1, t2, BOB, 0x44, 2, 1_700_000_100, b + 5, 0),
            // BOB bids on a token of a collection the nft walk never sees.
            sales_index::tests::bid_created_log(c9, t9, BOB, 0x66, 3, 1_700_000_000, b + 6, 0),
        ],
        1_000_000,
        100,
    );
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(100, 1_000_000), false);
    // The sales walk runs ahead of the nft walk: the bids fold without the
    // owners; the next fold after the nft walk fills exactly the sellers
    // it reached, and the one it never reaches is not revisited.
    assert!(publish(&st, &url, &SalesIndex).await.published);
    assert_eq!(
        bids_of(&db.pool)
            .await
            .iter()
            .filter(|row| row.3.is_none())
            .count(),
        4,
        "every bid but the accepted one waits for its seller"
    );
    assert!(
        publish_nft(&st, &url, &NftIndex::new(ChainNet::Matic))
            .await
            .published
    );
    let folded = chain_walk::cursor(&db.pool, INDEXER_SALES, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap()
        .fold_through
        .unwrap();
    assert_eq!(
        sales_index::fold(&db.pool, Some(folded)).await.unwrap(),
        3,
        "the sellers the nft walk reached are filled"
    );
    assert!(publish(&st, &url, &SalesIndex).await.published);
    assert!(sales_index::ready(&db.pool, chrono::Utc::now()).await);
    let status = sales_index::status(&db.pool).await;
    assert_eq!(status["matic"]["ready"], true, "{status}");

    let kinds: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, count(*) FROM shop.sale_event WHERE network = 'matic' GROUP BY kind ORDER BY kind",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        kinds,
        vec![
            ("bid_accepted".to_string(), 1),
            ("bid_cancelled".to_string(), 1),
            ("bid_created".to_string(), 6),
            ("mint".to_string(), 1),
        ],
        "the creator's own Issue is not a sale"
    );
    let (buyer, item, tx): (String, String, String) = sqlx::query_as(
        "SELECT buyer, item_id::text, tx_hash FROM shop.sale_event WHERE network = 'matic' AND kind = 'mint'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (buyer.as_str(), item.as_str(), tx),
        (ALICE, "0", format!("0x{:064x}", (s + 100) * 100))
    );
    assert_eq!(
        bids_of(&db.pool).await,
        vec![
            (
                t1.to_string(),
                BOB.into(),
                bid_id(0x11),
                Some(ALICE.into()),
                "7".into(),
                "cancelled".into()
            ),
            (
                t1.to_string(),
                CARL.into(),
                bid_id(0x22),
                Some(ALICE.into()),
                "8".into(),
                "accepted".into()
            ),
            (
                t1.to_string(),
                DAVE.into(),
                bid_id(0x55),
                Some(ALICE.into()),
                "9".into(),
                "invalidated".into()
            ),
            (
                t2.to_string(),
                BOB.into(),
                bid_id(0x44),
                Some(CARL.into()),
                "2".into(),
                "open".into()
            ),
            (
                t9.to_string(),
                BOB.into(),
                bid_id(0x66),
                None,
                "3".into(),
                "open".into()
            ),
        ],
        "the seller is the owner when the bid was placed; a re-bid replaces the bidder's row; an acceptance invalidates the token's other open bids"
    );
    sqlx::query(
        "UPDATE shop.nft_bid SET status = 'cancelled' WHERE network = 'matic' AND bid_id = $1",
    )
    .bind(bid_id(0x66))
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(sales_index::fold(&db.pool, Some(folded)).await.unwrap(), 0);
    let untouched: String = sqlx::query_scalar(
        "SELECT status FROM shop.nft_bid WHERE network = 'matic' AND bid_id = $1",
    )
    .bind(bid_id(0x66))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        untouched, "cancelled",
        "a seller-less bid on a token the nft walk has no transfer for is not revisited"
    );
    // A database folded before the invalidation rule existed keeps DAVE's
    // bid open: the next incremental fold revisits it without a cursor reset.
    sqlx::query("UPDATE shop.nft_bid SET status = 'open' WHERE network = 'matic' AND bid_id = $1")
        .bind(bid_id(0x55))
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(sales_index::fold(&db.pool, Some(folded)).await.unwrap(), 1);
    let healed: String = sqlx::query_scalar(
        "SELECT status FROM shop.nft_bid WHERE network = 'matic' AND bid_id = $1",
    )
    .bind(bid_id(0x55))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        healed, "invalidated",
        "an open bid with a later acceptance is invalidated whatever fold saw the acceptance"
    );
    let (expires, created, updated): (i64, i64, i64) = sqlx::query_as(
        "SELECT expires_at, created_block, updated_block FROM shop.nft_bid WHERE network = 'matic' AND bid_id = $1",
    )
    .bind(bid_id(0x22))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (expires, created, updated),
        (1_700_000_000, (b + 1) as i64, (b + 2) as i64)
    );

    // The venue check: both charging venues answer the pinned cut; a changed
    // seed row is reported as a difference (and left as it is).
    let rpc = Rpc::at(&st.http, &url);
    let checks = sales_index::check_venues(&rpc, &db.pool).await.unwrap();
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert!(
        checks.iter().all(|c| c.pinned == 25_000 && c.matches()),
        "{checks:?}"
    );
    sqlx::query(
        "UPDATE shop.sale_venue SET royalties_cut_per_million = 30000 WHERE kind = 'bid_v2'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let checks = sales_index::check_venues(&rpc, &db.pool).await.unwrap();
    let bid_v2 = checks.iter().find(|c| c.kind == "bid_v2").unwrap();
    assert!(
        !bid_v2.matches() && bid_v2.live == Some(U256::from(25_000)),
        "{checks:?}"
    );
    let pinned: i32 = sqlx::query_scalar(
        "SELECT royalties_cut_per_million FROM shop.sale_venue WHERE kind = 'bid_v2'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(pinned, 30_000);
    db.drop().await;
}

#[tokio::test]
async fn a_bid_past_the_nft_walk_takes_no_seller_until_the_walk_passes_it() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let c1 = "0x00000000000000000000000000000000000000c1";
    let t1 = v2_token(0, 1).to_string();
    // The nft walk has indexed the mint to ALICE but not her transfer to
    // CARL; DAVE bids after the transfer.
    sqlx::query(
        "INSERT INTO shop.nft_event (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, from_address, to_address) \
         VALUES ('matic', 100, 0, 100, 'transfer', $1, $1, $2::numeric, $3, $4), ('matic', 110, 0, 110, 'transfer', $1, $1, $2::numeric, $4, $5)",
    )
    .bind(c1)
    .bind(&t1)
    .bind(ZERO)
    .bind(ALICE)
    .bind(CARL)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO shop.chain_cursor (indexer, network, start_block, last_block, full_history) VALUES ('nft', 'matic', 1, 105, true)")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO shop.sale_event (network, block_number, log_index, block_time, kind, emitter, contract_address, token_id, bid_id, buyer, price, expires_at, tx_hash) \
         VALUES ('matic', 120, 0, 120, 'bid_created', $1, $2, $3::numeric, $4, $5, 9, 1700000000, $6)",
    )
    .bind(sales_index::BID_V2_MATIC)
    .bind(c1)
    .bind(&t1)
    .bind(bid_id(0x77))
    .bind(DAVE)
    .bind(format!("0x{}", "7".repeat(64)))
    .execute(&db.pool)
    .await
    .unwrap();
    let seller = |pool: PgPool| async move {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT seller FROM shop.nft_bid WHERE network = 'matic' AND bid_id = $1",
        )
        .bind(bid_id(0x77))
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(sales_index::fold(&db.pool, None).await.unwrap(), 1);
    assert_eq!(
        seller(db.pool.clone()).await,
        None,
        "the owner at the bid is past the nft walk: no seller, not the last owner it knows"
    );
    assert_eq!(
        sales_index::fold(&db.pool, Some(120)).await.unwrap(),
        0,
        "not revisited while the walk is short of the bid"
    );
    sqlx::query(
        "UPDATE shop.chain_cursor SET last_block = 125 WHERE indexer = 'nft' AND network = 'matic'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    // The contract is still owed a backfill below its walked range: the
    // transfers before the bid may be missing, so the seller still waits.
    sqlx::query(
        "INSERT INTO shop.nft_contract (network, contract_address, walked_from_block, backfill_from_block) VALUES ('matic', $1, 120, 50)",
    )
    .bind(c1)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(sales_index::fold(&db.pool, Some(120)).await.unwrap(), 0);
    assert_eq!(
        seller(db.pool.clone()).await,
        None,
        "a backfill owed hides the owner"
    );
    sqlx::query(
        "UPDATE shop.nft_contract SET backfilled_to_block = 119 WHERE network = 'matic' AND contract_address = $1",
    )
    .bind(c1)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(sales_index::fold(&db.pool, Some(120)).await.unwrap(), 1);
    assert_eq!(seller(db.pool.clone()).await.as_deref(), Some(CARL));
    db.drop().await;
}

#[tokio::test]
async fn a_sales_reorg_rewinds_and_the_refold_follows_the_new_chain() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let s = nft_index::start_block(ChainNet::Matic);
    let b = sales_index::BID_V2_MATIC_START_BLOCK + 100;
    let c1 = "0x00000000000000000000000000000000000000c1";
    seed_collection(
        &db.pool,
        "matic",
        c1,
        s + 10,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Pirates",
    )
    .await;
    let t1 = v2_token(0, 1);
    let chain = FakeChain::new(
        b + 1_000,
        vec![
            transfer(c1, ZERO, ALICE, t1, s + 100, 0),
            sales_index::tests::bid_created_log(c1, t1, CARL, 0x22, 8, 1_700_000_000, b, 0),
            sales_index::tests::bid_accepted_log(c1, t1, CARL, ALICE, 0x22, 8, b + 900, 0),
        ],
        1_000_000,
        100,
    );
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(100, 1_000_000), false);
    assert!(
        publish_nft(&st, &url, &NftIndex::new(ChainNet::Matic))
            .await
            .published
    );
    assert!(publish(&st, &url, &SalesIndex).await.published);
    assert_eq!(
        bids_of(&db.pool).await,
        vec![(
            t1.to_string(),
            CARL.into(),
            bid_id(0x22),
            Some(ALICE.into()),
            "8".into(),
            "accepted".into()
        )]
    );
    let cursor = chain_walk::cursor(&db.pool, INDEXER_SALES, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    // The chain reorganizes: the acceptance is gone and CARL cancels instead.
    chain
        .forks
        .lock()
        .insert(cursor.last_block as u64, format!("0x{}", "ee".repeat(32)));
    {
        let mut logs = chain.logs.lock();
        logs.retain(|l| l["blockNumber"] != json!(format!("0x{:x}", b + 900)));
        logs.push(log_json(&sales_index::tests::bid_cancelled_log(
            c1,
            t1,
            CARL,
            0x22,
            b + 880,
            0,
        )));
    }
    let rewound = pass(&st, &url, &SalesIndex).await;
    assert_eq!(
        rewound.reorg_rewound_to,
        Some(cursor.start_block - 1),
        "{rewound:?}"
    );
    assert!(
        !sales_index::ready(&db.pool, chrono::Utc::now()).await,
        "a rewind unpublishes"
    );
    chain.forks.lock().clear();
    assert!(publish(&st, &url, &SalesIndex).await.published);
    assert_eq!(
        bids_of(&db.pool).await,
        vec![(
            t1.to_string(),
            CARL.into(),
            bid_id(0x22),
            Some(ALICE.into()),
            "8".into(),
            "cancelled".into()
        )]
    );
    let orphans: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shop.sale_event WHERE block_number = $1")
            .bind((b + 900) as i64)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(orphans, 0);
    db.drop().await;
}

#[test]
fn the_readiness_rule_is_shared_by_every_reader() {
    assert_eq!(chain_walk::READY_WINDOW, crate::land_authz::READY_WINDOW);
    assert_eq!(INDEXER_TRADE_EVENTS, "trade_events");
}

#[tokio::test]
async fn mana_sync_atomic_replay_and_reorg() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    use super::mana_transfers::{self, ManaTransfersIndex};
    let mana = mana_transfers::contract(ChainNet::Matic);
    let event = |block: u64, index: u64, from: &str, to: &str, amount: U256| {
        crate::chain_index::rpc::RpcLog {
            address: mana.clone(),
            topics: vec![
                mana_transfers::TRANSFER.into(),
                format!("0x{:0>64}", &from[2..]),
                format!("0x{:0>64}", &to[2..]),
            ],
            data: format!("0x{amount:064x}"),
            block_number: format!("0x{block:x}"),
            log_index: format!("0x{index:x}"),
            block_timestamp: Some(format!("0x{:x}", time_of(block))),
            block_hash: None,
            transaction_hash: Some(format!("0x{:064x}", block)),
        }
    };
    let chain = FakeChain::new(
        100,
        vec![
            event(90, 0, ALICE, BOB, U256::MAX),
            event(91, 1, ALICE, ALICE, U256::ZERO),
        ],
        10_000,
        1000,
    );
    chain.finalized_lag.store(2, Ordering::SeqCst);
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(100, 10_000), false);
    let app = super::market_router(st.clone());
    let path = format!("/v1/mana/transfers?network=matic&address={ALICE}&limit=1");
    assert_eq!(
        get_json(&app, &path).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let ix = ManaTransfersIndex {
        net: ChainNet::Matic,
    };
    let first = pass(&st, &url, &ix).await;
    assert_eq!(
        first.indexed_to, 98,
        "uses finalized head, not fixed 64 blocks"
    );
    assert!(first.published);
    let c = chain_walk::cursor(&db.pool, chain_walk::INDEXER_MANA, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.last_block_hash, Some(chain.hash_of(98)));
    let (code, body) = get_json(&app, &path).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["amount"], "0");
    let next = format!("{path}&beforeBlock=91&beforeLog=1");
    let (_, page) = get_json(&app, &next).await;
    assert_eq!(page["data"][0]["amount"], U256::MAX.to_string());
    assert_eq!(page["next"], Value::Null);
    let replay = pass(&st, &url, &ix).await;
    assert_eq!(replay.rows_written, 0);
    let mut tx = db.pool.begin().await.unwrap();
    assert!(
        chain_walk::advance(
            &mut tx,
            chain_walk::INDEXER_MANA,
            ChainNet::Matic,
            97,
            &crate::chain_index::rpc::BlockHeader {
                number: 99,
                hash: chain.hash_of(99),
                timestamp: time_of(99)
            }
        )
        .await
        .is_err()
    );
    tx.rollback().await.unwrap();
    chain
        .forks
        .lock()
        .insert(98, format!("0x{}", "ee".repeat(32)));
    chain.logs.lock().clear();
    let rewound = pass(&st, &url, &ix).await;
    assert_eq!(rewound.reorg_rewound_to, Some(-1));
    assert_eq!(
        get_json(&app, &path).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM shop.mana_transfer")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        0
    );
    assert!(pass(&st, &url, &ix).await.published);
    db.drop().await;
}

#[tokio::test]
async fn mana_sync_failed_window_rolls_back_and_legacy_cursor_replays() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    use super::mana_transfers::{self, ManaTransfersIndex};
    let bad = crate::chain_index::rpc::RpcLog {
        address: mana_transfers::contract(ChainNet::Matic),
        topics: vec![mana_transfers::TRANSFER.into()],
        data: "0x00".into(),
        block_number: "0x1".into(),
        log_index: "0x0".into(),
        block_timestamp: Some("0x1".into()),
        block_hash: None,
        transaction_hash: None,
    };
    let chain = FakeChain::new(100, vec![bad], 1000, 1000);
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(100, 1000), false);
    let ix = ManaTransfersIndex {
        net: ChainNet::Matic,
    };
    assert!(
        chain_walk::run_pass(
            &db.pool,
            &Rpc::at(&st.http, &url),
            &ix,
            &st.settings.market.indexes,
            None,
            &|_| {}
        )
        .await
        .is_err()
    );
    let c = chain_walk::cursor(&db.pool, chain_walk::INDEXER_MANA, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.last_block, -1);
    chain.logs.lock().clear();
    assert!(pass(&st, &url, &ix).await.published);
    sqlx::query("UPDATE shop.chain_cursor SET last_block_hash=NULL WHERE indexer='mana_transfers'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(pass(&st, &url, &ix).await.reorg_rewound_to, Some(-1));
    db.drop().await;
}

#[tokio::test]
async fn mana_sync_migration_preserves_existing_cursors() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    sqlx::query("INSERT INTO shop.chain_cursor(indexer,network,start_block,last_block,full_history) VALUES ('nft','matic',1,123,true)")
        .execute(&db.pool).await.unwrap();
    sqlx::raw_sql("ALTER TABLE shop.chain_cursor DROP CONSTRAINT chain_cursor_indexer_known; ALTER TABLE shop.chain_cursor ADD CONSTRAINT chain_cursor_indexer_known CHECK(indexer IN ('nft','trade_events','rentals')); DELETE FROM meta.schema_steps WHERE step='0006_chain_cursor_mana_transfers';")
        .execute(&db.pool).await.unwrap();
    crate::db::schema_migrate::migrate(&db.pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
        .await
        .unwrap();
    assert_eq!(
        chain_walk::cursor(&db.pool, INDEXER_NFT, ChainNet::Matic)
            .await
            .unwrap()
            .unwrap()
            .last_block,
        123
    );
    chain_walk::ensure_cursor(&db.pool, chain_walk::INDEXER_MANA, ChainNet::Matic, 0, true)
        .await
        .unwrap();
    db.drop().await;
}

#[tokio::test]
async fn mana_sync_refuses_missing_finality_and_a_fork_during_fetch() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let chain = FakeChain::new(100, vec![], 1000, 1000);
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(1, 1000), false);
    let ix = super::mana_transfers::ManaTransfersIndex {
        net: ChainNet::Matic,
    };
    chain.finalized_available.store(false, Ordering::SeqCst);
    assert!(
        chain_walk::run_pass(
            &db.pool,
            &Rpc::at(&st.http, &url),
            &ix,
            &st.settings.market.indexes,
            None,
            &|_| {}
        )
        .await
        .is_err()
    );
    chain.finalized_available.store(true, Ordering::SeqCst);
    chain.reorg_on_logs.store(true, Ordering::SeqCst);
    assert!(
        chain_walk::run_pass(
            &db.pool,
            &Rpc::at(&st.http, &url),
            &ix,
            &st.settings.market.indexes,
            None,
            &|_| {}
        )
        .await
        .is_err()
    );
    assert_eq!(
        chain_walk::cursor(&db.pool, chain_walk::INDEXER_MANA, ChainNet::Matic)
            .await
            .unwrap()
            .unwrap()
            .last_block,
        -1
    );
    assert!(
        pass(&st, &url, &ix).await.published,
        "even a one-request budget must make progress"
    );
    db.drop().await;
}

#[tokio::test]
async fn mana_sync_crash_after_window_keeps_the_hash() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let chain = FakeChain::new(100, vec![], 1000, 1000);
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(100, 1000), false);
    let runner = st.clone();
    let endpoint = url.clone();
    let crashed = tokio::spawn(async move {
        chain_walk::run_pass(
            &runner.pool,
            &Rpc::at(&runner.http, &endpoint),
            &super::mana_transfers::ManaTransfersIndex {
                net: ChainNet::Matic,
            },
            &runner.settings.market.indexes,
            None,
            &|_| panic!("injected stop after window commit"),
        )
        .await
    })
    .await;
    assert!(crashed.unwrap_err().is_panic());
    let c = chain_walk::cursor(&db.pool, chain_walk::INDEXER_MANA, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.last_block, 36);
    assert_eq!(
        c.last_block_hash,
        Some(chain.hash_of(36)),
        "committed window must survive a crash with its reorg anchor"
    );
    assert_eq!(c.folded_block, None);
    assert!(
        pass(
            &st,
            &url,
            &super::mana_transfers::ManaTransfersIndex {
                net: ChainNet::Matic
            }
        )
        .await
        .published
    );
    db.drop().await;
}

#[tokio::test]
async fn mana_sync_stop_and_regressed_finality_clear_publication() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let chain = FakeChain::new(100, vec![], 1000, 1000);
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(100, 1000), false);
    let ix = super::mana_transfers::ManaTransfersIndex {
        net: ChainNet::Matic,
    };
    let mut settings = st.settings.market.indexes.clone();
    settings.stop_block_matic = Some(36);
    let outcome = chain_walk::run_pass(
        &db.pool,
        &Rpc::at(&st.http, &url),
        &ix,
        &settings,
        None,
        &|_| {
            *chain.head.lock() = 101;
        },
    )
    .await
    .unwrap();
    assert!(!outcome.published, "stop became historical while folding");
    assert!(pass(&st, &url, &ix).await.published);
    *chain.head.lock() = 90;
    assert!(
        !pass(&st, &url, &ix).await.published,
        "never publish beyond a regressed safe head"
    );
    let c = chain_walk::cursor(&db.pool, chain_walk::INDEXER_MANA, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    assert!(!chain_walk::is_ready(&c, chrono::Utc::now()));
    db.drop().await;
}

#[tokio::test]
async fn api_jobs_never_attach_market_indexing_even_when_enabled() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let mut settings = index_settings(100, 1000);
    settings.mana_transfers_enabled = true;
    settings.sales_enabled = true;
    let mut state = state_over(db.pool.clone(), "https://rpc.example.org/", settings, false);
    let inner = Arc::get_mut(&mut state).unwrap();
    inner.settings.market.mirrors_enabled = true;
    inner.settings.trades_sync_upstream_url = Some("https://market.example.org/".into());
    let api = crate::jobs::Jobs::new().with_fleet_jobs(false);
    state.spawn_api_jobs(&api);
    let names: Vec<_> = api.report().into_iter().map(|j| j.name).collect();
    for forbidden in [
        "nft-index",
        "trade-events-index",
        "mana-transfers-index",
        "sales-index",
        "collections-mirror",
        "items-mirror",
        "trades-mirror",
        "item-neighbours",
    ] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "API attached {forbidden}: {names:?}"
        );
    }
    let worker = crate::jobs::Jobs::new().with_fleet_jobs(false);
    state.spawn_index_jobs(&worker);
    let names: Vec<_> = worker.report().into_iter().map(|j| j.name).collect();
    for required in [
        "nft-index",
        "trade-events-index",
        "mana-transfers-index",
        "sales-index",
        "collections-mirror",
        "items-mirror",
        "trades-mirror",
        "item-neighbours",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "worker omitted {required}: {names:?}"
        );
    }
    assert!(
        api.shutdown(std::time::Duration::from_secs(5))
            .await
            .is_empty()
    );
    assert!(
        worker
            .shutdown(std::time::Duration::from_secs(5))
            .await
            .is_empty()
    );
    db.drop().await;
}

/// `catalyst index` attaches `rentals-index` only when switched on and a
/// chain-1 endpoint is configured.
#[tokio::test]
async fn the_rentals_index_attaches_behind_its_switch_and_a_chain_one_endpoint() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let attached = |state: &SharedState, enabled: bool| {
        let jobs = crate::jobs::Jobs::new().with_fleet_jobs(false);
        crate::rentals::index::spawn_job(state, &jobs, enabled);
        let names: Vec<_> = jobs.report().into_iter().map(|j| j.name).collect();
        (jobs, names.iter().any(|n| n == "rentals-index"))
    };
    let with_rpc = state_over(
        db.pool.clone(),
        "https://rpc.example.org/",
        IndexSettings::default(),
        false,
    );
    let (off, on_without_switch) = attached(&with_rpc, false);
    let (on, on_with_switch) = attached(&with_rpc, true);
    let mut no_rpc = state_over(
        db.pool.clone(),
        "https://rpc.example.org/",
        IndexSettings::default(),
        false,
    );
    Arc::get_mut(&mut no_rpc)
        .unwrap()
        .settings
        .market
        .trade_rpc_urls = RpcEndpoints::from_env(None);
    let (bare, on_without_rpc) = attached(&no_rpc, true);
    assert!(!on_without_switch && on_with_switch && !on_without_rpc);
    for jobs in [off, on, bare] {
        assert!(
            jobs.shutdown(std::time::Duration::from_secs(5))
                .await
                .is_empty()
        );
    }
    db.drop().await;
}

/// The rentals walk over a fake chain feeds the notifications: a rent before
/// the first notification pass is history; a later rent notifies its start,
/// and its end once the published block's time passes it, with the
/// events-notifier metadata.
#[tokio::test]
async fn the_rentals_walk_feeds_rental_started_and_rental_ended() {
    use crate::market::transfer_notices::Pass;
    use crate::rentals::index::{RENTALS_MAINNET, RentalsIndex, tests::rented_log};
    use crate::rentals::notices::{ended_id, run_ended, run_started, started_id};
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let base = "https://decentraland.org/marketplace";
    let land = crate::land_authz::LAND_REGISTRY_MAINNET.to_ascii_lowercase();
    let lessor = "0x00000000000000000000000000000000000000a1";
    let tenant = "0x00000000000000000000000000000000000000b0";
    let day_blocks = 86_400 / 2;
    let rent = |token: U256, days: u64, block: u64| {
        let mut l = rented_log(
            &land,
            token,
            lessor,
            tenant,
            days,
            U256::from(1),
            &[block as u8; 65],
            block,
            0,
        );
        l.block_timestamp = Some(format!("0x{:x}", time_of(block)));
        l
    };
    // Parcel 1,2.
    let parcel = (U256::from(1u64) << 128) | U256::from(2u64);
    let chain = FakeChain::new(
        1_000 + 2 * day_blocks,
        vec![rent(U256::from(7u64), 1, 1_100)],
        10_000_000,
        10,
    );
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(10, 1_000_000), false);
    let ix = RentalsIndex::at(RENTALS_MAINNET, 1_000);
    assert!(pass(&st, &url, &ix).await.published);
    assert_eq!(
        run_started(&st.pool, base).await.unwrap(),
        Pass::Emitted(0),
        "no past rent"
    );
    assert_eq!(
        run_ended(&st.pool, base).await.unwrap(),
        Pass::Emitted(0),
        "no past end"
    );

    let rented_at = 1_000 + 2 * day_blocks + 100;
    chain
        .logs
        .lock()
        .push(log_json(&rent(parcel, 1, rented_at)));
    *chain.head.lock() = rented_at + 1_000;
    assert!(pass(&st, &url, &ix).await.published);
    assert_eq!(run_started(&st.pool, base).await.unwrap(), Pass::Emitted(1));
    assert_eq!(
        run_ended(&st.pool, base).await.unwrap(),
        Pass::Emitted(0),
        "still running"
    );

    *chain.head.lock() = rented_at + day_blocks + 1_000;
    assert!(pass(&st, &url, &ix).await.published);
    assert_eq!(run_ended(&st.pool, base).await.unwrap(), Pass::Emitted(1));
    assert_eq!(
        run_started(&st.pool, base).await.unwrap(),
        Pass::Emitted(0),
        "nothing twice"
    );

    let rows: Vec<(uuid::Uuid, String, String, Value)> = sqlx::query_as(
        "SELECT id, address, type, metadata FROM social.notifications ORDER BY occurred_at",
    )
    .fetch_all(&st.pool)
    .await
    .unwrap();
    let key: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM shop.rental_event e WHERE e.kind = 'rented' AND e.block_number = $1",
        crate::market::notice_cursor::tx_log_key("rental_event", "e", "coalesce(e.signature, '')")
    )))
    .bind(rented_at as i64)
    .fetch_one(&st.pool)
    .await
    .unwrap();
    let (started, ended) = (time_of(rented_at), time_of(rented_at) + 86_400);
    let token = parcel.to_string();
    let link = format!("{base}/contracts/{land}/tokens/{token}/manage");
    let meta = |title: &str, description: String| {
        json!({
            "address": lessor, "contract": land, "lessor": lessor, "tenant": tenant,
            "operator": tenant, "startedAt": started.to_string(), "endedAt": ended.to_string(), "tokenId": token,
            "link": link, "title": title, "land": "1,2", "description": description,
        })
    };
    assert_eq!(
        rows,
        vec![
            (
                started_id("ethereum", &key),
                lessor.to_string(),
                "rental_started".to_string(),
                meta(
                    "LAND Rented",
                    format!("Your LAND at 1,2 was rented by {tenant}.")
                )
            ),
            (
                ended_id("ethereum", &key),
                lessor.to_string(),
                "rental_ended".to_string(),
                meta(
                    "Rent Period Ending",
                    "The rent of your LAND at 1,2 has ended.".to_string()
                )
            ),
        ]
    );
    db.drop().await;
}

/// The rewind path of `run_pass` (plan, delete, apply) moves a producer's
/// cursor back: the gift that replaces a reorged one is announced once the
/// index republishes.
#[tokio::test]
async fn a_reorg_rewind_replays_the_notification_producers() {
    use super::transfer_notices::{self, Pass};
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let s = nft_index::start_block(ChainNet::Matic);
    let c1 = "0x00000000000000000000000000000000000000c1";
    seed_collection(
        &db.pool,
        "matic",
        c1,
        s + 10,
        "urn:decentraland:matic:collections-v2:0xc1",
        "Pirates",
    )
    .await;
    let t1 = v2_token(0, 1);
    let chain = FakeChain::new(
        s + 1_500,
        vec![transfer(c1, ZERO, ALICE, t1, s + 100, 0)],
        1_000_000,
        100,
    );
    let url = serve(chain.clone()).await;
    let st = state_over(db.pool.clone(), &url, index_settings(50, 100_000), false);
    let ix = NftIndex::new(ChainNet::Matic);
    assert!(publish_nft(&st, &url, &ix).await.published);
    transfer_notices::tests::publish(
        &db.pool,
        INDEXER_TRADE_EVENTS,
        "matic",
        (s + 1_000_000) as i64,
        chrono::Utc::now().timestamp(),
    )
    .await;
    let notified = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT address FROM social.notifications WHERE type = 'transfer_received' ORDER BY address",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap()
    };
    assert_eq!(
        transfer_notices::run_once(&db.pool, 137).await.unwrap(),
        Pass::Emitted(0),
        "seeded at the mint"
    );

    chain
        .logs
        .lock()
        .push(log_json(&transfer(c1, ALICE, BOB, t1, s + 1_900, 0)));
    *chain.head.lock() = s + 2_000;
    assert!(publish_nft(&st, &url, &ix).await.published);
    assert_eq!(
        transfer_notices::run_once(&db.pool, 137).await.unwrap(),
        Pass::Emitted(1)
    );

    let cursor = chain_walk::cursor(&db.pool, INDEXER_NFT, ChainNet::Matic)
        .await
        .unwrap()
        .unwrap();
    chain
        .forks
        .lock()
        .insert(cursor.last_block as u64, format!("0x{}", "ee".repeat(32)));
    {
        let mut logs = chain.logs.lock();
        logs.retain(|l| l["blockNumber"] != json!(format!("0x{:x}", s + 1_900)));
        logs.push(log_json(&transfer(c1, ALICE, CARL, t1, s + 1_880, 0)));
    }
    assert!(pass(&st, &url, &ix).await.reorg_rewound_to.is_some());
    chain.forks.lock().clear();
    assert!(publish_nft(&st, &url, &ix).await.published);
    assert_eq!(
        transfer_notices::run_once(&db.pool, 137).await.unwrap(),
        Pass::Emitted(1),
        "the replacing gift is read again below the old cursor"
    );
    assert_eq!(notified().await, vec![BOB.to_string(), CARL.to_string()]);
    db.drop().await;
}
