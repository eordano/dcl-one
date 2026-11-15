// from: shop@94584a5c:app/src/lib/sales.ts (fetchSellerSales, fetchSalesSummary: the seller's earnings from /v1/sales), app/src/lib/payouts.ts (what upstream matched a sale's proceeds against, under proceeds-to-treasury: not carried); marketplace-server@ae872a4a:src/controllers/routes.ts:197 (GET /v1/sales, the reader these rows come from)
//! Proceeds history: what a seller received, sale by sale and in total.
//!
//! A sale on the Decentraland marketplaces pays its seller in MANA on
//! chain, net of the DAO fee and the creator royalty the contract splits
//! off (`feesCollectorCut`, `royaltiesCut` on the collections subgraph's
//! `Sale`): `net = price - fees - royalties`, exact wei, never a float.
//! That holds for a primary mint and for a resale alike; the Shop does not
//! carry upstream's proceeds-to-treasury flow, which named the treasury as a
//! resale's beneficiary and paid the seller in credits later (SHOP.md: a
//! resale's proceeds are the MANA the seller receives; the crediting
//! consumer and its reconciliation are not ported), so every row here is
//! `settlement: mana` and `treasuryRouting: false`, and a creator's
//! royalties on other people's resales of their items are a separate
//! figure (`royaltiesEarnedWei`), not proceeds of a sale they made.
//!
//! Both routes read the collections subgraphs through the bounded readers
//! (`market::readers`: first ≤ 100, the skip window, the summary's newest
//! `SUMMARY_MAX_SALES` per network); a reader that is not configured, or a
//! seller past the bound, is the unavailable envelope, never a short total
//! presented as the whole.

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::{Value, json};

use super::market::readers::{self, GraphPage, Ttl, seconds_to_ms};
use super::market::sales::{bounded_sales_rows, parse_summary_filters};
use super::market::wire::{ApiError, ChainId, Network, Params};
use super::market::{MarketplaceState, SharedState};

const PROCEEDS_FIELDS: &str = "id type buyer seller price feesCollectorCut royaltiesCut timestamp txHash searchContractAddress searchItemId searchTokenId";
const ROUTE_LIST: &str = "/v1/proceeds";
const ROUTE_SUMMARY: &str = "/v1/proceeds/summary";

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProceedsRow {
    pub id: String,
    /// Milliseconds.
    pub timestamp: i64,
    pub network: Network,
    pub chain_id: ChainId,
    #[serde(rename = "type")]
    pub sale_type: String,
    pub contract_address: String,
    pub item_id: Option<String>,
    pub token_id: Option<String>,
    pub buyer: String,
    pub seller: String,
    pub gross_wei: String,
    pub fees_wei: String,
    pub royalties_wei: String,
    pub net_wei: String,
    pub tx_hash: String,
    pub currency: &'static str,
    pub settlement: &'static str,
}

fn wei(v: &Value, key: &str) -> u128 {
    readers::str_of(v, key).parse::<u128>().unwrap_or(0)
}

/// One subgraph `Sale` as a proceeds row: the seller keeps what the
/// contract did not split off.
pub fn row_from_graph(s: &Value, network: Network) -> ProceedsRow {
    let gross = wei(s, "price");
    let fees = wei(s, "feesCollectorCut").min(gross);
    let royalties = wei(s, "royaltiesCut").min(gross - fees);
    ProceedsRow {
        id: readers::str_of(s, "id"),
        timestamp: seconds_to_ms(readers::int_of(s, "timestamp")),
        network,
        chain_id: network.chain_id(),
        sale_type: readers::str_of(s, "type"),
        contract_address: readers::str_of(s, "searchContractAddress"),
        item_id: readers::opt_str_of(s, "searchItemId"),
        token_id: readers::opt_str_of(s, "searchTokenId"),
        buyer: readers::str_of(s, "buyer"),
        seller: readers::str_of(s, "seller"),
        gross_wei: gross.to_string(),
        fees_wei: fees.to_string(),
        royalties_wei: royalties.to_string(),
        net_wei: (gross - fees - royalties).to_string(),
        tx_hash: readers::str_of(s, "txHash"),
        currency: "MANA",
        settlement: "mana",
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProceedsFilters {
    pub seller: String,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub network: Option<Network>,
    pub first: Option<i64>,
    pub skip: Option<i64>,
}

pub fn parse_filters(pairs: &[(String, String)]) -> Result<ProceedsFilters, ApiError> {
    let summary = parse_summary_filters(pairs)?;
    let p = Params::new(pairs);
    let network = match p.get_string("network", None) {
        Some(raw) => Some(
            Network::parse(&raw)
                .ok_or_else(|| ApiError::bad_request("network must be ETHEREUM or MATIC"))?,
        ),
        None => None,
    };
    Ok(ProceedsFilters {
        seller: summary.seller,
        from: summary.from,
        to: summary.to,
        network,
        first: p.get_number("first", None).map(|v| v as i64),
        skip: p.get_number("skip", None).map(|v| v as i64),
    })
}

fn where_of(f: &ProceedsFilters) -> Value {
    let mut w = json!({ "seller": f.seller.to_lowercase() });
    if let Some(from) = f.from {
        w["timestamp_gte"] = json!((from / 1000).to_string());
    }
    if let Some(to) = f.to {
        w["timestamp_lte"] = json!((to / 1000).to_string());
    }
    w
}

/// `GET /v1/proceeds`: newest first, merged across the networks.
pub async fn list_proceeds(
    state: &MarketplaceState,
    f: &ProceedsFilters,
) -> Result<(Vec<ProceedsRow>, i64), Response> {
    let (first, skip) = readers::deep_page(f.first, f.skip, 24)?;
    let networks = readers::require_networks(state, f.network.map(Network::graph))?;
    let where_ = where_of(f);
    let mut rows: Vec<ProceedsRow> = Vec::new();
    let mut total = 0;
    let mut offset = 0;
    let count = networks.len();
    for (network, url) in networks {
        let page = GraphPage {
            entity: "sales",
            filter_type: "Sale_filter",
            fields: PROCEEDS_FIELDS,
            where_: where_.clone(),
            order_by: "timestamp",
            direction: "desc",
            first,
            skip,
        };
        let (got, skip_after) =
            readers::source_rows(state, network, url, Ttl::S60, &page, first, skip, count)
                .await
                .map_err(|e| readers::upstream_failed(ROUTE_LIST, &e))?;
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
        .map_err(|e| readers::upstream_failed(ROUTE_LIST, &e))?;
        rows.extend(
            got.iter()
                .map(|s| row_from_graph(s, Network::from(network))),
        );
    }
    rows.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(a.id.cmp(&b.id)));
    Ok((
        rows.into_iter()
            .skip(offset as usize)
            .take(first as usize)
            .collect(),
        total,
    ))
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProceedsSummary {
    pub seller: String,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub currency: &'static str,
    pub settlement: &'static str,
    /// Upstream's proceeds-to-treasury routing is not carried.
    pub treasury_routing: bool,
    pub sales: i64,
    pub mints: i64,
    pub resales: i64,
    pub gross_wei: String,
    pub fees_wei: String,
    pub royalties_wei: String,
    pub net_wei: String,
    /// Royalties the seller earned as a creator on other wallets' resales of their items.
    pub royalty_resales: i64,
    pub royalties_earned_wei: String,
    pub first_sale_at: Option<i64>,
    pub last_sale_at: Option<i64>,
}

/// The totals over the rows, every figure a saturating exact integer.
pub fn summarize(
    seller: &str,
    own: &[ProceedsRow],
    creator_resales: &[Value],
    from: Option<i64>,
    to: Option<i64>,
) -> ProceedsSummary {
    let in_window = |ts: i64| from.is_none_or(|f| ts >= f) && to.is_none_or(|t| ts <= t);
    let rows: Vec<&ProceedsRow> = own.iter().filter(|r| in_window(r.timestamp)).collect();
    let sum = |pick: fn(&ProceedsRow) -> &String| {
        rows.iter().fold(0u128, |acc, r| {
            acc.saturating_add(pick(r).parse::<u128>().unwrap_or(0))
        })
    };
    let royalty_rows: Vec<&Value> = creator_resales
        .iter()
        .filter(|s| in_window(seconds_to_ms(readers::int_of(s, "timestamp"))))
        .collect();
    ProceedsSummary {
        seller: seller.to_lowercase(),
        from,
        to,
        currency: "MANA",
        settlement: "mana",
        treasury_routing: false,
        sales: rows.len() as i64,
        mints: rows.iter().filter(|r| r.sale_type == "mint").count() as i64,
        resales: rows
            .iter()
            .filter(|r| matches!(r.sale_type.as_str(), "order" | "bid"))
            .count() as i64,
        gross_wei: sum(|r| &r.gross_wei).to_string(),
        fees_wei: sum(|r| &r.fees_wei).to_string(),
        royalties_wei: sum(|r| &r.royalties_wei).to_string(),
        net_wei: sum(|r| &r.net_wei).to_string(),
        royalty_resales: royalty_rows.len() as i64,
        royalties_earned_wei: royalty_rows
            .iter()
            .fold(0u128, |acc, s| acc.saturating_add(wei(s, "royaltiesCut")))
            .to_string(),
        first_sale_at: rows.iter().map(|r| r.timestamp).min(),
        last_sale_at: rows.iter().map(|r| r.timestamp).max(),
    }
}

pub async fn proceeds_summary(
    state: &MarketplaceState,
    f: &ProceedsFilters,
) -> Result<ProceedsSummary, Response> {
    let seller = f.seller.to_lowercase();
    let own: Vec<ProceedsRow> = bounded_sales_rows(
        state,
        json!({ "seller": seller }),
        PROCEEDS_FIELDS,
        ROUTE_SUMMARY,
    )
    .await?
    .iter()
    .map(|(n, s)| row_from_graph(s, Network::from(*n)))
    .collect();
    let creator_resales: Vec<Value> = bounded_sales_rows(state, json!({ "type_in": ["order", "bid"], "item_": { "creator": seller }, "seller_not": seller }), PROCEEDS_FIELDS, ROUTE_SUMMARY).await?.into_iter().map(|(_, s)| s).collect();
    Ok(summarize(&seller, &own, &creator_resales, f.from, f.to))
}

/// `GET /v1/proceeds?seller=&first=&skip=&from=&to=&network=`.
pub async fn get_proceeds(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let f = match parse_filters(&pairs) {
        Ok(f) => f,
        Err(e) => return e.into_response(),
    };
    match list_proceeds(&state, &f).await {
        Ok((data, total)) => (
            [(CACHE_CONTROL, "public, max-age=60")],
            Json(json!({ "data": data, "total": total })),
        )
            .into_response(),
        Err(r) => r,
    }
}

/// `GET /v1/proceeds/summary?seller=&from=&to=`.
pub async fn get_proceeds_summary(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let f = match parse_filters(&pairs) {
        Ok(f) => f,
        Err(e) => return e.into_response(),
    };
    match proceeds_summary(&state, &f).await {
        Ok(summary) => (
            [(CACHE_CONTROL, "public, max-age=300")],
            Json(json!({ "ok": true, "data": summary })),
        )
            .into_response(),
        Err(r) => r,
    }
}

pub fn router() -> Router<SharedState> {
    Router::new()
        .route(ROUTE_SUMMARY, get(get_proceeds_summary))
        .route(ROUTE_LIST, get(get_proceeds))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sale(id: &str, kind: &str, ts: i64, price: &str, fees: &str, royalties: &str) -> Value {
        json!({ "id": id, "type": kind, "buyer": "0xb", "seller": "0xs", "price": price, "feesCollectorCut": fees, "royaltiesCut": royalties, "timestamp": ts.to_string(), "txHash": "0xt", "searchContractAddress": "0xc", "searchItemId": "1", "searchTokenId": null })
    }

    #[test]
    fn a_row_keeps_what_the_contract_did_not_split_off() {
        let r = row_from_graph(
            &sale(
                "s1",
                "order",
                1_700_000_000,
                "1000000000000000000",
                "25000000000000000",
                "25000000000000000",
            ),
            Network::Matic,
        );
        assert_eq!(
            (
                r.gross_wei.as_str(),
                r.fees_wei.as_str(),
                r.royalties_wei.as_str(),
                r.net_wei.as_str()
            ),
            (
                "1000000000000000000",
                "25000000000000000",
                "25000000000000000",
                "950000000000000000"
            )
        );
        assert_eq!(
            (r.timestamp, r.chain_id, r.currency, r.settlement),
            (1_700_000_000_000, Network::Matic.chain_id(), "MANA", "mana")
        );
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["type"], "order");
        assert_eq!(v["netWei"], "950000000000000000");
        // Cuts a malformed row claims beyond the price never go negative.
        let odd = row_from_graph(&sale("s2", "mint", 1, "10", "8", "8"), Network::Ethereum);
        assert_eq!(
            (
                odd.fees_wei.as_str(),
                odd.royalties_wei.as_str(),
                odd.net_wei.as_str()
            ),
            ("8", "2", "0")
        );
    }

    #[test]
    fn the_summary_sums_exactly_and_windows_by_milliseconds() {
        let own: Vec<ProceedsRow> = [
            sale("a", "mint", 1000, "100", "3", "0"),
            sale("b", "order", 2000, "200", "5", "10"),
            sale("c", "bid", 3000, "300", "7", "20"),
        ]
        .iter()
        .map(|s| row_from_graph(s, Network::Matic))
        .collect();
        let royalties = vec![
            sale("d", "order", 2500, "400", "10", "40"),
            sale("e", "order", 9000, "400", "10", "40"),
        ];
        let s = summarize("0xS", &own, &royalties, Some(1_500_000), Some(3_500_000));
        assert_eq!((s.sales, s.mints, s.resales), (2, 0, 2));
        assert_eq!(
            (
                s.gross_wei.as_str(),
                s.fees_wei.as_str(),
                s.royalties_wei.as_str(),
                s.net_wei.as_str()
            ),
            ("500", "12", "30", "458")
        );
        assert_eq!(
            (s.royalty_resales, s.royalties_earned_wei.as_str()),
            (1, "40")
        );
        assert_eq!(
            (
                s.first_sale_at,
                s.last_sale_at,
                s.seller.as_str(),
                s.treasury_routing
            ),
            (Some(2_000_000), Some(3_000_000), "0xs", false)
        );
        let all = summarize("0xs", &own, &royalties, None, None);
        assert_eq!(
            (all.sales, all.net_wei.as_str(), all.royalty_resales),
            (3, "555", 2)
        );
    }

    #[test]
    fn filters_need_a_seller_and_a_known_network() {
        let pairs = |q: &[(&str, &str)]| {
            q.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        assert!(parse_filters(&pairs(&[])).is_err());
        let f = parse_filters(&pairs(&[
            ("seller", "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            ("network", "MATIC"),
            ("from", "5"),
            ("first", "10"),
        ]))
        .unwrap();
        assert_eq!(
            (f.network, f.from, f.first, f.seller.as_str()),
            (
                Some(Network::Matic),
                Some(5),
                Some(10),
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
        );
        assert!(
            parse_filters(&pairs(&[
                ("seller", "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
                ("network", "MARS")
            ]))
            .is_err()
        );
        assert_eq!(where_of(&f)["timestamp_gte"], "0");
    }
}
