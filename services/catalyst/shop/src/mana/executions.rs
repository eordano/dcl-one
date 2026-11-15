// from: marketplace@f3956826:webapp/src/modules/transaction (the pending-transaction list a buyer watches), shop@94584a5c:app/src/lib/cart-checkout.ts (a group's submitted hash, polled to a receipt), SHOP.md "Show per-group completion/pending/failure and resume only unfinished groups"
//! `shop.executions`: every MANA transaction a wallet reported after
//! sending it, tracked by the `executions-track` job ([`super::tracker`])
//! from the receipt through the index or the chain's finality to a settled
//! (or reverted, mismatched, dropped) row. The row carries what the
//! settlement must show, taken from the record the report names rather
//! than the body: the asset, the venue, the counterparty, the signed price
//! (a trade's coupon is read from its transaction at settlement) and the
//! address the asset must end with. `(chain_id, tx_hash)` is the
//! idempotency key: a hash reported twice is one row.

use alloy_primitives::{Address, U256};
use axum::Json;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use super::facts::{self, Listing};
use super::wire::{ExecutionDto, ExecutionRequest};
use super::{SharedState, error};
use crate::types::{Session, TokenRef};

pub const ROUTE: &str = "/v1/executions";
pub const ROUTE_ONE: &str = "/v1/executions/{id}";

pub const KINDS: [&str; 10] = [
    "accept_trade",
    "execute_order",
    "accept_rental",
    "register_name",
    "transfer",
    "cancel_trade",
    "cancel_order",
    "claim_rental",
    "cancel_rental",
    "approve_mana",
];
pub const STATUSES: [&str; 6] = [
    "submitted",
    "mined",
    "reverted",
    "settled",
    "mismatch",
    "dropped",
];

/// What a new row expects of its settlement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expectation {
    pub venue: Address,
    pub contract: Option<Address>,
    pub token_id: Option<U256>,
    pub reference: Option<String>,
    pub price: Option<U256>,
    pub mana_token: Option<Address>,
    pub seller: Option<Address>,
    pub owner: Option<Address>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRow {
    pub id: uuid::Uuid,
    pub chain_id: i64,
    pub tx_hash: String,
    pub kind: String,
    pub actor: String,
    pub venue: String,
    pub contract_address: Option<String>,
    pub token_id: Option<String>,
    pub reference: Option<String>,
    pub expected_price: Option<String>,
    pub mana_token: Option<String>,
    pub expected_seller: Option<String>,
    pub expected_owner: Option<String>,
    pub status: String,
    pub block_number: Option<i64>,
    pub confirmed_by: Option<String>,
    pub settlement: Option<Value>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub next_check_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

const COLUMNS: &str = "id, chain_id, tx_hash, kind, actor, venue, contract_address, token_id::text AS token_id, reference, expected_price::text AS expected_price, \
                       mana_token, expected_seller, expected_owner, status, block_number, confirmed_by, settlement, attempts, last_error, next_check_at, created_at, updated_at";

fn row(r: &sqlx::postgres::PgRow) -> ExecutionRow {
    ExecutionRow {
        id: r.get("id"),
        chain_id: r.get("chain_id"),
        tx_hash: r.get("tx_hash"),
        kind: r.get("kind"),
        actor: r.get("actor"),
        venue: r.get("venue"),
        contract_address: r.get("contract_address"),
        token_id: r.get("token_id"),
        reference: r.get("reference"),
        expected_price: r.get("expected_price"),
        mana_token: r.get("mana_token"),
        expected_seller: r.get("expected_seller"),
        expected_owner: r.get("expected_owner"),
        status: r.get("status"),
        block_number: r.get("block_number"),
        confirmed_by: r.get("confirmed_by"),
        settlement: r.get("settlement"),
        attempts: r.get("attempts"),
        last_error: r.get("last_error"),
        next_check_at: r.get("next_check_at"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

impl ExecutionRow {
    pub fn dto(&self) -> ExecutionDto {
        ExecutionDto {
            id: self.id.to_string(),
            chain_id: crate::types::ChainId(self.chain_id.max(0) as u64),
            tx_hash: self.tx_hash.clone(),
            kind: self.kind.clone(),
            actor: self.actor.clone(),
            venue: self.venue.clone(),
            contract_address: self.contract_address.clone(),
            token_id: self.token_id.clone(),
            reference: self.reference.clone(),
            expected_price: self.expected_price.clone(),
            expected_seller: self.expected_seller.clone(),
            expected_owner: self.expected_owner.clone(),
            status: self.status.clone(),
            block_number: self.block_number,
            confirmed_by: self.confirmed_by.clone(),
            settlement: self.settlement.clone(),
            attempts: self.attempts,
            last_error: self.last_error.clone(),
            created_at: self.created_at.and_utc().timestamp_millis(),
            updated_at: self.updated_at.and_utc().timestamp_millis(),
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "submitted" | "mined")
    }
}

fn lower(a: Address) -> String {
    format!("{a:#x}")
}

/// Inserts the row, or returns the one the hash already has. `(claimed, row)`.
/// A row the tracker found was sent by another wallet than its reporter is
/// claimed again, and re-checked, by a reporter proven to have sent it
/// (`proven_sender`).
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &PgPool,
    chain_id: u64,
    tx_hash: &str,
    kind: &str,
    actor: Address,
    e: &Expectation,
    now: NaiveDateTime,
    proven_sender: bool,
) -> Result<(bool, ExecutionRow), sqlx::Error> {
    let inserted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO shop.executions (chain_id, tx_hash, kind, actor, venue, contract_address, token_id, reference, expected_price, mana_token, expected_seller, expected_owner, \
                                      status, next_check_at, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7::numeric, $8, $9::numeric, $10, $11, $12, 'submitted', $13, $13, $13) \
         ON CONFLICT (chain_id, tx_hash) DO UPDATE SET \
             kind = EXCLUDED.kind, actor = EXCLUDED.actor, venue = EXCLUDED.venue, \
             contract_address = EXCLUDED.contract_address, token_id = EXCLUDED.token_id, \
             reference = EXCLUDED.reference, expected_price = EXCLUDED.expected_price, \
             mana_token = EXCLUDED.mana_token, expected_seller = EXCLUDED.expected_seller, \
             expected_owner = EXCLUDED.expected_owner, status = 'submitted', block_number = NULL, \
             confirmed_by = NULL, settlement = NULL, attempts = 0, last_error = NULL, \
             next_check_at = EXCLUDED.next_check_at, updated_at = EXCLUDED.updated_at \
         WHERE $14 AND shop.executions.actor <> EXCLUDED.actor \
           AND shop.executions.status = 'mismatch' \
           AND (shop.executions.settlement ->> '{sender_mismatch}')::boolean IS TRUE \
         RETURNING {COLUMNS}",
        sender_mismatch = super::tracker::SENDER_MISMATCH,
    )))
    .bind(chain_id as i64)
    .bind(tx_hash)
    .bind(kind)
    .bind(lower(actor))
    .bind(lower(e.venue))
    .bind(e.contract.map(lower))
    .bind(e.token_id.map(|t| t.to_string()))
    .bind(&e.reference)
    .bind(e.price.map(|p| p.to_string()))
    .bind(e.mana_token.map(lower))
    .bind(e.seller.map(lower))
    .bind(e.owner.map(lower))
    .bind(now)
    .bind(proven_sender)
    .fetch_optional(pool)
    .await?;
    if let Some(r) = inserted {
        return Ok((true, row(&r)));
    }
    let existing = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM shop.executions WHERE chain_id = $1 AND tx_hash = $2"
    )))
    .bind(chain_id as i64)
    .bind(tx_hash)
    .fetch_one(pool)
    .await?;
    Ok((false, row(&existing)))
}

pub async fn get(pool: &PgPool, id: uuid::Uuid) -> Result<Option<ExecutionRow>, sqlx::Error> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM shop.executions WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(r.as_ref().map(row))
}

pub async fn by_actor(
    pool: &PgPool,
    actor: &str,
    limit: i64,
) -> Result<Vec<ExecutionRow>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {COLUMNS} FROM shop.executions WHERE actor = $1 ORDER BY created_at DESC, id LIMIT $2")))
        .bind(actor.to_ascii_lowercase())
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row).collect())
}

/// The open rows due at `now`, oldest due first.
pub async fn due(
    pool: &PgPool,
    now: NaiveDateTime,
    limit: i64,
) -> Result<Vec<ExecutionRow>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM shop.executions WHERE status IN ('submitted', 'mined') AND next_check_at <= $1 ORDER BY next_check_at, id LIMIT $2"
    )))
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// One tracker step's outcome on a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    pub status: &'static str,
    pub block_number: Option<i64>,
    pub confirmed_by: Option<&'static str>,
    pub settlement: Option<Value>,
    pub error: Option<String>,
    pub next_check_at: NaiveDateTime,
}

/// Writes a step: guarded on the status the tracker read, so two trackers
/// never move one row twice.
pub async fn advance(
    pool: &PgPool,
    id: uuid::Uuid,
    from: &str,
    a: &Advance,
    now: NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE shop.executions SET status = $3, block_number = COALESCE($4, block_number), confirmed_by = COALESCE($5, confirmed_by), \
             settlement = COALESCE($6, settlement), attempts = attempts + 1, last_error = $7, next_check_at = $8, updated_at = $9 \
         WHERE id = $1 AND status = $2",
    )
    .bind(id)
    .bind(from)
    .bind(a.status)
    .bind(a.block_number)
    .bind(a.confirmed_by)
    .bind(&a.settlement)
    .bind(&a.error)
    .bind(a.next_check_at)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// A reported transaction hash, trimmed and lowercased, when it is `0x` plus
/// 64 hex digits. Lowercasing comes first, so a `0X` prefix is accepted on
/// purpose, unlike `types::normalize_tx_hash`: switching to that helper
/// would start refusing execution reports this route accepts today.
fn valid_hash(h: &str) -> Option<String> {
    let h = h.trim().to_ascii_lowercase();
    let hex = h.strip_prefix("0x")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// What the report names, resolved to the record: the expectation the
/// tracker verifies the settlement against.
async fn expectation(
    state: &SharedState,
    actor: Address,
    req: &ExecutionRequest,
) -> Result<(String, Expectation), Response> {
    let bad = |m: String| error(StatusCode::BAD_REQUEST, "Bad Request", &m);
    let kind = req.kind.as_str();
    if !KINDS.contains(&kind) {
        return Err(bad(format!("kind: one of {}", KINDS.join(", "))));
    }
    let chain = state
        .venues
        .chain(req.chain_id)
        .ok_or_else(|| {
            bad(format!(
                "chain {} is not one the Shop settles on",
                req.chain_id
            ))
        })?
        .clone();
    let session = Session::Web3 {
        wallet: crate::types::EvmAddress::new(actor),
    };
    let token =
        |asset: Option<crate::types::AssetId>| -> Result<(Address, Option<U256>), Response> {
            let asset = asset.ok_or_else(|| bad("asset is required".into()))?;
            if asset.chain_id != req.chain_id {
                return Err(bad("the asset is on another chain".into()));
            }
            Ok(match asset.token {
                TokenRef::Token { token_id } => (asset.contract.address(), Some(token_id)),
                TokenRef::Item { .. } => (asset.contract.address(), None),
            })
        };
    let line = super::wire::LineRequest {
        asset: req.asset,
        listing: req.listing.clone(),
        quantity: 1,
        period_index: req.period_index,
        rental_days: req.rental_days,
    };
    let expectation = match kind {
        "accept_trade" | "execute_order" | "accept_rental" | "register_name" => {
            let resolved = facts::resolve(
                &state.pool,
                &state.rentals.db,
                &state.venues,
                session,
                &line,
            )
            .await
            .map_err(|e| bad(e.to_string()))?;
            if resolved.chain.chain_id != req.chain_id {
                return Err(bad("the listing is on another chain".into()));
            }
            match (&resolved.listing, kind) {
                (
                    Listing::Trade {
                        trade,
                        venue,
                        hashed_signature,
                        ..
                    },
                    "accept_trade",
                ) => {
                    let is_bid = trade.trade_type == "bid";
                    let (contract, token_id) = match resolved.asset.token {
                        TokenRef::Token { token_id } => {
                            (resolved.asset.contract.address(), Some(token_id))
                        }
                        TokenRef::Item { .. } => (resolved.asset.contract.address(), None),
                    };
                    let signer: Address = trade
                        .signer
                        .trim()
                        .parse()
                        .map_err(|_| bad("the trade's signer is not an address".into()))?;
                    let leg = if is_bid {
                        super::calls::mana_leg_sent(trade)
                    } else {
                        super::calls::mana_leg(trade)
                    };
                    let usd_pegged = (if is_bid { &trade.sent } else { &trade.received })
                        .iter()
                        .any(|a| a.asset_type == 2);
                    Expectation {
                        venue: *venue,
                        contract: Some(contract),
                        token_id,
                        reference: Some(hashed_signature.clone()),
                        // The signed price. A coupon is not resolved here: the
                        // tracker reads the one the transaction carried from its
                        // calldata and discounts this price by it, so a coupon
                        // that ends, runs out or is outbid after sending does not
                        // move the expectation (`tracker::expected_price`).
                        price: if usd_pegged {
                            None
                        } else {
                            leg.map(|(_, a)| a)
                        },
                        mana_token: Some(chain.mana),
                        seller: Some(if is_bid { actor } else { signer }),
                        owner: Some(if is_bid { signer } else { actor }),
                    }
                }
                (Listing::LegacyOrder { order, .. }, "execute_order") => {
                    let (contract, token_id) = token(req.asset)?;
                    Expectation {
                        venue: order.marketplace,
                        contract: Some(contract),
                        token_id,
                        reference: Some(order.order_id.clone()),
                        price: Some(order.price),
                        mana_token: Some(chain.mana),
                        seller: Some(order.seller),
                        owner: Some(actor),
                    }
                }
                (
                    Listing::Rental {
                        listing,
                        terms,
                        rentals,
                        ..
                    },
                    "accept_rental",
                ) => {
                    let (contract, token_id) = token(req.asset)?;
                    let lessor: Option<Address> =
                        listing.lessor.as_deref().and_then(|l| l.parse().ok());
                    Expectation {
                        venue: *rentals,
                        contract: Some(contract),
                        token_id,
                        reference: Some(listing.id.clone()),
                        price: Some(terms.total.get()),
                        mana_token: Some(chain.mana),
                        seller: lessor,
                        owner: Some(*rentals),
                    }
                }
                (
                    Listing::Name {
                        label,
                        registrar,
                        controller,
                    },
                    "register_name",
                ) => Expectation {
                    venue: *controller,
                    contract: Some(*registrar),
                    token_id: Some(label.token_id()),
                    reference: Some(label.as_str().to_string()),
                    price: None,
                    mana_token: Some(chain.mana),
                    seller: None,
                    owner: Some(actor),
                },
                _ => {
                    return Err(bad(format!(
                        "the listing named is not one a {kind} settles"
                    )));
                }
            }
        }
        "cancel_trade" => {
            let id = req
                .listing
                .trade_id
                .as_deref()
                .ok_or_else(|| bad("listing.tradeId is required".into()))?;
            let trade = facts::load_trade(&state.pool, id, crate::market::trades::TradeScope::Any)
                .await
                .map_err(|e| bad(e.to_string()))?
                .ok_or_else(|| bad(format!("trade {id} is not known to this node")))?;
            let venue: Address = trade
                .contract
                .as_deref()
                .and_then(|c| c.trim().parse().ok())
                .ok_or_else(|| bad("the trade names no venue".into()))?;
            Expectation {
                venue,
                reference: Some(super::facts::hashed(&trade.signature)),
                ..Default::default()
            }
        }
        "cancel_order" => {
            let (contract, token_id) = token(req.asset)?;
            Expectation {
                venue: chain
                    .legacy_marketplace
                    .ok_or_else(|| bad("the chain has no legacy marketplace".into()))?,
                contract: Some(contract),
                token_id,
                reference: req.listing.order_id.clone(),
                ..Default::default()
            }
        }
        "transfer" => {
            let (contract, token_id) = token(req.asset)?;
            let to = req.to.ok_or_else(|| bad("to is required".into()))?;
            Expectation {
                venue: contract,
                contract: Some(contract),
                token_id,
                owner: Some(to.address()),
                seller: Some(actor),
                ..Default::default()
            }
        }
        "claim_rental" | "cancel_rental" => {
            let (contract, token_id) = token(req.asset)?;
            let rentals = chain
                .rentals
                .ok_or_else(|| bad("the chain has no Rentals contract".into()))?;
            Expectation {
                venue: rentals,
                contract: Some(contract),
                token_id,
                reference: req.listing.rental_id.clone(),
                owner: (kind == "claim_rental").then_some(actor),
                ..Default::default()
            }
        }
        _ => Expectation {
            venue: req.to.map(|t| t.address()).unwrap_or(chain.mana),
            mana_token: Some(chain.mana),
            ..Default::default()
        },
    };
    Ok((kind.to_string(), expectation))
}

/// What the chain already says about who sent a reported transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reporter {
    /// The reporter sent it, signed its meta-transaction, or is the contract
    /// wallet a trusted event of its mined receipt names.
    Proven,
    /// Not known yet (pending, unreadable, or a contract wallet without that
    /// proof): the tracker decides once it is mined.
    Unknown,
    /// Mined, sent by another key, and the reporter has no code.
    Foreign,
}

async fn check_reporter(
    state: &super::State,
    chain_id: crate::types::ChainId,
    tx_hash: &str,
    kind: &str,
    actor: Address,
    e: &Expectation,
) -> Reporter {
    let Ok(Some(reader)) = state.reader(chain_id).await else {
        return Reporter::Unknown;
    };
    let Ok(Some((from, input))) = reader.transaction_sender(tx_hash).await else {
        return Reporter::Unknown;
    };
    if super::tracker::sent_by(actor, from, &input) {
        return Reporter::Proven;
    }
    // A pending transaction may deploy the reporter's account (ERC-4337).
    let Ok(Some(receipt)) = reader.receipt(tx_hash).await else {
        return Reporter::Unknown;
    };
    match reader.has_code(actor).await {
        Ok(false) => Reporter::Foreign,
        Ok(true) => {
            let row = expected_row(chain_id.0, tx_hash, kind, actor, e);
            if super::tracker::names_actor(&row, &receipt, actor) {
                Reporter::Proven
            } else {
                Reporter::Unknown
            }
        }
        Err(_) => Reporter::Unknown,
    }
}

/// The row a report would store, for checking a receipt against it.
fn expected_row(
    chain_id: u64,
    tx_hash: &str,
    kind: &str,
    actor: Address,
    e: &Expectation,
) -> ExecutionRow {
    let now = Utc::now().naive_utc();
    ExecutionRow {
        id: uuid::Uuid::nil(),
        chain_id: chain_id as i64,
        tx_hash: tx_hash.to_string(),
        kind: kind.to_string(),
        actor: lower(actor),
        venue: lower(e.venue),
        contract_address: e.contract.map(lower),
        token_id: e.token_id.map(|t| t.to_string()),
        reference: e.reference.clone(),
        expected_price: e.price.map(|p| p.to_string()),
        mana_token: e.mana_token.map(lower),
        expected_seller: e.seller.map(lower),
        expected_owner: e.owner.map(lower),
        status: "submitted".into(),
        block_number: None,
        confirmed_by: None,
        settlement: None,
        attempts: 0,
        last_error: None,
        next_check_at: now,
        created_at: now,
        updated_at: now,
    }
}

pub async fn post_execution(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let req: ExecutionRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "Bad Request",
                &format!("body: {e}"),
            );
        }
    };
    let actor = match state
        .require_signer(&headers, "post", &original, ROUTE)
        .await
    {
        Ok(a) => a,
        Err(r) => return r,
    };
    let Some(tx_hash) = valid_hash(&req.tx_hash) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Bad Request",
            "txHash: 0x and 64 hex digits",
        );
    };
    let (kind, expectation) = match expectation(&state, actor, &req).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let reporter = check_reporter(&state, req.chain_id, &tx_hash, &kind, actor, &expectation).await;
    if reporter == Reporter::Foreign {
        return error(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "the transaction was not sent by the signing wallet",
        );
    }
    match record(
        &state.pool,
        req.chain_id.0,
        &tx_hash,
        &kind,
        actor,
        &expectation,
        Utc::now().naive_utc(),
        reporter == Reporter::Proven,
    )
    .await
    {
        Ok((claimed, row)) => (
            if claimed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(json!(row.dto())),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "executions insert failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                "the execution could not be recorded",
            )
        }
    }
}

pub async fn get_execution(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let Ok(id) = uuid::Uuid::parse_str(&id) else {
        return error(StatusCode::NOT_FOUND, "Not Found", "no such execution");
    };
    match get(&state.pool, id).await {
        Ok(Some(row)) => Json(json!(row.dto())).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "Not Found", "no such execution"),
        Err(e) => {
            tracing::error!(error = %e, "executions read failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                "the execution could not be read",
            )
        }
    }
}

pub async fn list_executions(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let actor = match state
        .require_signer(&headers, "get", &original, ROUTE)
        .await
    {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Some(address) = params.get("address")
        && !address.eq_ignore_ascii_case(&lower(actor))
    {
        return error(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "a wallet lists its own executions",
        );
    }
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    match by_actor(&state.pool, &lower(actor), limit).await {
        Ok(rows) => Json(json!({ "data": rows.iter().map(|r| r.dto()).collect::<Vec<_>>(), "total": rows.len() })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "executions list failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error", "the executions could not be read")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Polygon JSON-RPC stub: transactions by hash (`from`, `input`), their
    /// receipts (absent: pending) and per-address code.
    #[derive(Default)]
    struct Stub {
        txs: std::collections::HashMap<String, (Address, Option<Value>)>,
        code: std::collections::HashSet<Address>,
    }

    async fn stub_rpc(
        axum::extract::State(stub): axum::extract::State<std::sync::Arc<Stub>>,
        axum::Json(req): axum::Json<Value>,
    ) -> axum::Json<Value> {
        let p0 = req["params"][0]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let result = match req["method"].as_str().unwrap() {
            "eth_chainId" => json!("0x89"),
            "eth_blockNumber" => json!("0x64"),
            "eth_getCode" => {
                let a: Address = p0.parse().unwrap();
                json!(if stub.code.contains(&a) {
                    "0x6000"
                } else {
                    "0x"
                })
            }
            "eth_getTransactionByHash" => match stub.txs.get(&p0) {
                Some((from, _)) => json!({ "from": format!("{from:#x}"), "input": "0x" }),
                None => Value::Null,
            },
            "eth_getTransactionReceipt" => stub
                .txs
                .get(&p0)
                .and_then(|(_, r)| r.clone())
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };
        axum::Json(json!({ "jsonrpc": "2.0", "id": req["id"], "result": result }))
    }

    fn mined(hash: &str, from: Address, logs: Vec<Value>) -> Value {
        json!({ "transactionHash": hash, "status": "0x1", "blockNumber": "0x10",
                "from": format!("{from:#x}"), "to": format!("{:#x}", Address::repeat_byte(0xee)), "logs": logs })
    }

    fn mana_out(emitter: Address, from: Address) -> Value {
        json!({ "address": format!("{emitter:#x}"),
                "topics": [crate::market::nft_index::TOPIC_TRANSFER,
                           format!("0x{:0>64}", hex::encode(from)), format!("0x{:0>64}", hex::encode(Address::repeat_byte(0x5e)))],
                "data": format!("0x{:064x}", 5u8) })
    }

    /// The reporter check and the tracker over one stub chain: an EOA cannot
    /// claim another key's transaction; a contract wallet can, on a MANA
    /// transfer of its own emitted by the row's MANA token; anything else waits
    /// for (and is refused by) the tracker.
    #[tokio::test]
    async fn reports_of_another_keys_transaction_through_the_chain() {
        let Some(market) =
            crate::market::tests::support::state_with_db(crate::market::Settings::for_tests())
                .await
        else {
            return;
        };
        let pool = market.pool.clone();
        let (owner, eoa, wallet) = (
            Address::repeat_byte(0x01),
            Address::repeat_byte(0xe0),
            Address::repeat_byte(0xc0),
        );
        let (mana, other) = (Address::repeat_byte(0xaa), Address::repeat_byte(0x99));
        let hash = |n: u8| {
            format!(
                "0x{}{:02x}",
                &uuid::Uuid::new_v4().simple().to_string().repeat(2)[..62],
                n
            )
        };
        let (foreign, safe, forged, pending) = (hash(1), hash(2), hash(3), hash(4));
        let mut stub = Stub::default();
        stub.code.insert(wallet);
        stub.txs.insert(
            foreign.clone(),
            (
                owner,
                Some(mined(&foreign, owner, vec![mana_out(mana, eoa)])),
            ),
        );
        stub.txs.insert(
            safe.clone(),
            (
                owner,
                Some(mined(&safe, owner, vec![mana_out(mana, wallet)])),
            ),
        );
        stub.txs.insert(
            forged.clone(),
            (
                owner,
                Some(mined(&forged, owner, vec![mana_out(other, wallet)])),
            ),
        );
        stub.txs.insert(pending.clone(), (owner, None));
        let app = axum::Router::new()
            .route("/", axum::routing::post(stub_rpc))
            .with_state(std::sync::Arc::new(stub));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let rentals = std::sync::Arc::new(crate::rentals::State::new(
            pool.clone(),
            None,
            crate::rentals::Settings {
                writes_enabled: false,
            },
            None,
        ));
        let state = super::super::State::with_venues(
            pool.clone(),
            crate::http::Outbound::for_tests(),
            market,
            rentals,
            super::super::Settings::default(),
            super::super::venues::Book::production(|id| (id == 137).then(|| url.clone())),
        );
        let e = Expectation {
            venue: Address::repeat_byte(0xee),
            mana_token: Some(mana),
            ..Default::default()
        };
        let check = |h: String, actor: Address| {
            let (state, e) = (&state, &e);
            async move {
                check_reporter(
                    state,
                    crate::types::ChainId(137),
                    &h,
                    "approve_mana",
                    actor,
                    e,
                )
                .await
            }
        };
        assert_eq!(
            check(foreign.clone(), eoa).await,
            Reporter::Foreign,
            "the POST answers 403"
        );
        assert_eq!(check(safe.clone(), wallet).await, Reporter::Proven);
        assert_eq!(
            check(forged.clone(), wallet).await,
            Reporter::Unknown,
            "left to the tracker"
        );
        assert_eq!(
            check(pending.clone(), eoa).await,
            Reporter::Unknown,
            "a pending transaction may deploy the reporter's account"
        );

        let now = Utc::now().naive_utc();
        let mut steps = Vec::new();
        for (h, actor) in [(&foreign, eoa), (&safe, wallet), (&forged, wallet)] {
            let (_, row) = record(&pool, 137, h, "approve_mana", actor, &e, now, false)
                .await
                .unwrap();
            let status = super::super::tracker::step(&state, &row, now)
                .await
                .unwrap();
            let stored = get(&pool, row.id).await.unwrap().unwrap();
            let sender_mismatch = stored
                .settlement
                .as_ref()
                .and_then(|s| s.get(super::super::tracker::SENDER_MISMATCH))
                .is_some();
            steps.push((status, sender_mismatch));
            sqlx::query("DELETE FROM shop.executions WHERE id = $1")
                .bind(row.id)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(
            steps[0],
            ("mismatch", true),
            "an EOA actor with a foreign sender"
        );
        assert!(
            !steps[1].1,
            "the Safe-like wallet's own MANA transfer: {:?}",
            steps[1]
        );
        assert_eq!(
            steps[2],
            ("mismatch", true),
            "a transfer from another contract proves nothing"
        );
    }

    #[tokio::test]
    async fn a_row_another_wallet_sent_is_claimed_by_the_next_reporter() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let hash = format!("0x{}", uuid::Uuid::new_v4().simple().to_string().repeat(2));
        let (first, second, third) = (
            Address::repeat_byte(0xa1),
            Address::repeat_byte(0xa2),
            Address::repeat_byte(0xa3),
        );
        let e = Expectation {
            venue: Address::repeat_byte(0x01),
            ..Default::default()
        };
        let now = Utc::now().naive_utc();
        let (claimed, row) = record(&pool, 137, &hash, "approve_mana", first, &e, now, false)
            .await
            .unwrap();
        assert!(claimed);
        let (claimed, kept) = record(&pool, 137, &hash, "approve_mana", second, &e, now, true)
            .await
            .unwrap();
        assert!(!claimed, "an unchecked row keeps its reporter");
        assert_eq!(kept.actor, row.actor);

        sqlx::query(
            "UPDATE shop.executions SET status = 'mismatch', settlement = $2 WHERE id = $1",
        )
        .bind(row.id)
        .bind(json!({ (crate::mana::tracker::SENDER_MISMATCH): true }))
        .execute(&pool)
        .await
        .unwrap();
        let (claimed, _) = record(&pool, 137, &hash, "approve_mana", third, &e, now, false)
            .await
            .unwrap();
        assert!(
            !claimed,
            "a reporter not proven to have sent it cannot re-claim"
        );
        let (claimed, reclaimed) = record(&pool, 137, &hash, "approve_mana", second, &e, now, true)
            .await
            .unwrap();
        assert!(
            claimed,
            "the sender's report replaces a foreign reporter's row"
        );
        assert_eq!(reclaimed.id, row.id);
        assert_eq!(reclaimed.actor, format!("{second:#x}"));
        assert_eq!(reclaimed.status, "submitted");
        let (claimed, _) = record(&pool, 137, &hash, "approve_mana", third, &e, now, true)
            .await
            .unwrap();
        assert!(
            !claimed,
            "a re-claimed row is checked before anyone else claims it"
        );

        sqlx::query("DELETE FROM shop.executions WHERE id = $1")
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[test]
    fn the_kinds_and_statuses_are_the_schemas() {
        let ddl = crate::db::SCHEMA_SQL;
        let kinds: Vec<String> = KINDS.iter().map(|k| format!("'{k}'")).collect();
        assert!(
            ddl.contains(&format!("CHECK (kind IN ({}))", kinds.join(", "))),
            "executions_kind_known"
        );
        let statuses: Vec<String> = STATUSES.iter().map(|s| format!("'{s}'")).collect();
        assert!(
            ddl.contains(&format!("CHECK (status IN ({}))", statuses.join(", "))),
            "executions_status_known"
        );
        assert_eq!(
            valid_hash(&format!("0x{}", "AB".repeat(32))),
            Some(format!("0x{}", "ab".repeat(32)))
        );
        assert_eq!(
            valid_hash(&format!("0X{}", "ab".repeat(32))),
            Some(format!("0x{}", "ab".repeat(32))),
            "a 0X prefix is accepted"
        );
        assert_eq!(valid_hash("0x12"), None);
        for kind in super::super::wire::ActionKind::ALL {
            assert!(KINDS.contains(&kind.execution_kind()), "{kind:?}");
        }
    }
}
