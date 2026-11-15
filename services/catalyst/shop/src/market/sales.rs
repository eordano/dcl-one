//! `/v1/sales` over the collections subgraph `sales` reader (first ≤ 100,
//! 60 s; past the graph's 5,000 skip by keyset up to `readers::DEEP_WINDOW`;
//! the total is a capped count) and `/v1/sales/summary` over the seller's
//! sales, every one up to `readers::DEEP_WINDOW` per network (5 min): a
//! seller with more gets the unavailable envelope rather than a summary that
//! silently stops at the bound.

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::graph;
use super::readers::{self, GraphPage, Ttl, seconds_to_ms};
use super::wire::{ApiError, ChainId, DataOnly, DataTotal, InvalidParameterError, Network, Params};
use super::{MarketplaceState, SharedState, Source, unavailable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaleSortBy {
    MostExpensive,
    RecentlySold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaleType {
    Order,
    Bid,
    Mint,
}

impl SaleType {
    fn as_str(&self) -> &'static str {
        match self {
            SaleType::Order => "order",
            SaleType::Bid => "bid",
            SaleType::Mint => "mint",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SaleFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub sort_by: Option<SaleSortBy>,
    pub sale_type: Option<SaleType>,
    pub categories: Vec<String>,
    pub seller: Option<String>,
    pub buyer: Option<String>,
    pub contract_address: Option<String>,
    pub token_id: Option<String>,
    pub item_id: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub min_price: Option<String>,
    pub max_price: Option<String>,
    pub network: Option<Network>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct Sale {
    pub id: String,
    #[serde(rename = "itemId")]
    pub item_id: Option<String>,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    pub buyer: String,
    #[serde(rename = "chainId")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    pub network: Network,
    pub price: String,
    pub seller: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub timestamp: i64,
    #[serde(rename = "tokenId")]
    pub token_id: Option<String>,
    #[serde(rename = "txHash")]
    pub tx_hash: String,
    #[serde(rename = "type")]
    #[cfg_attr(feature = "ts", ts(rename = "type"))]
    pub sale_type: String,
}

pub fn parse_filters(pairs: &[(String, String)]) -> Result<SaleFilters, InvalidParameterError> {
    let p = Params::new(pairs);
    let sort_by = p
        .get_value("sortBy", &["most_expensive", "recently_sold"], None)
        .map(|s| {
            if s == "most_expensive" {
                SaleSortBy::MostExpensive
            } else {
                SaleSortBy::RecentlySold
            }
        });
    let sale_type = p
        .get_value("type", &["order", "bid", "mint"], None)
        .map(|s| match s.as_str() {
            "bid" => SaleType::Bid,
            "mint" => SaleType::Mint,
            _ => SaleType::Order,
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
    Ok(SaleFilters {
        first: p.get_number("first", None).map(|v| v as i64),
        skip: p.get_number("skip", None).map(|v| v as i64),
        sort_by,
        sale_type,
        categories: p.get_list(
            "category",
            &["parcel", "estate", "wearable", "ens", "emote"],
        ),
        seller: p.get_address("seller", true, None),
        buyer: p.get_address("buyer", true, None),
        contract_address: p.get_address("contractAddress", true, None),
        token_id: p.get_string("tokenId", None),
        item_id: p.get_string("itemId", None),
        from: p.get_number("from", None).map(|v| v as i64),
        to: p.get_number("to", None).map(|v| v as i64),
        min_price: p.get_string("minPrice", None),
        max_price: p.get_string("maxPrice", None),
        network,
    })
}

const SALE_FIELDS: &str = "id type buyer seller price timestamp txHash searchContractAddress searchItemId searchTokenId searchCategory";

pub fn graph_where(f: &SaleFilters) -> Value {
    let mut w = json!({});
    if let Some(t) = f.sale_type {
        w["type"] = json!(t.as_str());
    }
    if let Some(v) = &f.buyer {
        w["buyer"] = json!(v);
    }
    if let Some(v) = &f.seller {
        w["seller"] = json!(v);
    }
    if let Some(v) = &f.contract_address {
        w["searchContractAddress"] = json!(v);
    }
    if let Some(v) = &f.item_id {
        w["searchItemId"] = json!(v);
    }
    if let Some(v) = &f.token_id {
        w["searchTokenId"] = json!(v);
    }
    if let Some(v) = &f.min_price {
        w["price_gte"] = json!(v);
    }
    if let Some(v) = &f.max_price {
        w["price_lte"] = json!(v);
    }
    if !f.categories.is_empty() {
        w["searchCategory_in"] = json!(f.categories);
    }
    if let Some(v) = f.from {
        w["timestamp_gte"] = json!((v / 1000).to_string());
    }
    if let Some(v) = f.to {
        w["timestamp_lte"] = json!((v / 1000).to_string());
    }
    w
}

pub fn sale_from_graph(s: &Value, network: Network) -> Sale {
    Sale {
        id: readers::str_of(s, "id"),
        item_id: readers::opt_str_of(s, "searchItemId"),
        contract_address: readers::str_of(s, "searchContractAddress"),
        buyer: readers::str_of(s, "buyer"),
        chain_id: network.chain_id(),
        network,
        price: readers::str_of(s, "price"),
        seller: readers::str_of(s, "seller"),
        timestamp: seconds_to_ms(readers::int_of(s, "timestamp")),
        token_id: readers::opt_str_of(s, "searchTokenId"),
        tx_hash: readers::str_of(s, "txHash"),
        sale_type: readers::str_of(s, "type"),
    }
}

pub async fn list_sales(
    state: &MarketplaceState,
    f: &SaleFilters,
) -> Result<(Vec<Sale>, i64), Response> {
    let (first, skip) = readers::deep_page(f.first, f.skip, 100)?;
    let networks: Vec<(graph::Network, &str)> = match f.network {
        Some(n) => vec![(n.graph(), readers::require_collections(state, n.graph())?)],
        None => readers::require_both_collections(state)?,
    };
    let where_ = graph_where(f);
    let (order_by, direction) = match f.sort_by {
        Some(SaleSortBy::MostExpensive) => ("price", "desc"),
        _ => ("timestamp", "desc"),
    };
    let mut rows: Vec<Sale> = Vec::new();
    let mut total = 0;
    let mut offset = 0;
    let count = networks.len();
    for (network, url) in networks {
        let page = GraphPage {
            entity: "sales",
            filter_type: "Sale_filter",
            fields: SALE_FIELDS,
            where_: where_.clone(),
            order_by,
            direction,
            first,
            skip,
        };
        let (got, skip_after) =
            readers::source_rows(state, network, url, Ttl::S60, &page, first, skip, count)
                .await
                .map_err(|e| readers::upstream_failed("/v1/sales", &e))?;
        offset = skip_after;
        total += readers::count_to(
            state,
            network,
            url,
            "sales",
            "Sale_filter",
            where_.clone(),
            readers::count_cap(first, skip),
        )
        .await
        .map_err(|e| readers::upstream_failed("/v1/sales", &e))?;
        rows.extend(
            got.iter()
                .map(|s| sale_from_graph(s, Network::from(network))),
        );
    }
    rows.sort_by(|a, b| {
        let primary = match f.sort_by {
            Some(SaleSortBy::MostExpensive) => b
                .price
                .parse::<u128>()
                .unwrap_or(0)
                .cmp(&a.price.parse::<u128>().unwrap_or(0)),
            _ => b.timestamp.cmp(&a.timestamp),
        };
        primary.then(a.id.cmp(&b.id))
    });
    Ok((
        rows.into_iter()
            .skip(offset as usize)
            .take(first as usize)
            .collect(),
        total,
    ))
}

pub async fn get_sales(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let filters = match parse_filters(&pairs) {
        Ok(f) => f,
        Err(e) => return ApiError::from(e).into_response(),
    };
    match list_sales(&state, &filters).await {
        Ok((data, total)) => Json(DataTotal { data, total }).into_response(),
        Err(r) => r,
    }
}

#[derive(Debug, Clone, Default)]
pub struct SalesSummaryFilters {
    pub seller: String,
    pub from: Option<i64>,
    pub to: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct SalesSummaryCollection {
    pub contract_address: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub sold: i64,
    pub earned_wei: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct SalesSummaryItem {
    pub contract_address: String,
    pub item_id: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub sold_lifetime: i64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct SalesSummaryRoyalties {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub resales: i64,
    pub volume_wei: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct SalesSummary {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub mints: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub resales: i64,
    pub earned_wei: String,
    pub by_collection: Vec<SalesSummaryCollection>,
    pub by_item: Vec<SalesSummaryItem>,
    pub royalties: SalesSummaryRoyalties,
}

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
/// The reader's bound: the seller's newest sales per network read (by
/// keyset past the graph's window); a seller with more is not summarised.
pub const SUMMARY_MAX_SALES: i64 = readers::DEEP_WINDOW;

pub fn parse_summary_filters(pairs: &[(String, String)]) -> Result<SalesSummaryFilters, ApiError> {
    let p = Params::new(pairs);
    let seller = p
        .get_address("seller", true, None)
        .ok_or_else(|| ApiError::bad_request("A valid seller address is required"))?;
    let mut bounds: [Option<i64>; 2] = [None, None];
    for (slot, key) in ["from", "to"].iter().enumerate() {
        if let Some(raw) = p.get_string(key, None) {
            let parsed = raw
                .parse::<i64>()
                .ok()
                .filter(|v| *v >= 0 && *v <= MAX_SAFE_INTEGER)
                .filter(|_| raw.chars().all(|c| c.is_ascii_digit()) && !raw.is_empty());
            bounds[slot] = Some(parsed.ok_or_else(|| {
                ApiError::bad_request(format!("{key} must be an epoch timestamp in milliseconds"))
            })?);
        }
    }
    if let (Some(from), Some(to)) = (bounds[0], bounds[1])
        && from > to
    {
        return Err(ApiError::bad_request(
            "from must be less than or equal to to",
        ));
    }
    Ok(SalesSummaryFilters {
        seller,
        from: bounds[0],
        to: bounds[1],
    })
}

/// The old SQL's aggregate in memory: every leg over the seller's sales,
/// the window on collections/totals, lifetime mints by item, and the
/// royalties leg over resales of items the seller created.
pub fn summarize(
    seller_sales: &[Value],
    creator_resales: &[Value],
    f: &SalesSummaryFilters,
) -> SalesSummary {
    let in_window = |s: &Value| {
        let ts = readers::int_of(s, "timestamp") * 1000;
        f.from.is_none_or(|from| ts >= from) && f.to.is_none_or(|to| ts <= to)
    };
    let price = |s: &Value| readers::str_of(s, "price").parse::<u128>().unwrap_or(0);
    let windowed: Vec<&Value> = seller_sales.iter().filter(|s| in_window(s)).collect();
    let mut by_collection: std::collections::BTreeMap<String, (i64, u128)> = Default::default();
    for s in &windowed {
        let e = by_collection
            .entry(readers::str_of(s, "searchContractAddress"))
            .or_default();
        e.0 += 1;
        e.1 = e.1.saturating_add(price(s));
    }
    let mut by_item: std::collections::BTreeMap<(String, String), i64> = Default::default();
    for s in seller_sales
        .iter()
        .filter(|s| readers::str_of(s, "type") == "mint")
    {
        if let Some(item) = readers::opt_str_of(s, "searchItemId") {
            *by_item
                .entry((readers::str_of(s, "searchContractAddress"), item))
                .or_default() += 1;
        }
    }
    let royalties: Vec<&Value> = creator_resales.iter().filter(|s| in_window(s)).collect();
    SalesSummary {
        total: windowed.len() as i64,
        mints: windowed
            .iter()
            .filter(|s| readers::str_of(s, "type") == "mint")
            .count() as i64,
        resales: windowed
            .iter()
            .filter(|s| matches!(readers::str_of(s, "type").as_str(), "order" | "bid"))
            .count() as i64,
        earned_wei: windowed
            .iter()
            .fold(0u128, |acc, s| acc.saturating_add(price(s)))
            .to_string(),
        by_collection: by_collection
            .into_iter()
            .map(
                |(contract_address, (sold, earned))| SalesSummaryCollection {
                    contract_address,
                    sold,
                    earned_wei: earned.to_string(),
                },
            )
            .collect(),
        by_item: by_item
            .into_iter()
            .map(
                |((contract_address, item_id), sold_lifetime)| SalesSummaryItem {
                    contract_address,
                    item_id,
                    sold_lifetime,
                },
            )
            .collect(),
        royalties: SalesSummaryRoyalties {
            resales: royalties.len() as i64,
            volume_wei: royalties
                .iter()
                .fold(0u128, |acc, s| acc.saturating_add(price(s)))
                .to_string(),
        },
    }
}

const SUMMARY_FIELDS: &str = "id type price timestamp searchContractAddress searchItemId";

pub(crate) async fn bounded_sales_rows(
    state: &MarketplaceState,
    where_: Value,
    fields: &'static str,
    route: &'static str,
) -> Result<Vec<(super::graph::Network, Value)>, Response> {
    let mut out = Vec::new();
    for (network, url) in readers::require_both_collections(state)? {
        let page = GraphPage {
            entity: "sales",
            filter_type: "Sale_filter",
            fields,
            where_: where_.clone(),
            order_by: "timestamp",
            direction: "desc",
            first: 0,
            skip: 0,
        };
        let readers::Bounded::Complete(rows) =
            readers::bounded_rows(state, network, url, Ttl::M5, &page, SUMMARY_MAX_SALES)
                .await
                .map_err(|e| readers::upstream_failed(route, &e))?
        else {
            return Err(unavailable(
                Source::CollectionsSubgraph(network),
                &format!(
                    "this seller has more than {SUMMARY_MAX_SALES} sales on {}, more than the bounded reader summarises",
                    network.db_name()
                ),
            ));
        };
        out.extend(rows.into_iter().map(|row| (network, row)));
    }
    Ok(out)
}

async fn all_networks_page(
    state: &MarketplaceState,
    where_: Value,
    route: &'static str,
) -> Result<Vec<Value>, Response> {
    Ok(bounded_sales_rows(state, where_, SUMMARY_FIELDS, route)
        .await?
        .into_iter()
        .map(|(_, row)| row)
        .collect())
}

/// The seller's sales rows the summary is built from (the bounded reader:
/// a seller past `SUMMARY_MAX_SALES` on a network is the unavailable
/// envelope), for the store dashboard's per-item window figures.
pub async fn seller_sales_rows(
    state: &MarketplaceState,
    seller: &str,
    route: &'static str,
) -> Result<Vec<Value>, Response> {
    all_networks_page(state, json!({ "seller": seller.to_lowercase() }), route).await
}

pub async fn sales_summary(
    state: &MarketplaceState,
    f: &SalesSummaryFilters,
) -> Result<SalesSummary, Response> {
    let seller_sales =
        all_networks_page(state, json!({ "seller": f.seller }), "/v1/sales/summary").await?;
    let creator_resales = all_networks_page(
        state,
        json!({ "type_in": ["order", "bid"], "item_": { "creator": f.seller } }),
        "/v1/sales/summary",
    )
    .await?;
    Ok(summarize(&seller_sales, &creator_resales, f))
}

pub async fn get_sales_summary(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let filters = match parse_summary_filters(&pairs) {
        Ok(f) => f,
        Err(e) => return e.into_response(),
    };
    match sales_summary(&state, &filters).await {
        Ok(data) => Json(DataOnly { data }).into_response(),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_rows_keep_network_tags_order_and_requested_fields() {
        use super::super::tests::support::state_without_db_settings;
        use axum::{Router, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            post(|Json(body): Json<Value>| async move {
                assert!(body["query"].as_str().unwrap().contains("feesCollectorCut"));
                Json(json!({"data": {"sales": [{"id": "sale", "feesCollectorCut": "12"}]}}))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("http://{addr}/");
        let state = state_without_db_settings(super::super::Settings {
            eth_collections_subgraph_url: Some(url.clone()),
            matic_collections_subgraph_url: Some(url),
            ..super::super::Settings::for_tests()
        });
        state
            .test_allow_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let rows = bounded_sales_rows(&state, json!({}), "id feesCollectorCut", "/test")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.db_name(), "ethereum");
        assert_eq!(rows[1].0.db_name(), "matic");
        assert_eq!(rows[0].1, json!({"id": "sale", "feesCollectorCut": "12"}));
        assert_eq!(rows[0].1, rows[1].1);
        server.abort();
    }

    /// One network of [`bounded_source`]: how many sales the seller has, how
    /// many consecutive sales share a timestamp, and whether every request
    /// fails.
    #[derive(Clone, Copy)]
    struct Sales {
        rows: i64,
        ties: i64,
        fail: bool,
    }

    fn sales(rows: i64, ties: i64) -> Sales {
        Sales {
            rows,
            ties,
            fail: false,
        }
    }

    /// A request a source answered: `first`, `skip`, keyset bound or not.
    type Requests = std::sync::Arc<std::sync::Mutex<Vec<(i64, i64, bool)>>>;

    /// A collections subgraph that honours the graph's page bounds (`first`
    /// up to 1000, `skip` up to 5000, refused past them), answering newest
    /// first with ties in id order and applying a `timestamp_lte` bound.
    fn source_route(spec: Sales, log: Requests) -> axum::routing::MethodRouter {
        let rows: std::sync::Arc<Vec<Value>> = std::sync::Arc::new(
            (0..spec.rows)
                .map(|i| {
                    json!({
                        "id": format!("s{i:06}"),
                        "type": "mint",
                        "price": "1",
                        "timestamp": (10_000_000 - i / spec.ties).to_string(),
                        "searchContractAddress": "0xc",
                        "searchItemId": "1",
                    })
                })
                .collect(),
        );
        axum::routing::post(move |Json(body): Json<Value>| {
            let rows = rows.clone();
            let log = log.clone();
            async move {
                let vars = &body["variables"];
                let first = vars["first"].as_i64().unwrap();
                let skip = vars["skip"].as_i64().unwrap();
                let bound = vars["where"]
                    .get("timestamp_lte")
                    .map(|v| v.as_str().unwrap().parse::<i64>().unwrap());
                log.lock().unwrap().push((first, skip, bound.is_some()));
                if spec.fail || first > 1000 || skip > 5000 {
                    return (axum::http::StatusCode::BAD_REQUEST, "refused").into_response();
                }
                let ts = |r: &Value| readers::int_of(r, "timestamp");
                let start = bound.map_or(0, |b| rows.partition_point(|r| ts(r) > b));
                let from = (start + skip as usize).min(rows.len());
                let to = (from + first as usize).min(rows.len());
                Json(json!({ "data": { "sales": rows[from..to] } })).into_response()
            }
        })
    }

    /// A state reading Ethereum and Polygon sales from two bounded sources,
    /// with each source's request log.
    async fn bounded_source(
        eth: Sales,
        matic: Sales,
    ) -> (SharedState, [Requests; 2], tokio::task::JoinHandle<()>) {
        use super::super::tests::support::state_without_db_settings;
        let logs: [Requests; 2] = Default::default();
        let app = axum::Router::new()
            .route("/eth", source_route(eth, logs[0].clone()))
            .route("/matic", source_route(matic, logs[1].clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = state_without_db_settings(super::super::Settings {
            eth_collections_subgraph_url: Some(format!("http://{addr}/eth")),
            matic_collections_subgraph_url: Some(format!("http://{addr}/matic")),
            ..super::super::Settings::for_tests()
        });
        state
            .test_allow_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        (state, logs, server)
    }

    async fn parts(response: Response) -> (u16, Option<String>, Option<String>, Value) {
        let header = |name| {
            response
                .headers()
                .get(name)
                .map(|v: &axum::http::HeaderValue| v.to_str().unwrap().to_string())
        };
        let (status, content_type, retry_after) = (
            response.status().as_u16(),
            header(axum::http::header::CONTENT_TYPE),
            header(axum::http::header::RETRY_AFTER),
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            content_type,
            retry_after,
            serde_json::from_slice(&body).unwrap(),
        )
    }

    /// The unavailable envelope the bounded reader answers for a seller past
    /// `SUMMARY_MAX_SALES` on `network`.
    async fn assert_over_limit(response: Response, network: &str, configure: &str) {
        assert_eq!(
            parts(response).await,
            (
                503,
                Some("application/json".into()),
                Some("60".into()),
                json!({
                    "error": "Service Unavailable",
                    "message": format!("this seller has more than 25000 sales on {network}, more than the bounded reader summarises"),
                    "source": format!("collections-subgraph:{network}"),
                    "configure": configure,
                })
            )
        );
    }

    const MAX: i64 = SUMMARY_MAX_SALES;

    /// The bound decided on a small `max`: every row up to it, the overflow
    /// at one row past it, on untied and tied orders.
    #[tokio::test]
    async fn bounded_rows_overflow_one_row_past_the_bound() {
        use super::super::graph::Network;
        const SMALL: i64 = 12;
        for ties in [1, 7] {
            for (count, complete) in [
                (0, true),
                (SMALL - 1, true),
                (SMALL, true),
                (SMALL + 1, false),
                (3 * SMALL, false),
            ] {
                let (state, logs, server) =
                    bounded_source(sales(count, ties), sales(0, ties)).await;
                let url = Network::Ethereum.url(&state.index).unwrap();
                let page = GraphPage {
                    entity: "sales",
                    filter_type: "Sale_filter",
                    fields: SUMMARY_FIELDS,
                    where_: json!({}),
                    order_by: "timestamp",
                    direction: "desc",
                    first: 0,
                    skip: 0,
                };
                let read =
                    readers::bounded_rows(&state, Network::Ethereum, url, Ttl::M5, &page, SMALL)
                        .await
                        .unwrap();
                match read {
                    readers::Bounded::Complete(rows) => {
                        assert!(
                            complete,
                            "{count} rows (ties {ties}) were presented as complete"
                        );
                        let ids: Vec<_> = rows.iter().map(|r| readers::str_of(r, "id")).collect();
                        let expected: Vec<_> = (0..count).map(|i| format!("s{i:06}")).collect();
                        assert_eq!(ids, expected, "ties {ties}");
                    }
                    readers::Bounded::Overflow => {
                        assert!(!complete, "{count} rows (ties {ties}) overflowed");
                    }
                }
                assert_eq!(
                    *logs[0].lock().unwrap(),
                    vec![(SMALL + 1, 0, false)],
                    "one request for the bound and its probe row (ties {ties})"
                );
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn a_seller_past_the_bound_on_either_network_is_unavailable() {
        assert_eq!(MAX, 25_000);
        for (eth, matic, network, configure) in [
            (MAX + 1, 0, "ethereum", "ETH_COLLECTIONS_SUBGRAPH_URL"),
            (MAX, MAX + 1, "matic", "MATIC_COLLECTIONS_SUBGRAPH_URL"),
        ] {
            let (state, _, server) = bounded_source(sales(eth, 7), sales(matic, 7)).await;
            match bounded_sales_rows(&state, json!({}), SUMMARY_FIELDS, "/test").await {
                Ok(_) => panic!("a seller past the bound on {network} was presented as complete"),
                Err(response) => assert_over_limit(response, network, configure).await,
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn an_in_limit_seller_reads_the_same_pages_and_the_bound_one_probe_more() {
        let (state, logs, server) = bounded_source(sales(MAX - 1, 7), sales(MAX, 7)).await;
        let rows = bounded_sales_rows(&state, json!({}), SUMMARY_FIELDS, "/test")
            .await
            .unwrap_or_else(|_| panic!("in limit"));
        let on = |name: &str| rows.iter().filter(|(n, _)| n.db_name() == name).count();
        assert_eq!((on("ethereum") as i64, on("matic") as i64), (MAX - 1, MAX));
        let ids: std::collections::HashSet<_> = rows
            .iter()
            .map(|(n, r)| (n.db_name(), readers::str_of(r, "id")))
            .collect();
        assert_eq!(ids.len(), rows.len(), "no row read twice");
        let below = logs[0].lock().unwrap().clone();
        assert_eq!(below.len(), 25, "six skip pages, then keyset pages");
        assert!(below.iter().all(|(first, _, _)| *first == 1000));
        assert_eq!(
            below[..6].iter().map(|r| r.1).collect::<Vec<_>>(),
            vec![0, 1000, 2000, 3000, 4000, 5000]
        );
        assert!(
            below[6..].iter().all(|r| r.2),
            "past the skip window by keyset"
        );
        let at = logs[1].lock().unwrap().clone();
        assert_eq!(at.len(), 26);
        assert_eq!(at[..25], below[..25]);
        let (first, skip, keyset) = at[25];
        assert_eq!((first, keyset), (1, true), "the overflow probe is one row");
        assert_eq!(skip, (MAX - 1) % 7 + 1, "past the rows tied at the cursor");
        server.abort();
    }

    #[tokio::test]
    async fn a_failing_source_is_the_upstream_envelope_never_a_partial_summary() {
        for (eth, matic) in [
            (
                sales(3, 1),
                Sales {
                    fail: true,
                    ..sales(3, 1)
                },
            ),
            (
                Sales {
                    fail: true,
                    ..sales(3, 1)
                },
                sales(3, 1),
            ),
        ] {
            let (state, _, server) = bounded_source(eth, matic).await;
            match bounded_sales_rows(&state, json!({}), SUMMARY_FIELDS, "/test").await {
                Ok(_) => panic!("a failing source was presented as complete"),
                Err(response) => assert_eq!(
                    parts(response).await,
                    (
                        503,
                        Some("application/json".into()),
                        None,
                        json!({ "error": "Service Unavailable", "message": "upstream reader unavailable" })
                    )
                ),
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn both_summary_routes_refuse_an_over_limit_seller() {
        let seller = "0x1111111111111111111111111111111111111111";
        let query = || Query(pairs(&[("seller", seller)]));
        let (state, _, server) = bounded_source(sales(MAX, 7), sales(4, 7)).await;
        let (status, _, _, body) =
            parts(get_sales_summary(State(state.clone()), query()).await).await;
        assert_eq!((status, &body["data"]["total"]), (200, &json!(MAX + 4)));
        let (status, _, _, body) =
            parts(crate::proceeds::get_proceeds_summary(State(state), query()).await).await;
        assert_eq!((status, &body["data"]["sales"]), (200, &json!(MAX + 4)));
        server.abort();

        let (state, _, server) = bounded_source(sales(4, 7), sales(MAX + 1, 7)).await;
        assert_over_limit(
            get_sales_summary(State(state.clone()), query()).await,
            "matic",
            "MATIC_COLLECTIONS_SUBGRAPH_URL",
        )
        .await;
        assert_over_limit(
            crate::proceeds::get_proceeds_summary(State(state), query()).await,
            "matic",
            "MATIC_COLLECTIONS_SUBGRAPH_URL",
        )
        .await;
        server.abort();
    }

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn message(pairs: &[(String, String)]) -> String {
        match parse_summary_filters(pairs) {
            Err(e) => e.message(),
            Ok(_) => panic!("expected a rejection"),
        }
    }

    #[test]
    fn a_summary_needs_a_seller_that_is_really_an_address() {
        assert_eq!(message(&pairs(&[])), "A valid seller address is required");
        assert_eq!(
            message(&pairs(&[("seller", "vitalik.eth")])),
            "A valid seller address is required"
        );
        let filters = parse_summary_filters(&pairs(&[(
            "seller",
            "0xABCDEF0123456789012345678901234567890123",
        )]))
        .unwrap();
        assert_eq!(filters.seller, "0xabcdef0123456789012345678901234567890123");
        assert_eq!((filters.from, filters.to), (None, None));
    }

    #[test]
    fn a_bound_that_is_not_an_epoch_millisecond_names_itself() {
        for (key, value) in [
            ("from", "yesterday"),
            ("to", "-1"),
            ("from", "1.5"),
            ("to", "9007199254740992"),
            ("from", ""),
        ] {
            assert_eq!(
                message(&pairs(&[
                    ("seller", "0x1111111111111111111111111111111111111111"),
                    (key, value)
                ])),
                format!("{key} must be an epoch timestamp in milliseconds"),
                "{key}={value}"
            );
        }
        assert_eq!(
            message(&pairs(&[
                ("seller", "0x1111111111111111111111111111111111111111"),
                ("from", "200"),
                ("to", "100")
            ])),
            "from must be less than or equal to to"
        );
        let equal = parse_summary_filters(&pairs(&[
            ("seller", "0x1111111111111111111111111111111111111111"),
            ("from", "100"),
            ("to", "100"),
        ]))
        .unwrap();
        assert_eq!((equal.from, equal.to), (Some(100), Some(100)));
    }

    #[test]
    fn the_summary_aggregates_like_the_old_sql() {
        let sale = |t: &str, price: &str, ts: i64, contract: &str, item: Option<&str>| json!({ "id": format!("{t}-{ts}"), "type": t, "price": price, "timestamp": ts.to_string(), "searchContractAddress": contract, "searchItemId": item });
        let seller = vec![
            sale("mint", "10", 100, "0xa", Some("1")),
            sale("mint", "5", 200, "0xa", Some("1")),
            sale("order", "7", 300, "0xb", Some("2")),
            sale("mint", "1", 50, "0xc", None),
        ];
        let resales = vec![
            sale("order", "3", 150, "0xa", Some("1")),
            sale("bid", "4", 10, "0xa", Some("1")),
        ];
        let f = SalesSummaryFilters {
            seller: "0xs".into(),
            from: Some(100_000),
            to: Some(300_000),
        };
        let s = summarize(&seller, &resales, &f);
        assert_eq!(
            (s.total, s.mints, s.resales, s.earned_wei.as_str()),
            (3, 2, 1, "22")
        );
        assert_eq!(
            s.by_collection,
            vec![
                SalesSummaryCollection {
                    contract_address: "0xa".into(),
                    sold: 2,
                    earned_wei: "15".into()
                },
                SalesSummaryCollection {
                    contract_address: "0xb".into(),
                    sold: 1,
                    earned_wei: "7".into()
                }
            ]
        );
        assert_eq!(
            s.by_item,
            vec![SalesSummaryItem {
                contract_address: "0xa".into(),
                item_id: "1".into(),
                sold_lifetime: 2
            }],
            "lifetime mints ignore the window and need an item"
        );
        assert_eq!(
            s.royalties,
            SalesSummaryRoyalties {
                resales: 1,
                volume_wei: "3".into()
            }
        );
        let v = serde_json::to_value(&s).unwrap();
        assert!(
            v.get("earnedWei").is_some()
                && v["royalties"].get("volumeWei").is_some()
                && v["byItem"][0].get("soldLifetime").is_some()
        );
    }

    #[test]
    fn the_sales_where_speaks_the_graph_and_a_row_keeps_the_shape() {
        let f = parse_filters(&pairs(&[
            ("type", "mint"),
            ("from", "1700000000000"),
            ("category", "wearable"),
            ("seller", "0xABCDEF0123456789012345678901234567890123"),
        ]))
        .unwrap();
        let w = graph_where(&f);
        assert_eq!(w["type"], "mint");
        assert_eq!(w["timestamp_gte"], "1700000000");
        assert_eq!(w["searchCategory_in"], json!(["wearable"]));
        assert_eq!(w["seller"], "0xabcdef0123456789012345678901234567890123");
        let s = sale_from_graph(
            &json!({ "id": "s1", "type": "order", "buyer": "0xb", "seller": "0xs", "price": "9", "timestamp": "1700000000", "txHash": "0xt", "searchContractAddress": "0xc", "searchItemId": "3", "searchTokenId": "77", "searchCategory": "wearable" }),
            Network::Matic,
        );
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({ "id": "s1", "itemId": "3", "contractAddress": "0xc", "buyer": "0xb", "chainId": 137, "network": "MATIC", "price": "9", "seller": "0xs", "timestamp": 1_700_000_000_000i64, "tokenId": "77", "txHash": "0xt", "type": "order" })
        );
    }
}
