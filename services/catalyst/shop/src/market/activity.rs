//! `GET /v1/activity?address=` (signed by that address, a marketplace or
//! builder signer): one wallet's market history, newest first: the sales
//! it bought and sold, the bids it placed and received, the orders it
//! created and filled, and the trades it signed or is a beneficiary of.
//! Each source is read to its newest 500 (the old internal cap; the
//! subgraph readers in pages of 100, the trades from the local table), the
//! events are merged by time, one per transaction and role (or per
//! token/time/role without a transaction), and paged in memory (`limit` at
//! most 500, `offset`). A source that cannot be read contributes nothing,
//! as before.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

use super::SharedState;
use super::bids::{Bid, BidFilters, list_bids};
use super::orders::{Order, OrderFilters, list_orders};
use super::readers::READER_MAX_FIRST;
use super::sales::{Sale, SaleFilters, list_sales};
use super::signed::{MARKETPLACE_AUTH_SIGNERS, require_auth_metadata};
use super::trades::{Trade, TradeAsset, trades_of_address};
use super::wire::{ApiError, get_number_parameter};
use crate::shared::signed_fetch::{
    self, AUTH_CHAIN_HEADER_PREFIX, AUTH_TIMESTAMP_HEADER, PayloadPolicy, SignedFetchError,
};
use crate::types::{ASSET_TYPE_ERC20, ASSET_TYPE_USD_PEGGED_MANA};

const INTERNAL_FETCH_CAP: i64 = 500;
const MAX_PAGE_SIZE: i64 = 500;

#[derive(Debug, Serialize)]
pub struct ActivityEvent {
    pub id: String,
    pub timestamp: i64,
    pub network: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(rename = "txHash", skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(rename = "contractAddress", skip_serializing_if = "Option::is_none")]
    pub contract_address: Option<String>,
    #[serde(rename = "tokenId", skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    #[serde(rename = "itemId", skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    pub details: Value,
}

#[derive(Debug, Serialize)]
pub struct ActivityEnvelope {
    pub data: Vec<ActivityEvent>,
    pub total: i64,
}

fn network_name<T: Serialize>(network: &T) -> String {
    serde_json::to_value(network)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Every page of a subgraph-backed source up to the internal cap.
async fn pages<T, F, Fut>(fetch: F) -> Vec<T>
where
    F: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<(Vec<T>, i64), Response>>,
{
    let skips: Vec<i64> = (0..INTERNAL_FETCH_CAP)
        .step_by(READER_MAX_FIRST as usize)
        .collect();
    let results = futures::future::join_all(skips.into_iter().map(&fetch)).await;
    let mut out = Vec::new();
    for result in results {
        match result {
            Ok((rows, _)) => {
                let short = (rows.len() as i64) < READER_MAX_FIRST;
                out.extend(rows);
                if short {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    out
}

pub async fn user_activity(
    state: &super::MarketplaceState,
    address: &str,
    limit: Option<i64>,
    offset: Option<i64>,
) -> (Vec<ActivityEvent>, i64) {
    let limit = limit
        .filter(|v| *v > 0)
        .unwrap_or(MAX_PAGE_SIZE)
        .min(MAX_PAGE_SIZE);
    let offset = offset.filter(|v| *v > 0).unwrap_or(0);
    let lower = address.to_lowercase();
    let first = Some(READER_MAX_FIRST);
    let (sales_buyer, sales_seller, bids_bidder, bids_seller, orders_owner, orders_buyer, trades) = tokio::join!(
        pages(|skip| {
            let f = SaleFilters {
                buyer: Some(lower.clone()),
                first,
                skip: Some(skip),
                ..Default::default()
            };
            async move { list_sales(state, &f).await }
        }),
        pages(|skip| {
            let f = SaleFilters {
                seller: Some(lower.clone()),
                first,
                skip: Some(skip),
                ..Default::default()
            };
            async move { list_sales(state, &f).await }
        }),
        pages(|skip| {
            let f = BidFilters {
                bidder: Some(lower.clone()),
                limit: READER_MAX_FIRST,
                offset: skip,
                ..Default::default()
            };
            async move { list_bids(state, &f).await }
        }),
        pages(|skip| {
            let f = BidFilters {
                seller: Some(lower.clone()),
                limit: READER_MAX_FIRST,
                offset: skip,
                ..Default::default()
            };
            async move { list_bids(state, &f).await }
        }),
        pages(|skip| {
            let f = OrderFilters {
                owner: Some(lower.clone()),
                first,
                skip: Some(skip),
                ..Default::default()
            };
            async move { list_orders(state, &f).await }
        }),
        pages(|skip| {
            let f = OrderFilters {
                buyer: Some(lower.clone()),
                first,
                skip: Some(skip),
                ..Default::default()
            };
            async move { list_orders(state, &f).await }
        }),
        trades_of_address(
            &state.pool,
            &lower,
            INTERNAL_FETCH_CAP,
            super::trades::mirrored_since(&state.settings, chrono::Utc::now().naive_utc()),
        ),
    );
    let trades = trades.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "activity: the trades leg failed");
        Vec::new()
    });
    let mut events: Vec<ActivityEvent> = Vec::new();
    events.extend(sales_buyer.iter().map(|s| sale_to_event(s, "sale_buyer")));
    events.extend(sales_seller.iter().map(|s| sale_to_event(s, "sale_seller")));
    events.extend(bids_bidder.iter().map(|b| bid_to_event(b, "bid_placed")));
    events.extend(
        bids_seller
            .iter()
            .filter(|b| b.bidder.to_lowercase() != lower)
            .map(|b| bid_to_event(b, "bid_received")),
    );
    events.extend(
        orders_owner
            .iter()
            .map(|o| order_to_event(o, "order_created")),
    );
    events.extend(
        orders_buyer
            .iter()
            .filter(|o| o.status == "sold")
            .map(|o| order_to_event(o, "order_filled")),
    );
    events.extend(trades.iter().map(trade_to_event));
    events.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    let mut seen = HashSet::new();
    events.retain(|e| seen.insert(dedup_key(e)));
    let total = events.len() as i64;
    (
        events
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect(),
        total,
    )
}

pub async fn get_activity(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    match activity(&state, &headers, &pairs).await {
        Ok(envelope) => Json(envelope).into_response(),
        Err(e) => e.into_response(),
    }
}

/// The old route's own wording and codes, stage by stage: a missing chain or
/// timestamp is a 400 before the signer allow-list, an expired or failing
/// signature a 401, an EIP-1654 chain a 501, anything else malformed a 400.
fn refusal(e: SignedFetchError) -> ApiError {
    match e {
        SignedFetchError::Expired { .. } => ApiError::unauthorized("Expired signature"),
        SignedFetchError::InvalidSignature(_) => ApiError::unauthorized("Invalid Auth Chain"),
        SignedFetchError::EipNotImplemented => {
            ApiError::http(501, "EIP-1654 not supported on this route")
        }
        _ => ApiError::bad_request("Invalid Auth Chain"),
    }
}

async fn activity(
    state: &super::MarketplaceState,
    headers: &HeaderMap,
    pairs: &[(String, String)],
) -> Result<ActivityEnvelope, ApiError> {
    if !headers.contains_key(format!("{AUTH_CHAIN_HEADER_PREFIX}0").as_str())
        || !headers.contains_key(AUTH_TIMESTAMP_HEADER)
    {
        return Err(ApiError::bad_request("Invalid Auth Chain"));
    }
    require_auth_metadata(headers, MARKETPLACE_AUTH_SIGNERS, None)?;
    let recovered = signed_fetch::require_signer(
        headers,
        "get",
        "/v1/activity",
        PayloadPolicy::V6ThenLegacy,
        None,
    )
    .await
    .map_err(refusal)?
    .into_string();
    let query_address = pairs
        .iter()
        .find(|(k, _)| k == "address")
        .map(|(_, v)| v.to_lowercase())
        .ok_or_else(|| ApiError::bad_request("Unauthorized"))?;
    if recovered != query_address {
        return Err(ApiError::bad_request("Forbidden: address mismatch"));
    }
    let limit = get_number_parameter("limit", pairs)?;
    let offset = get_number_parameter("offset", pairs)?;
    let (data, total) = user_activity(state, &recovered, limit, offset).await;
    Ok(ActivityEnvelope { data, total })
}

fn dedup_key(e: &ActivityEvent) -> String {
    match &e.tx_hash {
        Some(tx) => format!("tx:{}:{}", tx.to_lowercase(), e.event_type),
        None => format!(
            "{}|{}|{}|{}",
            e.contract_address.as_deref().unwrap_or("-"),
            e.token_id
                .as_deref()
                .or(e.item_id.as_deref())
                .unwrap_or("-"),
            e.timestamp,
            e.event_type
        ),
    }
}

fn sale_to_event(s: &Sale, ty: &str) -> ActivityEvent {
    ActivityEvent {
        id: format!("{ty}:{}", s.id),
        timestamp: s.timestamp,
        network: network_name(&s.network),
        event_type: ty.to_string(),
        tx_hash: Some(s.tx_hash.clone()),
        contract_address: Some(s.contract_address.clone()),
        token_id: s.token_id.clone(),
        item_id: s.item_id.clone(),
        price: Some(s.price.clone()),
        counterparty: if ty == "sale_buyer" {
            Some(s.seller.clone())
        } else {
            Some(s.buyer.clone())
        },
        details: json!({ "sale": s }),
    }
}

fn bid_to_event(b: &Bid, ty: &str) -> ActivityEvent {
    ActivityEvent {
        id: format!("{ty}:{}", b.id),
        timestamp: b.created_at,
        network: network_name(&b.network),
        event_type: ty.to_string(),
        tx_hash: None,
        contract_address: Some(b.contract_address.clone()),
        token_id: b.token_id.clone(),
        item_id: b.item_id.clone(),
        price: Some(b.price.clone()),
        counterparty: if ty == "bid_placed" {
            Some(b.seller.clone())
        } else {
            Some(b.bidder.clone())
        },
        details: json!({ "bid": b }),
    }
}

fn order_to_event(o: &Order, ty: &str) -> ActivityEvent {
    ActivityEvent {
        id: format!("{ty}:{}", o.id),
        timestamp: o.created_at,
        network: network_name(&o.network),
        event_type: ty.to_string(),
        tx_hash: None,
        contract_address: Some(o.contract_address.clone()),
        token_id: o.token_id.clone(),
        item_id: None,
        price: Some(o.price.clone()),
        counterparty: if ty == "order_created" {
            None
        } else {
            Some(o.owner.clone())
        },
        details: json!({ "order": o }),
    }
}

fn trade_to_event(t: &Trade) -> ActivityEvent {
    let is_payment = |a: &&TradeAsset| {
        a.asset_type == ASSET_TYPE_ERC20 || a.asset_type == ASSET_TYPE_USD_PEGGED_MANA
    };
    let assets = t.sent.iter().chain(t.received.iter());
    let non_payment = assets.clone().find(|a| !is_payment(a));
    let payment = assets.clone().find(is_payment);
    ActivityEvent {
        id: format!("trade_created:{}", t.id),
        timestamp: t.created_at.timestamp_millis(),
        network: t.network.clone(),
        event_type: "trade_created".to_string(),
        tx_hash: None,
        contract_address: non_payment.map(|a| a.contract_address.clone()),
        token_id: non_payment.and_then(|a| a.token_id.clone()),
        item_id: non_payment.and_then(|a| a.item_id.clone()),
        price: payment.and_then(|a| a.amount.clone()),
        counterparty: None,
        details: json!({ "trade": t }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::wire::{ChainId, Network};
    use chrono::TimeZone;

    fn asset(asset_type: i32, token_id: Option<&str>, amount: Option<&str>) -> TradeAsset {
        TradeAsset {
            asset_type,
            contract_address: "0xnft".into(),
            beneficiary: None,
            extra: String::new(),
            amount: amount.map(|s| s.to_string()),
            token_id: token_id.map(|s| s.to_string()),
            item_id: None,
        }
    }

    fn trade(sent: Vec<TradeAsset>, received: Vec<TradeAsset>) -> Trade {
        Trade {
            id: "t1".into(),
            signer: "0xsigner".into(),
            signature: String::new(),
            trade_type: "public_nft_order".into(),
            network: "ETHEREUM".into(),
            chain_id: 1,
            checks: Value::Null,
            created_at: chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            sent,
            received,
            contract: String::new(),
            mirrored_from: None,
        }
    }

    #[test]
    fn trade_event_picks_non_payment_and_payment() {
        let ev = trade_to_event(&trade(
            vec![asset(3, Some("42"), None)],
            vec![asset(1, None, Some("1000"))],
        ));
        assert_eq!(ev.id, "trade_created:t1");
        assert_eq!(ev.event_type, "trade_created");
        assert_eq!(ev.contract_address.as_deref(), Some("0xnft"));
        assert_eq!(ev.token_id.as_deref(), Some("42"));
        assert_eq!(ev.price.as_deref(), Some("1000"));
        assert_eq!(ev.counterparty, None);
        assert_eq!(ev.timestamp, 1_700_000_000_000);
    }

    #[test]
    fn trade_event_treats_usd_pegged_mana_as_payment() {
        let ev = trade_to_event(&trade(
            vec![asset(2, None, Some("500"))],
            vec![asset(4, None, None)],
        ));
        assert_eq!(ev.price.as_deref(), Some("500"));
        assert_eq!(ev.contract_address.as_deref(), Some("0xnft"));
    }

    #[test]
    fn trade_event_has_no_tx_hash() {
        assert_eq!(
            trade_to_event(&trade(
                vec![asset(3, Some("1"), None)],
                vec![asset(1, None, Some("9"))]
            ))
            .tx_hash,
            None
        );
    }

    fn ev(
        event_type: &str,
        tx_hash: Option<&str>,
        contract: Option<&str>,
        token: Option<&str>,
        item: Option<&str>,
        timestamp: i64,
    ) -> ActivityEvent {
        ActivityEvent {
            id: format!("{event_type}:x"),
            timestamp,
            network: "ETHEREUM".into(),
            event_type: event_type.into(),
            tx_hash: tx_hash.map(|s| s.to_string()),
            contract_address: contract.map(|s| s.to_string()),
            token_id: token.map(|s| s.to_string()),
            item_id: item.map(|s| s.to_string()),
            price: None,
            counterparty: None,
            details: json!({}),
        }
    }

    #[test]
    fn dedup_key_uses_lowercased_txhash_and_type_when_present() {
        assert_eq!(
            dedup_key(&ev(
                "sale_buyer",
                Some("0xABCdef"),
                Some("0xnft"),
                Some("42"),
                None,
                100
            )),
            "tx:0xabcdef:sale_buyer"
        );
    }

    #[test]
    fn dedup_key_keeps_buyer_and_seller_of_same_tx_distinct() {
        assert_ne!(
            dedup_key(&ev("sale_buyer", Some("0xdead"), None, None, None, 1)),
            dedup_key(&ev("sale_seller", Some("0xdead"), None, None, None, 1))
        );
    }

    #[test]
    fn dedup_key_falls_back_to_composite_without_txhash() {
        assert_eq!(
            dedup_key(&ev("bid_placed", None, Some("0xNFT"), None, Some("7"), 55)),
            "0xNFT|7|55|bid_placed"
        );
        assert_eq!(
            dedup_key(&ev("trade_created", None, None, None, None, 9)),
            "-|-|9|trade_created"
        );
    }

    async fn insert_trade(pool: &sqlx::PgPool, signer: &str, beneficiary: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let signature = format!("0x{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO shop.trades (id, network, chain_id, signature, hashed_signature, checks, signer, type, expires_at, effective_since) \
             VALUES ($1::uuid, 'MATIC', 137, $2, $2, '{}'::jsonb, $3, 'public_nft_order', timezone('UTC', now()) + interval '1 day', timezone('UTC', now()))",
        )
        .bind(&id)
        .bind(&signature)
        .bind(signer)
        .execute(pool)
        .await
        .unwrap();
        for (direction, asset_type, table, column, value, who) in [
            ("sent", 3i16, "trade_assets_erc721", "token_id", "42", None),
            (
                "received",
                1,
                "trade_assets_erc20",
                "amount",
                "1000",
                Some(beneficiary),
            ),
        ] {
            let asset_id = uuid::Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO shop.trade_assets (id, trade_id, direction, asset_type, contract_address, beneficiary, extra) VALUES ($1::uuid, $2::uuid, $3::shop.asset_direction_type, $4, '0x0000000000000000000000000000000000000abc', $5, '')")
                .bind(&asset_id)
                .bind(&id)
                .bind(direction)
                .bind(asset_type)
                .bind(who)
                .execute(pool)
                .await
                .unwrap();
            let cast = if asset_type == 1 { "::numeric" } else { "" };
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO shop.{table} (asset_id, {column}) VALUES ($1::uuid, $2{cast})"
            )))
            .bind(&asset_id)
            .bind(value)
            .execute(pool)
            .await
            .unwrap();
        }
        id
    }

    async fn call(
        state: crate::market::SharedState,
        uri: &str,
        headers: HeaderMap,
    ) -> (axum::http::StatusCode, Value) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder().uri(uri);
        for (k, v) in headers.iter() {
            req = req.header(k, v);
        }
        let resp = crate::market::market_router(state)
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Under local authority a mirrored copy counts only within the trades
    /// budget (`trades::mirrored_since`), as on `/v1/trades`: a leftover past
    /// it leaves the activity on either leg, a fresh copy and a local trade stay.
    #[tokio::test]
    async fn a_leftover_mirrored_trade_leaves_the_activity_past_the_budget() {
        let settings = crate::market::Settings {
            data_writes_enabled: true,
            ..crate::market::Settings::for_tests()
        };
        let Some(state) = crate::market::tests::support::state_with_db(settings.clone()).await
        else {
            return;
        };
        let addr = || format!("0x{:0>40}", uuid::Uuid::new_v4().simple());
        let (me, other) = (addr(), addr());
        let local = insert_trade(&state.pool, &me, &other).await;
        let fresh = insert_trade(&state.pool, &me, &other).await;
        let stale_signed = insert_trade(&state.pool, &me, &other).await;
        let stale_received = insert_trade(&state.pool, &other, &me).await;
        let budget = crate::market::trades::max_age(settings.trades_sync_interval).as_secs() as i32;
        // Distinct instants, so no two collapse under the activity's dedup key.
        sqlx::query(
            "UPDATE shop.trades SET \
                created_at = timezone('UTC', now()) - make_interval(secs => array_position($1::uuid[], id)), \
                mirrored_from = CASE WHEN id = ($1::uuid[])[1] THEN NULL ELSE 'https://former-upstream.example/v1/trades' END, \
                mirrored_at = CASE WHEN id = ($1::uuid[])[2] THEN timezone('UTC', now()) \
                                   ELSE timezone('UTC', now()) - make_interval(secs => $2 + 60) END \
             WHERE id = ANY($1::uuid[])",
        )
        .bind(
            [&local, &fresh, &stale_signed, &stale_received]
                .iter()
                .map(|id| uuid::Uuid::parse_str(id).unwrap())
                .collect::<Vec<_>>(),
        )
        .bind(budget)
        .execute(&state.pool)
        .await
        .unwrap();
        let (events, _) = user_activity(&state, &me, None, None).await;
        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        for (id, want) in [
            (&local, true),
            (&fresh, true),
            (&stale_signed, false),
            (&stale_received, false),
        ] {
            assert_eq!(
                ids.contains(&format!("trade_created:{id}").as_str()),
                want,
                "{id}: {ids:?}"
            );
        }
    }

    /// The route over the local trades (no subgraph configured, so the
    /// reader legs contribute nothing, as a failing source did before): the
    /// signer's own trades and the ones naming it as beneficiary, not a
    /// stranger's; the old status and wording of every refusal.
    #[tokio::test]
    async fn activity_answers_the_signer_its_trades_and_refuses_as_before() {
        use crate::shared::signed_fetch::test_support::{address, signed_headers};
        let Some(state) =
            crate::market::tests::support::state_with_db(crate::market::Settings::for_tests())
                .await
        else {
            return;
        };
        let me = address();
        let other = format!("0x{}", &uuid::Uuid::new_v4().simple().to_string()[..32])
            .to_lowercase()
            + "00000000";
        let mine = insert_trade(&state.pool, &me, &other).await;
        let to_me = insert_trade(&state.pool, &other, &me).await;
        let stranger = insert_trade(&state.pool, &other, &other).await;
        let marketplace = r#"{"signer":"dcl:marketplace"}"#;

        let (status, body) = call(
            state.clone(),
            &format!(
                "/v1/activity?address={}",
                me.to_uppercase().replacen("0X", "0x", 1)
            ),
            signed_headers("get", "/v1/activity", marketplace, true).await,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["id"].as_str())
            .collect();
        assert!(
            ids.contains(&format!("trade_created:{mine}").as_str()),
            "{ids:?}"
        );
        assert!(
            ids.contains(&format!("trade_created:{to_me}").as_str()),
            "{ids:?}"
        );
        assert!(
            !ids.contains(&format!("trade_created:{stranger}").as_str()),
            "{ids:?}"
        );
        assert!(body["total"].as_i64().unwrap() >= 2);
        let event = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == format!("trade_created:{mine}"))
            .unwrap();
        assert_eq!(
            (
                event["tokenId"].as_str(),
                event["price"].as_str(),
                event["type"].as_str()
            ),
            (Some("42"), Some("1000"), Some("trade_created"))
        );

        let (status, body) = call(
            state.clone(),
            &format!("/v1/activity?address={me}&limit=1"),
            signed_headers("get", "/v1/activity", marketplace, false).await,
        )
        .await;
        assert_eq!(
            (status, body["data"].as_array().map(Vec::len)),
            (axum::http::StatusCode::OK, Some(1)),
            "{body}"
        );

        let (status, body) = call(
            state.clone(),
            &format!("/v1/activity?address={other}"),
            signed_headers("get", "/v1/activity", marketplace, true).await,
        )
        .await;
        assert_eq!(
            (status, body["message"].as_str()),
            (
                axum::http::StatusCode::BAD_REQUEST,
                Some("Forbidden: address mismatch")
            ),
            "{body}"
        );
        let (status, body) = call(
            state.clone(),
            &format!("/v1/activity?address={me}"),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            (status, body["message"].as_str()),
            (
                axum::http::StatusCode::BAD_REQUEST,
                Some("Invalid Auth Chain")
            ),
            "{body}"
        );
        let (status, body) = call(
            state.clone(),
            &format!("/v1/activity?address={me}"),
            signed_headers("get", "/v1/activity", r#"{"signer":"dcl:other"}"#, true).await,
        )
        .await;
        assert_eq!(
            (status, body["message"].as_str()),
            (
                axum::http::StatusCode::BAD_REQUEST,
                Some("Invalid auth signer")
            ),
            "{body}"
        );
        let (status, body) = call(
            state.clone(),
            &format!("/v1/activity?address={me}"),
            signed_headers("get", "/v1/other", marketplace, true).await,
        )
        .await;
        assert_eq!(
            (status, body["message"].as_str()),
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Some("Invalid Auth Chain")
            ),
            "{body}"
        );
        let (status, _) = call(
            state.clone(),
            &format!("/v1/activity?address={me}&limit=x"),
            signed_headers("get", "/v1/activity", marketplace, true).await,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

        sqlx::query("DELETE FROM shop.trades WHERE id = ANY($1::uuid[])")
            .bind(vec![mine, to_me, stranger])
            .execute(&state.pool)
            .await
            .unwrap();
    }

    #[test]
    fn sale_event_threads_and_serializes_tx_hash() {
        let sale = Sale {
            id: "s1".into(),
            item_id: None,
            contract_address: "0xnft".into(),
            buyer: "0xbuyer".into(),
            chain_id: ChainId::EthereumMainnet,
            network: Network::Ethereum,
            price: "1000".into(),
            seller: "0xseller".into(),
            timestamp: 100,
            token_id: Some("42".into()),
            tx_hash: "0xTXHASH".into(),
            sale_type: "order".into(),
        };
        let e = sale_to_event(&sale, "sale_buyer");
        assert_eq!(e.tx_hash.as_deref(), Some("0xTXHASH"));
        assert_eq!(e.network, "ETHEREUM");
        assert_eq!(serde_json::to_value(&e).unwrap()["txHash"], "0xTXHASH");
    }
}
