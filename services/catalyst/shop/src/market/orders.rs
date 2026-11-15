//! `/v1/orders` and `/v1/orders/open-by-items`: the off-chain v3 leg from the
//! trades mirror (an open `public_nft_order` is one that is effective, not
//! expired and not in `trade_cancellations`; the old "signer still owns the
//! nft" guard becomes `graph::owned_urns` on the item URN, so a listing whose
//! signer no longer holds any copy of the item drops out) merged with the
//! legacy on-chain `orders` of the collections subgraph reader (first ≤ 100,
//! 30 s). The two legs are sorted together in memory over the `first + skip`
//! window, read past the graph's 5,000 skip by keyset up to
//! `readers::DEEP_WINDOW` (a deeper window is a 400). The legacy leg's total
//! is a count capped at 5,000, or at the deep window for a deeper page.
//! `nftName` is parsed and not applied, as the old query builder did (it
//! selected the name and never filtered on it).

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;

use super::catalog::{ITEM_ID_SHIFT, unified_trades_cte};
use super::graph;
use super::readers::{self, GraphPage, Ttl, seconds_to_ms};
use super::wire::{
    ApiError, ChainId, DataTotalString, InvalidParameterError, Network, Params, clamp_first,
    clamp_skip, network_and_chain,
};
use super::{MarketplaceState, SharedState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSortBy {
    Oldest,
    RecentlyListed,
    RecentlyUpdated,
    Cheapest,
    IssuedIdAsc,
    IssuedIdDesc,
}

#[derive(Debug, Clone, Default)]
pub struct OrderFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub sort_by: Option<OrderSortBy>,
    pub marketplace_address: Option<String>,
    pub owner: Option<String>,
    pub buyer: Option<String>,
    pub contract_address: Option<String>,
    pub token_id: Option<String>,
    pub status: Option<String>,
    pub network: Option<Network>,
    pub item_id: Option<String>,
    pub nft_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct Order {
    pub id: String,
    #[serde(rename = "marketplaceAddress")]
    pub marketplace_address: String,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    #[serde(rename = "tokenId")]
    pub token_id: Option<String>,
    pub owner: String,
    pub buyer: Option<String>,
    pub price: String,
    pub status: String,
    #[serde(rename = "expiresAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub expires_at: i64,
    #[serde(rename = "createdAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub updated_at: i64,
    pub network: Network,
    #[serde(rename = "chainId")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    #[serde(rename = "issuedId")]
    pub issued_id: Option<String>,
    #[serde(rename = "tradeId")]
    pub trade_id: Option<String>,
    /// Who a v3 listing pays (its received asset's beneficiary); not on the wire.
    #[serde(skip)]
    pub payee: Option<String>,
}

/// The sort keys both legs carry so one comparison orders the merged list.
#[derive(Debug, Clone)]
pub struct Keyed {
    pub order: Order,
    pub sort_price: u128,
    pub sort_token_id: String,
}

pub fn from_seconds_to_milliseconds(time: f64) -> i64 {
    if time < 1e11_f64 {
        (time * 1000.0).round() as i64
    } else {
        time.round() as i64
    }
}

pub fn parse_filters(pairs: &[(String, String)]) -> Result<OrderFilters, InvalidParameterError> {
    let p = Params::new(pairs);
    let sort_by = p
        .get_value(
            "sortBy",
            &[
                "oldest",
                "recently_listed",
                "recently_updated",
                "cheapest",
                "issued_id_asc",
                "issued_id_desc",
            ],
            None,
        )
        .map(|s| match s.as_str() {
            "oldest" => OrderSortBy::Oldest,
            "recently_updated" => OrderSortBy::RecentlyUpdated,
            "cheapest" => OrderSortBy::Cheapest,
            "issued_id_asc" => OrderSortBy::IssuedIdAsc,
            "issued_id_desc" => OrderSortBy::IssuedIdDesc,
            _ => OrderSortBy::RecentlyListed,
        });
    let network = p
        .get_value("network", &["ETHEREUM", "MATIC"], None)
        .map(|s| {
            if s == "ETHEREUM" {
                Network::Ethereum
            } else {
                Network::Matic
            }
        });
    Ok(OrderFilters {
        first: p.get_number("first", None).map(|v| v as i64),
        skip: p.get_number("skip", None).map(|v| v as i64),
        sort_by,
        marketplace_address: p.get_string("marketplaceAddress", None),
        owner: p.get_string("owner", None),
        buyer: p.get_string("buyer", None),
        contract_address: p.get_string("contractAddress", None),
        token_id: p.get_string("tokenId", None),
        status: p.get_string("status", None),
        network,
        item_id: p.get_string("itemId", None),
        nft_name: p.get_string("nftName", None),
    })
}

pub const OPEN_BY_ITEMS_MAX_ITEMS: usize = 200;
pub const OPEN_BY_ITEMS_DEFAULT_PER_CONTRACT: i64 = 500;
pub const OPEN_BY_ITEMS_MAX_PER_CONTRACT: i64 = 5000;

pub(crate) fn parse_open_by_items(
    pairs: &[(String, String)],
) -> Result<(Vec<(String, String)>, i64), InvalidParameterError> {
    let p = Params::new(pairs);
    let raw = p.get_list("item", &[]);
    if raw.is_empty() {
        return Err(InvalidParameterError::new("item", ""));
    }
    if raw.len() > OPEN_BY_ITEMS_MAX_ITEMS {
        return Err(InvalidParameterError::new(
            "item",
            format!("{} items (max {OPEN_BY_ITEMS_MAX_ITEMS})", raw.len()),
        ));
    }
    let mut items = Vec::with_capacity(raw.len());
    for entry in &raw {
        let Some(item) = super::wire::split_item_wire_id(entry) else {
            return Err(InvalidParameterError::new("item", entry.clone()));
        };
        items.push(item);
    }
    let per_contract = p
        .get_number("perContract", None)
        .map(|v| v as i64)
        .unwrap_or(OPEN_BY_ITEMS_DEFAULT_PER_CONTRACT)
        .clamp(1, OPEN_BY_ITEMS_MAX_PER_CONTRACT);
    Ok((items, per_contract))
}

/// Open `public_nft_order` trades with the item they sell (when the mirror
/// has it) and the filters that apply to the leg; `$1` binds `now`.
const TRADES_LEG_SQL: &str = "SELECT u.id::text AS id, u.trade_contract AS marketplace_address, u.contract_address_sent AS nft_address, u.token_id, \
        u.amount_received::text AS price, u.signer AS owner, u.beneficiary AS payee, \
        (EXTRACT(EPOCH FROM u.created_at) * 1000)::bigint AS created_at, (EXTRACT(EPOCH FROM u.expires_at) * 1000)::bigint AS expires_at, \
        upper(u.network) AS network, mod(u.token_id::numeric, SHIFT::numeric)::text AS issued_id, u.item_id::text AS item_id, i.urn \
 FROM unified_trades u \
 LEFT JOIN shop.items i ON i.contract_address = u.contract_address_sent AND i.item_id = u.item_id AND i.network = lower(u.network) \
 WHERE u.type = 'public_nft_order' AND u.token_id IS NOT NULL";

/// `$1` is now, `$2` the trades floor; `extra` binds from `$3`.
fn trades_leg_sql(extra: &str) -> String {
    format!(
        "WITH {} {} {extra} ORDER BY u.created_at DESC, u.id::text ASC LIMIT 1000",
        unified_trades_cte("$1", "$2"),
        TRADES_LEG_SQL.replace("SHIFT", ITEM_ID_SHIFT)
    )
}

/// An open v3 listing ([`TRADES_LEG_SQL`]'s row) as an order: its id is the
/// trade's, and `tradeId` names it so a client cancels that trade.
fn trade_row_order(r: &sqlx::postgres::PgRow) -> Order {
    let (network, chain_id) = network_and_chain(&r.get::<String, _>("network"));
    let created_at: i64 = r.get("created_at");
    Order {
        id: r.get("id"),
        trade_id: Some(r.get("id")),
        marketplace_address: r
            .get::<Option<String>, _>("marketplace_address")
            .unwrap_or_default(),
        contract_address: r.get("nft_address"),
        token_id: r.get("token_id"),
        owner: r.get("owner"),
        buyer: Some(String::new()),
        price: r.get("price"),
        status: "open".to_string(),
        expires_at: r.get("expires_at"),
        created_at,
        updated_at: created_at,
        network,
        chain_id,
        issued_id: r.get("issued_id"),
        payee: r.get("payee"),
    }
}

/// The newest open v3 listing of each `(network, contract, token, owner)`,
/// signed by that owner: the `order` an nft page carries when it has no
/// legacy one. One statement for the whole page; `network` is the
/// database name (`matic`, `ethereum`), the rest lowercase.
pub async fn open_trade_orders(
    state: &MarketplaceState,
    wanted: &[(String, String, String, String)],
) -> Result<std::collections::HashMap<(String, String, String), Order>, ApiError> {
    let mut out = std::collections::HashMap::new();
    if wanted.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "WITH {} SELECT DISTINCT ON (x.network, x.nft_address, x.token_id) x.* FROM ({} \
           AND (lower(u.network), u.contract_address_sent, u.token_id, u.signer) IN \
               (SELECT * FROM unnest($3::text[], $4::text[], $5::text[], $6::text[]))) x \
         ORDER BY x.network, x.nft_address, x.token_id, x.created_at DESC, x.id ASC",
        unified_trades_cte("$1", "$2"),
        TRADES_LEG_SQL.replace("SHIFT", ITEM_ID_SHIFT)
    );
    let column = |i: usize| {
        wanted
            .iter()
            .map(|w| match i {
                0 => w.0.clone(),
                1 => w.1.clone(),
                2 => w.2.clone(),
                _ => w.3.clone(),
            })
            .collect::<Vec<String>>()
    };
    let now = Utc::now().naive_utc();
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(now)
        .bind(super::trades::mirrored_since(&state.settings, now))
        .bind(column(0))
        .bind(column(1))
        .bind(column(2))
        .bind(column(3))
        .fetch_all(&state.pool)
        .await?;
    for r in &rows {
        let order = trade_row_order(r);
        out.insert(
            (
                order.network.db_name().to_string(),
                order.contract_address.clone(),
                order.token_id.clone().unwrap_or_default(),
            ),
            order,
        );
    }
    Ok(out)
}

fn keyed(order: Order, sort_token_id: String) -> Keyed {
    let sort_price = order.price.parse::<u128>().unwrap_or(0);
    Keyed {
        order,
        sort_price,
        sort_token_id,
    }
}

/// The open v3 listings matching `f`, with the signer-still-owns guard.
pub async fn trades_leg(
    state: &MarketplaceState,
    f: &OrderFilters,
) -> Result<Vec<Keyed>, ApiError> {
    if f.status.as_deref().is_some_and(|s| s != "open") || f.buyer.is_some() {
        return Ok(Vec::new());
    }
    let mut extra = String::new();
    let mut binds: Vec<String> = Vec::new();
    let next = |binds: &mut Vec<String>, v: String| {
        binds.push(v);
        format!("${}", binds.len() + 2)
    };
    if let Some(v) = &f.marketplace_address {
        let p = next(&mut binds, v.to_lowercase());
        extra.push_str(&format!(" AND lower(u.trade_contract) = {p}"));
    }
    if let Some(v) = &f.owner {
        let p = next(&mut binds, v.to_lowercase());
        extra.push_str(&format!(" AND u.signer = {p}"));
    }
    if let Some(v) = &f.contract_address {
        let p = next(&mut binds, v.to_lowercase());
        extra.push_str(&format!(" AND u.contract_address_sent = {p}"));
    }
    if let Some(v) = &f.token_id {
        let p = next(&mut binds, v.clone());
        extra.push_str(&format!(" AND u.token_id = {p}"));
    }
    if let Some(v) = &f.item_id {
        let bare = v
            .rsplit_once('-')
            .map(|(_, id)| id.to_string())
            .unwrap_or_else(|| v.clone());
        let p = next(&mut binds, bare);
        extra.push_str(&format!(" AND u.item_id::text = {p}"));
    }
    if let Some(n) = f.network {
        let p = next(&mut binds, n.db_name().to_string());
        extra.push_str(&format!(" AND lower(u.network) = {p}"));
    }
    let now = Utc::now().naive_utc();
    let mut q = sqlx::query(sqlx::AssertSqlSafe(trades_leg_sql(&extra)))
        .bind(now)
        .bind(super::trades::mirrored_since(&state.settings, now));
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(&state.pool).await?;
    let mut out: Vec<(Keyed, Option<String>)> = rows
        .iter()
        .map(|r| {
            let order = trade_row_order(r);
            let token_id = order.token_id.clone().unwrap_or_default();
            (keyed(order, token_id), r.get::<Option<String>, _>("urn"))
        })
        .collect();
    let requested: HashSet<(String, String)> = out
        .iter()
        .filter_map(|(k, urn)| {
            urn.as_ref()
                .map(|u| (k.order.owner.clone(), graph::normalize_urn(u)))
        })
        .collect();
    if !requested.is_empty()
        && graph::any_collections_reader(&state.index)
        && let Ok(owned) = graph::owned_urns(&state.index, &requested).await
    {
        out.retain(|(k, urn)| match urn {
            Some(u) => owned
                .get(&k.order.owner)
                .is_some_and(|set| set.contains(&graph::normalize_urn(u))),
            None => true,
        });
    }
    Ok(out.into_iter().map(|(k, _)| k).collect())
}

const ORDER_FIELDS: &str = "id marketplaceAddress nftAddress tokenId owner buyer price status expiresAt createdAt updatedAt nft { issuedId }";

fn legacy_where(f: &OrderFilters, now_ms: i64) -> Value {
    let mut w = json!({});
    if let Some(v) = &f.marketplace_address {
        w["marketplaceAddress"] = json!(v.to_lowercase());
    }
    if let Some(v) = &f.owner {
        w["owner"] = json!(v.to_lowercase());
    }
    if let Some(v) = &f.buyer {
        w["buyer"] = json!(v.to_lowercase());
    }
    if let Some(v) = &f.contract_address {
        w["nftAddress"] = json!(v.to_lowercase());
    }
    if let Some(v) = &f.token_id {
        w["tokenId"] = json!(v);
    }
    match f.status.as_deref() {
        Some("open") => {
            w["status"] = json!("open");
            w["expiresAt_gt"] = json!(now_ms.to_string());
        }
        Some(s) => w["status"] = json!(s),
        None => w["expiresAt_gt"] = json!(now_ms.to_string()),
    }
    if let Some(v) = &f.item_id {
        let composite = if v.contains('-') {
            v.to_lowercase()
        } else {
            format!(
                "{}-{v}",
                f.contract_address.as_deref().unwrap_or("").to_lowercase()
            )
        };
        w["item"] = json!(composite);
    }
    w
}

fn legacy_order_by(sort_by: Option<OrderSortBy>) -> (&'static str, &'static str) {
    match sort_by {
        Some(OrderSortBy::Oldest) => ("createdAt", "asc"),
        Some(OrderSortBy::RecentlyUpdated) => ("updatedAt", "desc"),
        Some(OrderSortBy::Cheapest) => ("price", "asc"),
        Some(OrderSortBy::IssuedIdAsc) => ("tokenId", "asc"),
        Some(OrderSortBy::IssuedIdDesc) => ("tokenId", "desc"),
        _ => ("createdAt", "desc"),
    }
}

/// A subgraph `orders` row as the wire order.
pub fn order_from_graph(o: &Value, network: Network) -> Order {
    let ms = |key: &str| {
        let raw = readers::int_of(o, key);
        if raw != 0 && raw < 100_000_000_000 {
            seconds_to_ms(raw)
        } else {
            raw
        }
    };
    Order {
        id: readers::str_of(o, "id"),
        marketplace_address: readers::str_of(o, "marketplaceAddress"),
        contract_address: readers::str_of(o, "nftAddress"),
        token_id: readers::opt_str_of(o, "tokenId"),
        owner: readers::str_of(o, "owner"),
        buyer: readers::opt_str_of(o, "buyer"),
        price: readers::str_of(o, "price"),
        status: readers::str_of(o, "status"),
        expires_at: ms("expiresAt"),
        created_at: ms("createdAt"),
        updated_at: ms("updatedAt"),
        network,
        chain_id: network.chain_id(),
        issued_id: readers::at(o, "nft/issuedId").and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        }),
        trade_id: Some(String::new()),
        payee: None,
    }
}

/// The legacy rows over the merged window and their capped total.
pub async fn legacy_leg(
    state: &MarketplaceState,
    f: &OrderFilters,
    window: i64,
) -> Result<(Vec<Keyed>, i64), Response> {
    let networks = match f.network {
        Some(n) => vec![n.graph()],
        None => vec![graph::Network::Ethereum, graph::Network::Matic],
    };
    let now_ms = readers::now_ms();
    let where_ = legacy_where(f, now_ms);
    let (order_by, direction) = legacy_order_by(f.sort_by);
    let mut rows = Vec::new();
    let mut total = 0;
    for network in networks {
        let Some(url) = network.url(&state.index) else {
            continue;
        };
        let page = GraphPage {
            entity: "orders",
            filter_type: "Order_filter",
            fields: ORDER_FIELDS,
            where_: where_.clone(),
            order_by,
            direction,
            first: 0,
            skip: 0,
        };
        let got = readers::top_rows(state, network, url, Ttl::S30, &page, window)
            .await
            .map_err(|e| readers::upstream_failed("/v1/orders", &e))?;
        total += readers::count_to(
            state,
            network,
            url,
            "orders",
            "Order_filter",
            where_.clone(),
            readers::count_cap(0, window),
        )
        .await
        .map_err(|e| readers::upstream_failed("/v1/orders", &e))?;
        rows.extend(got.iter().map(|o| {
            let order = order_from_graph(o, Network::from(network));
            let token = order.token_id.clone().unwrap_or_default();
            keyed(order, token)
        }));
    }
    Ok((rows, total))
}

fn numeric_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

pub fn sort_merged(rows: &mut [Keyed], sort_by: Option<OrderSortBy>) {
    rows.sort_by(|a, b| {
        let id = a.order.id.cmp(&b.order.id);
        match sort_by {
            Some(OrderSortBy::Oldest) => a.order.created_at.cmp(&b.order.created_at).then(id),
            Some(OrderSortBy::RecentlyUpdated) => {
                b.order.updated_at.cmp(&a.order.updated_at).then(id)
            }
            Some(OrderSortBy::Cheapest) => a.sort_price.cmp(&b.sort_price).then(id),
            Some(OrderSortBy::IssuedIdAsc) => {
                numeric_cmp(&a.sort_token_id, &b.sort_token_id).then(id)
            }
            Some(OrderSortBy::IssuedIdDesc) => {
                numeric_cmp(&b.sort_token_id, &a.sort_token_id).then(id)
            }
            _ => b.order.created_at.cmp(&a.order.created_at).then(id),
        }
    });
}

pub async fn list_orders(
    state: &MarketplaceState,
    f: &OrderFilters,
) -> Result<(Vec<Order>, i64), Response> {
    let limit = clamp_first(f.first, 1000).min(readers::READER_MAX_FIRST);
    let offset = clamp_skip(f.skip);
    readers::check_window(limit, offset)?;
    readers::require_networks(state, f.network.map(|n| n.graph()))?;
    super::trades::require_ready(state).await?;
    let (mut merged, legacy_total) = legacy_leg(state, f, offset + limit).await?;
    let trades = trades_leg(state, f)
        .await
        .map_err(IntoResponse::into_response)?;
    let total = legacy_total + trades.len() as i64;
    merged.extend(trades);
    sort_merged(&mut merged, f.sort_by);
    let page = merged
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|k| k.order)
        .collect();
    Ok((page, total))
}

pub async fn get_orders(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let filters = match parse_filters(&pairs) {
        Ok(f) => f,
        Err(e) => return ApiError::from(e).into_response(),
    };
    match list_orders(&state, &filters).await {
        Ok((data, total)) => Json(DataTotalString {
            data,
            total: total.to_string(),
        })
        .into_response(),
        Err(r) => r,
    }
}

/// The item id a collection token id encodes.
pub fn item_of_token(token_id: &str) -> Option<String> {
    let token = alloy_primitives::U256::from_str_radix(token_id, 10).ok()?;
    Some((token >> 216u32).to_string())
}

/// Open orders of `(contract, item)` pairs within each contract's
/// cheapest-first window: the mirror leg ranks in SQL, the reader leg reads
/// one cheapest page (≤ 1000) per contract and keeps the wanted items.
pub async fn open_orders_by_items(
    state: &MarketplaceState,
    items: &[(String, String)],
    per_contract: i64,
) -> Result<Vec<Order>, Response> {
    readers::require_both_collections(state)?;
    super::trades::require_ready(state).await?;
    let contracts: Vec<String> = items
        .iter()
        .map(|(c, _)| c.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let wanted: HashSet<(String, String)> = items.iter().cloned().collect();
    let mut all: Vec<Keyed> = Vec::new();
    for contract in &contracts {
        let f = OrderFilters {
            contract_address: Some(contract.clone()),
            status: Some("open".to_string()),
            sort_by: Some(OrderSortBy::Cheapest),
            ..Default::default()
        };
        let (legacy, _) =
            legacy_leg(state, &f, per_contract.min(graph::THE_GRAPH_PAGE_SIZE)).await?;
        let trades = trades_leg(state, &f)
            .await
            .map_err(IntoResponse::into_response)?;
        let mut rows = legacy;
        rows.extend(trades);
        sort_merged(&mut rows, Some(OrderSortBy::Cheapest));
        rows.truncate(per_contract as usize);
        all.extend(rows.into_iter().filter(|k| {
            k.order
                .token_id
                .as_deref()
                .and_then(item_of_token)
                .is_some_and(|item| wanted.contains(&(contract.clone(), item)))
        }));
    }
    all.sort_by(|a, b| {
        a.order
            .contract_address
            .cmp(&b.order.contract_address)
            .then(a.sort_price.cmp(&b.sort_price))
            .then(a.order.id.cmp(&b.order.id))
    });
    Ok(all.into_iter().map(|k| k.order).collect())
}

pub async fn get_open_orders_by_items(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let (items, per_contract) = match parse_open_by_items(&pairs) {
        Ok(v) => v,
        Err(e) => return ApiError::from(e).into_response(),
    };
    match open_orders_by_items(&state, &items, per_contract).await {
        Ok(data) => Json(DataTotalString {
            total: data.len().to_string(),
            data,
        })
        .into_response(),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_repeated_items_and_the_window() {
        let (items, per) = parse_open_by_items(&pairs(&[
            ("item", "0x016A61FEB6377239E34425B82E5C4B367E52457F-12"),
            ("item", "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-0"),
            ("perContract", "500"),
        ]))
        .unwrap();
        assert_eq!(
            items,
            vec![
                (
                    "0x016a61feb6377239e34425b82e5c4b367e52457f".to_string(),
                    "12".to_string()
                ),
                (
                    "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                    "0".to_string()
                )
            ]
        );
        assert_eq!(per, 500);
        let one = pairs(&[("item", "0x016a61feb6377239e34425b82e5c4b367e52457f-1")]);
        assert_eq!(parse_open_by_items(&one).unwrap().1, 500);
        assert_eq!(
            parse_open_by_items(&pairs(&[
                ("item", "0x016a61feb6377239e34425b82e5c4b367e52457f-1"),
                ("perContract", "999999")
            ]))
            .unwrap()
            .1,
            5000
        );
        assert_eq!(
            parse_open_by_items(&pairs(&[
                ("item", "0x016a61feb6377239e34425b82e5c4b367e52457f-1"),
                ("perContract", "0")
            ]))
            .unwrap()
            .1,
            1
        );
        for bad in [
            "",
            "0x016a61feb6377239e34425b82e5c4b367e52457f",
            "0x016a61feb6377239e34425b82e5c4b367e52457f-",
            "0x016a61feb6377239e34425b82e5c4b367e52457f-abc",
            "0x016a-12",
            "016a61feb6377239e34425b82e5c4b367e52457f-12",
        ] {
            assert!(
                parse_open_by_items(&pairs(&[("item", bad)])).is_err(),
                "{bad:?}"
            );
        }
        assert!(parse_open_by_items(&[]).is_err());
        let max_id = alloy_primitives::U256::MAX.to_string();
        let (items, _) = parse_open_by_items(&pairs(&[(
            "item",
            &format!("0x016A61FEB6377239E34425B82E5C4B367E52457F-{max_id}"),
        )]))
        .unwrap();
        assert_eq!(
            items,
            vec![(
                "0x016a61feb6377239e34425b82e5c4b367e52457f".to_string(),
                max_id.clone()
            )]
        );
        assert!(
            parse_open_by_items(&pairs(&[(
                "item",
                &format!("0x016a61feb6377239e34425b82e5c4b367e52457f-{max_id}0"),
            )]))
            .is_err()
        );
    }

    #[test]
    fn seconds_are_scaled_to_milliseconds_and_millis_pass_through() {
        assert_eq!(
            from_seconds_to_milliseconds(1_700_000_000.0),
            1_700_000_000_000
        );
        assert_eq!(from_seconds_to_milliseconds(0.0), 0);
        assert_eq!(
            from_seconds_to_milliseconds(1_700_000_000_000.0),
            1_700_000_000_000
        );
        assert_eq!(from_seconds_to_milliseconds(1e11_f64), 100_000_000_000);
    }

    #[test]
    fn the_token_id_encodes_the_item_and_the_legacy_filter_names_the_graph_fields() {
        assert_eq!(
            item_of_token("105312291668557186697918027683670432318895095400549111254310977536")
                .as_deref(),
            Some("1")
        );
        assert_eq!(item_of_token("7").as_deref(), Some("0"));
        assert_eq!(item_of_token("x"), None);
        let f = parse_filters(&pairs(&[
            ("contractAddress", "0xABC"),
            ("itemId", "12"),
            ("status", "open"),
            ("sortBy", "cheapest"),
        ]))
        .unwrap();
        let w = legacy_where(&f, 1_000);
        assert_eq!(w["nftAddress"], "0xabc");
        assert_eq!(w["item"], "0xabc-12");
        assert_eq!(w["expiresAt_gt"], "1000");
        assert_eq!(legacy_order_by(f.sort_by), ("price", "asc"));
        let sql = trades_leg_sql(" AND u.signer = $3");
        assert!(
            sql.contains("u.type = 'public_nft_order'")
                && sql.contains("trade_cancellations")
                && sql.contains("LIMIT 1000"),
            "{sql}"
        );
    }

    #[test]
    fn a_graph_order_keeps_the_wire_shape() {
        let o = json!({ "id": "0x1-1", "marketplaceAddress": "0xm", "nftAddress": "0xc", "tokenId": "5", "owner": "0xo", "buyer": null, "price": "10", "status": "open", "expiresAt": "1800000000000", "createdAt": "1700000000", "updatedAt": "1700000001", "nft": { "issuedId": "3" } });
        let order = order_from_graph(&o, Network::Matic);
        assert_eq!(
            serde_json::to_value(&order).unwrap(),
            json!({
                "id": "0x1-1", "marketplaceAddress": "0xm", "contractAddress": "0xc", "tokenId": "5", "owner": "0xo", "buyer": null, "price": "10", "status": "open",
                "expiresAt": 1_800_000_000_000i64, "createdAt": 1_700_000_000_000i64, "updatedAt": 1_700_000_001_000i64, "network": "MATIC", "chainId": 137, "issuedId": "3", "tradeId": ""
            })
        );
        let mut rows = vec![
            keyed(order.clone(), "5".into()),
            keyed(
                Order {
                    id: "b".into(),
                    price: "9".into(),
                    created_at: 1,
                    ..order.clone()
                },
                "12".into(),
            ),
        ];
        sort_merged(&mut rows, Some(OrderSortBy::Cheapest));
        assert_eq!(rows[0].order.id, "b");
        sort_merged(&mut rows, Some(OrderSortBy::IssuedIdDesc));
        assert_eq!(rows[0].sort_token_id, "12", "numeric, not lexicographic");
        sort_merged(&mut rows, None);
        assert_eq!(rows[0].order.id, "0x1-1", "newest first");
    }
}
