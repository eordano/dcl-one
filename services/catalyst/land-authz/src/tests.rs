//! The land_authz contracts on a scratch database per test (created on the
//! `CATALYST_TEST_DATABASE_URL` cluster with the real schema, dropped at the
//! end; skipped without the variable): the fold semantics, the rights
//! reading, the readiness rule, and the indexer against a fake JSON-RPC
//! chain (the 10,000-log refusal with a range hint, the request budget, a
//! kill between passes, the head-lag publish rule, the freshness window,
//! the stop block and the replay), then the routes and `can_deploy` over
//! the result. One live probe against a real endpoint runs only with
//! `LAND_AUTHZ_LIVE_RPC_URL`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;

use super::events::*;
use super::indexer::{
    self, ReplayStart, begin_replay, cursor, fold, insert_events, mark_folded, set_cursor,
};
use super::routes::SOURCE_HEADER;
use super::*;
use crate::chain_index::rpc::{HEADER_BATCH, Rpc, hex_to_i64};
use crate::jobs::{JobContext, Jobs};

const LAND: &str = LAND_REGISTRY_MAINNET;
const ESTATE: &str = ESTATE_REGISTRY_MAINNET;
const ZERO_ADDR: &str = "0x0000000000000000000000000000000000000000";
const ALICE: &str = "0xaaaa000000000000000000000000000000000001";
const BOB: &str = "0xbbbb000000000000000000000000000000000002";
const CARL: &str = "0xcccc000000000000000000000000000000000003";
const DANA: &str = "0xdddd000000000000000000000000000000000004";
const OWNER: &str = "0x1111000000000000000000000000000000000003";
/// LAND (37, 55) and (1, 1), (2, 2), (3, 3), (4, 4) as the registry packs them.
const TOKEN_37_55: &str = "12590447576074723148144860474975423823927";
const START: u64 = LAND_START_BLOCK_MAINNET;

/// The decimal token id of LAND (x, y): the two 128-bit two's-complement
/// halves as the registry packs them, converted from the hex form (they
/// exceed u128).
fn land_token(x: i32, y: i32) -> String {
    let hex = format!("{:032x}{:032x}", x as i128 as u128, y as i128 as u128);
    let mut digits: Vec<u8> = vec![0];
    for c in hex.chars() {
        let mut carry = c.to_digit(16).unwrap();
        for d in digits.iter_mut() {
            let v = (*d as u32) * 16 + carry;
            *d = (v % 10) as u8;
            carry = v / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    while digits.len() > 1 && *digits.last().unwrap() == 0 {
        digits.pop();
    }
    digits.iter().rev().map(|d| (b'0' + d) as char).collect()
}

struct ScratchDb {
    name: String,
    pub pool: PgPool,
    admin: PgPool,
}

impl ScratchDb {
    /// `None` (skip) without `CATALYST_TEST_DATABASE_URL`.
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
            "catalyst_test_laz_{}",
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

fn state_over(
    pool: PgPool,
    settings: Settings,
    market: Option<Arc<crate::chain_index::ChainIndex>>,
) -> SharedState {
    Arc::new(LandAuthzState::new(
        pool,
        crate::http::Outbound::for_tests(),
        settings,
        market,
    ))
}

#[tokio::test]
async fn estate_membership_uses_only_the_configured_fresh_publication_on_the_callers_connection() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    seed_rights(&db.pool).await;
    let state = state_over(db.pool.clone(), Settings::for_tests(), None);
    let mut conn = db.pool.acquire().await.unwrap();
    assert!(state.estate_at(&mut conn, 2, 2).await.unwrap().is_none());
    sqlx::query("INSERT INTO land_authz.index_cursor (network,land_registry,estate_registry,last_block,folded_block,folded_at,full_replay_started) VALUES ($1,$2,$3,1,1,timezone('UTC',now()),true)")
        .bind(NETWORK_MAINNET).bind(LAND).bind(ESTATE).execute(&mut *conn).await.unwrap();
    assert_eq!(
        state
            .estate_at(&mut conn, 2, 2)
            .await
            .unwrap()
            .unwrap()
            .estate_id
            .as_deref(),
        Some("500")
    );
    assert!(
        state
            .estate_at(&mut conn, 1, 1)
            .await
            .unwrap()
            .unwrap()
            .estate_id
            .is_none()
    );
    sqlx::query("DELETE FROM land_authz.estate_land WHERE land_token_id=$1::numeric")
        .bind(land_token(2, 2))
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        state
            .estate_at(&mut conn, 2, 2)
            .await
            .unwrap()
            .unwrap()
            .estate_id
            .is_none()
    );
    for assignment in [
        "folded_at=timezone('UTC',now())-interval '31 minutes'",
        "last_block=2",
        "full_replay_started=false",
        "land_registry='other'",
    ] {
        sqlx::query("UPDATE land_authz.index_cursor SET folded_at=timezone('UTC',now()),last_block=1,full_replay_started=true,land_registry=$1").bind(LAND).execute(&mut *conn).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE land_authz.index_cursor SET {assignment}"
        )))
        .execute(&mut *conn)
        .await
        .unwrap();
        assert!(
            state.estate_at(&mut conn, 2, 2).await.unwrap().is_none(),
            "{assignment}"
        );
    }
    drop(conn);
    db.drop().await;
}

async fn job_ctx() -> (Jobs, JobContext) {
    let jobs = Jobs::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    jobs.spawn("probe", move |ctx| async move {
        let _ = tx.send(ctx.clone());
        ctx.cancel.cancelled().await;
        Ok(())
    });
    let ctx = rx.await.unwrap();
    (jobs, ctx)
}

#[allow(clippy::too_many_arguments)]
fn ev(
    block: i64,
    index: i32,
    kind: &'static str,
    token: Option<&str>,
    account: Option<&str>,
    operator: Option<&str>,
    approved: Option<bool>,
) -> AuthzEvent {
    AuthzEvent {
        block_number: block,
        log_index: index,
        block_time: block * 12,
        token_address: LAND.to_string(),
        kind,
        token_id: token.map(|t| t.to_string()),
        estate_id: None,
        account: account.map(|a| a.to_string()),
        operator: operator.map(|o| o.to_string()),
        approved,
    }
}

fn estate_ev(block: i64, index: i32, kind: &'static str, estate: &str, land: &str) -> AuthzEvent {
    AuthzEvent {
        token_address: ESTATE.to_string(),
        estate_id: Some(estate.to_string()),
        ..ev(block, index, kind, Some(land), None, None, None)
    }
}

async fn insert(pool: &PgPool, events: &[AuthzEvent]) -> usize {
    let mut conn = pool.acquire().await.unwrap();
    insert_events(&mut conn, events).await.unwrap()
}

/// The rebuild fold, under the cursor of the configured registries that a
/// pass always holds before it folds (created here when the test has none).
async fn fold_all(pool: &PgPool) -> indexer::FoldCounts {
    let settings = Settings::for_tests();
    indexer::ensure_cursor(pool, &settings).await.unwrap();
    fold(pool, &settings, None).await.unwrap()
}

async fn token_leg(pool: &PgPool, token: &str, column: &str) -> Option<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {column} FROM land_authz.token_right WHERE token_id = $1::numeric"
    )))
    .bind(token)
    .fetch_optional(pool)
    .await
    .unwrap()
    .flatten()
}

async fn account_leg(pool: &PgPool, account: &str, kind: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT operator FROM land_authz.account_right WHERE account = $1 AND kind = $2 AND is_approved ORDER BY operator",
    )
    .bind(account)
    .bind(kind)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn estate_of(pool: &PgPool, land: &str) -> Option<String> {
    sqlx::query_scalar("SELECT estate_token_id::text FROM land_authz.estate_land WHERE land_token_id = $1::numeric")
        .bind(land)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_granted_then_revoked_update_operator_ends_revoked() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(
                10,
                0,
                KIND_UPDATE_OPERATOR,
                Some("1"),
                None,
                Some(ALICE),
                None,
            ),
            ev(
                20,
                0,
                KIND_UPDATE_OPERATOR,
                Some("1"),
                None,
                Some(ZERO_ADDR),
                None,
            ),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    assert_eq!(
        token_leg(&db.pool, "1", "update_operator").await.as_deref(),
        Some(ZERO_ADDR),
        "a cleared leg keeps the zero address, as the land-manager subgraph \
         stores it (captures/lambdas-land/LL-18-*.json)"
    );
    db.drop().await;
}

/// The land-manager subgraph stores `null` for a leg never set and the zero
/// address for one cleared on chain; lamb2 passes both through
/// (`captures/lambdas-land/LL-18-*.json`). A transfer still clears to NULL.
/// The deploy check reads a cleared leg as no leg: the zero address is
/// never granted, and a parcel's cleared update operator falls back to its
/// estate's, as before.
#[tokio::test]
async fn a_cleared_leg_answers_the_zero_address_and_never_grants() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(
                10,
                0,
                KIND_APPROVAL,
                Some("6"),
                Some(OWNER),
                Some(BOB),
                None,
            ),
            ev(
                11,
                0,
                KIND_APPROVAL,
                Some("6"),
                Some(OWNER),
                Some(ZERO_ADDR),
                None,
            ),
            ev(
                12,
                0,
                KIND_APPROVAL,
                Some("7"),
                Some(OWNER),
                Some(ZERO_ADDR),
                None,
            ),
            ev(
                12,
                1,
                KIND_UPDATE_OPERATOR,
                Some("7"),
                None,
                Some(ZERO_ADDR),
                None,
            ),
            ev(13, 0, KIND_TRANSFER, Some("7"), Some(BOB), None, None),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    assert_eq!(
        token_leg(&db.pool, "6", "operator").await.as_deref(),
        Some(ZERO_ADDR)
    );
    assert_eq!(
        token_leg(&db.pool, "6", "update_operator").await,
        None,
        "never set"
    );
    assert_eq!(
        token_leg(&db.pool, "7", "operator").await,
        None,
        "transferred"
    );
    assert_eq!(token_leg(&db.pool, "7", "update_operator").await, None);

    seed_rights(&db.pool).await;
    sqlx::query("UPDATE land_authz.token_right SET update_operator = $2 WHERE token_address = $1 AND x = 2 AND y = 2")
        .bind(LAND)
        .bind(ZERO_ADDR)
        .execute(&db.pool)
        .await
        .unwrap();
    let store = Store::new(db.pool.clone(), LAND, ESTATE);
    let rights = store.rights(2, 2).await.unwrap().unwrap();
    assert_eq!(
        rights.subject.update_operator.as_deref(),
        Some(ZERO_ADDR),
        "lamb2's `parcel.updateOperator ?? estate.updateOperator` keeps the parcel's zero"
    );
    assert!(!rights.flags_for(CARL).update_operator);
    assert!(
        rights.flags_for(ZERO_ADDR).update_operator,
        "lamb2's comparison"
    );
    assert_eq!(
        rights.deploy_flags_for(CARL).granting_leg(),
        Some(LandRightLeg::Operator),
        "the estate's operator"
    );
    assert!(
        rights.deploy_flags_for(CARL).update_operator,
        "the estate's update operator still reaches the parcel"
    );
    assert!(!rights.deploy_flags_for(ZERO_ADDR).grants_deploy());
    let updatable = store.parcels_updatable_by(CARL).await.unwrap();
    assert_eq!(
        updatable.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>(),
        vec![(2, 2)]
    );
    db.drop().await;
}

/// `schemas/migrations/0027_land_authz_cleared_legs.sql` rewrites the legs
/// the old fold stored as NULL for a grant to the zero address, once, and
/// leaves a leg a later transfer cleared.
#[tokio::test]
async fn the_cleared_legs_step_backfills_the_zero_address() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(
                10,
                0,
                KIND_UPDATE_OPERATOR,
                Some("1"),
                None,
                Some(ALICE),
                None,
            ),
            ev(
                11,
                0,
                KIND_UPDATE_OPERATOR,
                Some("1"),
                None,
                Some(ZERO_ADDR),
                None,
            ),
            ev(
                11,
                1,
                KIND_APPROVAL,
                Some("1"),
                Some(OWNER),
                Some(ZERO_ADDR),
                None,
            ),
            ev(
                12,
                0,
                KIND_UPDATE_OPERATOR,
                Some("2"),
                None,
                Some(ZERO_ADDR),
                None,
            ),
            ev(13, 0, KIND_TRANSFER, Some("2"), Some(BOB), None, None),
            ev(
                14,
                0,
                KIND_UPDATE_OPERATOR,
                Some("3"),
                None,
                Some(CARL),
                None,
            ),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    sqlx::query("UPDATE land_authz.token_right SET operator = NULL, update_operator = NULL WHERE token_id IN (1, 2)")
        .execute(&db.pool)
        .await
        .unwrap();
    let step = include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/schemas/migrations/0027_land_authz_cleared_legs.sql"
    ));
    for _ in 0..2 {
        sqlx::raw_sql(step).execute(&db.pool).await.unwrap();
    }
    assert_eq!(
        token_leg(&db.pool, "1", "update_operator").await.as_deref(),
        Some(ZERO_ADDR)
    );
    assert_eq!(
        token_leg(&db.pool, "1", "operator").await.as_deref(),
        Some(ZERO_ADDR)
    );
    assert_eq!(token_leg(&db.pool, "2", "update_operator").await, None);
    assert_eq!(
        token_leg(&db.pool, "3", "update_operator").await.as_deref(),
        Some(CARL)
    );
    db.drop().await;
}

#[tokio::test]
async fn the_later_grant_wins_regardless_of_insertion_order_and_log_index_breaks_ties() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(
                99,
                0,
                KIND_UPDATE_OPERATOR,
                Some("7"),
                None,
                Some(BOB),
                None,
            ),
            ev(
                5,
                0,
                KIND_UPDATE_OPERATOR,
                Some("7"),
                None,
                Some(ALICE),
                None,
            ),
            ev(
                42,
                3,
                KIND_UPDATE_OPERATOR,
                Some("9"),
                None,
                Some(BOB),
                None,
            ),
            ev(
                42,
                1,
                KIND_UPDATE_OPERATOR,
                Some("9"),
                None,
                Some(ALICE),
                None,
            ),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    assert_eq!(
        token_leg(&db.pool, "7", "update_operator").await.as_deref(),
        Some(BOB)
    );
    assert_eq!(
        token_leg(&db.pool, "9", "update_operator").await.as_deref(),
        Some(BOB)
    );
    db.drop().await;
}

#[tokio::test]
async fn a_transfer_clears_both_per_token_legs_and_settles_the_owner() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(9, 0, KIND_TRANSFER, Some("3"), Some(OWNER), None, None),
            ev(
                10,
                0,
                KIND_UPDATE_OPERATOR,
                Some("3"),
                None,
                Some(ALICE),
                None,
            ),
            ev(
                10,
                1,
                KIND_APPROVAL,
                Some("3"),
                Some(OWNER),
                Some(BOB),
                None,
            ),
            ev(30, 0, KIND_TRANSFER, Some("3"), Some(BOB), None, None),
            ev(30, 0, KIND_TRANSFER, Some("3"), Some(CARL), None, None),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    assert_eq!(token_leg(&db.pool, "3", "update_operator").await, None);
    assert_eq!(token_leg(&db.pool, "3", "operator").await, None);
    assert_eq!(
        token_leg(&db.pool, "3", "owner").await.as_deref(),
        Some(BOB),
        "the duplicate (30, 0) row was dropped"
    );
    let (block, log): (i64, i32) = sqlx::query_as(
        "SELECT updated_block, updated_log FROM land_authz.token_right WHERE token_id = 3",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (block, log),
        (30, 0),
        "the row names the event that last wrote it"
    );
    db.drop().await;
}

#[tokio::test]
async fn a_grant_after_a_transfer_survives_it_and_a_burn_clears_the_owner() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(30, 0, KIND_TRANSFER, Some("4"), Some(BOB), None, None),
            ev(
                31,
                0,
                KIND_UPDATE_OPERATOR,
                Some("4"),
                None,
                Some(ALICE),
                None,
            ),
            ev(32, 0, KIND_TRANSFER, Some("8"), Some(ZERO_ADDR), None, None),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    assert_eq!(
        token_leg(&db.pool, "4", "update_operator").await.as_deref(),
        Some(ALICE)
    );
    assert_eq!(
        token_leg(&db.pool, "4", "owner").await.as_deref(),
        Some(BOB)
    );
    assert_eq!(token_leg(&db.pool, "8", "owner").await, None);
    db.drop().await;
}

#[tokio::test]
async fn account_wide_legs_are_revocable_and_do_not_bleed_into_each_other_or_out_of_token_grants() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(
                10,
                0,
                KIND_APPROVED_FOR_ALL,
                None,
                Some(OWNER),
                Some(ALICE),
                Some(true),
            ),
            ev(
                11,
                0,
                KIND_APPROVED_FOR_ALL,
                None,
                Some(OWNER),
                Some(BOB),
                Some(true),
            ),
            ev(
                12,
                0,
                KIND_APPROVED_FOR_ALL,
                None,
                Some(OWNER),
                Some(ALICE),
                Some(false),
            ),
            ev(
                13,
                0,
                KIND_UPDATE_MANAGER,
                None,
                Some(OWNER),
                Some(ALICE),
                Some(true),
            ),
            ev(
                14,
                0,
                KIND_UPDATE_MANAGER,
                None,
                Some(OWNER),
                Some(ALICE),
                Some(false),
            ),
            ev(
                15,
                0,
                KIND_UPDATE_OPERATOR,
                Some("5"),
                None,
                Some(ALICE),
                None,
            ),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    assert_eq!(
        account_leg(&db.pool, OWNER, KIND_APPROVED_FOR_ALL).await,
        vec![BOB.to_string()]
    );
    assert!(
        account_leg(&db.pool, OWNER, KIND_UPDATE_MANAGER)
            .await
            .is_empty()
    );
    assert!(
        account_leg(&db.pool, ALICE, KIND_UPDATE_MANAGER)
            .await
            .is_empty()
    );
    assert!(
        account_leg(&db.pool, ALICE, KIND_APPROVED_FOR_ALL)
            .await
            .is_empty()
    );
    db.drop().await;
}

#[tokio::test]
async fn folding_twice_is_idempotent_and_the_incremental_fold_matches_the_rebuild() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let settings = Settings::for_tests();
    indexer::ensure_cursor(&db.pool, &settings).await.unwrap();
    insert(
        &db.pool,
        &[
            ev(
                10,
                0,
                KIND_UPDATE_OPERATOR,
                Some("6"),
                None,
                Some(ALICE),
                None,
            ),
            ev(
                11,
                0,
                KIND_APPROVED_FOR_ALL,
                None,
                Some(OWNER),
                Some(BOB),
                Some(true),
            ),
            estate_ev(12, 0, KIND_ADD_LAND, "500", "6"),
        ],
    )
    .await;
    let first = fold(&db.pool, &settings, None).await.unwrap();
    assert_eq!(
        first,
        indexer::FoldCounts {
            tokens: 1,
            accounts: 1,
            estate_lands: 1
        }
    );
    fold(&db.pool, &settings, None).await.unwrap();
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM land_authz.token_right")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(tokens, 1);
    assert_eq!(
        token_leg(&db.pool, "6", "update_operator").await.as_deref(),
        Some(ALICE)
    );

    // New events past block 12: the incremental fold touches only their keys ...
    insert(
        &db.pool,
        &[
            ev(
                20,
                0,
                KIND_UPDATE_OPERATOR,
                Some("6"),
                None,
                Some(CARL),
                None,
            ),
            ev(21, 0, KIND_TRANSFER, Some("7"), Some(DANA), None, None),
            estate_ev(22, 0, KIND_REMOVE_LAND, "500", "6"),
        ],
    )
    .await;
    let incremental = fold(&db.pool, &settings, Some(12)).await.unwrap();
    assert_eq!(
        incremental,
        indexer::FoldCounts {
            tokens: 2,
            accounts: 0,
            estate_lands: 0
        }
    );
    assert_eq!(
        token_leg(&db.pool, "6", "update_operator").await.as_deref(),
        Some(CARL)
    );
    assert_eq!(
        token_leg(&db.pool, "7", "owner").await.as_deref(),
        Some(DANA)
    );
    assert_eq!(
        estate_of(&db.pool, "6").await,
        None,
        "the RemoveLand deleted the membership"
    );
    // ... and arrives at the rows the rebuild produces.
    type TokenRow = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i32,
    );
    let snapshot = |pool: PgPool| async move {
        let tokens: Vec<TokenRow> = sqlx::query_as(
            "SELECT token_id::text, owner, operator, update_operator, updated_block, updated_log FROM land_authz.token_right ORDER BY token_address, token_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let accounts: Vec<(String, String, bool)> = sqlx::query_as(
            "SELECT account, operator, is_approved FROM land_authz.account_right ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let estates: Vec<(String, String)> =
            sqlx::query_as("SELECT land_token_id::text, estate_token_id::text FROM land_authz.estate_land ORDER BY 1").fetch_all(&pool).await.unwrap();
        (tokens, accounts, estates)
    };
    let after_incremental = snapshot(db.pool.clone()).await;
    sqlx::query(
        "TRUNCATE land_authz.token_right, land_authz.account_right, land_authz.estate_land",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    fold(&db.pool, &settings, None).await.unwrap();
    assert_eq!(snapshot(db.pool.clone()).await, after_incremental);
    db.drop().await;
}

#[tokio::test]
async fn coordinates_come_back_out_of_the_token_id_and_estate_tokens_have_none() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            ev(
                10,
                0,
                KIND_UPDATE_OPERATOR,
                Some(TOKEN_37_55),
                None,
                Some(ALICE),
                None,
            ),
            AuthzEvent {
                token_address: ESTATE.into(),
                ..ev(11, 0, KIND_TRANSFER, Some("500"), Some(ALICE), None, None)
            },
        ],
    )
    .await;
    fold_all(&db.pool).await;
    let xy: (Option<i32>, Option<i32>) =
        sqlx::query_as("SELECT x, y FROM land_authz.token_right WHERE token_id = $1::numeric")
            .bind(TOKEN_37_55)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(xy, (Some(37), Some(55)));
    assert_eq!(land_token(37, 55), TOKEN_37_55);
    let estate_xy: (Option<i32>, Option<i32>) = sqlx::query_as(
        "SELECT x, y FROM land_authz.token_right WHERE token_address = $1 AND token_id = 500",
    )
    .bind(ESTATE)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(estate_xy, (None, None));
    db.drop().await;
}

#[tokio::test]
async fn estate_membership_follows_the_latest_add_or_remove_land() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    insert(
        &db.pool,
        &[
            estate_ev(10, 0, KIND_ADD_LAND, "500", "12"),
            estate_ev(11, 0, KIND_ADD_LAND, "500", "13"),
            estate_ev(12, 0, KIND_REMOVE_LAND, "500", "12"),
            estate_ev(13, 0, KIND_ADD_LAND, "501", "12"),
            estate_ev(14, 0, KIND_ADD_LAND, "501", "14"),
            estate_ev(15, 0, KIND_REMOVE_LAND, "501", "14"),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    assert_eq!(estate_of(&db.pool, "12").await.as_deref(), Some("501"));
    assert_eq!(estate_of(&db.pool, "13").await.as_deref(), Some("500"));
    assert_eq!(estate_of(&db.pool, "14").await, None);
    db.drop().await;
}

/// The old fixture: parcel (1,1) owned by BOB with CARL as operator; (2,2)
/// and (3,3) in estate 500 owned by ALICE with CARL as its operator and
/// update operator, (3,3) with its own update operator DANA; (4,4) minted
/// to nobody (burned). BOB granted DANA and CARL update-manager on LAND,
/// ALICE granted DANA approved-for-all on the Estate registry.
async fn seed_rights(pool: &PgPool) {
    let sql = format!(
        "INSERT INTO land_authz.token_right (token_address, token_id, x, y, owner, operator, update_operator, updated_block, updated_log) VALUES \
            ('{LAND}', {t11}, 1, 1, '{BOB}', '{CARL}', NULL, 1, 0), \
            ('{LAND}', {t22}, 2, 2, '{ESTATE}', NULL, NULL, 1, 1), \
            ('{LAND}', {t33}, 3, 3, '{ESTATE}', NULL, '{DANA}', 1, 2), \
            ('{LAND}', {t44}, 4, 4, NULL, NULL, NULL, 1, 3), \
            ('{ESTATE}', 500, NULL, NULL, '{ALICE}', '{CARL}', '{CARL}', 1, 4); \
         INSERT INTO land_authz.estate_land VALUES ({t22}, 500, 1, 1), ({t33}, 500, 1, 2); \
         INSERT INTO land_authz.account_right VALUES \
            ('{LAND}', '{BOB}', '{DANA}', 'update_manager', true, 1, 3), \
            ('{LAND}', '{BOB}', '{CARL}', 'update_manager', true, 1, 4), \
            ('{LAND}', '{BOB}', '{ALICE}', 'update_manager', false, 1, 5), \
            ('{ESTATE}', '{ALICE}', '{DANA}', 'approved_for_all', true, 1, 6)",
        t11 = land_token(1, 1),
        t22 = land_token(2, 2),
        t33 = land_token(3, 3),
        t44 = land_token(4, 4),
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .unwrap();
}

/// lamb2 folds the owner's authorization log into a JS `Map`
/// (`parcel-rights-fetcher.ts`): the legs list operators by the approval
/// that opened their current grant, a repeated approval keeps its place and
/// a revoke then approve moves the operator to the end.
#[tokio::test]
async fn grant_legs_keep_lamb2s_map_order() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let grant = |block, index, kind, operator, approved| {
        ev(
            block,
            index,
            kind,
            None,
            Some(BOB),
            Some(operator),
            Some(approved),
        )
    };
    insert(
        &db.pool,
        &[
            ev(
                10,
                0,
                KIND_TRANSFER,
                Some(TOKEN_37_55),
                Some(BOB),
                None,
                None,
            ),
            grant(11, 0, KIND_UPDATE_MANAGER, DANA, true),
            grant(12, 0, KIND_UPDATE_MANAGER, CARL, true),
            grant(13, 0, KIND_UPDATE_MANAGER, ALICE, true),
            grant(14, 0, KIND_UPDATE_MANAGER, DANA, true),
            grant(15, 0, KIND_UPDATE_MANAGER, CARL, false),
            grant(16, 0, KIND_UPDATE_MANAGER, CARL, true),
            grant(17, 0, KIND_APPROVED_FOR_ALL, CARL, true),
            grant(17, 1, KIND_APPROVED_FOR_ALL, ALICE, true),
            grant(18, 0, KIND_APPROVED_FOR_ALL, DANA, true),
            grant(18, 1, KIND_APPROVED_FOR_ALL, DANA, false),
        ],
    )
    .await;
    fold_all(&db.pool).await;
    let store = Store::new(db.pool.clone(), LAND, ESTATE);
    let rights = store.rights(37, 55).await.unwrap().unwrap();
    assert_eq!(rights.subject.owner, BOB);
    assert_eq!(rights.operators.update_managers, vec![DANA, ALICE, CARL]);
    assert_eq!(rights.operators.approved_for_all, vec![CARL, ALICE]);
    assert_eq!(
        store
            .account_grants(LAND, BOB, KIND_UPDATE_MANAGER)
            .await
            .unwrap(),
        vec![DANA, ALICE, CARL]
    );
    db.drop().await;
}

#[tokio::test]
async fn rights_batch_matches_the_per_leg_reading() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    seed_rights(&db.pool).await;
    let store = Store::new(db.pool.clone(), LAND, ESTATE);

    let parcels = [(1, 1), (2, 2), (9, 9), (3, 3), (4, 4), (1, 1)];
    let batch = store.rights_batch(&parcels).await.unwrap();
    assert_eq!(batch.len(), parcels.len(), "positional by input");

    for (i, &(x, y)) in parcels.iter().enumerate() {
        let subject = store.parcel_subject(x, y).await.unwrap();
        let got = batch[i].as_ref().map(|r| r.subject.clone());
        assert_eq!(got, subject, "subject of ({x},{y})");
        let Some(subject) = subject else { continue };
        let rights = batch[i].as_ref().unwrap();
        let managers = store
            .account_grants(&subject.registry, &subject.owner, KIND_UPDATE_MANAGER)
            .await
            .unwrap();
        let approved = store
            .account_grants(&subject.registry, &subject.owner, KIND_APPROVED_FOR_ALL)
            .await
            .unwrap();
        assert_eq!(rights.operators.update_managers, managers, "({x},{y})");
        assert_eq!(rights.operators.approved_for_all, approved, "({x},{y})");
        assert_eq!(rights.operators.operator, subject.operator);
        assert_eq!(rights.operators.update_operator, subject.update_operator);
    }

    let p1 = batch[0].as_ref().unwrap();
    assert_eq!(p1.subject.owner, BOB);
    assert_eq!(p1.subject.operator.as_deref(), Some(CARL));
    assert_eq!(p1.operators.update_managers, vec![CARL, DANA]);
    assert!(p1.operators.approved_for_all.is_empty());

    let p2 = batch[1].as_ref().unwrap();
    assert!(p2.subject.belongs_to_estate);
    assert_eq!(p2.subject.owner, ALICE);
    assert_eq!(p2.subject.registry, ESTATE);
    assert_eq!(p2.subject.operator.as_deref(), Some(CARL));
    assert_eq!(p2.subject.update_operator.as_deref(), Some(CARL));
    assert_eq!(p2.operators.approved_for_all, vec![DANA]);
    assert!(p2.operators.update_managers.is_empty());

    assert!(batch[2].is_none(), "unindexed parcel");
    let p3 = batch[3].as_ref().unwrap();
    assert_eq!(
        p3.subject.update_operator.as_deref(),
        Some(DANA),
        "a per-parcel update operator wins over the estate's"
    );
    let p4 = batch[4].as_ref().unwrap();
    assert_eq!(p4.subject.owner, "");
    assert_eq!(p4.subject.registry, LAND);
    assert!(store.rights_batch(&[]).await.unwrap().is_empty());

    let deploy = store.deploy_rights_batch(&parcels).await.unwrap();
    for (ordered, deploy) in batch.iter().zip(&deploy) {
        let sorted = |r: &Option<ParcelRights>| {
            r.clone().map(|mut r| {
                r.operators.update_managers.sort();
                r.operators.approved_for_all.sort();
                r
            })
        };
        assert_eq!(
            sorted(ordered),
            sorted(deploy),
            "the deploy batch only drops the order"
        );
    }

    assert_eq!(p1.flags_for(BOB).granting_leg(), Some(LandRightLeg::Owner));
    assert_eq!(
        p1.flags_for(DANA).granting_leg(),
        Some(LandRightLeg::UpdateManager)
    );
    assert_eq!(
        p2.flags_for(ALICE).granting_leg(),
        Some(LandRightLeg::Owner),
        "the estate's owner owns the parcel"
    );
    assert_eq!(
        p2.flags_for(DANA).granting_leg(),
        Some(LandRightLeg::ApprovedForAll)
    );
    assert!(!p4.flags_for("").grants_deploy());

    assert!(
        store.owns_land_at(ALICE, &[(2, 2)]).await.unwrap(),
        "through the estate"
    );
    assert!(store.owns_land_at(BOB, &[(9, 9), (1, 1)]).await.unwrap());
    assert!(!store.owns_land_at(CARL, &[(1, 1), (2, 2)]).await.unwrap());
    assert!(
        !store
            .owns_land_at("not-an-address", &[(1, 1)])
            .await
            .unwrap()
    );
    let owned = store.parcels_owned_by(ALICE, 10).await.unwrap();
    assert_eq!(
        owned
            .iter()
            .map(|p| (p.x, p.y, p.via_estate))
            .collect::<Vec<_>>(),
        vec![(2, 2, true), (3, 3, true)]
    );

    let updatable = store.parcels_updatable_by(CARL).await.unwrap();
    assert_eq!(
        updatable
            .iter()
            .map(|p| (p.x, p.y, p.via_estate, p.owner.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, 2, true, ALICE)],
        "(3,3) has its own update operator, so the estate grant does not reach it"
    );
    db.drop().await;
}

/// The land-manager subgraph's `Parcel.id` is the token id as lower-case
/// hex without leading zeros, and lamb2 lists `lands-permissions` in its
/// string order (`captures/lambdas-land/LL-17-*.json`): (9, 0) sorts after
/// (22, -19).
#[tokio::test]
async fn update_operator_page_answers_the_subgraph_parcel_id_in_string_order() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    for (i, (x, y)) in [
        (16, 9),
        (16, 65),
        (22, -19),
        (0, 0),
        (0, 1),
        (-47, 111),
        (17, 147),
        (9, 0),
        (0, -1),
        (-1, -1),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO land_authz.token_right (token_address, token_id, x, y, owner, update_operator, updated_block, updated_log) \
             VALUES ($1, $2::numeric, $3, $4, $5, $6, 1, $7)",
        )
        .bind(LAND)
        .bind(land_token(x, y))
        .bind(x)
        .bind(y)
        .bind(BOB)
        .bind(DANA)
        .bind(i as i32)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    let store = Store::new(db.pool.clone(), LAND, ESTATE);
    let (page, total) = store
        .parcels_with_update_operator_page(DANA, 100, 0)
        .await
        .unwrap();
    assert_eq!(total, 10);
    assert_eq!(
        page.iter()
            .map(|p| (p.id.as_str(), p.x, p.y))
            .collect::<Vec<_>>(),
        vec![
            ("0x0", 0, 0),
            ("0x1", 0, 1),
            ("0x1000000000000000000000000000000009", 16, 9),
            ("0x1000000000000000000000000000000041", 16, 65),
            ("0x1100000000000000000000000000000093", 17, 147),
            ("0x16ffffffffffffffffffffffffffffffed", 22, -19),
            ("0x900000000000000000000000000000000", 9, 0),
            (
                "0xffffffffffffffffffffffffffffffd10000000000000000000000000000006f",
                -47,
                111
            ),
            ("0xffffffffffffffffffffffffffffffff", 0, -1),
            (
                "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                -1,
                -1
            ),
        ]
    );
    let (page, _) = store
        .parcels_with_update_operator_page(DANA, 2, 6)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>(),
        vec![(9, 0), (-47, 111)]
    );
    db.drop().await;
}

#[tokio::test]
async fn update_operator_page_carries_the_total() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    seed_rights(&db.pool).await;
    let store = Store::new(db.pool.clone(), LAND, ESTATE);
    sqlx::query("UPDATE land_authz.token_right SET update_operator = $2 WHERE token_address = $1 AND x IN (2, 4)")
        .bind(LAND)
        .bind(DANA)
        .execute(&db.pool)
        .await
        .unwrap();

    // (16, 0) and (0, 5) sort apart in numeric and in string order.
    for (i, (x, y)) in [(16, 0), (0, 5)].into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO land_authz.token_right (token_address, token_id, x, y, owner, update_operator, updated_block, updated_log) \
             VALUES ($1, $2::numeric, $3, $4, $5, $6, 2, $7)",
        )
        .bind(LAND)
        .bind(land_token(x, y))
        .bind(x)
        .bind(y)
        .bind(BOB)
        .bind(DANA)
        .bind(i as i32)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    let all = [(16, 0), (2, 2), (3, 3), (4, 4), (0, 5)];
    let coords = |page: Vec<resolve::OperatedParcel>| -> Vec<(i32, i32)> {
        page.iter().map(|p| (p.x, p.y)).collect()
    };
    let (page, total) = store
        .parcels_with_update_operator_page(DANA, 2, 0)
        .await
        .unwrap();
    assert_eq!((total, coords(page)), (5, all[..2].to_vec()));
    let (page, total) = store
        .parcels_with_update_operator_page(DANA, 2, 2)
        .await
        .unwrap();
    assert_eq!((total, coords(page)), (5, all[2..4].to_vec()));
    let (page, total) = store
        .parcels_with_update_operator_page(DANA, 2, 4)
        .await
        .unwrap();
    assert_eq!((total, coords(page)), (5, all[4..].to_vec()));
    let (page, total) = store
        .parcels_with_update_operator_page(DANA, 2, 10)
        .await
        .unwrap();
    assert_eq!(
        (page.len(), total),
        (0, 5),
        "past the end still reports the total"
    );
    let (page, total) = store
        .parcels_with_update_operator_page(ALICE, 2, 0)
        .await
        .unwrap();
    assert_eq!((page.len(), total), (0, 0));
    db.drop().await;
}

async fn present(pool: &PgPool) -> bool {
    indexer::ready_cursor(pool, NETWORK_MAINNET, LAND, ESTATE)
        .await
        .is_some()
}

async fn set(pool: &PgPool, block: i64) {
    let mut conn = pool.acquire().await.unwrap();
    set_cursor(&mut conn, &Settings::for_tests(), block)
        .await
        .unwrap();
}

#[tokio::test]
async fn local_rights_are_used_only_after_a_recent_complete_fold() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let settings = Settings::for_tests();
    let base = START as i64;
    assert!(!present(pool).await, "a new table is not a ready index");
    assert!(cursor(pool, NETWORK_MAINNET).await.unwrap().is_none());

    // The old server's cursor had no full-replay record: a row planted by hand.
    sqlx::query("INSERT INTO land_authz.index_cursor (network, land_registry, estate_registry, last_block, full_replay_started) VALUES ($1, $2, $3, $4, false)")
        .bind(NETWORK_MAINNET).bind(LAND).bind(ESTATE).bind(base + 100)
        .execute(pool).await.unwrap();
    assert!(
        !present(pool).await,
        "an unfolded cursor cannot grant rights"
    );
    assert!(
        !mark_folded(pool, &Settings::for_tests(), (base + 101) as u64)
            .await
            .unwrap(),
        "partial sync cannot publish readiness"
    );
    assert!(!present(pool).await);
    assert!(
        !mark_folded(pool, &Settings::for_tests(), (base + 100) as u64)
            .await
            .unwrap(),
        "an existing cursor does not prove its full history was scanned"
    );

    begin_replay(pool, &settings, START).await.unwrap();
    assert_eq!(
        cursor(pool, NETWORK_MAINNET)
            .await
            .unwrap()
            .unwrap()
            .last_block,
        base - 1
    );
    begin_replay(pool, &settings, (base + 95) as u64)
        .await
        .unwrap();
    assert_eq!(
        cursor(pool, NETWORK_MAINNET)
            .await
            .unwrap()
            .unwrap()
            .last_block,
        base - 1,
        "a second repair cannot skip the earlier gap"
    );
    assert!(
        !present(pool).await,
        "repair must disable stale rights immediately"
    );
    assert!(
        !mark_folded(pool, &Settings::for_tests(), (base + 100) as u64)
            .await
            .unwrap()
    );
    set(pool, base + 100).await;
    sqlx::query("UPDATE land_authz.index_cursor SET history_timestamps_ready = true")
        .execute(pool)
        .await
        .unwrap();
    assert!(
        mark_folded(pool, &Settings::for_tests(), (base + 100) as u64)
            .await
            .unwrap()
    );
    assert!(present(pool).await);

    set(pool, base + 101).await;
    assert!(!present(pool).await, "backfill invalidates the old fold");
    assert!(
        mark_folded(pool, &Settings::for_tests(), (base + 101) as u64)
            .await
            .unwrap()
    );
    assert!(present(pool).await);

    sqlx::query("UPDATE land_authz.index_cursor SET folded_at = folded_at - interval '31 minutes'")
        .execute(pool)
        .await
        .unwrap();
    assert!(!present(pool).await, "a stopped indexer falls back");

    // A fresh database starts from the deployment block and records the full replay.
    sqlx::query("DELETE FROM land_authz.index_cursor")
        .execute(pool)
        .await
        .unwrap();
    let row = indexer::ensure_cursor(pool, &settings).await.unwrap();
    assert_eq!((row.last_block, row.full_replay_started), (base - 1, true));
    let other = Settings {
        land_registry: LAND_REGISTRY_SEPOLIA.into(),
        ..settings.clone()
    };
    let err = indexer::ensure_cursor(pool, &other)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("LAND_AUTHZ_REPLAY_FROM_BLOCK"), "{err}");
    db.drop().await;
}

fn topic_addr(a: &str) -> String {
    format!("0x{:0>64}", a.trim_start_matches("0x"))
}

fn topic_uint(decimal: &str) -> String {
    let n: u128 = decimal.parse().unwrap_or(0);
    format!("0x{n:0>64x}")
}

fn topic_token(decimal: &str) -> String {
    // The packed LAND ids exceed u128: rebuild the hex from the decimal digits.
    let mut digits: Vec<u32> = decimal.bytes().map(|b| (b - b'0') as u32).collect();
    let mut hex = Vec::new();
    while !(digits.is_empty() || digits.iter().all(|&d| d == 0)) {
        let mut rem = 0u32;
        for d in digits.iter_mut() {
            let v = rem * 10 + *d;
            *d = v / 16;
            rem = v % 16;
        }
        hex.push(std::char::from_digit(rem, 16).unwrap());
        while digits.first() == Some(&0) {
            digits.remove(0);
        }
    }
    let body: String = hex.iter().rev().collect();
    format!("0x{body:0>64}")
}

struct Log {
    block: u64,
    index: u32,
    address: &'static str,
    topics: Vec<String>,
    data: String,
}

impl Log {
    fn json(&self) -> Value {
        json!({
            "address": self.address,
            "topics": self.topics,
            "data": self.data,
            "blockNumber": format!("0x{:x}", self.block),
            "logIndex": format!("0x{:x}", self.index),
            "blockTimestamp": format!("0x{:x}", 1_500_000_000 + self.block * 12),
            "transactionHash": "0x00", "transactionIndex": "0x0", "blockHash": format!("0x{:064x}", self.block), "removed": false
        })
    }
}

fn transfer(
    block: u64,
    index: u32,
    address: &'static str,
    from: &str,
    to: &str,
    token: &str,
) -> Log {
    let topic0 = if address == LAND {
        TOPIC_TRANSFER_5
    } else {
        TOPIC_TRANSFER_3
    };
    Log {
        block,
        index,
        address,
        topics: vec![
            topic0.into(),
            topic_addr(from),
            topic_addr(to),
            topic_token(token),
        ],
        data: "0x".into(),
    }
}

fn update_operator(
    block: u64,
    index: u32,
    address: &'static str,
    token: &str,
    operator: &str,
) -> Log {
    Log {
        block,
        index,
        address,
        topics: vec![
            TOPIC_UPDATE_OPERATOR.into(),
            topic_token(token),
            topic_addr(operator),
        ],
        data: "0x".into(),
    }
}

fn account_grant(
    block: u64,
    index: u32,
    address: &'static str,
    topic0: &str,
    owner: &str,
    operator: &str,
    approved: bool,
) -> Log {
    let data = format!("0x{:0>64}", if approved { "1" } else { "0" });
    let mut topics = vec![topic0.to_string(), topic_addr(owner), topic_addr(operator)];
    if topic0 == TOPIC_UPDATE_MANAGER {
        topics.push(topic_addr(owner));
    }
    Log {
        block,
        index,
        address,
        topics,
        data,
    }
}

fn add_land(block: u64, index: u32, estate: &str, land: &str) -> Log {
    Log {
        block,
        index,
        address: ESTATE,
        topics: vec![TOPIC_ADD_LAND.into(), topic_uint(estate), topic_token(land)],
        data: "0x".into(),
    }
}

/// A JSON-RPC endpoint with a fixed log set: `eth_blockNumber` answers
/// `head` (moving by `head_step` per call), `eth_getLogs` refuses a range
/// holding more than `cap` logs with the public endpoints' hint (the range
/// that fits) and a range wider than `max_span` blocks without a hint.
struct FakeChain {
    head: AtomicU64,
    head_step: AtomicU64,
    logs: Mutex<Vec<Value>>,
    cap: usize,
    max_span: u64,
    calls: Mutex<Vec<(u64, u64)>>,
    requests: AtomicUsize,
    header_time: AtomicU64,
    headers_missing: AtomicBool,
    batch_limit: AtomicUsize,
    reject_batches: AtomicBool,
    batch_reverse: AtomicBool,
    batch_omit: AtomicU64,
    logs_reverse: AtomicBool,
    header_block_from: AtomicU64,
    header_started: tokio::sync::Notify,
    header_release: tokio::sync::Notify,
}

impl FakeChain {
    fn new(head: u64, logs: Vec<Log>, cap: usize, max_span: u64) -> Arc<Self> {
        Arc::new(Self {
            head: AtomicU64::new(head),
            head_step: AtomicU64::new(0),
            logs: Mutex::new(logs.iter().map(Log::json).collect()),
            cap,
            max_span,
            calls: Mutex::new(Vec::new()),
            requests: AtomicUsize::new(0),
            header_time: AtomicU64::new(u64::MAX),
            headers_missing: AtomicBool::new(false),
            batch_limit: AtomicUsize::new(usize::MAX),
            reject_batches: AtomicBool::new(false),
            batch_reverse: AtomicBool::new(false),
            batch_omit: AtomicU64::new(u64::MAX),
            logs_reverse: AtomicBool::new(false),
            header_block_from: AtomicU64::new(u64::MAX),
            header_started: tokio::sync::Notify::new(),
            header_release: tokio::sync::Notify::new(),
        })
    }

    fn push(&self, log: Log) {
        self.logs.lock().push(log.json());
    }

    fn answered_logs(&self) -> Vec<(u64, u64)> {
        self.calls.lock().clone()
    }
}

fn block_of(v: &Value) -> u64 {
    u64::from_str_radix(
        v["blockNumber"].as_str().unwrap().trim_start_matches("0x"),
        16,
    )
    .unwrap()
}

async fn rpc(
    State(chain): State<Arc<FakeChain>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    chain.requests.fetch_add(1, Ordering::SeqCst);
    if let Some(batch) = body.as_array() {
        if batch.iter().any(|request| {
            hex_to_i64(request["params"][0].as_str().unwrap()).unwrap() as u64
                >= chain.header_block_from.load(Ordering::SeqCst)
        }) {
            chain.header_started.notify_one();
            chain.header_release.notified().await;
        }
        if chain.reject_batches.load(Ordering::SeqCst) {
            return (StatusCode::BAD_REQUEST, "batch unsupported").into_response();
        }
        if chain.headers_missing.load(Ordering::SeqCst) {
            return axum::Json(json!([])).into_response();
        }
        let mut replies: Vec<Value> = batch
            .iter()
            .filter(|request| hex_to_i64(request["params"][0].as_str().unwrap()).unwrap() as u64 != chain.batch_omit.load(Ordering::SeqCst))
            .take(chain.batch_limit.load(Ordering::SeqCst))
            .map(|request| {
                let number = hex_to_i64(request["params"][0].as_str().unwrap()).unwrap();
                json!({"jsonrpc": "2.0", "id": request["id"], "result": {
                    "number": format!("0x{number:x}"), "hash": format!("0x{:064x}", number),
                    "timestamp": format!("0x{:x}", if chain.header_time.load(Ordering::SeqCst) == u64::MAX { 1_500_000_000 + number as u64 * 12 } else { chain.header_time.load(Ordering::SeqCst) })
                }})
            })
            .collect();
        if chain.batch_reverse.load(Ordering::SeqCst) {
            replies.reverse();
        }
        return axum::Json(replies).into_response();
    }
    let reply =
        |result: Value| axum::Json(json!({ "jsonrpc": "2.0", "id": body["id"], "result": result }));
    match body["method"].as_str() {
        Some("eth_getBlockByNumber") => {
            if chain.headers_missing.load(Ordering::SeqCst) {
                return reply(Value::Null).into_response();
            }
            let number = hex_to_i64(body["params"][0].as_str().unwrap()).unwrap();
            if number as u64 >= chain.header_block_from.load(Ordering::SeqCst) {
                chain.header_started.notify_one();
                chain.header_release.notified().await;
            }
            reply(json!({"number": format!("0x{number:x}"), "hash": format!("0x{number:064x}"),
                "timestamp": format!("0x{:x}", if chain.header_time.load(Ordering::SeqCst) == u64::MAX { 1_500_000_000 + number as u64 * 12 } else { chain.header_time.load(Ordering::SeqCst) })
            })).into_response()
        }

        Some("eth_blockNumber") => {
            let head = chain
                .head
                .fetch_add(chain.head_step.load(Ordering::SeqCst), Ordering::SeqCst);
            reply(json!(format!("0x{head:x}"))).into_response()
        }
        Some("eth_getLogs") => {
            let filter = &body["params"][0];
            let from = u64::from_str_radix(
                filter["fromBlock"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("0x"),
                16,
            )
            .unwrap();
            let to = u64::from_str_radix(
                filter["toBlock"].as_str().unwrap().trim_start_matches("0x"),
                16,
            )
            .unwrap();
            assert_eq!(filter["address"], json!([LAND, ESTATE]));
            assert_eq!(
                filter["topics"][0].as_array().unwrap().len(),
                ALL_TOPICS.len()
            );
            if to - from + 1 > chain.max_span {
                return axum::Json(json!({ "jsonrpc": "2.0", "id": body["id"], "error": { "code": -32602, "message": "block range too wide" } }))
                    .into_response();
            }
            let logs = chain.logs.lock();
            let mut hits: Vec<&Value> = logs
                .iter()
                .filter(|l| (from..=to).contains(&block_of(l)))
                .collect();
            hits.sort_by_key(|l| (block_of(l), l["logIndex"].as_str().unwrap().to_string()));
            if hits.len() > chain.cap {
                let hi = block_of(hits[chain.cap]).saturating_sub(1).max(from);
                let message = format!(
                    "Log response size exceeded. You can make eth_getLogs requests with up to a 2K block range and no limit on the response size, or you can request any block range with a cap of 10K logs in the response. Based on your parameters and the response size limit, this block range should work: [0x{from:x}, 0x{hi:x}]"
                );
                return axum::Json(json!({ "jsonrpc": "2.0", "id": body["id"], "error": { "code": -32602, "message": message } })).into_response();
            }
            chain.calls.lock().push((from, to));
            if chain.logs_reverse.load(Ordering::SeqCst) {
                hits.reverse();
            }
            reply(Value::Array(hits.into_iter().cloned().collect())).into_response()
        }
        other => (
            StatusCode::BAD_REQUEST,
            format!("unexpected method {other:?}"),
        )
            .into_response(),
    }
}

async fn serve(chain: Arc<FakeChain>) -> String {
    let app = Router::new().route("/", post(rpc)).with_state(chain);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/")
}

async fn get_json(app: &Router, path: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_json(
    app: &Router,
    path: &str,
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn event_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM land_authz.authz_event")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The fixture chain: the LAND (37,55) minted to ALICE with BOB as update
/// operator and CARL approved-for-all on LAND; estate 500 minted to ALICE
/// holding LAND (2,2) with DANA as update manager on the Estate registry;
/// 40 filler mints so a 12-log cap forces the range hints.
fn fixture_chain() -> (Arc<FakeChain>, usize) {
    let s = START;
    let mut logs = vec![
        transfer(s + 10, 0, LAND, ZERO_ADDR, ALICE, TOKEN_37_55),
        update_operator(s + 20, 0, LAND, TOKEN_37_55, BOB),
        account_grant(s + 30, 0, LAND, TOPIC_APPROVAL_FOR_ALL, ALICE, CARL, true),
        transfer(s + 40, 0, ESTATE, ZERO_ADDR, ALICE, "500"),
        transfer(s + 40, 1, LAND, ZERO_ADDR, ESTATE, &land_token(2, 2)),
        add_land(s + 40, 2, "500", &land_token(2, 2)),
        account_grant(s + 50, 0, ESTATE, TOPIC_UPDATE_MANAGER, ALICE, DANA, true),
    ];
    for i in 0..40u64 {
        logs.push(transfer(
            s + 100 + i * 10,
            0,
            LAND,
            ZERO_ADDR,
            OWNER,
            &land_token(10 + i as i32, -10),
        ));
    }
    let n = logs.len();
    (FakeChain::new(s + 1_000, logs, 12, 1_000_000), n)
}

#[tokio::test]
async fn the_indexer_catches_up_under_a_budget_resumes_after_a_kill_and_publishes_within_the_head_lag()
 {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let (chain, total_logs) = fixture_chain();
    let url = serve(chain.clone()).await;
    let settings = |budget: usize| Settings {
        rpc_url: Some(url.clone()),
        index_enabled: true,
        requests_per_pass: budget,
        ..Settings::for_tests()
    };
    let (_jobs, ctx) = job_ctx().await;

    // Pass 1, budget 2: the first request is refused with a hint (12 logs
    // fit), so one answered range lands, the cursor is durable, nothing is
    // published, and every reader is on the fallback.
    let s1 = state_over(db.pool.clone(), settings(2), None);
    let app1 = router(s1.clone());
    let out = indexer::run_pass(&s1, &ctx).await.unwrap();
    assert!(
        out.paused && out.requests == 2 && out.folded.is_none() && !out.published,
        "{out:?}"
    );
    let row = cursor(&db.pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(row.last_block, out.indexed_to);
    assert!(
        (START as i64..(START + 1_000) as i64).contains(&row.last_block),
        "{row:?}"
    );
    assert_eq!(chain.answered_logs().len(), 1);
    assert!(row.folded_block.is_none() && row.full_replay_started);
    let written_after_pass_1 = event_count(&db.pool).await;
    assert_eq!(written_after_pass_1, out.events_written as i64);
    assert!(written_after_pass_1 > 0);
    let (status, _, body) = get_json(&app1, "/lambdas/parcels/37/55/operators").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        (body["source"].as_str(), body["configure"].as_str()),
        (Some("land-authz-index"), Some("LAND_AUTHZ_INDEX_ENABLED"))
    );
    let (status, _, body) = get_json(&app1, "/lambdas/land-authz/status").await;
    assert_eq!(
        (status, body["ready"].as_bool()),
        (StatusCode::OK, Some(false))
    );
    assert_eq!(body["cursor"]["lastBlock"].as_i64(), Some(row.last_block));
    assert!(matches!(
        can_deploy(&s1, (37, 55), ALICE).await,
        Err(Unavailable::NoSource)
    ));

    // "kill -9": a new process (new state) resumes at the persisted cursor
    // and, with budget to spare, reaches the head, folds and publishes.
    let s2 = state_over(db.pool.clone(), settings(50), None);
    let out = indexer::run_pass(&s2, &ctx).await.unwrap();
    assert!(!out.paused && out.published, "{out:?}");
    assert_eq!(out.indexed_to, (START + 1_000) as i64);
    let counts = out.folded.unwrap();
    assert_eq!(
        (counts.tokens, counts.accounts, counts.estate_lands),
        (43, 2, 1)
    );
    assert_eq!(
        event_count(&db.pool).await,
        total_logs as i64,
        "every log once, no duplicates"
    );
    let ranges = chain.answered_logs();
    assert_eq!(ranges[0].0, START);
    assert_eq!(
        ranges[1].0,
        ranges[0].1 + 1,
        "the second process resumed right after the persisted cursor"
    );
    for w in ranges.windows(2) {
        assert_eq!(w[1].0, w[0].1 + 1, "contiguous ranges: {ranges:?}");
    }
    assert_eq!(ranges.last().unwrap().1, START + 1_000);
    let row = cursor(&db.pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(row.folded_block, Some(row.last_block));
    assert!(index_ready(&row, Utc::now()));

    // The readers now use the index.
    let app = router(s2.clone());
    let (status, _, body) = get_json(&app, "/lambdas/parcels/37/55/operators").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "owner": ALICE, "operator": null, "updateOperator": BOB, "updateManagers": [], "approvedForAll": [CARL] })
    );
    let (status, _, body) = get_json(&app, "/lambdas/parcels/2/2/operators").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "owner": ALICE, "operator": null, "updateOperator": null, "updateManagers": [DANA], "approvedForAll": [] })
    );
    let (status, _, body) = get_json(&app, "/lambdas/parcels/99/99/operators").await;
    assert_eq!(
        (status, body["message"].as_str()),
        (
            StatusCode::NOT_FOUND,
            Some("Parcel or estate rights not found for x: 99, y: 99")
        )
    );
    let (status, _, body) = get_json(&app, "/lambdas/parcels/151/0/operators").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["message"],
        "Coordinates X and Y must be valid numbers in a valid range"
    );
    // Past ±150 but inside an exception block: a parcel lamb2 looks up.
    let (status, _, body) = get_json(&app, "/lambdas/parcels/160/100/operators").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _, body) = get_json(
        &app,
        &format!("/lambdas/users/{BOB}/lands-permissions?pageSize=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totalAmount"], 1);
    assert_eq!(
        body["elements"][0],
        json!({ "id": "0x2500000000000000000000000000000037", "x": "37", "y": "55", "owner": ALICE, "updateOperator": BOB }),
        "the land-manager subgraph's Parcel.id"
    );
    // lamb2's `paginationObject` and `slice`: nothing is clamped, a
    // negative bound counts from the end, `NaN` is answered `null`.
    for (query, page_size, page_num, elements) in [
        ("pageSize=0&pageNum=-1", json!(0), json!(-1), 0),
        ("pageSize=1&pageNum=0", json!(1), json!(0), 0),
        ("pageSize=-1&pageNum=0", json!(-1), json!(0), 0),
        ("pageSize=abc", Value::Null, json!(1), 0),
        ("pageSize=1.9&pageNum=1x", json!(1), json!(1), 1),
    ] {
        let (status, _, body) = get_json(
            &app,
            &format!("/lambdas/users/{BOB}/lands-permissions?{query}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(
            (
                &body["pageSize"],
                &body["pageNum"],
                body["elements"].as_array().unwrap().len(),
                body["totalAmount"].as_i64()
            ),
            (&page_size, &page_num, elements, Some(1)),
            "{query}"
        );
    }
    let (_, _, body) = get_json(&app, &format!("/lambdas/users/{CARL}/lands-permissions")).await;
    assert_eq!(
        (
            body["elements"].as_array().unwrap().len(),
            body["totalAmount"].as_i64()
        ),
        (0, Some(0))
    );

    let (status, headers, body) = get_json(
        &app,
        &format!("/lambdas/users/{BOB}/parcels/37/55/permissions"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[SOURCE_HEADER], "land-authz-index");
    assert_eq!(
        body,
        json!({ "owner": false, "operator": false, "updateOperator": true, "updateManager": false, "approvedForAll": false })
    );
    let (_, _, body) = get_json(
        &app,
        &format!(
            "/lambdas/users/{}/parcels/37/55/permissions",
            ALICE.to_uppercase().replace("0X", "0x")
        ),
    )
    .await;
    assert_eq!(body["owner"], true, "case-insensitive address");
    // `EthAddress.validate` wants a lower-case `0x`.
    let (status, _, body) = get_json(
        &app,
        &format!("/lambdas/users/0X{}/parcels/37/55/permissions", &ALICE[2..]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["message"], "Address must be a valid Ethereum address");
    // `parseInt`: a fraction, a suffix and a hex prefix all read.
    for parcel in ["37.9/55", "37abc/55", "0x25/55", "%2037/+55"] {
        let (status, _, body) = get_json(
            &app,
            &format!("/lambdas/users/{BOB}/parcels/{parcel}/permissions"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{parcel}: {body}");
        assert_eq!(body["updateOperator"], true, "{parcel}");
    }
    let (status, _, _) = get_json(
        &app,
        &format!("/lambdas/users/{DANA}/parcels/99/99/permissions"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for (parcel, expected) in [
        ("62/158", StatusCode::NOT_FOUND),
        ("163/59", StatusCode::NOT_FOUND),
        ("164/100", StatusCode::BAD_REQUEST),
    ] {
        let (status, _, body) = get_json(
            &app,
            &format!("/lambdas/users/{DANA}/parcels/{parcel}/permissions"),
        )
        .await;
        assert_eq!(status, expected, "{parcel}: {body}");
    }
    let (status, _, body) = get_json(&app, "/lambdas/users/nobody/parcels/1/1/permissions").await;
    assert_eq!(
        (status, body["message"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            Some("Address must be a valid Ethereum address")
        )
    );

    let (status, headers, body) = post_json(
        &app,
        &format!("/lambdas/users/{DANA}/parcels/permissions"),
        json!({ "parcels": ["2,2", "37,55", "99,99", "2,2", "151,144"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[SOURCE_HEADER], "land-authz-index");
    assert_eq!(
        body["elements"].as_array().unwrap().len(),
        4,
        "duplicates collapse"
    );
    assert_eq!(body["elements"][0]["permissions"]["updateManager"], true);
    assert_eq!(
        body["elements"][1]["permissions"],
        json!({ "owner": false, "operator": false, "updateOperator": false, "updateManager": false, "approvedForAll": false })
    );
    assert_eq!(body["elements"][2]["permissions"], Value::Null);
    assert_eq!(
        (
            &body["elements"][3]["x"],
            &body["elements"][3]["permissions"]
        ),
        (&json!(151), &Value::Null),
        "an exception-block parcel is looked up, not refused"
    );
    let (status, _, body) = post_json(
        &app,
        &format!("/lambdas/users/{DANA}/parcels/permissions"),
        json!({ "parcels": ["2,2", "x,1"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .starts_with("Invalid parcel \"x,1\"")
    );

    // can_deploy: the write paths' decision, every leg from the index.
    let d = can_deploy(&s2, (37, 55), BOB).await.unwrap();
    assert_eq!(d.verdict, Verdict::Granted(LandRightLeg::UpdateOperator));
    assert_eq!(
        d.source,
        DecisionSource::Index {
            folded_block: (START + 1_000) as i64
        }
    );
    assert_eq!(
        can_deploy(&s2, (37, 55), CARL).await.unwrap().verdict,
        Verdict::Granted(LandRightLeg::ApprovedForAll)
    );
    assert_eq!(
        can_deploy(&s2, (2, 2), DANA).await.unwrap().verdict,
        Verdict::Granted(LandRightLeg::UpdateManager)
    );
    assert_eq!(
        can_deploy(&s2, (2, 2), ALICE).await.unwrap().verdict,
        Verdict::Granted(LandRightLeg::Owner)
    );
    assert_eq!(
        can_deploy(&s2, (2, 2), BOB).await.unwrap().verdict,
        Verdict::Denied
    );
    assert_eq!(
        can_deploy(&s2, (99, 99), ALICE).await.unwrap().verdict,
        Verdict::Unknown
    );
    let batch = can_deploy_batch(&s2, &[(37, 55), (10, -10), (2, 2)], OWNER)
        .await
        .unwrap();
    assert_eq!(
        batch.iter().map(|d| d.verdict.allows()).collect::<Vec<_>>(),
        vec![false, true, false]
    );
    assert!(
        owns_land_at(&s2, ALICE, &[(5, 5), (2, 2)]).await.unwrap(),
        "through the estate"
    );
    assert!(!owns_land_at(&s2, BOB, &[(37, 55)]).await.unwrap());

    // A new event past the fold: the next pass folds incrementally, and
    // publishes only when the head has not run more than 25 blocks ahead.
    chain.push(update_operator(
        START + 1_050,
        0,
        LAND,
        TOKEN_37_55,
        ZERO_ADDR,
    ));
    chain.head.store(START + 1_100, Ordering::SeqCst);
    chain.head_step.store(30, Ordering::SeqCst);
    let out = indexer::run_pass(&s2, &ctx).await.unwrap();
    assert_eq!(
        (out.indexed_to, out.published),
        ((START + 1_100) as i64, false),
        "{out:?}"
    );
    assert_eq!(
        out.folded.unwrap().tokens,
        1,
        "only the touched token was refolded"
    );
    assert_eq!(
        token_leg(&db.pool, TOKEN_37_55, "update_operator")
            .await
            .as_deref(),
        Some(ZERO_ADDR),
        "the cleared leg"
    );
    let row = cursor(&db.pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(
        row.folded_block,
        Some((START + 1_000) as i64),
        "the old marker stays; the cursor moved past it"
    );
    assert!(!index_ready(&row, Utc::now()));
    assert_eq!(
        get_json(&app, "/lambdas/parcels/37/55/operators").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    chain.head_step.store(0, Ordering::SeqCst);
    let out = indexer::run_pass(&s2, &ctx).await.unwrap();
    assert!(out.published, "{out:?}");
    assert!(index_ready(
        &cursor(&db.pool, NETWORK_MAINNET).await.unwrap().unwrap(),
        Utc::now()
    ));
    let (_, _, body) = get_json(&app, "/lambdas/parcels/37/55/operators").await;
    assert_eq!(
        body["updateOperator"], ZERO_ADDR,
        "lamb2 answers the cleared leg"
    );
    assert_eq!(
        can_deploy(&s2, (37, 55), BOB).await.unwrap().verdict,
        Verdict::Denied
    );

    // The freshness window: a stopped indexer falls back after 30 minutes,
    // and a pass with nothing new re-publishes.
    sqlx::query("UPDATE land_authz.index_cursor SET folded_at = folded_at - interval '31 minutes'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        can_deploy(&s2, (37, 55), ALICE).await,
        Err(Unavailable::NoSource)
    ));
    let requests_before = chain.requests.load(Ordering::SeqCst);
    let out = indexer::run_pass(&s2, &ctx).await.unwrap();
    assert!(out.published && out.requests == 0, "{out:?}");
    assert_eq!(
        chain.requests.load(Ordering::SeqCst) - requests_before,
        2,
        "two eth_blockNumber calls, no eth_getLogs"
    );
    assert!(
        can_deploy(&s2, (37, 55), ALICE)
            .await
            .unwrap()
            .verdict
            .allows()
    );

    // A stop block holds the index short of the head: folded, never published.
    let held = state_over(
        db.pool.clone(),
        Settings {
            stop_block: Some(START + 1_250),
            ..settings(50)
        },
        None,
    );
    chain.head.store(START + 1_400, Ordering::SeqCst);
    chain.push(transfer(START + 1_300, 0, LAND, ALICE, BOB, TOKEN_37_55));
    let out = indexer::run_pass(&held, &ctx).await.unwrap();
    assert_eq!(
        (out.indexed_to, out.published),
        ((START + 1_250) as i64, false),
        "{out:?}"
    );
    assert!(!index_ready(
        &cursor(&db.pool, NETWORK_MAINNET).await.unwrap().unwrap(),
        Utc::now()
    ));
    assert_eq!(
        token_leg(&db.pool, TOKEN_37_55, "owner").await.as_deref(),
        Some(ALICE),
        "the transfer past the stop block is not indexed yet"
    );

    // A replay from the start: the marker is cleared at once, the log is
    // re-read without duplicates, and the head is reached and published.
    let replay = state_over(
        db.pool.clone(),
        Settings {
            replay_from_block: Some(START),
            ..settings(50)
        },
        None,
    );
    let out = indexer::run_pass(&replay, &ctx).await.unwrap();
    assert!(out.published, "{out:?}");
    assert_eq!(out.indexed_to, (START + 1_400) as i64);
    assert_eq!(event_count(&db.pool).await, total_logs as i64 + 2);
    assert_eq!(
        token_leg(&db.pool, TOKEN_37_55, "owner").await.as_deref(),
        Some(BOB)
    );
    assert_eq!(
        chain.answered_logs()[ranges.len()..]
            .iter()
            .filter(|r| r.0 == START)
            .count(),
        1,
        "the replay re-read from the start once"
    );
    let again = indexer::run_pass(&replay, &ctx).await.unwrap();
    assert!(
        again.published && again.requests == 0,
        "the replay flag is consumed by the first pass"
    );

    // Shutdown between requests: the pass returns paused with its cursor durable.
    let (jobs2, ctx2) = job_ctx().await;
    chain.head.store(START + 5_000, Ordering::SeqCst);
    for i in 0..30u64 {
        chain.push(transfer(
            START + 2_000 + i * 50,
            0,
            LAND,
            ZERO_ADDR,
            CARL,
            &land_token(-20, i as i32),
        ));
    }
    ctx2.cancel.cancel();
    let out = indexer::run_pass(&replay, &ctx2).await.unwrap();
    assert!(out.paused && out.requests == 0, "{out:?}");
    jobs2.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn a_full_replay_rebinds_a_cursor_indexed_from_other_registries() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let (chain, _) = fixture_chain();
    let url = serve(chain.clone()).await;
    let configured = |replay_from_block: Option<u64>| Settings {
        rpc_url: Some(url.clone()),
        index_enabled: true,
        requests_per_pass: 50,
        replay_from_block,
        ..Settings::for_tests()
    };
    let (_jobs, ctx) = job_ctx().await;

    // A published index of other registries (a testnet stack before a
    // redeploy), holding an estate link the configured estate registry never
    // made: LAND (37, 55) in estate 500, which that registry gives ALICE.
    const OLD_LAND: &str = "0x00000000000000000000000000000000000000aa";
    const OLD_ESTATE: &str = "0x00000000000000000000000000000000000000bb";
    let old = Settings {
        land_registry: OLD_LAND.into(),
        estate_registry: OLD_ESTATE.into(),
        ..configured(None)
    };
    let old_last = (START + 5_000) as i64;
    indexer::ensure_cursor(pool, &old).await.unwrap();
    sqlx::query(
        "UPDATE land_authz.index_cursor SET last_block = $2, folded_block = $2, \
         folded_at = timezone('UTC', now()), history_timestamps_ready = true WHERE network = $1",
    )
    .bind(NETWORK_MAINNET)
    .bind(old_last)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO land_authz.estate_land (land_token_id, estate_token_id, updated_block, updated_log) \
         VALUES ($1::numeric, 500, $2, 0)",
    )
    .bind(TOKEN_37_55)
    .bind(old_last - 1)
    .execute(pool)
    .await
    .unwrap();
    // A pass of the old registries synced that link's event and stopped
    // before its fold (it resumes below, after the rebind).
    insert(
        pool,
        &[AuthzEvent {
            token_address: OLD_ESTATE.to_string(),
            ..estate_ev(old_last - 1, 0, KIND_ADD_LAND, "500", TOKEN_37_55)
        }],
    )
    .await;
    let links = || async {
        sqlx::query_as::<_, (String, String)>(
            "SELECT land_token_id::text, estate_token_id::text FROM land_authz.estate_land ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    };
    let kept = |why: &'static str| async move {
        let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
        assert_eq!(
            (
                row.land_registry.as_str(),
                row.estate_registry.as_str(),
                row.last_block
            ),
            (OLD_LAND, OLD_ESTATE, old_last),
            "{why}"
        );
        assert_eq!(links().await.len(), 1, "{why}");
    };

    // Without a replay, or with a partial one, the configured registries are
    // refused with the remedy named, and nothing is rewound or consumed.
    for replay_from_block in [None, Some(START + 100)] {
        let state = state_over(pool.clone(), configured(replay_from_block), None);
        let err = format!("{:#}", indexer::run_pass(&state, &ctx).await.unwrap_err());
        assert!(
            err.contains(&format!("LAND_AUTHZ_REPLAY_FROM_BLOCK={START}")),
            "{err}"
        );
        assert!(!state.replay_requested.load(Ordering::SeqCst));
        kept("a refused pass rewinds nothing").await;
    }

    // A full replay whose rewind fails is not consumed: the same state
    // retries it, and the failed write changed nothing.
    let full = state_over(pool.clone(), configured(Some(START)), None);
    sqlx::query(
        "CREATE FUNCTION land_authz.refuse_cursor_update() RETURNS trigger LANGUAGE plpgsql \
         AS $$ BEGIN RAISE EXCEPTION 'cursor write refused by the test'; END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_cursor_update BEFORE UPDATE ON land_authz.index_cursor \
         FOR EACH ROW EXECUTE FUNCTION land_authz.refuse_cursor_update()",
    )
    .execute(pool)
    .await
    .unwrap();
    let err = format!("{:#}", indexer::run_pass(&full, &ctx).await.unwrap_err());
    assert!(err.contains("cursor write refused by the test"), "{err}");
    assert!(!full.replay_requested.load(Ordering::SeqCst));
    kept("the failed rewind changed nothing").await;
    sqlx::query("DROP TRIGGER refuse_cursor_update ON land_authz.index_cursor")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION land_authz.refuse_cursor_update()")
        .execute(pool)
        .await
        .unwrap();

    // The retry rebinds the cursor to the configured registries, re-reads
    // their log from the start and publishes; the old estate link is gone
    // and only the configured estate registry's own link is folded.
    let out = indexer::run_pass(&full, &ctx).await.unwrap();
    assert!(out.published, "{out:?}");
    assert!(full.replay_requested.load(Ordering::SeqCst));
    let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(
        (
            row.land_registry.as_str(),
            row.estate_registry.as_str(),
            row.last_block
        ),
        (LAND, ESTATE, (START + 1_000) as i64)
    );
    assert_eq!(links().await, vec![(land_token(2, 2), "500".to_string())]);
    assert_eq!(
        token_leg(pool, TOKEN_37_55, "owner").await.as_deref(),
        Some(ALICE)
    );
    // The decisions at LAND (37, 55), which ALICE holds directly; DANA
    // manages ALICE's estates, not her LAND. A stale link from it into estate
    // 500 would turn both into Unknown, locking the owner out.
    let at_37_55 = || async {
        let mut verdicts = Vec::new();
        for actor in [ALICE, DANA] {
            let decision = can_deploy(&full, (37, 55), actor).await.unwrap();
            assert!(
                matches!(decision.source, DecisionSource::Index { .. }),
                "{:?}",
                decision.source
            );
            verdicts.push(decision.verdict);
        }
        verdicts
    };
    let decisions = vec![Verdict::Granted(LandRightLeg::Owner), Verdict::Denied];
    assert_eq!(at_37_55().await, decisions);

    // The old pass resumes its fold: the cursor no longer names its
    // registries, so it writes nothing, publishes nothing, and the new
    // authorization stands.
    let stale = fold(pool, &old, None).await;
    assert_eq!(links().await, vec![(land_token(2, 2), "500".to_string())]);
    assert_eq!(at_37_55().await, decisions);
    assert!(matches!(stale, Err(sqlx::Error::RowNotFound)), "{stale:?}");
    assert!(!mark_folded(pool, &old, old_last as u64).await.unwrap());
    let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(row.folded_block, Some((START + 1_000) as i64));

    // A node still configured with the old registries (a rolling deploy)
    // never reads the published index as its own: its rights rows are of
    // the old registries, the estate links of the new one. It falls back
    // (here there is no fallback reader), and its status shows why.
    assert!(full.ready().await.is_some());
    let stale_reader = state_over(pool.clone(), old.clone(), None);
    assert!(stale_reader.ready().await.is_none());
    let answer = can_deploy(&stale_reader, (37, 55), ALICE).await;
    assert!(matches!(answer, Err(Unavailable::NoSource)), "{answer:?}");
    let (_, _, body) = get_json(&router(stale_reader), "/lambdas/land-authz/status").await;
    assert_eq!(body["ready"], false);
    assert_eq!(
        (
            body["landRegistry"].as_str(),
            body["cursor"]["landRegistry"].as_str(),
            body["cursor"]["estateRegistry"].as_str(),
        ),
        (Some(OLD_LAND), Some(LAND), Some(ESTATE))
    );
    let (_, _, body) = get_json(&router(full.clone()), "/lambdas/land-authz/status").await;
    assert_eq!(body["ready"], true);
    db.drop().await;
}

/// The other order: an old registries' fold already running when a full
/// replay rebinds the cursor. Its transaction holds the cursor row, so the
/// rebind waits for it, and the links it wrote are deleted after it.
#[tokio::test]
async fn a_fold_in_flight_holds_the_rebind_until_it_commits_and_its_links_are_dropped() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings::for_tests();
    let old = Settings {
        land_registry: "0x00000000000000000000000000000000000000aa".into(),
        estate_registry: "0x00000000000000000000000000000000000000bb".into(),
        ..Settings::for_tests()
    };
    indexer::ensure_cursor(pool, &old).await.unwrap();
    insert(
        pool,
        &[AuthzEvent {
            token_address: old.estate_registry.clone(),
            ..estate_ev((START + 10) as i64, 0, KIND_ADD_LAND, "500", TOKEN_37_55)
        }],
    )
    .await;
    let links = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM land_authz.estate_land")
            .fetch_one(pool)
            .await
            .unwrap()
    };

    let mut folding = pool.begin().await.unwrap();
    let fold_pid = backend_pid(&mut folding).await;
    let counts = indexer::fold_in_tx(&mut folding, &old, None).await.unwrap();
    assert_eq!(counts.estate_lands, 1);
    // The cursor's ordinary writers do not wait on a running fold (one left
    // by a lost lease can run for many minutes).
    let advanced = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut conn = pool.acquire().await.unwrap();
        set_cursor(&mut conn, &old, (START + 20) as i64).await
    })
    .await;
    assert!(
        matches!(advanced, Ok(Ok(()))),
        "set_cursor waited on the fold: {advanced:?}"
    );
    let rebind = {
        let (pool, configured) = (pool.clone(), configured.clone());
        tokio::spawn(async move { begin_replay(&pool, &configured, START).await })
    };
    wait_until_blocked_by(pool, fold_pid).await;
    assert!(!rebind.is_finished(), "the rebind waits for the fold");
    folding.commit().await.unwrap();
    assert_eq!(rebind.await.unwrap().unwrap(), ReplayStart::Begun);

    // The rebind's deletion landed after the old link, and the old
    // registries can no longer fold.
    assert_eq!(links().await, 0);
    let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(
        (row.land_registry.as_str(), row.estate_registry.as_str()),
        (LAND, ESTATE)
    );
    let stale = fold(pool, &old, None).await;
    assert_eq!(links().await, 0);
    assert!(matches!(stale, Err(sqlx::Error::RowNotFound)), "{stale:?}");
    db.drop().await;
}

/// A request of a node still configured with the old registries that
/// checked readiness before a rebind, then stalled while the new registries
/// were rebound, folded and published, answers from the old registries'
/// coherent state when it resumes, never from their rights joined to the
/// new registries' estate links.
#[tokio::test]
async fn a_reader_that_checked_before_a_rebind_answers_from_its_registries_alone() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let (chain, _) = fixture_chain();
    let url = serve(chain.clone()).await;
    let configured = Settings {
        rpc_url: Some(url.clone()),
        index_enabled: true,
        requests_per_pass: 50,
        replay_from_block: Some(START),
        ..Settings::for_tests()
    };
    let old = Settings {
        land_registry: "0x00000000000000000000000000000000000000aa".into(),
        estate_registry: "0x00000000000000000000000000000000000000bb".into(),
        ..Settings::for_tests()
    };
    let (_jobs, ctx) = job_ctx().await;

    // The old registries' published index: CARL holds LAND (2, 2) directly,
    // and BOB holds their estate 500, which holds no LAND.
    indexer::ensure_cursor(pool, &old).await.unwrap();
    let old_block = (START + 10) as i64;
    insert(
        pool,
        &[
            AuthzEvent {
                token_address: old.land_registry.clone(),
                ..ev(
                    old_block - 2,
                    0,
                    KIND_TRANSFER,
                    Some(&land_token(2, 2)),
                    Some(CARL),
                    None,
                    None,
                )
            },
            AuthzEvent {
                token_address: old.estate_registry.clone(),
                ..ev(
                    old_block - 1,
                    0,
                    KIND_TRANSFER,
                    Some("500"),
                    Some(BOB),
                    None,
                    None,
                )
            },
        ],
    )
    .await;
    fold(pool, &old, None).await.unwrap();
    sqlx::query(
        "UPDATE land_authz.index_cursor SET last_block = $2, folded_block = $2, \
         folded_at = timezone('UTC', now()) WHERE network = $1",
    )
    .bind(NETWORK_MAINNET)
    .bind(old_block)
    .execute(pool)
    .await
    .unwrap();

    // The old node's request passes readiness and stalls there.
    let stale = state_over(pool.clone(), old.clone(), None);
    let mut held = stale
        .ready_snapshot()
        .await
        .expect("the old registries' index is published");

    // Meanwhile the configured node rebinds, folds its registries (its
    // estate 500 holds LAND (2, 2)) and publishes.
    let full = state_over(pool.clone(), configured, None);
    let out = indexer::run_pass(&full, &ctx).await.unwrap();
    assert!(out.published, "{out:?}");

    // The stalled request resumes on its snapshot: CARL holds the LAND
    // directly, as the old registries say, and BOB holds nothing on it.
    let rights = stale
        .store()
        .deploy_rights_batch_on(&mut held.tx, &[(2, 2)])
        .await
        .unwrap()
        .pop()
        .flatten()
        .expect("the old registries' LAND (2, 2)");
    assert_eq!(
        (
            rights.subject.belongs_to_estate,
            rights.subject.owner.as_str()
        ),
        (false, CARL)
    );
    assert_eq!(
        rights.deploy_flags_for(CARL).granting_leg(),
        Some(LandRightLeg::Owner)
    );
    assert_eq!(rights.deploy_flags_for(BOB).granting_leg(), None);
    drop(held);

    // A new request of the old node is not ready, and the configured node
    // answers from its own registries.
    let answer = can_deploy(&stale, (2, 2), BOB).await;
    assert!(matches!(answer, Err(Unavailable::NoSource)), "{answer:?}");
    assert_eq!(
        can_deploy(&full, (2, 2), ALICE).await.unwrap().verdict,
        Verdict::Granted(LandRightLeg::Owner)
    );
    db.drop().await;
}

#[tokio::test]
async fn a_registry_switch_starts_the_new_log_at_its_start_and_a_partial_rewind_never_crosses_one()
{
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings::for_tests();
    let other = Settings {
        land_registry: "0x00000000000000000000000000000000000000aa".into(),
        estate_registry: "0x00000000000000000000000000000000000000bb".into(),
        ..Settings::for_tests()
    };
    let base = START as i64;
    let row = || async { cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap() };
    let timestamps_ready = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT history_timestamps_ready FROM land_authz.index_cursor WHERE network = $1",
        )
        .bind(NETWORK_MAINNET)
        .fetch_one(pool)
        .await
        .unwrap()
    };

    // Another registries' cursor standing BEFORE the configured start: a
    // switch still starts the new log exactly at the start, owes the
    // timestamps again and drops the estate links.
    indexer::ensure_cursor(pool, &other).await.unwrap();
    sqlx::query(
        "UPDATE land_authz.index_cursor SET last_block = $2, history_timestamps_ready = true WHERE network = $1",
    )
    .bind(NETWORK_MAINNET)
    .bind(base - 50)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO land_authz.estate_land (land_token_id, estate_token_id, updated_block, updated_log) \
         VALUES ($1::numeric, 7, 1, 0)",
    )
    .bind(TOKEN_37_55)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(
        begin_replay(pool, &configured, START).await.unwrap(),
        ReplayStart::Begun
    );
    let r = row().await;
    assert_eq!(
        (
            r.land_registry.as_str(),
            r.estate_registry.as_str(),
            r.last_block,
            r.folded_block
        ),
        (LAND, ESTATE, base - 1, None)
    );
    assert!(r.full_replay_started);
    assert!(
        !timestamps_ready().await,
        "the new registries' timestamps are owed"
    );
    let estate_links: i64 = sqlx::query_scalar("SELECT count(*) FROM land_authz.estate_land")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(estate_links, 0);

    // On the same registries a rewind never moves the cursor forward, and the
    // timestamps already repaired stay so.
    sqlx::query("UPDATE land_authz.index_cursor SET history_timestamps_ready = true")
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        begin_replay(pool, &configured, START + 95).await.unwrap(),
        ReplayStart::Begun
    );
    assert_eq!(
        row().await.last_block,
        base - 1,
        "a later rewind skips no block"
    );
    set(pool, base + 500).await;
    assert_eq!(
        begin_replay(pool, &configured, START + 95).await.unwrap(),
        ReplayStart::Begun
    );
    assert_eq!(
        row().await.last_block,
        base + 94,
        "a rewind goes back to the block asked"
    );
    assert!(timestamps_ready().await);

    // A partial rewind for other registries writes nothing.
    assert_eq!(
        begin_replay(pool, &other, START + 95).await.unwrap(),
        ReplayStart::Refused
    );
    let r = row().await;
    assert_eq!((r.land_registry.as_str(), r.last_block), (LAND, base + 94));

    // Nor can a pass for other registries advance or publish this cursor.
    let mut conn = pool.acquire().await.unwrap();
    assert!(matches!(
        set_cursor(&mut conn, &other, base + 900).await,
        Err(sqlx::Error::RowNotFound)
    ));
    drop(conn);
    assert!(!mark_folded(pool, &other, (base + 94) as u64).await.unwrap());
    assert_eq!(row().await.last_block, base + 94);
    db.drop().await;
}

async fn serve_marketplace_subgraph(nfts: Value) -> String {
    async fn graphql(State(nfts): State<Arc<Value>>) -> axum::Json<Value> {
        axum::Json(json!({ "data": { "nfts": *nfts } }))
    }
    let app = Router::new()
        .route("/", post(graphql))
        .with_state(Arc::new(nfts));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/")
}

#[tokio::test]
async fn before_the_index_is_ready_the_marketplace_reader_answers_the_owner_leg_only() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let subgraph = serve_marketplace_subgraph(json!([
        { "id": "0xland-1", "searchParcelX": "1", "searchParcelY": "1", "owner": { "id": ALICE }, "parcel": { "estate": null } },
        { "id": "0xland-2", "searchParcelX": "2", "searchParcelY": "2", "owner": { "id": ESTATE }, "parcel": { "estate": { "owner": { "id": BOB } } } }
    ]))
    .await;
    let market = Arc::new(crate::chain_index::ChainIndex::new(
        crate::http::Outbound::for_tests(),
        crate::chain_index::IndexSettings {
            marketplace_subgraph_url: Some(subgraph),
            ..crate::chain_index::IndexSettings::for_tests()
        },
    ));
    market.allow_loopback_for_tests();
    let state = state_over(db.pool.clone(), Settings::for_tests(), Some(market.clone()));
    let app = router(state.clone());

    let d = can_deploy(&state, (1, 1), ALICE).await.unwrap();
    assert_eq!(
        (d.verdict, d.source),
        (
            Verdict::Granted(LandRightLeg::Owner),
            DecisionSource::SubgraphOwnerOnly
        )
    );
    assert_eq!(
        can_deploy(&state, (2, 2), BOB).await.unwrap().verdict,
        Verdict::Granted(LandRightLeg::Owner),
        "the estate's owner"
    );
    assert_eq!(
        can_deploy(&state, (1, 1), CARL).await.unwrap().verdict,
        Verdict::Denied,
        "an operator leg cannot be granted without the index"
    );
    assert_eq!(
        can_deploy(&state, (3, 3), ALICE).await.unwrap().verdict,
        Verdict::Unknown
    );
    assert!(owns_land_at(&state, BOB, &[(2, 2)]).await.unwrap());
    assert!(!owns_land_at(&state, CARL, &[(1, 1), (2, 2)]).await.unwrap());

    // The single route answers five booleans or nothing: owner-only is not
    // an answer, and without the LAND subgraph it is lamb2's bare 500.
    let (status, _, body) = get_json(
        &app,
        &format!("/lambdas/users/{ALICE}/parcels/1/1/permissions"),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "Internal Server Error" })
        )
    );
    let (status, _, body) = post_json(
        &app,
        &format!("/lambdas/users/{BOB}/parcels/permissions"),
        json!({ "parcels": ["2,2", "3,3"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            body["elements"][0]["permissions"]["owner"].as_bool(),
            body["elements"][1]["permissions"].is_null()
        ),
        (Some(true), true)
    );
    assert!(
        body["elements"][0]["permissions"]["updateOperator"].is_null()
            && body["elements"][0]["permissions"]["approvedForAll"].is_null(),
        "{body}"
    );
    let (status, _, body) = get_json(&app, "/lambdas/parcels/1/1/operators").await;
    assert_eq!(
        (status, body["source"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("land-authz-index")),
        "operators are never owner-only"
    );
    assert_eq!(
        get_json(&app, &format!("/lambdas/users/{ALICE}/lands-permissions"))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let (_, _, body) = get_json(&app, "/lambdas/land-authz/status").await;
    assert_eq!(
        (
            body["ready"].as_bool(),
            body["fallback"].as_str(),
            body["indexerEnabled"].as_bool()
        ),
        (
            Some(false),
            Some("marketplace-subgraph:owner-only"),
            Some(false)
        )
    );
    assert!(body["cursor"].is_null());

    // A reader attached without a subgraph URL is no source at all.
    let bare = Arc::new(crate::chain_index::ChainIndex::new(
        crate::http::Outbound::for_tests(),
        crate::chain_index::IndexSettings::for_tests(),
    ));
    let none = state_over(db.pool.clone(), Settings::for_tests(), Some(bare));
    assert!(matches!(
        can_deploy(&none, (1, 1), ALICE).await,
        Err(Unavailable::NoSource)
    ));
    let (status, _, body) = post_json(
        &router(none),
        &format!("/lambdas/users/{ALICE}/parcels/permissions"),
        json!({ "parcels": ["1,1"] }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("no marketplace subgraph is configured"),
        "{body}"
    );
    db.drop().await;
}

#[tokio::test]
async fn spawn_jobs_attaches_the_indexer_only_when_enabled_with_an_rpc_url() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1:1/never")
        .unwrap();
    let jobs = Jobs::new();
    state_over(pool.clone(), Settings::for_tests(), None).spawn_jobs(&jobs);
    state_over(
        pool.clone(),
        Settings {
            index_enabled: true,
            ..Settings::for_tests()
        },
        None,
    )
    .spawn_jobs(&jobs);
    assert!(jobs.report().is_empty());
    let on = Settings {
        index_enabled: true,
        rpc_url: Some("https://rpc.example/x".into()),
        interval: std::time::Duration::from_secs(3600),
        ..Settings::for_tests()
    };
    state_over(pool, on, None).spawn_jobs(&jobs);
    assert_eq!(
        jobs.report()
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        vec!["land-authz-index"]
    );
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
}

/// The real endpoint, two or three requests: the head and the first blocks
/// of the LAND registry's history decode with this decoder. Runs only with
/// `LAND_AUTHZ_LIVE_RPC_URL` (an https JSON-RPC endpoint to a public host).
#[tokio::test]
async fn live_rpc_reads_and_decodes_the_first_land_logs() {
    let Ok(url) = std::env::var("LAND_AUTHZ_LIVE_RPC_URL") else {
        eprintln!("skipping: LAND_AUTHZ_LIVE_RPC_URL not set");
        return;
    };
    let http = crate::http::Outbound::new(crate::http::HttpClientCfg::default()).unwrap();
    let settings = Settings::for_tests();
    let rpc = Rpc::at(&http, &url);
    let head = rpc.head_block().await.unwrap();
    assert!(head > START + 1_000_000, "head {head}");
    let (logs, reached, requests) = rpc
        .get_logs_matching(
            START,
            START + 20_000,
            &json!([settings.land_registry, settings.estate_registry]),
            &json!([ALL_TOPICS]),
        )
        .await
        .unwrap();
    assert!(
        reached >= START && requests <= 3,
        "reached {reached} in {requests} requests"
    );
    let events = indexer::decode_logs(&logs).unwrap();
    eprintln!(
        "live: {} logs in [{START}, {reached}] -> {} events, {requests} requests",
        logs.len(),
        events.len()
    );
    assert_eq!(
        events.len(),
        logs.len(),
        "every log of the seven topics decodes"
    );
}

/// The grant follows the signed deployment time, across the complete scene
/// footprint. Approval churn is deliberately independent of ownership history.
#[tokio::test]
async fn scene_collaborators_require_known_current_ownership_history() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let settings = Settings::for_tests();
    let entity_time = chrono::DateTime::from_timestamp(Utc::now().timestamp() - 3600, 0)
        .unwrap()
        .naive_utc();
    let cutoff = entity_time.and_utc().timestamp() - 900;
    let parcel = land_token(1, 1);
    let other = land_token(2, 2);
    let mut initial = vec![
        ev(1, 0, KIND_TRANSFER, Some(&parcel), Some(ALICE), None, None),
        ev(1, 1, KIND_TRANSFER, Some(&other), Some(BOB), None, None),
    ];
    for event in &mut initial {
        event.block_time = cutoff - 1;
    }
    insert(pool, &initial).await;
    fold_all(pool).await;
    indexer::ensure_cursor(pool, &settings).await.unwrap();
    sqlx::query("UPDATE land_authz.index_cursor SET last_block = 100, folded_block = 100, folded_at = timezone('UTC', now()), full_replay_started = true, history_timestamps_ready = true")
        .execute(pool).await.unwrap();
    async fn allowed(pool: &PgPool, pointers: &[&str], at: chrono::NaiveDateTime) -> bool {
        sqlx::query_scalar("SELECT land_authz.scene_collaborators_current($1,$2,$3,$4,$5)")
            .bind(NETWORK_MAINNET)
            .bind(LAND)
            .bind(ESTATE)
            .bind(pointers)
            .bind(at)
            .fetch_one(pool)
            .await
            .unwrap()
    }
    assert!(allowed(pool, &["1,1", "2,2"], entity_time).await);
    for pointers in [
        vec![],
        vec!["nope"],
        vec!["1,1", "9,9"],
        vec!["2147483648,1"],
        vec!["01,1"],
    ] {
        assert!(!allowed(pool, &pointers, entity_time).await, "{pointers:?}");
    }
    assert!(
        !allowed(
            pool,
            &["1,1"],
            Utc::now().naive_utc() + chrono::Duration::minutes(1)
        )
        .await
    );
    for update in [
        "folded_at = timezone('UTC', now()) - interval '31 minutes'",
        "folded_at = timezone('UTC', now()) + interval '1 minute'",
        "folded_block = NULL",
        "full_replay_started = false",
        "history_timestamps_ready = false",
        "land_registry = 'wrong'",
        "estate_registry = 'wrong'",
        "network = 'ethereum-sepolia'",
    ] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE land_authz.index_cursor SET {update}"
        )))
        .execute(&mut *tx)
        .await
        .unwrap();
        let grant: bool =
            sqlx::query_scalar("SELECT land_authz.scene_collaborators_current($1,$2,$3,$4,$5)")
                .bind(NETWORK_MAINNET)
                .bind(LAND)
                .bind(ESTATE)
                .bind(vec!["1,1"])
                .bind(entity_time)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert!(!grant, "{update}");
        tx.rollback().await.unwrap();
    }
    let mut approval = ev(2, 0, KIND_APPROVAL, Some(&parcel), None, Some(CARL), None);
    approval.block_time = cutoff + 10;
    insert(pool, &[approval]).await;
    fold_all(pool).await;
    assert!(
        allowed(pool, &["1,1"], entity_time).await,
        "approvals do not revoke"
    );
    let mut away = ev(3, 0, KIND_TRANSFER, Some(&other), Some(CARL), None, None);
    away.block_time = cutoff;
    insert(pool, &[away]).await;
    fold_all(pool).await;
    assert!(
        !allowed(pool, &["1,1", "2,2"], entity_time).await,
        "strict bound on every parcel"
    );
    let mut back = ev(4, 0, KIND_TRANSFER, Some(&other), Some(BOB), None, None);
    back.block_time = cutoff + 1;
    insert(pool, &[back]).await;
    fold_all(pool).await;
    assert!(
        !allowed(pool, &["2,2"], entity_time).await,
        "returning owner cannot revive deployment"
    );
    for bad_time in [0, -1, Utc::now().timestamp() + 60] {
        sqlx::query("UPDATE land_authz.authz_event SET block_time = $1 WHERE block_number = 1 AND log_index = 0")
            .bind(bad_time).execute(pool).await.unwrap();
        assert!(!allowed(pool, &["1,1"], entity_time).await);
    }
    sqlx::query("UPDATE land_authz.authz_event SET block_time = $1 WHERE block_number = 1 AND log_index = 0")
        .bind(cutoff - 1).execute(pool).await.unwrap();
    let mut estate_owner = ev(5, 0, KIND_TRANSFER, Some("42"), Some(ALICE), None, None);
    estate_owner.token_address = ESTATE.into();
    estate_owner.block_time = cutoff - 2;
    let mut membership = estate_ev(5, 1, KIND_ADD_LAND, "42", &parcel);
    membership.block_time = cutoff - 1;
    let mut escrow = ev(5, 2, KIND_TRANSFER, Some(&parcel), Some(ESTATE), None, None);
    escrow.block_time = cutoff - 1;
    insert(pool, &[estate_owner, membership, escrow]).await;
    fold_all(pool).await;
    assert!(allowed(pool, &["1,1"], entity_time).await);
    sqlx::query(
        "UPDATE land_authz.token_right SET owner = NULL WHERE token_address = $1 AND token_id = 42",
    )
    .bind(ESTATE)
    .execute(pool)
    .await
    .unwrap();
    assert!(
        !allowed(pool, &["1,1"], entity_time).await,
        "unknown estate owner"
    );
    fold_all(pool).await;
    let mut estate_transfer = ev(6, 0, KIND_TRANSFER, Some("42"), Some(BOB), None, None);
    estate_transfer.token_address = ESTATE.into();
    estate_transfer.block_time = cutoff;
    insert(pool, &[estate_transfer]).await;
    fold_all(pool).await;
    assert!(
        !allowed(pool, &["1,1"], entity_time).await,
        "estate transfer revokes"
    );
    let mut removed = estate_ev(7, 0, KIND_REMOVE_LAND, "42", &parcel);
    removed.block_time = cutoff;
    let mut released = ev(7, 1, KIND_TRANSFER, Some(&parcel), Some(ALICE), None, None);
    released.block_time = cutoff;
    insert(pool, &[removed, released]).await;
    fold_all(pool).await;
    assert!(
        !allowed(pool, &["1,1"], entity_time).await,
        "membership removal revokes"
    );
    assert!(
        allowed(pool, &["1,1"], entity_time + chrono::Duration::seconds(1)).await,
        "a later signed deployment can grant"
    );
    sqlx::query("DELETE FROM land_authz.authz_event WHERE token_address = $1 AND token_id = $2::numeric AND kind = 'transfer'")
        .bind(LAND).bind(&parcel).execute(pool).await.unwrap();
    assert!(
        !allowed(pool, &["1,1"], entity_time).await,
        "missing transfer never grants"
    );
    db.drop().await;
}

#[tokio::test]
async fn indexer_repairs_omitted_and_legacy_zero_timestamps_before_collaborator_eligibility() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let chain = FakeChain::new(
        START + 1,
        vec![transfer(START, 0, LAND, ZERO_ADDR, ALICE, &token)],
        100,
        u64::MAX,
    );
    chain.logs.lock()[0]
        .as_object_mut()
        .unwrap()
        .remove("blockTimestamp");
    let url = serve(chain.clone()).await;
    let mut settings = Settings::for_tests();
    settings.rpc_url = Some(url);
    let state = state_over(db.pool.clone(), settings, None);
    let (jobs, ctx) = job_ctx().await;
    let out = indexer::run_pass(&state, &ctx).await.unwrap();
    assert!(out.published);
    let time: i64 = sqlx::query_scalar("SELECT block_time FROM land_authz.authz_event")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(time, 1_500_000_000 + START as i64 * 12);
    sqlx::query("UPDATE land_authz.authz_event SET block_time = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE land_authz.index_cursor SET history_timestamps_ready = false")
        .execute(&db.pool)
        .await
        .unwrap();
    chain.header_time.store(0, Ordering::SeqCst);
    chain.push(transfer(START + 2, 0, LAND, ALICE, BOB, &token));
    chain.head.store(START + 2, Ordering::SeqCst);
    let continued = indexer::run_pass(&state, &ctx).await.unwrap();
    assert!(
        continued.published && continued.events_written == 1,
        "ordinary indexing continues while historical timestamp repair fails"
    );
    assert_eq!(
        token_leg(&db.pool, &token, "owner").await.as_deref(),
        Some(BOB)
    );
    chain.header_time.store(u64::MAX, Ordering::SeqCst);
    let repair = indexer::run_pass(&state, &ctx).await.unwrap();
    assert!(repair.published);
    let ready: bool =
        sqlx::query_scalar("SELECT history_timestamps_ready FROM land_authz.index_cursor")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        ready,
        "the final repaired batch establishes readiness in the same pass"
    );
    let repaired: i64 = sqlx::query_scalar(
        "SELECT block_time FROM land_authz.authz_event ORDER BY block_number LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(repaired, time);
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().published);
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn timestamp_batches_bound_prefixes_and_minimum_budget_retries_make_progress() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let logs = (0..201)
        .map(|offset| transfer(START + offset, 0, LAND, ZERO_ADDR, ALICE, &token))
        .collect();
    let chain = FakeChain::new(START + 200, logs, 1000, u64::MAX);
    for log in chain.logs.lock().iter_mut() {
        log.as_object_mut().unwrap().remove("blockTimestamp");
    }
    let mut settings = Settings::for_tests();
    settings.rpc_url = Some(serve(chain.clone()).await);
    settings.requests_per_pass = 1; // Normalized to the useful minimum: two.
    let state = state_over(db.pool.clone(), settings, None);
    let (jobs, ctx) = job_ctx().await;
    for expected in [100, 200, 201] {
        let outcome = indexer::run_pass(&state, &ctx).await.unwrap();
        assert_eq!(outcome.requests, 2);
        assert_eq!(event_count(&db.pool).await, expected);
        assert_eq!(outcome.indexed_to, START as i64 + expected - 1);
        assert_eq!(outcome.published, expected == 201);
    }
    // A separate cursor rewind exercises provider range refusals with the
    // minimum budget. Narrowed in-memory retry hints retain useful progress.
    begin_replay(&db.pool, &state.settings, START)
        .await
        .unwrap();
    let refusing = FakeChain::new(
        START + 200,
        vec![transfer(START, 0, LAND, ZERO_ADDR, ALICE, &token)],
        1000,
        10,
    );
    refusing.logs.lock()[0]
        .as_object_mut()
        .unwrap()
        .remove("blockTimestamp");
    let mut retry_settings = state.settings.clone();
    retry_settings.rpc_url = Some(serve(refusing.clone()).await);
    let retry = state_over(db.pool.clone(), retry_settings, None);
    let mut progressed = false;
    for _ in 0..8 {
        let outcome = indexer::run_pass(&retry, &ctx).await.unwrap();
        assert!(outcome.requests <= 2);
        if outcome.indexed_to >= START as i64 {
            progressed = true;
            break;
        }
    }
    assert!(progressed, "refusal hints must survive budget pauses");
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn invalid_header_does_not_advance_the_cursor_or_persist_unknown_times() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let chain = FakeChain::new(
        START,
        vec![transfer(START, 0, LAND, ZERO_ADDR, ALICE, &token)],
        1000,
        u64::MAX,
    );
    chain.logs.lock()[0]
        .as_object_mut()
        .unwrap()
        .remove("blockTimestamp");
    chain.header_time.store(0, Ordering::SeqCst);
    let mut settings = Settings::for_tests();
    settings.rpc_url = Some(serve(chain.clone()).await);
    let state = state_over(db.pool.clone(), settings, None);
    let (jobs, ctx) = job_ctx().await;
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().paused);
    assert_eq!(event_count(&db.pool).await, 0);
    assert_eq!(
        cursor(&db.pool, NETWORK_MAINNET)
            .await
            .unwrap()
            .unwrap()
            .last_block,
        START as i64 - 1
    );
    chain.header_time.store(u64::MAX, Ordering::SeqCst);
    chain.headers_missing.store(true, Ordering::SeqCst);
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().paused);
    assert_eq!(event_count(&db.pool).await, 0);
    chain.headers_missing.store(false, Ordering::SeqCst);
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().published);
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn timestamp_migration_preserves_existing_history_and_requires_repair() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    // Model a stamped database from before the additive timestamp repair gate.
    sqlx::raw_sql("DROP FUNCTION land_authz.scene_collaborators_current(text,text,text,text[],timestamp); DROP INDEX land_authz.authz_event_ownership_idx; DROP INDEX land_authz.authz_event_missing_time_idx; ALTER TABLE land_authz.index_cursor DROP COLUMN history_timestamps_ready; DELETE FROM meta.schema_steps WHERE step = '0011_land_history_timestamps';")
        .execute(&db.pool).await.unwrap();
    let mut event = ev(
        1,
        0,
        KIND_TRANSFER,
        Some(&land_token(1, 1)),
        Some(ALICE),
        None,
        None,
    );
    event.block_time = 0;
    insert(&db.pool, &[event]).await;
    indexer::ensure_cursor(&db.pool, &Settings::for_tests())
        .await
        .unwrap();
    crate::db::schema_migrate::migrate(&db.pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
        .await
        .unwrap();
    let (time, ready): (i64, bool) = sqlx::query_as("SELECT e.block_time, c.history_timestamps_ready FROM land_authz.authz_event e CROSS JOIN land_authz.index_cursor c")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!((time, ready), (0, false));
    let indexes: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_indexes WHERE schemaname = 'land_authz' AND indexname IN ('authz_event_ownership_idx', 'authz_event_missing_time_idx')").fetch_one(&db.pool).await.unwrap();
    assert_eq!(indexes, 2, "new indexes are added by the object pass");
    // Restarting migrate does not replay or discard the legacy history.
    crate::db::schema_migrate::migrate(&db.pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
        .await
        .unwrap();
    assert_eq!(event_count(&db.pool).await, 1);
    db.drop().await;
}

#[tokio::test]
async fn partial_and_rejected_header_batches_resume_with_the_minimum_budget() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let chain = FakeChain::new(
        START + 2,
        (0..3)
            .map(|i| transfer(START + i, 0, LAND, ZERO_ADDR, ALICE, &token))
            .collect(),
        1000,
        u64::MAX,
    );
    for log in chain.logs.lock().iter_mut() {
        log.as_object_mut().unwrap().remove("blockTimestamp");
    }
    chain.batch_limit.store(1, Ordering::SeqCst);
    let mut settings = Settings::for_tests();
    settings.requests_per_pass = 1;
    settings.rpc_url = Some(serve(chain.clone()).await);
    let state = state_over(db.pool.clone(), settings.clone(), None);
    let (jobs, ctx) = job_ctx().await;
    for expected in 1..=3 {
        let out = indexer::run_pass(&state, &ctx).await.unwrap();
        assert!(out.requests <= 2);
        assert_eq!(out.indexed_to, START as i64 + expected - 1);
    }
    // A new endpoint rejects batches entirely. Its first attempt may pause,
    // but the next pass must spend its one header call on a single block.
    let rejected = FakeChain::new(
        START + 3,
        vec![transfer(START + 3, 0, LAND, ALICE, BOB, &token)],
        1000,
        u64::MAX,
    );
    rejected.reject_batches.store(true, Ordering::SeqCst);
    rejected.logs.lock()[0]
        .as_object_mut()
        .unwrap()
        .remove("blockTimestamp");
    settings.rpc_url = Some(serve(rejected).await);
    let resumed = state_over(db.pool.clone(), settings, None);
    assert!(indexer::run_pass(&resumed, &ctx).await.unwrap().paused);
    let out = indexer::run_pass(&resumed, &ctx).await.unwrap();
    assert!(out.published && out.requests <= 2);
    assert_eq!(out.indexed_to, START as i64 + 3);
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn repair_uses_spare_batches_scopes_registries_and_never_starves_live_headers() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let mut history: Vec<AuthzEvent> = (0..205)
        .map(|i| {
            let mut event = ev(
                START as i64 + i,
                0,
                KIND_TRANSFER,
                Some(&token),
                Some(ALICE),
                None,
                None,
            );
            event.block_time = 0;
            event
        })
        .collect();
    let mut foreign = history[0].clone();
    foreign.block_number += 500;
    foreign.token_address = "0xffffffffffffffffffffffffffffffffffffffff".into();
    history.push(foreign);
    insert(&db.pool, &history).await;
    let chain = FakeChain::new(
        START + 205,
        vec![transfer(START + 205, 0, LAND, ALICE, BOB, &token)],
        1000,
        u64::MAX,
    );
    chain.logs.lock()[0]
        .as_object_mut()
        .unwrap()
        .remove("blockTimestamp");
    let mut settings = Settings::for_tests();
    settings.rpc_url = Some(serve(chain.clone()).await);
    settings.requests_per_pass = 1;
    indexer::ensure_cursor(&db.pool, &settings).await.unwrap();
    let mut conn = db.pool.acquire().await.unwrap();
    set_cursor(&mut conn, &Settings::for_tests(), START as i64 + 204)
        .await
        .unwrap();
    drop(conn);
    let state = state_over(db.pool.clone(), settings.clone(), None);
    let (jobs, ctx) = job_ctx().await;
    let live = indexer::run_pass(&state, &ctx).await.unwrap();
    assert!(live.published && live.requests == 2 && live.events_written == 1);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM land_authz.authz_event WHERE block_time=0 AND token_address=$1",
    )
    .bind(LAND)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 205, "live hydration reserves the minimum budget");
    chain.push(transfer(START + 206, 0, LAND, BOB, CARL, &token));
    chain
        .logs
        .lock()
        .last_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("blockTimestamp");
    chain.head.store(START + 206, Ordering::SeqCst);
    let next_live = indexer::run_pass(&state, &ctx).await.unwrap();
    assert!(next_live.published && next_live.requests == 2 && next_live.events_written == 1);
    assert_eq!(next_live.indexed_to, START as i64 + 206);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM land_authz.authz_event WHERE block_time=0 AND token_address=$1",
    )
    .bind(LAND)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        remaining, 205,
        "continued live traffic cannot be starved by the repair backlog"
    );
    settings.requests_per_pass = 8;
    let repair = state_over(db.pool.clone(), settings, None);
    let out = indexer::run_pass(&repair, &ctx).await.unwrap();
    assert_eq!(out.requests, 3, "all three repair batches fit this pass");
    let ready: bool =
        sqlx::query_scalar("SELECT history_timestamps_ready FROM land_authz.index_cursor")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(ready);
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM land_authz.authz_event WHERE block_time=0")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 1,
        "another registry's history is neither repaired nor trusted"
    );
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn header_identity_and_clock_tolerance_are_enforced_before_cursor_advance() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let chain = FakeChain::new(
        START,
        vec![transfer(START, 0, LAND, ZERO_ADDR, ALICE, &token)],
        1000,
        u64::MAX,
    );
    chain.logs.lock()[0]
        .as_object_mut()
        .unwrap()
        .remove("blockTimestamp");
    chain.logs.lock()[0]["blockHash"] = json!(format!("0x{:064x}", START + 1));
    let mut settings = Settings::for_tests();
    settings.rpc_url = Some(serve(chain.clone()).await);
    let state = state_over(db.pool.clone(), settings, None);
    let (jobs, ctx) = job_ctx().await;
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().paused);
    assert_eq!(event_count(&db.pool).await, 0);
    chain.logs.lock()[0]["blockHash"] = json!(format!("0x{START:064x}"));
    chain
        .header_time
        .store((Utc::now().timestamp() + 120) as u64, Ordering::SeqCst);
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().paused);
    chain
        .header_time
        .store((Utc::now().timestamp() + 30) as u64, Ordering::SeqCst);
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().published);
    let at: bool = sqlx::query_scalar("SELECT folded_at BETWEEN timezone('UTC',now())-interval '5 seconds' AND timezone('UTC',now()) FROM land_authz.index_cursor").fetch_one(&db.pool).await.unwrap();
    assert!(at, "publish timestamps use the database clock");
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn published_history_survives_unrelated_ingest_and_revokes_pending_subject_changes() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let entity_time = chrono::DateTime::from_timestamp(Utc::now().timestamp() - 3600, 0)
        .unwrap()
        .naive_utc();
    let cutoff = entity_time.and_utc().timestamp() - 900;
    let token = land_token(1, 1);
    let mut parcel = ev(1, 0, KIND_TRANSFER, Some(&token), Some(ESTATE), None, None);
    parcel.block_time = cutoff - 100;
    let mut estate = ev(2, 0, KIND_TRANSFER, Some("42"), Some(ALICE), None, None);
    estate.token_address = ESTATE.into();
    estate.block_time = cutoff - 100;
    let mut membership = estate_ev(3, 0, KIND_ADD_LAND, "42", &token);
    membership.block_time = cutoff - 100;
    insert(pool, &[parcel, estate, membership]).await;
    fold_all(pool).await;
    indexer::ensure_cursor(pool, &Settings::for_tests())
        .await
        .unwrap();
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=100, folded_block=100, folded_at=timezone('UTC',now())-interval '29 minutes', history_timestamps_ready=true")
        .execute(pool).await.unwrap();
    async fn allowed(pool: &PgPool, at: chrono::NaiveDateTime) -> bool {
        sqlx::query_scalar(
            "SELECT land_authz.scene_collaborators_current($1,$2,$3,ARRAY['1,1'],$4)",
        )
        .bind(NETWORK_MAINNET)
        .bind(LAND)
        .bind(ESTATE)
        .bind(at)
        .fetch_one(pool)
        .await
        .unwrap()
    }
    assert!(allowed(pool, entity_time).await);
    // Existing databases already have this function: the additive migration
    // must replace its body, not rely on missing-object creation.
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION land_authz.scene_collaborators_current(configured_network text, configured_land text, configured_estate text, pointers text[], entity_timestamp timestamp without time zone) RETURNS boolean LANGUAGE sql STABLE AS $$ SELECT false $$; DELETE FROM meta.schema_steps WHERE step='0012_land_history_published_readiness';")
        .execute(pool).await.unwrap();
    assert!(!allowed(pool, entity_time).await);
    crate::db::schema_migrate::migrate(pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
        .await
        .unwrap();
    assert!(
        allowed(pool, entity_time).await,
        "0012 replaces the already-installed function"
    );
    let mut approval = ev(101, 0, KIND_APPROVAL, Some(&token), None, Some(CARL), None);
    approval.block_time = cutoff + 1;
    insert(pool, &[approval]).await;
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=101")
        .execute(pool)
        .await
        .unwrap();
    assert!(
        allowed(pool, entity_time).await,
        "unfolded approvals do not cause a blanket outage"
    );
    sqlx::query(
        "UPDATE land_authz.index_cursor SET folded_at=timezone('UTC',now())-interval '31 minutes'",
    )
    .execute(pool)
    .await
    .unwrap();
    assert!(
        !allowed(pool, entity_time).await,
        "the published snapshot still expires"
    );
    sqlx::query("UPDATE land_authz.index_cursor SET folded_at=timezone('UTC',now())")
        .execute(pool)
        .await
        .unwrap();
    // Isolate membership history: no paired LAND transfer accompanies these
    // events, so a passing result cannot come from the LAND-transfer check.
    let mut removed = estate_ev(102, 0, KIND_REMOVE_LAND, "42", &token);
    removed.block_time = cutoff - 10;
    insert(pool, &[removed]).await;
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=102")
        .execute(pool)
        .await
        .unwrap();
    assert!(
        !allowed(pool, entity_time).await,
        "pending removal cannot use stale membership"
    );
    let mut new_estate = ev(103, 0, KIND_TRANSFER, Some("43"), Some(BOB), None, None);
    new_estate.token_address = ESTATE.into();
    new_estate.block_time = cutoff - 10;
    let mut added = estate_ev(103, 1, KIND_ADD_LAND, "43", &token);
    added.block_time = cutoff - 10;
    insert(pool, &[new_estate, added]).await;
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=103")
        .execute(pool)
        .await
        .unwrap();
    assert!(
        !allowed(pool, entity_time).await,
        "pending addition cannot hide the new estate subject"
    );
    fold_all(pool).await;
    assert!(
        !allowed(pool, entity_time).await,
        "fold commit alone is not publication"
    );
    mark_folded(pool, &Settings::for_tests(), 103)
        .await
        .unwrap();
    assert!(allowed(pool, entity_time).await);
    let mut recent_membership = estate_ev(104, 0, KIND_ADD_LAND, "43", &token);
    recent_membership.block_time = cutoff;
    insert(pool, &[recent_membership]).await;
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=104")
        .execute(pool)
        .await
        .unwrap();
    fold_all(pool).await;
    mark_folded(pool, &Settings::for_tests(), 104)
        .await
        .unwrap();
    assert!(
        !allowed(pool, entity_time).await,
        "membership timestamp independently revokes at the strict boundary"
    );
    sqlx::query("UPDATE land_authz.authz_event SET block_time=$1 WHERE block_number=104")
        .bind(cutoff - 1)
        .execute(pool)
        .await
        .unwrap();
    assert!(allowed(pool, entity_time).await);
    let mut transferred = ev(105, 0, KIND_TRANSFER, Some("43"), Some(CARL), None, None);
    transferred.token_address = ESTATE.into();
    transferred.block_time = cutoff - 1;
    insert(pool, &[transferred]).await;
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=105")
        .execute(pool)
        .await
        .unwrap();
    assert!(
        !allowed(pool, entity_time).await,
        "even an old timestamp cannot bypass the pending estate transfer fence"
    );
    fold_all(pool).await;
    mark_folded(pool, &Settings::for_tests(), 105)
        .await
        .unwrap();
    assert!(allowed(pool, entity_time).await);
    let mut transferred = ev(
        106,
        0,
        KIND_TRANSFER,
        Some(&token),
        Some(ESTATE),
        None,
        None,
    );
    transferred.block_time = cutoff - 1;
    insert(pool, &[transferred]).await;
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=106")
        .execute(pool)
        .await
        .unwrap();
    assert!(
        !allowed(pool, entity_time).await,
        "pending LAND transfer fails closed before folding"
    );
    db.drop().await;
}

#[tokio::test]
async fn readiness_and_status_use_database_freshness_boundaries() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let settings = Settings::for_tests();
    indexer::ensure_cursor(&db.pool, &settings).await.unwrap();
    sqlx::query("UPDATE land_authz.index_cursor SET last_block=100, folded_block=100, folded_at=timezone('UTC',now())").execute(&db.pool).await.unwrap();
    assert!(
        indexer::ready_cursor(&db.pool, NETWORK_MAINNET, LAND, ESTATE)
            .await
            .is_some()
    );
    let state = state_over(db.pool.clone(), settings, None);
    assert!(state.ready().await.is_some());
    let app = router(state.clone());
    let (_, _, body) = get_json(&app, "/lambdas/land-authz/status").await;
    assert_eq!(body["ready"], true);
    assert!(
        body["cursor"]["foldAgeSecs"]
            .as_i64()
            .is_some_and(|age| (0..5).contains(&age))
    );
    sqlx::query(
        "UPDATE land_authz.index_cursor SET folded_at=timezone('UTC',now())+interval '1 minute'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        indexer::ready_cursor(&db.pool, NETWORK_MAINNET, LAND, ESTATE)
            .await
            .is_none()
    );
    assert!(state.ready().await.is_none());
    let (_, _, body) = get_json(&app, "/lambdas/land-authz/status").await;
    assert_eq!(body["ready"], false);
    assert!(body["cursor"]["foldAgeSecs"].as_i64().unwrap() < 0);
    for (minutes, expected) in [(29, true), (31, false)] {
        sqlx::query("UPDATE land_authz.index_cursor SET folded_at=timezone('UTC',now())-make_interval(mins=>$1)").bind(minutes).execute(&db.pool).await.unwrap();
        assert_eq!(
            indexer::ready_cursor(&db.pool, NETWORK_MAINNET, LAND, ESTATE)
                .await
                .is_some(),
            expected
        );
        assert_eq!(state.ready().await.is_some(), expected);
        let (_, _, body) = get_json(&app, "/lambdas/land-authz/status").await;
        assert_eq!(body["ready"], expected);
        assert!(body["cursor"]["foldAgeSecs"].as_i64().unwrap() >= minutes as i64 * 60);
    }
    db.drop().await;
}

#[tokio::test]
async fn transient_partial_batches_recover_without_restarting_the_indexer() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let chain = FakeChain::new(
        START + 200,
        (0..201)
            .map(|i| transfer(START + i, 0, LAND, ZERO_ADDR, ALICE, &token))
            .collect(),
        1000,
        u64::MAX,
    );
    for log in chain.logs.lock().iter_mut() {
        log.as_object_mut().unwrap().remove("blockTimestamp");
    }
    chain.batch_limit.store(1, Ordering::SeqCst);
    let mut settings = Settings::for_tests();
    settings.rpc_url = Some(serve(chain.clone()).await);
    settings.requests_per_pass = 1;
    let state = state_over(db.pool.clone(), settings, None);
    let (jobs, ctx) = job_ctx().await;
    assert_eq!(
        indexer::run_pass(&state, &ctx)
            .await
            .unwrap()
            .events_written,
        1
    );
    chain.batch_limit.store(usize::MAX, Ordering::SeqCst);
    for _ in 0..indexer::HEADER_BATCH_RETRY_FETCHES {
        let out = indexer::run_pass(&state, &ctx).await.unwrap();
        assert_eq!(
            out.events_written, 1,
            "fallback still progresses at the minimum budget"
        );
        assert_eq!(out.requests, 2);
    }
    let recovered = indexer::run_pass(&state, &ctx).await.unwrap();
    assert_eq!(recovered.events_written, HEADER_BATCH);
    assert_eq!(
        recovered.requests, 2,
        "batch throughput recovers on the same state"
    );
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn unordered_headers_and_logs_commit_only_before_the_earliest_missing_or_mismatched_block() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let token = land_token(1, 1);
    let chain = FakeChain::new(
        START + 2,
        (0..3)
            .map(|i| transfer(START + i, 0, LAND, ZERO_ADDR, ALICE, &token))
            .collect(),
        1000,
        u64::MAX,
    );
    for log in chain.logs.lock().iter_mut() {
        log.as_object_mut().unwrap().remove("blockTimestamp");
    }
    chain.batch_reverse.store(true, Ordering::SeqCst);
    chain.logs_reverse.store(true, Ordering::SeqCst);
    chain.logs.lock()[1]["blockHash"] = json!(format!("0x{:064x}", START + 99));
    let mut settings = Settings::for_tests();
    settings.rpc_url = Some(serve(chain.clone()).await);
    settings.requests_per_pass = 1;
    let state = state_over(db.pool.clone(), settings, None);
    let (jobs, ctx) = job_ctx().await;
    let prefix = indexer::run_pass(&state, &ctx).await.unwrap();
    assert_eq!(
        (prefix.indexed_to, prefix.events_written),
        (START as i64, 1)
    );
    assert_eq!(
        event_count(&db.pool).await,
        1,
        "valid blocks after the mismatch cannot leap over it"
    );
    chain.logs.lock()[1]["blockHash"] = json!(format!("0x{:064x}", START + 1));
    chain.batch_omit.store(START + 1, Ordering::SeqCst);
    let missing = indexer::run_pass(&state, &ctx).await.unwrap();
    assert_eq!(
        (missing.indexed_to, missing.events_written),
        (START as i64, 0)
    );
    assert_eq!(event_count(&db.pool).await, 1);
    chain.batch_omit.store(u64::MAX, Ordering::SeqCst);
    let resumed = indexer::run_pass(&state, &ctx).await.unwrap();
    assert_eq!(
        (resumed.indexed_to, resumed.events_written),
        (START as i64 + 1, 1)
    );
    assert!(indexer::run_pass(&state, &ctx).await.unwrap().published);
    assert_eq!(event_count(&db.pool).await, 3);
    jobs.shutdown(std::time::Duration::from_secs(1)).await;
    db.drop().await;
}

#[tokio::test]
async fn slow_repair_publishes_first_and_keeps_committed_chunks_on_cancel_or_deadline() {
    for cancel in [true, false] {
        let Some(db) = ScratchDb::create().await else {
            return;
        };
        let token = land_token(1, 1);
        let history: Vec<AuthzEvent> = (0..205)
            .map(|i| {
                let mut event = ev(
                    START as i64 + i,
                    0,
                    KIND_TRANSFER,
                    Some(&token),
                    Some(ALICE),
                    None,
                    None,
                );
                event.block_time = 0;
                event
            })
            .collect();
        insert(&db.pool, &history).await;
        let chain = FakeChain::new(
            START + 205,
            vec![transfer(START + 205, 0, LAND, ALICE, BOB, &token)],
            1000,
            u64::MAX,
        );
        chain.header_block_from.store(START + 100, Ordering::SeqCst);
        let mut settings = Settings::for_tests();
        settings.rpc_url = Some(serve(chain.clone()).await);
        settings.requests_per_pass = 8;
        indexer::ensure_cursor(&db.pool, &settings).await.unwrap();
        sqlx::query("UPDATE land_authz.index_cursor SET last_block=$1")
            .bind(START as i64 + 204)
            .execute(&db.pool)
            .await
            .unwrap();
        let state = state_over(db.pool.clone(), settings, None);
        let (jobs, ctx) = job_ctx().await;
        let task_state = state.clone();
        let task_ctx = ctx.clone();
        let task = tokio::spawn(async move { indexer::run_pass(&task_state, &task_ctx).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            chain.header_started.notified(),
        )
        .await
        .unwrap();
        let published = indexer::ready_cursor(&db.pool, NETWORK_MAINNET, LAND, ESTATE)
            .await
            .unwrap();
        assert_eq!(
            published.folded_block,
            Some(START as i64 + 205),
            "publication is visible while repair is blocked"
        );
        assert_eq!(
            token_leg(&db.pool, &token, "owner").await.as_deref(),
            Some(BOB)
        );
        if cancel {
            ctx.cancel.cancel();
        } else {
            tokio::time::pause();
            tokio::time::advance(
                indexer::LEGACY_REPAIR_TIME_LIMIT + std::time::Duration::from_secs(1),
            )
            .await;
            tokio::time::resume();
        }
        let out = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(out.published && out.requests <= 8);
        assert_eq!(out.paused, cancel);
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM land_authz.authz_event WHERE block_time=0")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            remaining, 105,
            "the first repaired chunk survives interruption"
        );
        let ready: bool =
            sqlx::query_scalar("SELECT history_timestamps_ready FROM land_authz.index_cursor")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(!ready, "partial history must not enable collaborators");
        chain.header_release.notify_waiters();
        jobs.shutdown(std::time::Duration::from_secs(1)).await;
        db.drop().await;
    }
}

async fn backend_pid(conn: &mut sqlx::PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(conn)
        .await
        .unwrap()
}

/// Until a backend of `pool` waits on a lock `holder` (a backend pid) holds.
async fn wait_until_blocked_by(pool: &PgPool, holder: i32) {
    for _ in 0..500 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
        )
        .bind(holder)
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("nothing waited on backend {holder}");
}

/// A fold of the old registries left running (by a lost lease, say) holds
/// their cursor row: a switch waits for it at most `REBIND_LOCK_TIMEOUT`,
/// then defers without writing, and the pass ends without an error, so it
/// is retried at the next interval, not after the error backoff. A rewind
/// on the same registries never waits for the fold.
#[tokio::test]
async fn a_switch_defers_behind_a_fold_left_running_and_a_same_registries_rewind_never_waits() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings {
        // Never asked: a deferred replay ends the pass before any request.
        rpc_url: Some("http://127.0.0.1:9".into()),
        index_enabled: true,
        replay_from_block: Some(START),
        ..Settings::for_tests()
    };
    let old = Settings {
        land_registry: "0x00000000000000000000000000000000000000aa".into(),
        estate_registry: "0x00000000000000000000000000000000000000bb".into(),
        ..Settings::for_tests()
    };
    let (_jobs, ctx) = job_ctx().await;
    indexer::ensure_cursor(pool, &old).await.unwrap();
    insert(
        pool,
        &[AuthzEvent {
            token_address: old.estate_registry.clone(),
            ..estate_ev((START + 10) as i64, 0, KIND_ADD_LAND, "500", TOKEN_37_55)
        }],
    )
    .await;
    let links = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM land_authz.estate_land")
            .fetch_one(pool)
            .await
            .unwrap()
    };
    let registries = || async {
        let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
        (row.land_registry, row.estate_registry)
    };
    let old_registries = (old.land_registry.clone(), old.estate_registry.clone());

    let mut folding = pool.begin().await.unwrap();
    indexer::fold_in_tx(&mut folding, &old, None).await.unwrap();

    // A rewind on the same registries completes beside it.
    let rewound = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        begin_replay(pool, &old, START + 5),
    )
    .await;
    assert!(
        matches!(rewound, Ok(Ok(ReplayStart::Begun))),
        "the rewind waited on the fold: {rewound:?}"
    );

    // The switch gives up after the lock timeout and writes nothing.
    let asked = std::time::Instant::now();
    assert_eq!(
        begin_replay(pool, &configured, START).await.unwrap(),
        ReplayStart::Deferred
    );
    // It waited out the lock timeout (no other branch deferred it at once),
    // and no longer than that.
    let waited = asked.elapsed();
    assert!(
        waited >= std::time::Duration::from_secs(5) && waited < std::time::Duration::from_secs(30),
        "{waited:?}"
    );
    assert_eq!(registries().await, old_registries);

    // A pass asked for that switch ends without an error and keeps the
    // replay requested.
    let state = state_over(pool.clone(), configured.clone(), None);
    let out = indexer::run_pass(&state, &ctx).await.unwrap();
    assert!(!out.published && out.folded.is_none(), "{out:?}");
    assert!(!state.replay_requested.load(Ordering::SeqCst));
    assert_eq!(registries().await, old_registries);

    // Once the fold commits, its link stands, and the switch goes through.
    folding.commit().await.unwrap();
    assert_eq!(links().await, 1);
    assert_eq!(
        begin_replay(pool, &configured, START).await.unwrap(),
        ReplayStart::Begun
    );
    assert_eq!(
        registries().await,
        (
            configured.land_registry.clone(),
            configured.estate_registry.clone()
        )
    );
    assert_eq!(links().await, 0);
    db.drop().await;
}

/// A rewind on the same registries takes no lock, so a switch can commit
/// between its read and its write: the write then matches no row of its
/// registries, and the switch, links deleted, is never undone.
#[tokio::test]
async fn a_same_registries_rewind_never_undoes_a_switch_committed_under_it() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings::for_tests();
    let other = (
        "0x00000000000000000000000000000000000000aa",
        "0x00000000000000000000000000000000000000bb",
    );
    indexer::ensure_cursor(pool, &configured).await.unwrap();

    let mut switching = pool.begin().await.unwrap();
    let switch_pid = backend_pid(&mut switching).await;
    sqlx::query(
        "UPDATE land_authz.index_cursor SET land_registry = $2, estate_registry = $3 WHERE network = $1",
    )
    .bind(NETWORK_MAINNET)
    .bind(other.0)
    .bind(other.1)
    .execute(&mut *switching)
    .await
    .unwrap();
    let rewind = {
        let (pool, configured) = (pool.clone(), configured.clone());
        tokio::spawn(async move { begin_replay(&pool, &configured, START + 95).await })
    };
    wait_until_blocked_by(pool, switch_pid).await;
    switching.commit().await.unwrap();
    assert_eq!(rewind.await.unwrap().unwrap(), ReplayStart::Deferred);
    let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(
        (row.land_registry.as_str(), row.estate_registry.as_str()),
        other
    );
    db.drop().await;
}

/// `estate_land` holds one network's links (it is keyed by the LAND token
/// alone), so a switch refuses to run beside another network's cursor,
/// whose links it would delete; the pass names why.
#[tokio::test]
async fn a_switch_refuses_a_database_shared_with_another_network() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings {
        rpc_url: Some("http://127.0.0.1:9".into()),
        index_enabled: true,
        replay_from_block: Some(START),
        ..Settings::for_tests()
    };
    let old = Settings {
        land_registry: "0x00000000000000000000000000000000000000aa".into(),
        estate_registry: "0x00000000000000000000000000000000000000bb".into(),
        ..Settings::for_tests()
    };
    let (_jobs, ctx) = job_ctx().await;
    indexer::ensure_cursor(pool, &old).await.unwrap();
    sqlx::query(
        "INSERT INTO land_authz.index_cursor (network, land_registry, estate_registry, last_block, full_replay_started) \
         VALUES ($1, $2, $3, 1, false)",
    )
    .bind(NETWORK_SEPOLIA)
    .bind("0x00000000000000000000000000000000000000cc")
    .bind("0x00000000000000000000000000000000000000dd")
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO land_authz.estate_land (land_token_id, estate_token_id, updated_block, updated_log) \
         VALUES ($1::numeric, 7, 1, 0)",
    )
    .bind(TOKEN_37_55)
    .execute(pool)
    .await
    .unwrap();

    assert_eq!(
        begin_replay(pool, &configured, START).await.unwrap(),
        ReplayStart::SharedDatabase {
            other: NETWORK_SEPOLIA.to_string()
        }
    );
    let state = state_over(pool.clone(), configured.clone(), None);
    let refused = indexer::run_pass(&state, &ctx).await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("shares this database"),
        "{refused:#}"
    );
    assert!(!state.replay_requested.load(Ordering::SeqCst));
    let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(row.land_registry, old.land_registry);
    let links: i64 = sqlx::query_scalar("SELECT count(*) FROM land_authz.estate_land")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(links, 1, "the other network's links are kept");
    db.drop().await;
}

/// The networks whose cursor this database holds, in order.
async fn cursor_networks(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT network FROM land_authz.index_cursor ORDER BY network")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Until a backend of this test's database waits on an advisory lock.
async fn wait_until_an_advisory_lock_is_awaited(pool: &PgPool) {
    for _ in 0..500 {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND wait_event = 'advisory')",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("nothing waited on an advisory lock");
}

/// A switch admitted to the database, and still waiting on the old
/// registries' fold, holds the admission: another network's cursor created
/// meanwhile waits for it, then is refused, so the switch never deletes a
/// link of a network that joined under it.
#[tokio::test]
async fn a_network_created_while_a_switch_waits_is_refused_once_the_switch_lands() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings::for_tests();
    let old = Settings {
        land_registry: "0x00000000000000000000000000000000000000aa".into(),
        estate_registry: "0x00000000000000000000000000000000000000bb".into(),
        ..Settings::for_tests()
    };
    let sepolia = Settings::for_network("sepolia", None).unwrap();
    indexer::ensure_cursor(pool, &old).await.unwrap();
    insert(
        pool,
        &[AuthzEvent {
            token_address: old.estate_registry.clone(),
            ..estate_ev((START + 10) as i64, 0, KIND_ADD_LAND, "500", TOKEN_37_55)
        }],
    )
    .await;

    let mut folding = pool.begin().await.unwrap();
    let fold_pid = backend_pid(&mut folding).await;
    indexer::fold_in_tx(&mut folding, &old, None).await.unwrap();
    let switch = {
        let (pool, configured) = (pool.clone(), configured.clone());
        tokio::spawn(async move { begin_replay(&pool, &configured, START).await })
    };
    wait_until_blocked_by(pool, fold_pid).await;
    let create = {
        let (pool, sepolia) = (pool.clone(), sepolia.clone());
        tokio::spawn(async move { indexer::ensure_cursor(&pool, &sepolia).await })
    };
    wait_until_an_advisory_lock_is_awaited(pool).await;
    assert!(!create.is_finished(), "the creation waits on the switch");

    folding.commit().await.unwrap();
    assert_eq!(switch.await.unwrap().unwrap(), ReplayStart::Begun);
    let refused = create.await.unwrap().unwrap_err();
    assert!(
        format!("{refused:#}").contains("shares this database"),
        "{refused:#}"
    );
    assert_eq!(
        cursor_networks(pool).await,
        vec![NETWORK_MAINNET.to_string()]
    );
    let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(row.land_registry, configured.land_registry);
    db.drop().await;
}

/// Two networks creating the first cursor of an empty database: whichever
/// is admitted first holds it, and the other, whether through
/// `ensure_cursor` or a replay that would create it, is refused.
#[tokio::test]
async fn the_first_cursor_of_an_empty_database_admits_one_network() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings::for_tests();
    let sepolia = Settings::for_network("sepolia", None).unwrap();

    // Mainnet's creation is admitted and holds the admission until it commits.
    let mut creating = pool.begin().await.unwrap();
    let creating_pid = backend_pid(&mut creating).await;
    assert_eq!(
        indexer::admit_network(&mut creating, NETWORK_MAINNET)
            .await
            .unwrap(),
        None
    );
    sqlx::query(
        "INSERT INTO land_authz.index_cursor (network, land_registry, estate_registry, last_block, full_replay_started) \
         VALUES ($1, $2, $3, $4, true)",
    )
    .bind(NETWORK_MAINNET)
    .bind(&configured.land_registry)
    .bind(&configured.estate_registry)
    .bind(START as i64 - 1)
    .execute(&mut *creating)
    .await
    .unwrap();

    let create = {
        let (pool, sepolia) = (pool.clone(), sepolia.clone());
        tokio::spawn(async move { indexer::ensure_cursor(&pool, &sepolia).await })
    };
    let replay = {
        let (pool, sepolia) = (pool.clone(), sepolia.clone());
        tokio::spawn(async move { begin_replay(&pool, &sepolia, sepolia.start_block).await })
    };
    wait_until_blocked_by(pool, creating_pid).await;
    creating.commit().await.unwrap();

    let refused = create.await.unwrap().unwrap_err();
    assert!(
        format!("{refused:#}").contains("shares this database"),
        "{refused:#}"
    );
    assert_eq!(
        replay.await.unwrap().unwrap(),
        ReplayStart::SharedDatabase {
            other: NETWORK_MAINNET.to_string()
        }
    );
    assert_eq!(
        cursor_networks(pool).await,
        vec![NETWORK_MAINNET.to_string()]
    );
    db.drop().await;
}

/// A pass whose network has no cursor yet waits on another pass's
/// admission at most the lock timeout, then ends without an error (no
/// error backoff), and creates the cursor once the admission is free.
#[tokio::test]
async fn a_pass_whose_new_cursor_waits_on_the_admission_ends_without_an_error() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings {
        // Never asked: the pass ends before any request.
        rpc_url: Some("http://127.0.0.1:9".into()),
        index_enabled: true,
        ..Settings::for_tests()
    };
    let (_jobs, ctx) = job_ctx().await;
    let mut admitting = pool.begin().await.unwrap();
    assert_eq!(
        indexer::admit_network(&mut admitting, NETWORK_MAINNET)
            .await
            .unwrap(),
        None
    );

    let state = state_over(pool.clone(), configured.clone(), None);
    let out = indexer::run_pass(&state, &ctx).await.unwrap();
    assert!(!out.published && out.folded.is_none(), "{out:?}");
    assert!(cursor(pool, NETWORK_MAINNET).await.unwrap().is_none());

    admitting.rollback().await.unwrap();
    let row = indexer::ensure_cursor(pool, &configured).await.unwrap();
    assert_eq!(row.land_registry, configured.land_registry);
    db.drop().await;
}

/// Two nodes overlapping after a lost lease both ask for the same switch.
/// The second, admitted after the first committed, finds the cursor already
/// of its registries under the row lock: it begins the replay without a
/// second rewind, and keeps the links the first one's replay rebuilt.
#[tokio::test]
async fn a_switch_another_pass_already_made_is_begun_without_a_second_rewind() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings::for_tests();
    let old = Settings {
        land_registry: "0x00000000000000000000000000000000000000aa".into(),
        estate_registry: "0x00000000000000000000000000000000000000bb".into(),
        ..Settings::for_tests()
    };
    indexer::ensure_cursor(pool, &old).await.unwrap();
    let progressed = (START + 50) as i64;

    // The first node is admitted and switches: the cursor of the configured
    // registries, its replay already past its start, and a link of theirs.
    let mut first = pool.begin().await.unwrap();
    let first_pid = backend_pid(&mut first).await;
    assert_eq!(
        indexer::admit_network(&mut first, NETWORK_MAINNET)
            .await
            .unwrap(),
        None
    );
    sqlx::query(
        "UPDATE land_authz.index_cursor SET land_registry = $2, estate_registry = $3, last_block = $4, \
         folded_block = NULL, folded_at = NULL, full_replay_started = true WHERE network = $1",
    )
    .bind(NETWORK_MAINNET)
    .bind(&configured.land_registry)
    .bind(&configured.estate_registry)
    .bind(progressed)
    .execute(&mut *first)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO land_authz.estate_land (land_token_id, estate_token_id, updated_block, updated_log) \
         VALUES ($1::numeric, 7, 1, 0)",
    )
    .bind(TOKEN_37_55)
    .execute(&mut *first)
    .await
    .unwrap();

    // The second node read the old registries and waits for the admission.
    let second = {
        let (pool, configured) = (pool.clone(), configured.clone());
        tokio::spawn(async move { begin_replay(&pool, &configured, START).await })
    };
    wait_until_blocked_by(pool, first_pid).await;
    first.commit().await.unwrap();
    assert_eq!(second.await.unwrap().unwrap(), ReplayStart::Begun);

    let row = cursor(pool, NETWORK_MAINNET).await.unwrap().unwrap();
    assert_eq!(
        (row.land_registry.as_str(), row.last_block),
        (configured.land_registry.as_str(), progressed),
        "no second rewind"
    );
    let links: i64 = sqlx::query_scalar("SELECT count(*) FROM land_authz.estate_land")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(links, 1, "the first replay's link is kept");
    db.drop().await;
}

/// A lock timeout on the replay's write, not only on its admission or the
/// cursor row, defers it: here the cursor it creates waits behind another
/// writer's uncommitted row of the same network.
#[tokio::test]
async fn a_replay_whose_write_waits_past_the_lock_timeout_defers() {
    let Some(db) = ScratchDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let configured = Settings::for_tests();
    let mut writer = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO land_authz.index_cursor (network, land_registry, estate_registry, last_block, full_replay_started) \
         VALUES ($1, $2, $3, $4, true)",
    )
    .bind(NETWORK_MAINNET)
    .bind(&configured.land_registry)
    .bind(&configured.estate_registry)
    .bind(START as i64 - 1)
    .execute(&mut *writer)
    .await
    .unwrap();

    let asked = std::time::Instant::now();
    assert_eq!(
        begin_replay(pool, &configured, START).await.unwrap(),
        ReplayStart::Deferred
    );
    assert!(
        asked.elapsed() >= std::time::Duration::from_secs(5),
        "{:?}",
        asked.elapsed()
    );
    writer.rollback().await.unwrap();
    assert!(cursor(pool, NETWORK_MAINNET).await.unwrap().is_none());
    db.drop().await;
}
