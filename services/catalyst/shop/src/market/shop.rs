// from: marketplace-server src/ports/shop-catalog/component.ts (couponJoin), catalyrst/crates/catalyrst-market/src/handlers/shop_catalog.rs, catalyrst/crates/catalyrst-market/src/ports/shop_catalog/types.rs, catalyrst/crates/catalyrst-market/src/ports/shop_catalog/sql.rs, catalyrst/crates/catalyrst-market/src/ports/shop_catalog/component.rs, catalyrst/crates/catalyrst-market/src/ports/shop_catalog/unified/mod.rs, catalyrst/crates/catalyrst-market/src/ports/shop_catalog/unified/params.rs, catalyrst/crates/catalyrst-market/src/handlers/suggestions.rs (the fallback envelope), catalyrst/crates/catalyrst-market/src/handlers/coupons.rs (the route list)
//! The shop's feeds (`/v3/catalog/shop|legacy|unified|related|trending|
//! creators|importable|suggested`) over the items mirror and the trades
//! mirror. The old feeds joined `mv_trades` to the squid's item, nft and
//! metadata tables and a coupon table; here every listing resolves to a row
//! of `shop.items` (an item order by its item id, an nft order by
//! `token_id >> 216`), so what the mirror does not carry the shop does not
//! sell: LAND, estate and name resales wait for the nft indexer. A native
//! primary carries the creator's best live coupon from `shop.coupons`
//! (paired with the marketplace version and chain the listing settles on):
//! it is priced, filtered (`discounted`) and sorted at the sale price, with
//! the list price as the compare-at; every other branch has no sale. The credit price of a
//! MANA listing uses the economy price snapshot (`items::mana_usd_rate`;
//! the price envelope without a fresh one).
//! `/v3/catalog/trending` and `/v3/catalog/creators` rank on the sales
//! reader (`items::recent_sales`: the last days of `sales` on each
//! configured collections subgraph, at most 1000 rows per network, 10 min),
//! so a window longer than those rows cover is truncated to them, and a
//! creator's `totalSales` is the windowed count rather than all time.
//! `/v3/catalog/suggested` ranks the co-ownership neighbours of a wallet's
//! holdings and the request's seeds (`neighbours`, built from the nft
//! index), with the trending floor filling the rest and answering alone
//! until a build exists (`personalized: false`). Coupons are stored and
//! served by `/v1/coupons` (`coupons`).

#[path = "shop_queries.rs"]
mod queries;
pub(crate) use queries::coupon_pairings;
use queries::{ReferenceItem, since};
pub use queries::{
    build_importable_listings_sql, build_items_by_ids_sql, build_legacy_listings_sql,
    build_related_items_sql, build_shop_listings_sql, build_unified_items_sql,
    build_unified_listings_sql,
};
#[cfg(test)]
use queries::{related_rarity_sort_key, unified_branch, unified_selection};
#[path = "shop_projection.rs"]
mod projection;
use projection::{
    listing_type, map_unified_item, map_unified_listing, opt, parse_available, top_level_category,
};
#[path = "shop_recommendations.rs"]
mod recommendations;
#[cfg(test)]
use recommendations::{
    MAX_EQUIPPED, RailCandidate, SeedSource, build_profile, item_id_values, rerank_for_diversity,
    urn_to_item_id,
};
use recommendations::{MIN_PERSONAL_ROWS, personal_rows};
pub use recommendations::{aggregate_sales, rank_trending};

use crate::types::ManaUsdRate;
use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::catalog::{Builder, ITEM_ID_SHIFT, MAX_NUMERIC_NUMBER, escape_like, unified_trades_cte};
use super::items::{USD_WEI_PER_CREDIT, mana_usd_rate, rate_to_numeric_string, recent_sales};
use super::readers::{self, Ttl, cached};
use super::wire::{
    ApiError, ChainId, DataTotal, Network, Params, body_shapes_for_genders, emote_play_mode_clause,
    network_and_chain, where_from,
};
use super::{MarketplaceState, SharedState};

pub const SHOP_DEFAULT_PAGE_SIZE: i64 = 48;
pub const SHOP_MIN_PAGE_SIZE: i64 = 1;
pub const SHOP_MAX_PAGE_SIZE: i64 = 1000;

/// The smallest published catalogue a "top creator" can have: a 30-day
/// window can be won on one lucky item and the rail exists to send a shopper
/// off to browse.
pub const TOP_CREATORS_MIN_ITEMS: i64 = 10;
/// A rate: this many sales per default-length window, scaled to the window asked for.
pub const TOP_CREATORS_MIN_SALES_PER_WINDOW: i64 = 5;
pub const TOP_CREATORS_MIN_WINDOW_SALES_FLOOR: i64 = 2;
pub const TOP_CREATORS_DEFAULT_LIMIT: i64 = 30;
pub const TOP_CREATORS_MIN_LIMIT: i64 = 1;
pub const TOP_CREATORS_MAX_LIMIT: i64 = 60;
pub const TOP_CREATORS_DEFAULT_DAYS: i64 = 30;
pub const TOP_CREATORS_MIN_DAYS: i64 = 1;
pub const TOP_CREATORS_MAX_DAYS: i64 = 365;

pub const TRENDING_DEFAULT_LIMIT: i64 = 12;
pub const TRENDING_MAX_LIMIT: i64 = 50;
pub const TRENDING_DEFAULT_DAYS: i64 = 7;
pub const TRENDING_MIN_DAYS: i64 = 1;
pub const TRENDING_MAX_DAYS: i64 = 7;
/// Share of the rail's slots going to the highest sale count; the rest go to
/// the biggest traded volume among what the first pass left behind.
pub const TRENDING_SALES_CUT: f64 = 0.6;

pub const RELATED_DEFAULT_LIMIT: i64 = 10;
pub const RELATED_MAX_LIMIT: i64 = 50;

/// 1e30 USD wei: rows above it are dropped rather than overflowing the bigint credit cast.
pub const MAX_USD_WEI: &str = "1000000000000000000000000000000";

const ASSET_TYPE_ERC20: i64 = 1;
const ASSET_TYPE_USD_PEGGED_MANA: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShopListingType {
    Primary,
    Secondary,
}

pub const SHOP_LISTING_TYPE_VALUES: &[&str] = &["primary", "secondary"];

impl ShopListingType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "primary" => Some(Self::Primary),
            "secondary" => Some(Self::Secondary),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShopSortBy {
    Newest,
    Cheapest,
    MostExpensive,
    Name,
    Discount,
    /// The default of a search; without one, `newest`.
    Relevance,
}

pub const SHOP_SORT_VALUES: &[&str] = &[
    "newest",
    "cheapest",
    "most_expensive",
    "name",
    "discount",
    "relevance",
];

impl ShopSortBy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "newest" => Some(Self::Newest),
            "cheapest" => Some(Self::Cheapest),
            "most_expensive" => Some(Self::MostExpensive),
            "name" => Some(Self::Name),
            "discount" => Some(Self::Discount),
            "relevance" => Some(Self::Relevance),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifiedSource {
    Native,
    Legacy,
}

pub const UNIFIED_SOURCE_VALUES: &[&str] = &["native", "legacy"];

impl UnifiedSource {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "native" => Some(Self::Native),
            "legacy" => Some(Self::Legacy),
            _ => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Legacy => "legacy",
        }
    }
}

/// `Trade`: a signed off-chain order, bought with `accept`. `Store`: a
/// CollectionStore mint, bought with `CollectionStore.buy`: no order, no
/// signature, finite supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifiedAcquisition {
    Trade,
    Store,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnifiedGroupBy {
    #[default]
    Listing,
    Item,
}

pub const UNIFIED_GROUP_BY_VALUES: &[&str] = &["listing", "item"];

impl UnifiedGroupBy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "listing" => Some(Self::Listing),
            "item" => Some(Self::Item),
            _ => None,
        }
    }
}

pub fn parse_unified_group_by(pairs: &[(String, String)]) -> UnifiedGroupBy {
    Params::new(pairs)
        .get_value("groupBy", UNIFIED_GROUP_BY_VALUES, None)
        .as_deref()
        .and_then(UnifiedGroupBy::parse)
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct ShopCatalogFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub category: Option<String>,
    pub contract_address: Option<String>,
    pub item_id: Option<String>,
    /// `creator`: one or more comma-separated creator addresses, any of which matches.
    pub creator: Vec<String>,
    pub rarities: Vec<String>,
    pub wearable_categories: Vec<String>,
    pub is_smart: bool,
    pub wearable_genders: Vec<String>,
    pub min_price_credits: Option<f64>,
    pub max_price_credits: Option<f64>,
    pub search: Option<String>,
    pub sort_by: Option<ShopSortBy>,
    pub discounted: Option<bool>,
    pub include_social_emotes: bool,
    pub listing_type: Option<ShopListingType>,
    /// `emotePlayMode` (`loop` | `simple`): `Some(true)` loops only, `Some(false)`
    /// plays once only; both or neither is no filter (as `/v3/catalog/items`).
    pub emote_loop: Option<bool>,
    /// `emoteHasSound` / `emoteHasGeometry`: emotes with sound / with props.
    pub emote_has_sound: bool,
    pub emote_has_geometry: bool,
}

impl Default for ShopCatalogFilters {
    fn default() -> Self {
        Self {
            first: None,
            skip: None,
            category: None,
            contract_address: None,
            item_id: None,
            creator: Vec::new(),
            rarities: Vec::new(),
            wearable_categories: Vec::new(),
            is_smart: false,
            wearable_genders: Vec::new(),
            min_price_credits: None,
            max_price_credits: None,
            search: None,
            sort_by: None,
            discounted: None,
            include_social_emotes: true,
            listing_type: None,
            emote_loop: None,
            emote_has_sound: false,
            emote_has_geometry: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LegacyCatalogFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub category: Option<String>,
    pub rarities: Vec<String>,
    pub wearable_categories: Vec<String>,
    pub search: Option<String>,
    pub sort_by: Option<ShopSortBy>,
}

#[derive(Debug, Clone, Default)]
pub struct UnifiedCatalogFilters {
    pub base: ShopCatalogFilters,
    pub source: Option<UnifiedSource>,
    /// `None` is no collection filter; `Some(vec![])` is a named set that resolved to nothing and yields an empty page.
    pub contract_addresses: Option<Vec<String>>,
    /// Individual items (`items=<contract>-<itemId>`), UNIONED with
    /// `contract_addresses`: the selection is empty only when both sets are,
    /// so a campaign whose collections resolved to nothing still shows the
    /// loose items it named. The singular `itemId` still intersects.
    pub item_ids: Option<Vec<String>>,
    /// Whether the legacy (MANA) trade branch may contribute resales; only an explicit `includeLegacySecondary=true` opens it.
    pub include_legacy_secondary: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RelatedItemsFilters {
    pub include_legacy_secondary: bool,
    pub listing_type: Option<ShopListingType>,
}

pub struct TrendingRequest {
    pub first: Option<i64>,
    pub days: Option<i64>,
    pub filters: UnifiedCatalogFilters,
}

fn csv(value: Option<String>) -> Vec<String> {
    value
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn finite_i64(v: Option<f64>) -> Option<i64> {
    v.filter(|n| n.is_finite()).map(|n| n as i64)
}

/// Read as a string: a presence check would read `discounted=false` as true.
fn parse_discounted(value: Option<String>) -> Option<bool> {
    match value.as_deref() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// An unrecognized value drops the filter, as upstream's `getValue` did.
fn parse_listing_type(p: &Params) -> Option<ShopListingType> {
    p.get_value("listingType", SHOP_LISTING_TYPE_VALUES, None)
        .as_deref()
        .and_then(ShopListingType::parse)
}

pub const SHOP_GENDER_VALUES: &[&str] = &["male", "female", "unisex"];

/// Either encoding: the comma list or the repeated key; anything outside the values is dropped.
fn parse_wearable_genders(p: &Params) -> Vec<String> {
    p.get_list("wearableGender", &[])
        .into_iter()
        .flat_map(|v| {
            v.split(',')
                .map(|s| s.trim().to_lowercase())
                .collect::<Vec<_>>()
        })
        .filter(|g| SHOP_GENDER_VALUES.contains(&g.as_str()))
        .collect()
}

/// The collections the unified feed is restricted to, or `None` when the
/// caller named none. A blank value reads as absent; a present non-address
/// yields an empty set.
fn contract_address_list(p: &Params) -> Option<Vec<String>> {
    let raw: Vec<String> = p
        .get_list("contractAddress", &[])
        .into_iter()
        .flat_map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.into_iter()
            .filter(|v| super::is_valid_eth_address(v))
            .map(|v| v.to_lowercase())
            .collect(),
    )
}

/// The individual items the unified feed selects, as `<contract>-<itemId>`
/// composite ids (the id the catalogue reports), or `None` when the caller
/// named none. Both encodings; malformed entries are dropped (a typo costs
/// one item, not the page), and leading zeros of the item id are stripped
/// since the column compares as `7`, never `007`.
fn item_id_list(p: &Params) -> Option<Vec<String>> {
    let raw: Vec<String> = p
        .get_list("items", &[])
        .into_iter()
        .flat_map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.into_iter()
            .filter_map(|v| {
                let (contract, item_id) = v.rsplit_once('-')?;
                if !super::is_valid_eth_address(contract)
                    || item_id.is_empty()
                    || !item_id.bytes().all(|c| c.is_ascii_digit())
                {
                    return None;
                }
                let item_id = item_id.trim_start_matches('0');
                Some(format!(
                    "{}-{}",
                    contract.to_lowercase(),
                    if item_id.is_empty() { "0" } else { item_id }
                ))
            })
            .collect(),
    )
}

pub fn parse_shop_filters(pairs: &[(String, String)]) -> ShopCatalogFilters {
    let p = Params::new(pairs);
    ShopCatalogFilters {
        first: finite_i64(p.get_number("first", None)),
        skip: finite_i64(p.get_number("skip", None)),
        category: p.get_string("category", None),
        contract_address: p.get_string("contractAddress", None),
        item_id: p.get_string("itemId", None),
        creator: csv(p.get_string("creator", None)),
        rarities: csv(p.get_string("rarity", None)),
        wearable_categories: csv(p.get_string("wearableCategory", None)),
        is_smart: p.get_boolean("isSmart"),
        wearable_genders: Vec::new(),
        min_price_credits: p.get_number("minPriceCredits", None),
        max_price_credits: p.get_number("maxPriceCredits", None),
        search: p.get_string("search", None),
        sort_by: p
            .get_value("sortBy", SHOP_SORT_VALUES, None)
            .as_deref()
            .and_then(ShopSortBy::parse),
        discounted: parse_discounted(p.get_string("discounted", None)),
        include_social_emotes: p.get_string("includeSocialEmotes", None).as_deref()
            != Some("false"),
        listing_type: parse_listing_type(&p),
        emote_loop: emote_play_mode_clause(&p.get_list("emotePlayMode", &["loop", "simple"])),
        emote_has_sound: p.get_boolean("emoteHasSound"),
        emote_has_geometry: p.get_boolean("emoteHasGeometry"),
    }
}

pub fn parse_legacy_filters(pairs: &[(String, String)]) -> LegacyCatalogFilters {
    let p = Params::new(pairs);
    LegacyCatalogFilters {
        first: finite_i64(p.get_number("first", None)),
        skip: finite_i64(p.get_number("skip", None)),
        category: p.get_string("category", None),
        rarities: csv(p.get_string("rarity", None)),
        wearable_categories: csv(p.get_string("wearableCategory", None)),
        search: p.get_string("search", None),
        sort_by: p
            .get_value("sortBy", SHOP_SORT_VALUES, None)
            .as_deref()
            .and_then(ShopSortBy::parse),
    }
}

fn parse_include_legacy_secondary(p: &Params) -> bool {
    p.get_string("includeLegacySecondary", None).as_deref() == Some("true")
}

pub fn parse_unified_filters(pairs: &[(String, String)]) -> UnifiedCatalogFilters {
    let p = Params::new(pairs);
    let mut base = parse_shop_filters(pairs);
    base.wearable_genders = parse_wearable_genders(&p);
    let contract_addresses = contract_address_list(&p);
    if contract_addresses.is_some() {
        base.contract_address = None;
    }
    UnifiedCatalogFilters {
        base,
        source: p
            .get_value("source", UNIFIED_SOURCE_VALUES, None)
            .as_deref()
            .and_then(UnifiedSource::parse),
        contract_addresses,
        item_ids: item_id_list(&p),
        include_legacy_secondary: parse_include_legacy_secondary(&p),
    }
}

pub fn parse_related_filters(pairs: &[(String, String)]) -> RelatedItemsFilters {
    let p = Params::new(pairs);
    RelatedItemsFilters {
        include_legacy_secondary: parse_include_legacy_secondary(&p),
        listing_type: parse_listing_type(&p),
    }
}

/// Narrowed to what the old trending handler read: a browse-only parameter
/// has no effect, the ranking is the sort and a rail has no pages.
pub fn parse_trending_filters(pairs: &[(String, String)]) -> TrendingRequest {
    let p = Params::new(pairs);
    let base = ShopCatalogFilters {
        category: p.get_string("category", None),
        rarities: csv(p.get_string("rarity", None)),
        wearable_categories: csv(p.get_string("wearableCategory", None)),
        include_social_emotes: p.get_string("includeSocialEmotes", None).as_deref()
            != Some("false"),
        listing_type: parse_listing_type(&p),
        ..Default::default()
    };
    TrendingRequest {
        first: finite_i64(p.get_number("first", None)),
        days: finite_i64(p.get_number("days", None)),
        filters: UnifiedCatalogFilters {
            base,
            source: p
                .get_value("source", UNIFIED_SOURCE_VALUES, None)
                .as_deref()
                .and_then(UnifiedSource::parse),
            contract_addresses: None,
            item_ids: None,
            include_legacy_secondary: parse_include_legacy_secondary(&p),
        },
    }
}

fn shop_clamp_first(first: Option<i64>) -> i64 {
    first
        .unwrap_or(SHOP_DEFAULT_PAGE_SIZE)
        .clamp(SHOP_MIN_PAGE_SIZE, SHOP_MAX_PAGE_SIZE)
}

fn shop_clamp_skip(skip: Option<i64>) -> i64 {
    skip.unwrap_or(0).max(0)
}

fn top_creators_clamp_first(first: Option<i64>) -> i64 {
    first
        .unwrap_or(TOP_CREATORS_DEFAULT_LIMIT)
        .clamp(TOP_CREATORS_MIN_LIMIT, TOP_CREATORS_MAX_LIMIT)
}

fn top_creators_clamp_days(days: Option<i64>) -> i64 {
    days.unwrap_or(TOP_CREATORS_DEFAULT_DAYS)
        .clamp(TOP_CREATORS_MIN_DAYS, TOP_CREATORS_MAX_DAYS)
}

/// `days` already clamped; never below the floor, so a one-day window cannot round the bar to one sale.
fn top_creators_min_sales(days: i64) -> i64 {
    let scaled =
        (days as f64 / TOP_CREATORS_DEFAULT_DAYS as f64) * TOP_CREATORS_MIN_SALES_PER_WINDOW as f64;
    (scaled.round() as i64).max(TOP_CREATORS_MIN_WINDOW_SALES_FLOOR)
}

fn trending_clamp_first(first: Option<i64>) -> i64 {
    first
        .unwrap_or(TRENDING_DEFAULT_LIMIT)
        .clamp(SHOP_MIN_PAGE_SIZE, TRENDING_MAX_LIMIT)
}

fn trending_clamp_days(days: Option<i64>) -> i64 {
    days.unwrap_or(TRENDING_DEFAULT_DAYS)
        .clamp(TRENDING_MIN_DAYS, TRENDING_MAX_DAYS)
}

fn related_clamp_first(first: Option<i64>) -> i64 {
    first
        .unwrap_or(RELATED_DEFAULT_LIMIT)
        .clamp(SHOP_MIN_PAGE_SIZE, RELATED_MAX_LIMIT)
}

pub fn credits_to_wei(credits: f64) -> Option<u128> {
    if !credits.is_finite() {
        return None;
    }
    Some(credits.max(0.0).floor() as u128 * USD_WEI_PER_CREDIT)
}

fn unified_min_price_bound_wei(min_credits: f64) -> Option<u128> {
    let min_wei = credits_to_wei(min_credits)?;
    if min_wei == 0 {
        return None;
    }
    Some(min_wei - USD_WEI_PER_CREDIT)
}

/// The TypeScript shape of the `coupon` object [`COUPON_COLUMNS`] builds
/// (plus the `proof` the row mapping adds): the buy side hands it to
/// `acceptWithCoupon` as is.
#[cfg(feature = "ts")]
pub mod coupon_shape {
    #[derive(ts_rs::TS)]
    #[ts(export, export_to = "market/", rename_all = "camelCase")]
    pub struct ShopCouponRow {
        pub id: String,
        pub signer: String,
        pub coupon_manager: String,
        pub coupon_address: String,
        #[ts(
            type = "{ uses: number; expiration: number; effective: number; salt: string; contractSignatureIndex: number; signerSignatureIndex: number; allowedRoot: string; externalChecks: Array<{ contractAddress: string; selector: string; value: string; required: boolean }> }"
        )]
        pub checks: (),
        #[ts(type = "number")]
        pub discount_type: i64,
        /// Parts per million: 300_000 is 30% off.
        #[ts(type = "number")]
        pub discount: i64,
        pub root: String,
        pub collections: Vec<String>,
        pub signature: String,
        /// Purchases already settled with this coupon (`shop.coupon_state.uses`).
        #[ts(type = "number")]
        pub used: i64,
    }

    #[derive(ts_rs::TS)]
    #[ts(export, export_to = "market/", rename_all = "camelCase")]
    pub struct ShopCoupon {
        #[ts(flatten)]
        pub row: ShopCouponRow,
        pub proof: Vec<String>,
    }
}

/// The sale half of a row: a creator coupon's discount on a native primary
/// (`sale_fields`), all `None` otherwise.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct ShopSaleFields {
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub compare_at_credits: Option<i64>,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub sale_ends_at: Option<i64>,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub sale_units_left: Option<i64>,
    #[cfg_attr(feature = "ts", ts(as = "Option<coupon_shape::ShopCoupon>"))]
    pub coupon: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct ShopListing {
    pub trade_id: String,
    pub listing_type: String,
    pub contract_address: String,
    pub item_id: Option<String>,
    pub token_id: Option<String>,
    pub name: String,
    pub thumbnail: String,
    pub rarity: String,
    pub category: String,
    pub wearable_category: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "\"male\" | \"female\" | \"unisex\" | null"))]
    pub gender: Option<String>,
    pub creator: String,
    pub seller: Option<String>,
    pub issued_id: Option<String>,
    /// `None` on a resale row: credits never price an existing token (SHOP.md).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub price_credits: Option<u64>,
    #[serde(flatten)]
    #[cfg_attr(feature = "ts", ts(flatten))]
    pub sale: ShopSaleFields,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available: i64,
    pub network: Network,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct ImportableListing {
    pub old_trade_id: String,
    pub listing_type: String,
    pub contract_address: String,
    pub item_id: Option<String>,
    pub token_id: Option<String>,
    pub name: String,
    pub thumbnail: String,
    pub rarity: String,
    pub category: String,
    pub wearable_category: Option<String>,
    pub mana_wei: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available: i64,
    pub network: Network,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct LegacyListing {
    pub trade_id: String,
    pub listing_type: String,
    pub contract_address: String,
    pub item_id: Option<String>,
    pub name: String,
    pub thumbnail: String,
    pub rarity: String,
    pub category: String,
    pub wearable_category: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "\"male\" | \"female\" | \"unisex\" | null"))]
    pub gender: Option<String>,
    pub creator: String,
    pub mana_wei: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available: i64,
    pub network: Network,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct UnifiedListing {
    #[cfg_attr(feature = "ts", ts(type = "\"native\" | \"legacy\""))]
    pub source: String,
    #[cfg_attr(feature = "ts", ts(type = "\"trade\" | \"store\""))]
    pub acquisition: String,
    /// `None` for a CollectionStore mint: no trade exists and a fabricated id would reach the money ledger.
    pub trade_id: Option<String>,
    pub listing_type: String,
    pub contract_address: String,
    pub item_id: Option<String>,
    pub token_id: Option<String>,
    pub name: String,
    pub thumbnail: String,
    pub rarity: String,
    pub category: String,
    pub wearable_category: Option<String>,
    /// `None` is "not an emote", `false` is "plays once"; the key always reaches the wire.
    pub emote_loop: Option<bool>,
    #[cfg_attr(feature = "ts", ts(type = "\"male\" | \"female\" | \"unisex\" | null"))]
    pub gender: Option<String>,
    pub creator: String,
    pub seller: Option<String>,
    pub issued_id: Option<String>,
    /// `None` on a resale row: credits never price an existing token (SHOP.md).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub price_credits: Option<i64>,
    #[serde(flatten)]
    #[cfg_attr(feature = "ts", ts(flatten))]
    pub sale: ShopSaleFields,
    pub mana_wei: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available: i64,
    pub network: Network,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct UnifiedItem {
    #[cfg_attr(feature = "ts", ts(type = "\"native\" | \"legacy\""))]
    pub source: String,
    #[cfg_attr(feature = "ts", ts(type = "\"trade\" | \"store\""))]
    pub acquisition: String,
    pub trade_id: Option<String>,
    pub listing_type: String,
    pub contract_address: String,
    pub item_id: Option<String>,
    pub token_id: Option<String>,
    pub name: String,
    pub thumbnail: String,
    pub rarity: String,
    pub category: String,
    pub wearable_category: Option<String>,
    pub emote_loop: Option<bool>,
    #[cfg_attr(feature = "ts", ts(type = "\"male\" | \"female\" | \"unisex\" | null"))]
    pub gender: Option<String>,
    pub creator: String,
    pub seller: Option<String>,
    pub issued_id: Option<String>,
    /// `None` on a resale row: credits never price an existing token (SHOP.md).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub price_credits: Option<i64>,
    #[serde(flatten)]
    #[cfg_attr(feature = "ts", ts(flatten))]
    pub sale: ShopSaleFields,
    pub mana_wei: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub listing_count: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available: i64,
    pub network: Network,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct TrendingItem {
    #[serde(flatten)]
    #[cfg_attr(feature = "ts", ts(flatten))]
    pub item: UnifiedItem,
    #[serde(rename = "trendingSales")]
    #[cfg_attr(feature = "ts", ts(rename = "trendingSales", type = "number"))]
    pub trending_sales: i64,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct TopCreator {
    pub id: String,
    pub volume_wei: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub sales: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total_sales: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub collections: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub items: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct ImportableResponseBody {
    pub data: Vec<ImportableListing>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct RelatedResponseBody {
    pub data: Vec<UnifiedItem>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct TrendingResponseBody {
    pub data: Vec<TrendingItem>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct TopCreatorsResponseBody {
    pub data: Vec<TopCreator>,
}

/// The `kind` strings `SuggestionReason` carries.
#[cfg(feature = "ts")]
#[derive(ts_rs::TS)]
#[ts(export, export_to = "market/", rename_all = "snake_case")]
pub enum SuggestionReasonKind {
    CoOwned,
    CreatorAffinity,
    FavoriteSimilar,
    EquippedSimilar,
    SeedSimilar,
    Trending,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct SuggestionReason {
    #[cfg_attr(feature = "ts", ts(as = "SuggestionReasonKind"))]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub creator: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct SuggestedItem {
    #[serde(flatten)]
    #[cfg_attr(feature = "ts", ts(flatten))]
    pub item: UnifiedItem,
    pub reason: SuggestionReason,
    pub score: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct SuggestionsResponse {
    pub data: Vec<SuggestedItem>,
    pub personalized: bool,
    pub algorithm: String,
}

pub const SUGGESTIONS_ALGORITHM_VERSION: &str = "v1";

async fn fetch(state: &MarketplaceState, b: Builder) -> Result<Vec<PgRow>, ApiError> {
    Ok(sqlx::query_with(sqlx::AssertSqlSafe(b.sql), b.args)
        .fetch_all(&state.pool)
        .await?)
}

fn total_of(rows: &[PgRow]) -> i64 {
    rows.first().map(|r| r.get::<i64, _>("total")).unwrap_or(0)
}

pub async fn shop_listings(
    state: &MarketplaceState,
    f: &ShopCatalogFilters,
) -> Result<(Vec<ShopListing>, i64), ApiError> {
    let rows = fetch(state, build_shop_listings_sql(f, since(state))).await?;
    let total = total_of(&rows);
    let data = rows
        .iter()
        .map(|r| {
            let l = map_unified_listing(r);
            ShopListing {
                trade_id: l.trade_id.unwrap_or_default(),
                listing_type: l.listing_type,
                contract_address: l.contract_address,
                item_id: l.item_id,
                token_id: l.token_id,
                name: l.name,
                thumbnail: l.thumbnail,
                rarity: l.rarity,
                category: l.category,
                wearable_category: l.wearable_category,
                gender: l.gender,
                creator: l.creator,
                seller: l.seller,
                issued_id: l.issued_id,
                price_credits: l.price_credits.map(|c| c.max(0) as u64),
                sale: l.sale,
                available: l.available,
                network: l.network,
                chain_id: l.chain_id,
                created_at: l.created_at,
            }
        })
        .collect();
    Ok((data, total))
}

pub async fn legacy_listings(
    state: &MarketplaceState,
    f: &LegacyCatalogFilters,
) -> Result<(Vec<LegacyListing>, i64), ApiError> {
    let rows = fetch(state, build_legacy_listings_sql(f, since(state))).await?;
    let total = total_of(&rows);
    let data = rows
        .iter()
        .map(|r| {
            let (network, chain_id) =
                network_and_chain(&opt::<String>(r, "network").unwrap_or_default());
            LegacyListing {
                trade_id: r.get("trade_id"),
                listing_type: "primary".to_string(),
                contract_address: opt::<String>(r, "contract_address").unwrap_or_default(),
                item_id: opt(r, "item_id"),
                name: opt::<String>(r, "name").unwrap_or_default(),
                thumbnail: opt::<String>(r, "image").unwrap_or_default(),
                rarity: opt::<String>(r, "rarity")
                    .as_deref()
                    .unwrap_or("common")
                    .to_lowercase(),
                category: top_level_category(opt::<String>(r, "item_type").as_deref()).to_string(),
                wearable_category: opt(r, "wearable_category"),
                gender: opt(r, "gender"),
                creator: opt::<String>(r, "creator").unwrap_or_default(),
                mana_wei: opt::<String>(r, "mana_wei").unwrap_or_default(),
                available: parse_available(opt::<String>(r, "available").as_deref()),
                network,
                chain_id,
                created_at: r.get("created_at"),
            }
        })
        .collect();
    Ok((data, total))
}

pub async fn importable_listings(
    state: &MarketplaceState,
    seller: &str,
) -> Result<Vec<ImportableListing>, ApiError> {
    let rows = fetch(state, build_importable_listings_sql(seller, since(state))).await?;
    Ok(rows
        .iter()
        .map(|r| {
            let (network, chain_id) =
                network_and_chain(&opt::<String>(r, "network").unwrap_or_default());
            let trade_type: String = r.get("trade_type");
            ImportableListing {
                old_trade_id: r.get("trade_id"),
                listing_type: listing_type(&trade_type).to_string(),
                contract_address: opt::<String>(r, "contract_address").unwrap_or_default(),
                item_id: opt(r, "item_id"),
                token_id: opt(r, "token_id"),
                name: opt::<String>(r, "name").unwrap_or_default(),
                thumbnail: opt::<String>(r, "image").unwrap_or_default(),
                rarity: opt::<String>(r, "rarity")
                    .as_deref()
                    .unwrap_or("common")
                    .to_lowercase(),
                category: top_level_category(opt::<String>(r, "item_type").as_deref()).to_string(),
                wearable_category: opt(r, "wearable_category"),
                mana_wei: opt::<String>(r, "mana_wei").unwrap_or_default(),
                available: parse_available(opt::<String>(r, "available").as_deref()),
                network,
                chain_id,
            }
        })
        .collect())
}

pub async fn unified_listings(
    state: &MarketplaceState,
    f: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
) -> Result<(Vec<UnifiedListing>, i64), ApiError> {
    let rows = fetch(state, build_unified_listings_sql(f, rate, since(state))).await?;
    Ok((
        rows.iter().map(map_unified_listing).collect(),
        total_of(&rows),
    ))
}

pub async fn unified_items(
    state: &MarketplaceState,
    f: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
) -> Result<(Vec<UnifiedItem>, i64), ApiError> {
    let rows = fetch(state, build_unified_items_sql(f, rate, since(state))).await?;
    Ok((rows.iter().map(map_unified_item).collect(), total_of(&rows)))
}

pub async fn items_by_ids(
    state: &MarketplaceState,
    ids: &[String],
    f: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
) -> Result<Vec<UnifiedItem>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = fetch(state, build_items_by_ids_sql(ids, f, rate, since(state))).await?;
    Ok(rows.iter().map(map_unified_item).collect())
}

async fn reference_item(
    state: &MarketplaceState,
    contract_address: &str,
    item_id: &str,
) -> Result<Option<ReferenceItem>, ApiError> {
    let row: Option<(String, String, String)> = sqlx::query_as("SELECT rarity, item_type, category FROM shop.items WHERE contract_address = $1 AND item_id = $2::numeric LIMIT 1")
        .bind(contract_address.to_lowercase())
        .bind(item_id)
        .fetch_optional(&state.pool)
        .await?;
    Ok(row.map(|(rarity, item_type, category)| ReferenceItem {
        category: top_level_category(Some(&item_type)),
        wearable_category: Some(category),
        rarity: Some(rarity),
    }))
}

pub async fn related_items(
    state: &MarketplaceState,
    contract_address: &str,
    item_id: &str,
    first: Option<i64>,
    rail: &RelatedItemsFilters,
    rate: ManaUsdRate,
) -> Result<Vec<UnifiedItem>, ApiError> {
    let Some(reference) = reference_item(state, contract_address, item_id).await? else {
        return Ok(Vec::new());
    };
    let rows = fetch(
        state,
        build_related_items_sql(
            contract_address,
            item_id,
            reference.category,
            reference.wearable_category.as_deref(),
            reference.rarity.as_deref(),
            first,
            rail,
            rate,
            since(state),
        ),
    )
    .await?;
    Ok(rows.iter().map(map_unified_item).collect())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SaleSignal {
    pub sales: i64,
    pub volume: u128,
}

pub async fn trending_items(
    state: &MarketplaceState,
    first: Option<i64>,
    days: Option<i64>,
    filters: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
) -> Result<Vec<TrendingItem>, Response> {
    let first = trending_clamp_first(first);
    let days = trending_clamp_days(days);
    let sales = recent_sales(state, days).await?;
    let signals = aggregate_sales(&sales);
    let ids: Vec<String> = signals.keys().cloned().collect();
    let items = items_by_ids(state, &ids, filters, rate)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(rank_trending(items, &signals, first))
}

/// Creators ranked by windowed revenue, attributing each sale to the item's
/// creator (the reader's `item.creator`) so primary mints count; floors keep
/// a thin window out of a revenue ranking and a one-hit month with nothing to
/// browse off the rail.
pub async fn top_creators(
    state: &MarketplaceState,
    first: Option<i64>,
    days: Option<i64>,
) -> Result<Vec<TopCreator>, Response> {
    let first = top_creators_clamp_first(first);
    let days = top_creators_clamp_days(days);
    let min_sales = top_creators_min_sales(days);
    let key = format!("creators:{first}:{days}");
    let value = cached(state, Ttl::M15, key, || async {
        let sales = recent_sales(state, days).await.map_err(|_| "sales reader unavailable".to_string())?;
        let mut ranked: HashMap<String, (u128, i64)> = HashMap::new();
        for sale in &sales {
            let Some(creator) = readers::at(sale, "item/creator").and_then(Value::as_str) else { continue };
            let entry = ranked.entry(creator.to_lowercase()).or_default();
            entry.0 = entry.0.saturating_add(readers::str_of(sale, "price").parse::<u128>().unwrap_or(0));
            entry.1 += 1;
        }
        let creators: Vec<String> = ranked.iter().filter(|(_, (_, sales))| *sales >= min_sales).map(|(c, _)| c.clone()).collect();
        let catalogue: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT creator, COUNT(*)::int8 AS items, COUNT(DISTINCT contract_address)::int8 AS collections \
             FROM shop.items WHERE search_is_collection_approved = true AND creator = ANY($1) GROUP BY creator",
        )
        .bind(&creators)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| e.to_string())?;
        let catalogue: HashMap<String, (i64, i64)> = catalogue.into_iter().map(|(c, items, collections)| (c, (items, collections))).collect();
        let mut rows: Vec<TopCreator> = creators
            .into_iter()
            .filter_map(|creator| {
                let (volume, sales) = ranked.get(&creator).copied()?;
                let (items, collections) = catalogue.get(&creator).copied().unwrap_or((0, 0));
                (items >= TOP_CREATORS_MIN_ITEMS).then(|| TopCreator { id: creator, volume_wei: volume.to_string(), sales, total_sales: sales, collections, items })
            })
            .collect();
        rows.sort_by(|a, b| b.volume_wei.parse::<u128>().unwrap_or(0).cmp(&a.volume_wei.parse::<u128>().unwrap_or(0)).then(a.id.cmp(&b.id)));
        rows.truncate(first as usize);
        serde_json::to_value(rows).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| readers::upstream_failed("/v3/catalog/creators", &e))?;
    serde_json::from_value((*value).clone()).map_err(|_| super::internal_server_error())
}

/// Every feed joins the items mirror to the open trades: both need a pass,
/// the items one within its money budget (the minter price).
async fn ready(state: &MarketplaceState) -> Result<(), Response> {
    super::mirrors::require_priced(state).await?;
    super::trades::require_ready(state).await
}

fn hour_cache() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        CACHE_CONTROL,
        "public,max-age=3600,s-maxage=3600".parse().unwrap(),
    );
    headers
}

pub async fn get_shop_catalog(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    match shop_listings(&state, &parse_shop_filters(&pairs)).await {
        Ok((data, total)) => Json(DataTotal { data, total }).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn get_legacy_catalog(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    match legacy_listings(&state, &parse_legacy_filters(&pairs)).await {
        Ok((data, total)) => Json(DataTotal { data, total }).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn get_unified_catalog(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    let filters = parse_unified_filters(&pairs);
    let rate = match mana_usd_rate(&state).await {
        Ok(rate) => rate,
        Err(envelope) => return envelope,
    };
    let result = match parse_unified_group_by(&pairs) {
        UnifiedGroupBy::Item => unified_items(&state, &filters, rate)
            .await
            .map(|(data, total)| Json(DataTotal { data, total }).into_response()),
        UnifiedGroupBy::Listing => unified_listings(&state, &filters, rate)
            .await
            .map(|(data, total)| Json(DataTotal { data, total }).into_response()),
    };
    result.unwrap_or_else(IntoResponse::into_response)
}

pub async fn get_importable_listings(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    let seller = Params::new(&pairs).get_address("seller", true, None);
    let data = match seller {
        Some(seller) => match importable_listings(&state, &seller).await {
            Ok(data) => data,
            Err(e) => return e.into_response(),
        },
        None => Vec::new(),
    };
    Json(ImportableResponseBody { data }).into_response()
}

/// Digits only, not a u64 parse: a huge id must still be accepted.
fn is_numeric_item_id(item_id: &str) -> bool {
    !item_id.is_empty() && item_id.bytes().all(|b| b.is_ascii_digit())
}

/// A malformed deep link yields an empty rail without touching the database.
pub async fn get_related_catalog(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    let p = Params::new(&pairs);
    let contract_address = p.get_address("contractAddress", true, None);
    let item_id = p.get_string("itemId", None);
    let first = finite_i64(p.get_number("first", None));
    let data = match (contract_address, item_id) {
        (Some(contract_address), Some(item_id)) if is_numeric_item_id(&item_id) => {
            let rate = match mana_usd_rate(&state).await {
                Ok(rate) => rate,
                Err(envelope) => return envelope,
            };
            match related_items(
                &state,
                &contract_address,
                &item_id,
                first,
                &parse_related_filters(&pairs),
                rate,
            )
            .await
            {
                Ok(data) => data,
                Err(e) => return e.into_response(),
            }
        }
        _ => Vec::new(),
    };
    Json(RelatedResponseBody { data }).into_response()
}

pub async fn get_trending_catalog(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    let req = parse_trending_filters(&pairs);
    let rate = match mana_usd_rate(&state).await {
        Ok(rate) => rate,
        Err(envelope) => return envelope,
    };
    match trending_items(&state, req.first, req.days, &req.filters, rate).await {
        Ok(data) => (hour_cache(), Json(TrendingResponseBody { data })).into_response(),
        Err(r) => r,
    }
}

/// The request lists the shop sends (`seeds`, `exclude`), comma-separated
/// or repeated, bounded (catalyrst-market handlers/suggestions.rs).
fn id_list(p: &Params, key: &str, cap: usize) -> Vec<String> {
    p.get_list(key, &[])
        .into_iter()
        .flat_map(|v| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .take(cap)
        .collect()
}

pub async fn get_suggested_catalog(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    let verified =
        match super::signed::optional_signer(&headers, "get", "/v3/catalog/suggested").await {
            Ok(v) => v.map(|a| a.to_lowercase()),
            Err(e) => return e.into_response(),
        };
    let p = Params::new(&pairs);
    let first = finite_i64(p.get_number("first", None));
    let limit = first.unwrap_or(12).clamp(1, 40) as usize;
    let filters = UnifiedCatalogFilters {
        base: ShopCatalogFilters {
            category: p.get_string("category", None),
            include_social_emotes: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let rate = match mana_usd_rate(&state).await {
        Ok(rate) => rate,
        Err(envelope) => return envelope,
    };
    let mut data: Vec<SuggestedItem> = Vec::new();
    if super::neighbours::built(&state.pool).await {
        match personal_rows(&state, &p, verified.as_deref(), &filters, rate, limit).await {
            Ok(rows) if rows.len() >= MIN_PERSONAL_ROWS => data = rows,
            Ok(_) => {}
            Err(e) => return e.into_response(),
        }
    }
    let personalized = !data.is_empty();
    if data.len() < limit {
        match trending_items(&state, Some(limit as i64 * 2), None, &filters, rate).await {
            Ok(items) => {
                for t in items {
                    if data.len() >= limit {
                        break;
                    }
                    let id = format!(
                        "{}-{}",
                        t.item.contract_address,
                        t.item.item_id.clone().unwrap_or_default()
                    );
                    if data.iter().any(|d| {
                        format!(
                            "{}-{}",
                            d.item.contract_address,
                            d.item.item_id.clone().unwrap_or_default()
                        ) == id
                    }) {
                        continue;
                    }
                    data.push(SuggestedItem {
                        item: t.item,
                        reason: SuggestionReason {
                            kind: "trending",
                            item_id: None,
                            creator: None,
                        },
                        score: 0.0,
                    });
                }
            }
            Err(r) => return r,
        }
    }
    (
        [(CACHE_CONTROL, "private, no-store")],
        Json(SuggestionsResponse {
            data,
            personalized,
            algorithm: SUGGESTIONS_ALGORITHM_VERSION.to_string(),
        }),
    )
        .into_response()
}

pub async fn get_top_creators(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(r) = ready(&state).await {
        return r;
    }
    let p = Params::new(&pairs);
    match top_creators(
        &state,
        finite_i64(p.get_number("first", None)),
        finite_i64(p.get_number("days", None)),
    )
    .await
    {
        Ok(data) => (hour_cache(), Json(TopCreatorsResponseBody { data })).into_response(),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn numeric_item_id_guard_keeps_non_digit_ids_away_from_the_numeric_cast() {
        for junk in [
            "abc",
            "1e3",
            "-1",
            "1.5",
            "3; DROP TABLE item",
            " ",
            "0x03",
            "",
        ] {
            assert!(!is_numeric_item_id(junk), "{junk:?}");
        }
        assert!(is_numeric_item_id("3") && is_numeric_item_id("90071992547409910000"));
    }

    #[test]
    fn the_unified_parameters_read_both_encodings() {
        let f = parse_unified_filters(&pairs(&[
            ("wearableGender", "male,female"),
            (
                "contractAddress",
                "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA,junk",
            ),
            ("includeLegacySecondary", "true"),
            ("source", "legacy"),
            ("discounted", "false"),
        ]));
        assert_eq!(f.base.wearable_genders, vec!["male", "female"]);
        assert_eq!(
            f.contract_addresses,
            Some(vec![
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()
            ])
        );
        assert!(f.base.contract_address.is_none() && f.include_legacy_secondary);
        assert_eq!(
            (f.source, f.base.discounted),
            (Some(UnifiedSource::Legacy), Some(false))
        );
        assert_eq!(
            parse_unified_group_by(&pairs(&[("groupBy", "item")])),
            UnifiedGroupBy::Item
        );
        assert_eq!(
            parse_unified_group_by(&pairs(&[("groupBy", "nope")])),
            UnifiedGroupBy::Listing
        );
        let blank = parse_unified_filters(&pairs(&[("contractAddress", " ")]));
        assert!(blank.contract_addresses.is_none());
        let junk = parse_unified_filters(&pairs(&[("contractAddress", "junk")]));
        assert_eq!(junk.contract_addresses, Some(vec![]));
        assert_eq!(
            body_shapes_for_genders(&["unisex".to_string()]),
            Some(vec!["BaseMale".to_string(), "BaseFemale".to_string()])
        );
        assert!(
            !parse_shop_filters(&pairs(&[("includeSocialEmotes", "false")])).include_social_emotes
        );
        assert_eq!(
            parse_shop_filters(&pairs(&[("listingType", "typo")])).listing_type,
            None
        );
    }

    fn candidate(
        id: &str,
        collection: &str,
        creator: &str,
        sub: &str,
        wearable: bool,
        trigger: &str,
        score: f64,
    ) -> RailCandidate {
        RailCandidate {
            id: id.into(),
            collection: collection.into(),
            creator: creator.into(),
            sub_category: sub.into(),
            is_wearable: wearable,
            trigger: Some(trigger.into()),
            score,
        }
    }

    #[test]
    fn the_rail_is_re_ranked_for_variety_and_relaxes_in_tiers() {
        // Eight items from one drop by one creator outscore everything else.
        let sub = |i: usize| {
            if i.is_multiple_of(2) {
                "wearable:hat"
            } else {
                "wearable:shoes"
            }
        };
        let mut pool: Vec<RailCandidate> = (0..8)
            .map(|i| {
                candidate(
                    &format!("a-{i}"),
                    "a",
                    "alice",
                    sub(i),
                    true,
                    &format!("t{i}"),
                    10.0 - i as f64,
                )
            })
            .collect();
        pool.push(candidate(
            "b-0",
            "b",
            "bob",
            "wearable:mask",
            true,
            "t8",
            1.0,
        ));
        pool.push(candidate(
            "e-0",
            "e",
            "erin",
            "wearable:top",
            true,
            "t9",
            0.95,
        ));
        pool.push(candidate(
            "c-0",
            "c",
            "carol",
            "emote:dance",
            false,
            "t10",
            0.9,
        ));
        let rail = rerank_for_diversity(pool.clone(), 4, 0.7);
        let ids: Vec<&str> = rail.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a-0", "a-1", "b-0", "c-0"],
            "two per collection, then the emote share over a fourth wearable: {ids:?}"
        );
        let same_sub = vec![
            candidate("h-0", "h", "h", "wearable:hat", true, "u0", 3.0),
            candidate("i-0", "i", "i", "wearable:hat", true, "u1", 2.0),
            candidate("j-0", "j", "j", "wearable:shoes", true, "u2", 1.0),
        ];
        let ids: Vec<String> = rerank_for_diversity(same_sub, 3, 1.0)
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(
            ids,
            vec!["h-0", "j-0", "i-0"],
            "no sub-category twice in a row while supply allows"
        );
        // Short supply relaxes rather than returning a short rail.
        let rail = rerank_for_diversity(pool[..8].to_vec(), 5, 0.7);
        assert_eq!(
            rail.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a-0", "a-1", "a-2", "a-3", "a-4"],
            "the last tier is score order"
        );
    }

    #[test]
    fn one_profile_item_explains_at_most_two_rows() {
        let pool: Vec<RailCandidate> = (0..6)
            .map(|i| {
                candidate(
                    &format!("x{i}-0"),
                    &format!("x{i}"),
                    &format!("creator{i}"),
                    &format!("wearable:c{i}"),
                    true,
                    if i < 4 { "ruler" } else { "other" },
                    10.0 - i as f64,
                )
            })
            .collect();
        let rail = rerank_for_diversity(pool, 4, 1.0);
        let triggers: Vec<&str> = rail.iter().map(|c| c.trigger.as_deref().unwrap()).collect();
        assert_eq!(
            triggers.iter().filter(|t| **t == "ruler").count(),
            2,
            "{triggers:?}"
        );
        assert_eq!(
            rail.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["x0-0", "x1-0", "x4-0", "x5-0"]
        );
    }

    #[test]
    fn the_profile_keeps_each_items_strongest_signal_and_reads_both_spellings() {
        let c = "0xcccccccccccccccccccccccccccccccccccccccc";
        assert_eq!(
            urn_to_item_id(&format!(
                "urn:decentraland:matic:collections-v2:{}:7",
                c.to_uppercase().replace("0X", "0x")
            )),
            Some(format!("{c}-7"))
        );
        assert_eq!(
            urn_to_item_id(&format!("urn:decentraland:amoy:collections-v2:{c}:0")),
            Some(format!("{c}-0"))
        );
        assert_eq!(
            urn_to_item_id("urn:decentraland:off-chain:base-avatars:eyebrows_00"),
            None
        );
        assert_eq!(
            urn_to_item_id(&format!("urn:decentraland:ethereum:collections-v1:{c}:7")),
            None
        );
        let pairs = pairs(&[(
            "equipped",
            &format!("urn:decentraland:matic:collections-v2:{c}:1,{c}-1,{c}-2,junk,{c}-x"),
        )]);
        let p = Params::new(&pairs);
        assert_eq!(
            item_id_values(&p, "equipped", MAX_EQUIPPED),
            vec![format!("{c}-1"), format!("{c}-2")],
            "de-duplicated across spellings"
        );
        assert_eq!(item_id_values(&p, "equipped", 1).len(), 1);
        let profile = build_profile(
            &["a-1".into(), "a-2".into()],
            &["a-2".into()],
            &["a-1".into()],
            &["a-2".into(), "a-3".into()],
        );
        assert_eq!(
            profile,
            vec![
                ("a-1".to_string(), SeedSource::Equipped),
                ("a-2".to_string(), SeedSource::Favorite),
                ("a-3".to_string(), SeedSource::Seed)
            ]
        );
        assert_eq!(
            [
                SeedSource::Owned,
                SeedSource::Favorite,
                SeedSource::Equipped,
                SeedSource::Seed
            ]
            .map(SeedSource::reason),
            [
                "co_owned",
                "favorite_similar",
                "equipped_similar",
                "seed_similar"
            ]
        );
    }

    #[test]
    fn a_coupon_pairs_with_the_marketplace_its_manager_is_wired_into() {
        let pairings = coupon_pairings();
        let mainnet_v3 = (
            "0xe38ef22abe871513555cba89adfe45ab4f548ada".to_string(),
            "0x655fdfa91d69ea49f4ce1a8f7f7e2622c8630813".to_string(),
        );
        let mainnet_v2 = (
            "0xa40b1d129b8906888720686f3a01921ddf37716f".to_string(),
            "0x3fd3056ee72a2a85e9392fab3a450e7736536081".to_string(),
        );
        assert!(
            pairings.contains(&(137, mainnet_v3.0.clone(), mainnet_v3.1.clone())),
            "{pairings:?}"
        );
        assert!(
            pairings.contains(&(137, mainnet_v2.0.clone(), mainnet_v2.1.clone())),
            "{pairings:?}"
        );
        assert!(
            !pairings.contains(&(137, mainnet_v3.0, mainnet_v2.1)),
            "a manager redeems only through its own marketplace"
        );
        assert!(
            pairings
                .iter()
                .all(|(chain, _, _)| [137, 1].contains(chain)),
            "only the chains this deployment serves: {pairings:?}"
        );
        let mut b = Builder::new();
        let legacy = unified_branch(
            &mut b,
            UnifiedSource::Legacy,
            UnifiedAcquisition::Store,
            Some("$1"),
            &UnifiedCatalogFilters::default(),
        );
        assert!(
            !legacy.contains("shop.coupons") && legacy.contains("NULL::jsonb AS coupon"),
            "{legacy}"
        );
    }

    #[test]
    fn individual_items_are_parsed_and_unioned_with_the_collections() {
        let a = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let c = "0xcccccccccccccccccccccccccccccccccccccccc";
        let f = parse_unified_filters(&pairs(&[
            (
                "items",
                "0xCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC-007,junk,0xcc-1",
            ),
            ("items", &format!("{c}-0, {c}-x")),
        ]));
        assert_eq!(
            f.item_ids,
            Some(vec![format!("{c}-7"), format!("{c}-0")]),
            "zeros stripped, malformed dropped"
        );
        assert!(f.contract_addresses.is_none());
        assert!(
            parse_unified_filters(&pairs(&[("items", " , ")]))
                .item_ids
                .is_none(),
            "blank reads as absent"
        );
        assert_eq!(
            parse_unified_filters(&pairs(&[("items", "junk")])).item_ids,
            Some(vec![])
        );

        let where_of = |f: &UnifiedCatalogFilters| {
            let mut b = Builder::new();
            unified_selection(&mut b, f)
        };
        let both = UnifiedCatalogFilters {
            contract_addresses: Some(vec![a.into()]),
            item_ids: Some(vec![format!("{c}-7")]),
            ..Default::default()
        };
        let clause = where_of(&both).unwrap();
        assert!(
            clause.contains(" OR ")
                && clause.contains("mv.contract_address || '-' || mv.item_id::text = ANY("),
            "a union: {clause}"
        );
        let only_items = UnifiedCatalogFilters {
            contract_addresses: Some(vec![]),
            item_ids: Some(vec![format!("{c}-7")]),
            ..Default::default()
        };
        let clause = where_of(&only_items).unwrap();
        assert!(
            !clause.contains("FALSE") && !clause.contains(" OR "),
            "an empty collection set keeps the named items: {clause}"
        );
        let none = UnifiedCatalogFilters {
            contract_addresses: Some(vec![]),
            item_ids: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(
            where_of(&none).as_deref(),
            Some("FALSE"),
            "both empty fails closed"
        );
        assert_eq!(where_of(&UnifiedCatalogFilters::default()), None);
    }

    #[test]
    fn clamps_and_floors_follow_the_old_bounds() {
        assert_eq!(
            (
                shop_clamp_first(None),
                shop_clamp_first(Some(0)),
                shop_clamp_first(Some(5000))
            ),
            (48, 1, 1000)
        );
        assert_eq!(
            (
                top_creators_min_sales(30),
                top_creators_min_sales(7),
                top_creators_min_sales(1),
                top_creators_min_sales(365)
            ),
            (5, 2, 2, 61)
        );
        assert_eq!(
            (
                trending_clamp_days(None),
                trending_clamp_days(Some(30)),
                trending_clamp_first(Some(500))
            ),
            (7, 7, 50)
        );
        assert_eq!(credits_to_wei(2.7), Some(2 * USD_WEI_PER_CREDIT));
        assert_eq!(unified_min_price_bound_wei(0.0), None);
        assert_eq!(
            unified_min_price_bound_wei(3.0),
            Some(2 * USD_WEI_PER_CREDIT)
        );
        assert_eq!(
            related_rarity_sort_key(Some("epic"))
                .unwrap()
                .matches("WHEN")
                .count(),
            8
        );
        assert!(related_rarity_sort_key(Some("weird")).is_none());
    }

    #[test]
    fn the_feeds_read_the_mirror_tables_and_bind_everything() {
        let shop = build_shop_listings_sql(
            &parse_shop_filters(&pairs(&[
                ("search", "hat"),
                ("rarity", "Epic"),
                ("minPriceCredits", "3"),
                ("sortBy", "cheapest"),
                ("discounted", "true"),
            ])),
            None,
        );
        assert!(
            shop.sql.contains("shop.items i")
                && shop.sql.contains("unified_trades u")
                && shop.sql.contains("u.received_asset_type = 2"),
            "{}",
            shop.sql
        );
        assert!(
            shop.sql.contains("cp.id IS NOT NULL")
                && shop.sql.contains("ORDER BY sub.usd_wei ASC, sub.trade_id"),
            "{}",
            shop.sql
        );
        assert!(
            shop.sql.contains("LEFT JOIN LATERAL") && shop.sql.contains("FROM shop.coupons c"),
            "the native branch joins the coupons: {}",
            shop.sql
        );
        assert!(
            !shop.sql.contains("hat") && !shop.sql.contains("epic"),
            "values are bound: {}",
            shop.sql
        );
        let legacy = build_legacy_listings_sql(
            &parse_legacy_filters(&pairs(&[("category", "emote"), ("sortBy", "discount")])),
            None,
        );
        assert!(
            legacy.sql.contains("u.received_asset_type = 1")
                && legacy.sql.contains("u.type = 'public_item_order'")
                && legacy.sql.contains("ILIKE 'emote%'")
                && legacy.sql.contains("ORDER BY sub.created_at DESC"),
            "{}",
            legacy.sql
        );
        let unified = build_unified_items_sql(
            &parse_unified_filters(&pairs(&[("groupBy", "item")])),
            ManaUsdRate::parse_usd_per_mana("0.5").unwrap(),
            None,
        );
        assert_eq!(
            unified.sql.matches("UNION ALL").count(),
            2,
            "{}",
            unified.sql
        );
        assert!(
            unified.sql.contains("search_is_store_minter = true")
                && unified
                    .sql
                    .contains("DISTINCT ON (f.contract_address, f.item_id)")
                && unified.sql.contains("listing_count"),
            "{}",
            unified.sql
        );
        let native = build_unified_listings_sql(
            &parse_unified_filters(&pairs(&[("source", "native")])),
            ManaUsdRate::parse_usd_per_mana("0.5").unwrap(),
            None,
        );
        assert!(
            !native.sql.contains("UNION ALL") && !native.sql.contains("store"),
            "{}",
            native.sql
        );
        let importable = build_importable_listings_sql("0xABC", None);
        assert!(
            importable.sql.contains("lower(sub.signer) =")
                && !importable.sql.contains(" AND u.type = 'public_item_order'"),
            "{}",
            importable.sql
        );
        let related = build_related_items_sql(
            "0xabc",
            "7",
            "wearable",
            Some("hat"),
            Some("rare"),
            None,
            &RelatedItemsFilters::default(),
            ManaUsdRate::parse_usd_per_mana("0.5").unwrap(),
            None,
        );
        assert!(
            related.sql.contains("CASE lower(d.rarity)")
                && related.sql.contains("NOT ILIKE 'emote%'"),
            "{}",
            related.sql
        );
    }

    #[test]
    fn trending_takes_the_sales_cut_then_volume() {
        let mk = |c: &str, i: &str| UnifiedItem {
            source: "native".into(),
            acquisition: "trade".into(),
            trade_id: Some("t".into()),
            listing_type: "primary".into(),
            contract_address: c.into(),
            item_id: Some(i.into()),
            token_id: None,
            name: String::new(),
            thumbnail: String::new(),
            rarity: "common".into(),
            category: "wearable".into(),
            wearable_category: None,
            emote_loop: None,
            gender: None,
            creator: String::new(),
            seller: None,
            issued_id: None,
            price_credits: Some(1),
            sale: ShopSaleFields::default(),
            mana_wei: None,
            listing_count: 1,
            available: 1,
            network: Network::Matic,
            chain_id: ChainId::MaticMainnet,
            created_at: 0,
        };
        let sales = |c: &str, i: &str, n: usize, price: &str| {
            (0..n)
                .map(|_| json!({"searchContractAddress": c, "searchItemId": i, "price": price}))
                .collect::<Vec<_>>()
        };
        let mut all = sales("0xa", "1", 5, "1");
        all.extend(sales("0xb", "2", 3, "1"));
        all.extend(sales("0xc", "3", 1, "1000"));
        all.extend(sales("0xd", "4", 2, "1"));
        let signals = aggregate_sales(&all);
        assert_eq!(
            signals["0xc-3"],
            SaleSignal {
                sales: 1,
                volume: 1000
            }
        );
        let ranked = rank_trending(
            vec![
                mk("0xd", "4"),
                mk("0xc", "3"),
                mk("0xb", "2"),
                mk("0xa", "1"),
            ],
            &signals,
            3,
        );
        let ids: Vec<(String, i64)> = ranked
            .iter()
            .map(|t| (t.item.contract_address.clone(), t.trending_sales))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("0xa".to_string(), 5),
                ("0xb".to_string(), 3),
                ("0xc".to_string(), 1)
            ],
            "two by sales, one by volume"
        );
        let body = serde_json::to_value(&ranked[0]).unwrap();
        assert!(
            body.get("trendingSales").is_some()
                && body.get("compareAtCredits").is_some()
                && body["emoteLoop"].is_null(),
            "flattened: {body}"
        );
    }
}
