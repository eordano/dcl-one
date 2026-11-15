//! `GET /v1/items`, `GET /v3/catalog/items` and `GET /v1/trendings` over the
//! `items` mirror. The item row is `shop.items` (the old
//! `squid_marketplace.item` + metadata joins folded); the one open v3 item
//! order an item carries (`tradeId`, `tradeExpiresAt`, the trade price) is
//! the newest open `public_item_order` of `shop.trades` for that
//! item, as the old `LATERAL ... ORDER BY id::text DESC LIMIT 1` picked it.
//! `/v3/catalog/items` prices in credits with the MANA/USD spot the economy
//! poller archives (`economy.price_snapshots`); without a spot younger than
//! `price::store::SNAPSHOT_MAX_AGE` it answers the price envelope, never a
//! price from a constant.
//! `/v1/trendings` ranks the last week's sales read from the collections
//! subgraphs (a reader, 10 min, at most 1,000 sales per network) and
//! hydrates the winners through the items query.

use crate::types::ManaUsdRate;
use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;

use super::catalog::{
    Builder, FRAGMENT_SMART_WEARABLE_V1, FRAGMENT_WEARABLE_V1, FRAGMENT_WEARABLE_V2,
    WEARABLE_ITEM_TYPES, escape_like, expand_urn_network_forms, unified_trades_cte, v3_minter_expr,
};
use super::mirrors::require_priced;
use super::readers::{self, Ttl, cached, int_of, str_of};
use super::shop::{MAX_USD_WEI, SHOP_SORT_VALUES, ShopSortBy};
use super::wire::{
    ApiError, ChainId, DataTotal, NETWORK_VALUES, NFT_CATEGORY_VALUES, Network, NftCategory,
    Params, body_shapes_for_genders, clamp_first, clamp_skip, emote_play_mode_clause, fix_urn,
};
use super::{MarketplaceState, SharedState};

pub const DEFAULT_LIMIT: i64 = 100;
pub const USD_WEI_PER_CREDIT: u128 = 100_000_000_000_000_000;
const USD_WEI_PER_CREDIT_SQL: &str = "100000000000000000::numeric";
const ASSET_TYPE_USD_PEGGED_MANA: i32 = 2;

pub fn item_types_for(category: NftCategory) -> Vec<&'static str> {
    match category {
        NftCategory::Wearable => WEARABLE_ITEM_TYPES.to_vec(),
        NftCategory::Emote => vec!["emote_v1"],
        _ => vec![],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemSortBy {
    Newest,
    RecentlyReviewed,
    RecentlySold,
    Name,
    Cheapest,
    RecentlyListed,
    /// The default of a search, meaningless without one (every row ties).
    Relevance,
}

impl ItemSortBy {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "newest" => Self::Newest,
            "recently_reviewed" => Self::RecentlyReviewed,
            "recently_sold" => Self::RecentlySold,
            "name" => Self::Name,
            "cheapest" => Self::Cheapest,
            "recently_listed" => Self::RecentlyListed,
            "relevance" => Self::Relevance,
            _ => return None,
        })
    }
}

/// The normalized words of a string, as the relevance tiers compare them:
/// lowercased, split on anything that is not a letter or digit.
fn search_words(value: &str) -> Vec<String> {
    value
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The relevance rank of the row whose name is `name_expr` for `search`,
/// lower first: 0 a name that IS the query (its words in any order), 1 a
/// name that starts with it as whole words, 2 every query word a whole word
/// of the name, 3 every one inside it, 4 the rest of the search's matches
/// (through the collection's name). "Hat", then "Hat Pirate", "Pirate Hat",
/// "Hatch". `None` for a search with no words.
pub fn search_relevance_rank(b: &mut Builder, name_expr: &str, search: &str) -> Option<String> {
    let words = search_words(search);
    if words.is_empty() {
        return None;
    }
    let phrase = format!("trim(regexp_replace(lower({name_expr}), '[^[:alnum:]]+', ' ', 'g'))");
    let mut sorted = words.clone();
    sorted.sort();
    let sorted_p = b.bind_string(sorted.join(" "));
    let prefix_p = b.bind_string(format!("{} %", escape_like(&words.join(" "))));
    let mut whole = Vec::new();
    let mut inside = Vec::new();
    for word in &words {
        let whole_p = b.bind_string(format!("% {} %", escape_like(word)));
        whole.push(format!("(' ' || {phrase} || ' ') LIKE {whole_p}"));
        let inside_p = b.bind_string(format!("%{}%", escape_like(word)));
        inside.push(format!("{phrase} LIKE {inside_p}"));
    }
    Some(format!(
        "CASE WHEN (SELECT string_agg(w, ' ' ORDER BY w) FROM regexp_split_to_table(lower({name_expr}), '[^[:alnum:]]+') AS w WHERE w <> '') = {sorted_p} THEN 0 \
              WHEN {phrase} LIKE {prefix_p} THEN 1 WHEN {whole} THEN 2 WHEN {inside} THEN 3 ELSE 4 END",
        whole = whole.join(" AND "),
        inside = inside.join(" AND "),
    ))
}

/// Whether a feed ranks by relevance: an explicit `relevance`, or no sort
/// on a search; `relevance` without a search falls back to the default.
pub fn ranks_by_relevance(explicit: Option<bool>, search: Option<&str>) -> bool {
    let searching = search.is_some_and(|s| !s.trim().is_empty());
    searching && explicit.unwrap_or(true)
}

#[derive(Debug, Clone)]
pub struct ItemFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub category: Option<NftCategory>,
    pub creator: Vec<String>,
    pub is_sold_out: bool,
    pub is_on_sale: Option<bool>,
    pub search: Option<String>,
    pub is_wearable_head: bool,
    pub is_wearable_accessory: bool,
    pub is_wearable_smart: bool,
    pub wearable_category: Option<String>,
    pub rarities: Vec<String>,
    pub wearable_genders: Vec<String>,
    pub emote_category: Option<String>,
    pub emote_genders: Vec<String>,
    pub emote_play_mode: Vec<String>,
    pub emote_has_geometry: bool,
    pub emote_has_sound: bool,
    pub emote_outcome_type: Option<String>,
    pub contract_addresses: Vec<String>,
    pub item_id: Option<String>,
    pub network: Option<Network>,
    pub max_price: Option<String>,
    pub min_price: Option<String>,
    pub urns: Vec<String>,
    pub ids: Vec<String>,
    pub sort_by: Option<ItemSortBy>,
    pub include_social_emotes: bool,
}

impl Default for ItemFilters {
    fn default() -> Self {
        Self {
            first: None,
            skip: None,
            category: None,
            creator: Vec::new(),
            is_sold_out: false,
            is_on_sale: None,
            search: None,
            is_wearable_head: false,
            is_wearable_accessory: false,
            is_wearable_smart: false,
            wearable_category: None,
            rarities: Vec::new(),
            wearable_genders: Vec::new(),
            emote_category: None,
            emote_genders: Vec::new(),
            emote_play_mode: Vec::new(),
            emote_has_geometry: false,
            emote_has_sound: false,
            emote_outcome_type: None,
            contract_addresses: Vec::new(),
            item_id: None,
            network: None,
            max_price: None,
            min_price: None,
            urns: Vec::new(),
            ids: Vec::new(),
            sort_by: None,
            include_social_emotes: true,
        }
    }
}

/// ethers `parseEther`: a decimal MANA amount as wei, at most 18 fraction digits.
pub fn parse_ether(value: &str) -> Option<String> {
    const DECIMALS: usize = 18;
    let (neg, body) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let mut parts = body.splitn(2, '.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next().unwrap_or("");
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || frac_part.len() > DECIMALS
    {
        return None;
    }
    let mut digits = String::with_capacity(int_part.len() + DECIMALS);
    digits.push_str(int_part);
    digits.push_str(frac_part);
    for _ in 0..(DECIMALS - frac_part.len()) {
        digits.push('0');
    }
    let trimmed = digits.trim_start_matches('0');
    let out = if trimmed.is_empty() { "0" } else { trimmed };
    if neg && out != "0" {
        Some(format!("-{out}"))
    } else {
        Some(out.to_string())
    }
}

fn parse_price_param(p: &Params, key: &str) -> Result<Option<String>, ApiError> {
    match p.get_string(key, None) {
        Some(raw) if !raw.trim().is_empty() => parse_ether(&raw)
            .map(Some)
            .ok_or_else(|| ApiError::bad_request(format!("Invalid {key} value: {raw}"))),
        _ => Ok(None),
    }
}

fn parse_optional_bool(p: &Params, key: &str) -> Option<bool> {
    if p.get_boolean(key) {
        p.get_string(key, None).map(|s| s == "true")
    } else {
        None
    }
}

pub fn parse_filters(pairs: &[(String, String)]) -> Result<ItemFilters, ApiError> {
    let p = Params::new(pairs);
    Ok(ItemFilters {
        first: p.get_number("first", None).map(|f| f as i64),
        skip: p.get_number("skip", None).map(|f| f as i64),
        category: p
            .get_value("category", NFT_CATEGORY_VALUES, None)
            .and_then(|s| NftCategory::parse(&s)),
        creator: p.get_list("creator", &[]),
        is_sold_out: p.get_boolean("isSoldOut"),
        is_on_sale: parse_optional_bool(&p, "isOnSale"),
        search: p.get_string("search", None),
        is_wearable_head: p.get_boolean("isWearableHead"),
        is_wearable_accessory: p.get_boolean("isWearableAccessory"),
        is_wearable_smart: p.get_boolean("isWearableSmart"),
        wearable_category: p.get_string("wearableCategory", None),
        rarities: p.get_list("rarity", &[]),
        wearable_genders: p.get_list("wearableGender", &[]),
        emote_category: p.get_string("emoteCategory", None),
        emote_genders: p.get_list("emoteGender", &[]),
        emote_play_mode: p.get_list("emotePlayMode", &[]),
        emote_has_geometry: p.get_boolean("emoteHasGeometry"),
        emote_has_sound: p.get_boolean("emoteHasSound"),
        emote_outcome_type: p.get_string("emoteOutcomeType", None),
        contract_addresses: p.get_address_list("contractAddress", false),
        item_id: p.get_string("itemId", None),
        network: p
            .get_value("network", NETWORK_VALUES, None)
            .and_then(|s| Network::parse(&s)),
        max_price: parse_price_param(&p, "maxPrice")?,
        min_price: parse_price_param(&p, "minPrice")?,
        urns: p.get_list("urn", &[]),
        ids: p.get_list("id", &[]),
        sort_by: p
            .get_string("sortBy", None)
            .and_then(|s| ItemSortBy::parse(&s)),
        include_social_emotes: p.get_string("includeSocialEmotes", None).as_deref()
            != Some("false"),
    })
}

/// `/v3/catalog/items` only: the shop's sort and the credit range.
#[derive(Debug, Clone, Default)]
pub struct CatalogItemsParams {
    pub sort_by: Option<ShopSortBy>,
    pub min_price_credits: Option<f64>,
    pub max_price_credits: Option<f64>,
}

pub fn parse_catalog_items_params(pairs: &[(String, String)]) -> CatalogItemsParams {
    let p = Params::new(pairs);
    CatalogItemsParams {
        sort_by: p
            .get_value("sortBy", SHOP_SORT_VALUES, None)
            .as_deref()
            .and_then(ShopSortBy::parse),
        min_price_credits: p
            .get_number("minPriceCredits", None)
            .filter(|n| n.is_finite()),
        max_price_credits: p
            .get_number("maxPriceCredits", None)
            .filter(|n| n.is_finite()),
    }
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct WearableData {
    #[serde(rename = "bodyShapes")]
    pub body_shapes: Vec<String>,
    pub category: String,
    pub description: String,
    pub rarity: String,
    #[serde(rename = "isSmart")]
    pub is_smart: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct EmoteData {
    #[serde(rename = "bodyShapes")]
    pub body_shapes: Vec<String>,
    pub category: String,
    pub description: String,
    pub rarity: String,
    #[serde(rename = "loop")]
    pub r#loop: bool,
    #[serde(rename = "hasSound")]
    pub has_sound: bool,
    #[serde(rename = "hasGeometry")]
    pub has_geometry: bool,
    #[serde(rename = "outcomeType")]
    pub outcome_type: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum ItemData {
    Wearable { wearable: WearableData },
    Emote { emote: EmoteData },
}

#[derive(Debug, Serialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct PicksCount {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub count: i64,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub thumbnail: String,
    pub url: String,
    pub category: NftCategory,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    #[serde(rename = "itemId")]
    pub item_id: String,
    pub rarity: String,
    pub price: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available: i64,
    #[serde(rename = "isOnSale")]
    pub is_on_sale: bool,
    pub creator: String,
    pub beneficiary: Option<String>,
    #[serde(rename = "createdAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub updated_at: i64,
    #[serde(rename = "reviewedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub reviewed_at: i64,
    #[serde(rename = "soldAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub sold_at: i64,
    #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
    pub data: ItemData,
    pub network: Network,
    #[serde(rename = "chainId")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    pub urn: String,
    #[serde(rename = "firstListedAt", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number", optional))]
    pub first_listed_at: Option<i64>,
    pub picks: PicksCount,
    pub utility: Option<String>,
    #[serde(rename = "tradeId")]
    pub trade_id: Option<String>,
    #[serde(rename = "tradeExpiresAt", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number", optional))]
    pub trade_expires_at: Option<i64>,
    #[serde(rename = "tradeContractAddress")]
    pub trade_contract_address: Option<String>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct CreditCatalogItem {
    #[serde(flatten)]
    #[cfg_attr(feature = "ts", ts(flatten))]
    pub item: Item,
    #[serde(rename = "priceCredits")]
    #[cfg_attr(feature = "ts", ts(rename = "priceCredits", type = "number"))]
    pub price_credits: i64,
}

pub fn is_address_zero(addr: &str) -> bool {
    addr.is_empty() || addr.eq_ignore_ascii_case("0x0000000000000000000000000000000000000000")
}

/// The credit price, a bare expression the WHERE and ORDER BY reuse so the
/// filter and the displayed price cannot disagree: a USD-pegged trade
/// bypasses the rate, a MANA one and the store price go through it.
///
/// A USD value above [`MAX_USD_WEI`] prices in no credits (0, as an item not
/// sold in credits), the ceiling the listings feed already drops rows over: a
/// price no bigint credit amount can hold (Amoy carries an item at 2^256-1 wei)
/// otherwise fails the cast, and with it every page that holds the row.
fn price_credits_expr(rate_p: &str) -> String {
    let usd_wei = format!(
        "CASE WHEN items.available > 0 AND ut.id IS NOT NULL AND {v3} THEN \
            CASE WHEN ut.received_asset_type = {usd} THEN ut.amount_received \
                 ELSE ut.amount_received * {rate_p}::numeric END \
         WHEN items.available > 0 AND items.search_is_store_minter = true THEN items.price * {rate_p}::numeric END",
        v3 = v3_minter_expr(),
        usd = ASSET_TYPE_USD_PEGGED_MANA,
    );
    format!(
        "CASE WHEN ({usd_wei}) <= {MAX_USD_WEI}::numeric THEN CEIL(({usd_wei}) / {credit}) ELSE 0 END",
        credit = USD_WEI_PER_CREDIT_SQL
    )
}

/// The page query over `items`, one open item trade per item; `rate` and
/// `catalog` turn it into the `/v3/catalog/items` credit feed.
pub fn build_items_query(
    f: &ItemFilters,
    rate: Option<&str>,
    catalog: Option<&CatalogItemsParams>,
    now: chrono::NaiveDateTime,
    since: Option<chrono::NaiveDateTime>,
    count_only: bool,
) -> (String, sqlx::postgres::PgArguments) {
    let mut b = Builder::new();
    let now_p = b.bind_time(now);
    let since_p = b.bind_opt_time(since);
    b.push("WITH ");
    b.push(&unified_trades_cte(&now_p, &since_p));
    let mut wheres: Vec<String> = Vec::new();
    if catalog.is_some() {
        wheres.push("items.search_is_collection_approved = true".to_string());
    }
    if let Some(c) = f.category {
        let types = item_types_for(c);
        if !types.is_empty() {
            let p = b.bind_strings(&types.into_iter().map(String::from).collect::<Vec<_>>());
            wheres.push(format!("items.item_type = ANY({p})"));
        }
    }
    if !f.creator.is_empty() {
        let p = b.bind_strings(
            &f.creator
                .iter()
                .map(|c| c.to_lowercase())
                .collect::<Vec<_>>(),
        );
        wheres.push(format!("items.creator = ANY({p})"));
    }
    if !f.rarities.is_empty() {
        let p = b.bind_strings(&f.rarities);
        wheres.push(format!("items.rarity = ANY({p})"));
    }
    if f.is_sold_out {
        wheres.push("items.available = 0".to_string());
    }
    // `isOnSale=false` is the complement of `isOnSale=true` (marketplace-server's
    // items query), not a no-op: COALESCE keeps an item whose predicate is NULL.
    if let Some(on_sale) = f.is_on_sale {
        let predicate = format!(
            "(((ut.id IS NOT NULL AND {}) OR items.search_is_store_minter = true) AND items.available > 0)",
            v3_minter_expr()
        );
        wheres.push(if on_sale {
            predicate
        } else {
            format!("NOT COALESCE({predicate}, false)")
        });
    }
    if let Some(term) = f.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        for word in term.split_whitespace().take(8) {
            let p = b.bind_string(format!("%{}%", escape_like(word)));
            wheres.push(format!("(items.name ILIKE {p} OR c.name ILIKE {p})"));
        }
    }
    if f.is_wearable_head {
        wheres.push(format!(
            "items.category IN ({})",
            super::catalog::HEAD_CATEGORIES
                .iter()
                .map(|c| format!("'{c}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if f.is_wearable_accessory {
        wheres.push(format!(
            "items.category IN ({})",
            super::catalog::ACCESSORY_CATEGORIES
                .iter()
                .map(|c| format!("'{c}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if f.is_wearable_smart {
        wheres.push(format!("items.item_type = '{FRAGMENT_SMART_WEARABLE_V1}'"));
    }
    if let Some(wc) = &f.wearable_category {
        let p = b.bind_string(wc.clone());
        wheres.push(format!(
            "(items.category = {p} AND items.item_type <> 'emote_v1')"
        ));
    }
    if let Some(arr) = body_shapes_for_genders(&f.wearable_genders) {
        let p = b.bind_strings(&arr);
        wheres.push(format!(
            "(items.body_shapes @> {p} AND items.item_type <> 'emote_v1')"
        ));
    }
    if let Some(ec) = &f.emote_category {
        let p = b.bind_string(ec.clone());
        wheres.push(format!(
            "(items.category = {p} AND items.item_type = 'emote_v1')"
        ));
    }
    if let Some(arr) = body_shapes_for_genders(&f.emote_genders) {
        let p = b.bind_strings(&arr);
        wheres.push(format!(
            "(items.body_shapes @> {p} AND items.item_type = 'emote_v1')"
        ));
    }
    if let Some(mode) = emote_play_mode_clause(&f.emote_play_mode) {
        let p = b.bind_bool(mode);
        wheres.push(format!("items.emote_loop = {p}"));
    }
    if !f.contract_addresses.is_empty() {
        let p = b.bind_strings(
            &f.contract_addresses
                .iter()
                .map(|c| c.to_lowercase())
                .collect::<Vec<_>>(),
        );
        wheres.push(format!("items.contract_address = ANY({p})"));
    }
    if let Some(it) = &f.item_id {
        let p = b.bind_string(it.clone());
        wheres.push(format!("items.item_id::text = {p}"));
    }
    if !f.ids.is_empty() {
        let p = b.bind_strings(&f.ids);
        wheres.push(format!(
            "(items.contract_address || '-' || items.item_id::text) = ANY({p})"
        ));
    }
    if let Some(n) = f.network {
        let p = b.bind_string(n.db_name().to_string());
        wheres.push(format!("items.network = {p}"));
    }
    if let Some(mn) = &f.min_price {
        let p = b.bind_string(mn.clone());
        wheres.push(format!("((items.search_is_store_minter = true AND items.price >= {p}::numeric) OR ({} AND ut.amount_received >= {p}::numeric))", v3_minter_expr()));
    }
    if let Some(mx) = &f.max_price {
        let p = b.bind_string(mx.clone());
        wheres.push(format!("((items.search_is_store_minter = true AND items.price <= {p}::numeric) OR ({} AND ut.amount_received <= {p}::numeric))", v3_minter_expr()));
    }
    if f.emote_has_sound {
        wheres.push("items.emote_has_sound = true".to_string());
    }
    if f.emote_has_geometry {
        wheres.push("items.emote_has_geometry = true".to_string());
    }
    if f.emote_outcome_type.is_some() {
        wheres.push("items.emote_outcome_type IS NOT NULL".to_string());
    }
    if !f.include_social_emotes {
        wheres.push("items.emote_outcome_type IS NULL".to_string());
    }
    if !f.urns.is_empty() {
        let p = b.bind_strings(&expand_urn_network_forms(&f.urns));
        wheres.push(format!("items.urn = ANY({p})"));
    }
    let (credits_column, credits_expr) = match rate {
        Some(rate) => {
            let rate_p = b.bind_string(rate.to_string());
            (
                format!(", {}::bigint AS price_credits", price_credits_expr(&rate_p)),
                Some(price_credits_expr(&rate_p)),
            )
        }
        None => (String::new(), None),
    };
    if let (Some(catalog), Some(expr)) = (catalog, credits_expr.as_deref()) {
        if let Some(min) = catalog.min_price_credits {
            let p = b.bind_string(min.to_string());
            wheres.push(format!("NULLIF({expr}, 0) >= {p}::numeric"));
        }
        if let Some(max) = catalog.max_price_credits {
            let p = b.bind_string(max.to_string());
            wheres.push(format!("NULLIF({expr}, 0) <= {p}::numeric"));
        }
    }
    let relevance = {
        let explicit = match (catalog, credits_expr.as_deref()) {
            (Some(catalog), Some(_)) => catalog.sort_by.map(|s| s == ShopSortBy::Relevance),
            _ => f.sort_by.map(|s| s == ItemSortBy::Relevance),
        };
        match f.search.as_deref() {
            Some(search) if !count_only && ranks_by_relevance(explicit, Some(search)) => {
                search_relevance_rank(&mut b, "items.name", search)
            }
            _ => None,
        }
    };
    let order_by = match (catalog, credits_expr.as_deref()) {
        _ if relevance.is_some() => {
            format!("ORDER BY {}, items.created_at DESC, items.contract_address ASC, items.item_id ASC", relevance.unwrap_or_default())
        }
        (Some(catalog), Some(expr)) => match catalog.sort_by {
            Some(ShopSortBy::Cheapest) => format!("ORDER BY NULLIF({expr}, 0) ASC NULLS LAST, items.contract_address ASC, items.item_id ASC"),
            Some(ShopSortBy::MostExpensive) => format!("ORDER BY {expr} DESC, items.contract_address ASC, items.item_id ASC"),
            Some(ShopSortBy::Name) => "ORDER BY items.name ASC, items.contract_address ASC, items.item_id ASC".to_string(),
            _ => "ORDER BY items.created_at DESC, items.contract_address ASC, items.item_id ASC".to_string(),
        },
        _ => match f.sort_by.unwrap_or(ItemSortBy::Newest) {
            ItemSortBy::Newest | ItemSortBy::Relevance => "ORDER BY items.created_at DESC, items.contract_address ASC, items.item_id ASC",
            ItemSortBy::RecentlyReviewed => "ORDER BY items.reviewed_at DESC, items.contract_address ASC, items.item_id ASC",
            ItemSortBy::RecentlySold => "ORDER BY items.sold_at DESC, items.contract_address ASC, items.item_id ASC",
            ItemSortBy::Name => "ORDER BY items.name ASC, items.contract_address ASC, items.item_id ASC",
            ItemSortBy::Cheapest => "ORDER BY items.price ASC, items.contract_address ASC, items.item_id ASC",
            ItemSortBy::RecentlyListed => "ORDER BY COALESCE(to_timestamp(items.first_listed_at) AT TIME ZONE 'UTC', ut.created_at) DESC NULLS LAST, items.contract_address ASC, items.item_id ASC",
        }
        .to_string(),
    };
    let where_clause = super::wire::where_from(&wheres);
    const TRADE_LATERAL: &str = "LEFT JOIN LATERAL (SELECT * FROM unified_trades u WHERE u.type = 'public_item_order' AND u.contract_address_sent = items.contract_address AND u.item_id = items.item_id ORDER BY u.id::text DESC LIMIT 1) ut ON TRUE";
    if count_only {
        // The count carries no window and joins the open trade only when a
        // predicate reads it: the approved default page is then one
        // index-only scan of `items_approved_created_idx` (METRICS.md,
        // "Stage 5: market half", Q9).
        let reads_trade = f.is_on_sale.is_some()
            || f.min_price.is_some()
            || f.max_price.is_some()
            || catalog
                .is_some_and(|c| c.min_price_credits.is_some() || c.max_price_credits.is_some());
        let reads_collection = f.search.is_some() || reads_trade;
        let collection = if reads_collection {
            "LEFT JOIN shop.collections c ON c.network = items.network AND c.contract_address = items.contract_address"
        } else {
            ""
        };
        let lateral = if reads_trade { TRADE_LATERAL } else { "" };
        b.push(&format!(" SELECT COUNT(*)::int8 AS count FROM shop.items items {collection} {lateral} {where_clause}"));
        return (b.sql, b.args);
    }
    let joins = format!(
        "FROM shop.items items \
         LEFT JOIN shop.collections c ON c.network = items.network AND c.contract_address = items.contract_address {TRADE_LATERAL}"
    );
    let limit_p = b.bind_i64(clamp_first(f.first, DEFAULT_LIMIT));
    let offset_p = b.bind_i64(clamp_skip(f.skip));
    // No `COUNT(*) OVER()`: the window made the planner materialize every
    // approved row before the LIMIT (41 ms, 24k buffers at 11k items); the
    // total is a second, key-only statement, and only when the page is full.
    b.push(&format!(
        " SELECT items.contract_address, items.item_id::text AS item_id, items.image, items.rarity, \
                 items.price::text AS price, items.available::text AS available, items.creator, items.beneficiary, items.created_at, items.updated_at, \
                 items.reviewed_at, items.sold_at, items.urn, items.network, items.search_is_store_minter, COALESCE({v3}, false) AS v3_minter, \
                 ut.id::text AS trade_id, items.name, items.body_shapes, items.category, items.item_type, items.emote_loop, items.emote_has_sound, \
                 items.emote_has_geometry, items.emote_outcome_type, items.description, \
                 COALESCE(to_timestamp(items.first_listed_at) AT TIME ZONE 'UTC', ut.created_at) AS first_listed_at, \
                 ut.beneficiary AS trade_beneficiary, ut.expires_at AS trade_expires_at, ut.trade_contract AS trade_contract, \
                 ut.amount_received::text AS trade_price{credits_column} \
          {joins} {where_clause} {order_by} LIMIT {limit_p} OFFSET {offset_p}",
        v3 = v3_minter_expr()
    ));
    (b.sql, b.args)
}

fn row_to_item(r: &sqlx::postgres::PgRow) -> Item {
    let item_type: String = r.get("item_type");
    let is_wearable = matches!(
        item_type.as_str(),
        FRAGMENT_WEARABLE_V1 | FRAGMENT_WEARABLE_V2 | FRAGMENT_SMART_WEARABLE_V1
    );
    let available: i64 = r.get::<String, _>("available").parse().unwrap_or(0);
    let store_minter: bool = r.get("search_is_store_minter");
    let v3_minter: bool = r.get("v3_minter");
    let trade_id: Option<String> = r.get("trade_id");
    let has_trade = trade_id.is_some();
    let mut price = "0".to_string();
    if available > 0 {
        if has_trade && v3_minter {
            price = r
                .get::<Option<String>, _>("trade_price")
                .unwrap_or_else(|| "0".to_string());
        } else if store_minter {
            price = r.get("price");
        }
    }
    let beneficiary = r
        .get::<Option<String>, _>("trade_beneficiary")
        .or_else(|| r.get::<Option<String>, _>("beneficiary"))
        .unwrap_or_default();
    let beneficiary_out = if is_address_zero(&beneficiary) {
        None
    } else {
        Some(beneficiary)
    };
    let rarity: String = r.get("rarity");
    let contract: String = r.get("contract_address");
    let item_id: String = r.get("item_id");
    let (network, chain_id) = super::wire::network_and_chain(&r.get::<String, _>("network"));
    let body_shapes: Vec<String> = r.get("body_shapes");
    let description: String = r
        .get::<Option<String>, _>("description")
        .unwrap_or_default();
    let category: String = r.get("category");
    let data = if is_wearable {
        ItemData::Wearable {
            wearable: WearableData {
                body_shapes,
                category,
                description,
                rarity: rarity.clone(),
                is_smart: item_type == FRAGMENT_SMART_WEARABLE_V1,
            },
        }
    } else {
        ItemData::Emote {
            emote: EmoteData {
                body_shapes,
                category,
                description,
                rarity: rarity.clone(),
                r#loop: r.get::<Option<bool>, _>("emote_loop").unwrap_or(false),
                has_sound: r.get::<Option<bool>, _>("emote_has_sound").unwrap_or(false),
                has_geometry: r
                    .get::<Option<bool>, _>("emote_has_geometry")
                    .unwrap_or(false),
                outcome_type: r.get("emote_outcome_type"),
            },
        }
    };
    Item {
        id: format!("{contract}-{item_id}"),
        name: r.get("name"),
        thumbnail: fix_urn(&r.get::<Option<String>, _>("image").unwrap_or_default()),
        url: format!("/contracts/{contract}/items/{item_id}"),
        category: if is_wearable {
            NftCategory::Wearable
        } else {
            NftCategory::Emote
        },
        contract_address: contract,
        item_id,
        rarity,
        price,
        available,
        is_on_sale: (store_minter || (has_trade && v3_minter)) && available > 0,
        creator: r.get("creator"),
        beneficiary: beneficiary_out,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        reviewed_at: r.get("reviewed_at"),
        sold_at: r.get("sold_at"),
        data,
        network,
        chain_id,
        urn: fix_urn(&r.get::<String, _>("urn")),
        first_listed_at: r
            .get::<Option<chrono::NaiveDateTime>, _>("first_listed_at")
            .map(|t| t.and_utc().timestamp_millis()),
        picks: PicksCount { count: 0 },
        utility: None,
        trade_id,
        trade_expires_at: r
            .get::<Option<chrono::NaiveDateTime>, _>("trade_expires_at")
            .map(|t| t.and_utc().timestamp_millis()),
        trade_contract_address: r.get("trade_contract"),
    }
}

/// The page, then its total: the rows themselves when the first page came
/// back short, one key-only count otherwise.
async fn run_items(
    state: &MarketplaceState,
    f: &ItemFilters,
    rate: Option<&str>,
    catalog: Option<&CatalogItemsParams>,
) -> Result<(Vec<sqlx::postgres::PgRow>, i64), ApiError> {
    let now = Utc::now().naive_utc();
    let since = super::trades::mirrored_since(&state.settings, now);
    let (sql, args) = build_items_query(f, rate, catalog, now, since, false);
    let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
        .fetch_all(&state.pool)
        .await?;
    let (limit, offset) = (clamp_first(f.first, DEFAULT_LIMIT), clamp_skip(f.skip));
    let total = if offset == 0 && (rows.len() as i64) < limit {
        rows.len() as i64
    } else {
        let (sql, args) = build_items_query(f, rate, catalog, now, since, true);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_one(&state.pool)
            .await?
            .get("count")
    };
    Ok((rows, total))
}

pub async fn list_items(
    state: &MarketplaceState,
    f: &ItemFilters,
) -> Result<(Vec<Item>, i64), ApiError> {
    let (rows, total) = run_items(state, f, None, None).await?;
    Ok((rows.iter().map(row_to_item).collect(), total))
}

/// The rate as the exact decimal text a `NUMERIC` bind takes: never through
/// a float, whose 0.1 is 0.100000000000000006 and moves a CEIL across a credit.
pub fn rate_to_numeric_string(rate: ManaUsdRate) -> String {
    rate.decimal_text()
}

/// The Shop's MANA/USD. Where the marketplace's feed is wired
/// (`mana::feed_rate::current`), its last good answer, and the price route's
/// unavailable envelope when there is none. Otherwise the spot: the newest
/// row of `economy.price_snapshots` from any source (the `mana-price-poll`
/// job's archive: CoinGecko, or the exchange it fell back to), as the price
/// route reads it, memoized 60 s, accepted only while younger than
/// `price::store::SNAPSHOT_MAX_AGE`; otherwise the same envelope. There is no
/// fallback rate: a credit price from a constant is a wrong checkout amount
/// (the 0.02 the slice first shipped priced 75 MANA items at 15 credits
/// against 69 at the day's spot).
pub async fn mana_usd_rate(state: &MarketplaceState) -> Result<ManaUsdRate, Response> {
    if let Some(feed) = crate::mana::feed_rate::current() {
        return feed.ok().map(|shop| shop.rate).ok_or_else(|| {
            crate::price::unavailable(
                "the marketplace's MANA/USD feed has no current answer; credit prices need one",
            )
        });
    }
    let v = cached(state, Ttl::S60, "economy:mana_usd".into(), || async {
        let row: Option<(String, chrono::NaiveDateTime)> = sqlx::query_as("SELECT mana_usd::text, LEAST(taken_at, COALESCE(source_updated_at, taken_at)) FROM economy.price_snapshots ORDER BY taken_at DESC LIMIT 1")
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(match row {
            Some((rate, taken_at)) => json!({ "rate": rate.trim(), "taken_at": taken_at.and_utc().timestamp() }),
            None => json!({}),
        })
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "price_snapshots read failed");
        super::internal_server_error()
    })?;
    let budget = crate::price::store::SNAPSHOT_MAX_AGE.as_secs() as i64;
    match (
        v["rate"].as_str().and_then(ManaUsdRate::parse_usd_per_mana),
        v["taken_at"].as_i64(),
    ) {
        (Some(rate), Some(taken_at)) if Utc::now().timestamp() - taken_at <= budget => Ok(rate),
        (Some(_), Some(taken_at)) => Err(crate::price::unavailable(&format!(
            "the newest MANA spot on this node is {} s old, past the {budget} s freshness budget (the mana-price-poll job is off or failing); credit prices need a live spot",
            Utc::now().timestamp() - taken_at
        ))),
        _ => Err(crate::price::unavailable(
            "no MANA spot has been taken on this node yet; credit prices need one",
        )),
    }
}

/// The items mirror and the trades mirror (the open item trade every row
/// carries: `tradeId`, the trade price, `isOnSale`) must both have a pass,
/// the items one within its money budget (the minter price).
async fn require_sources(state: &MarketplaceState) -> Result<(), Response> {
    require_priced(state).await?;
    super::trades::require_ready(state).await
}

pub async fn get_items(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(envelope) = require_sources(&state).await {
        return envelope;
    }
    let filters = match parse_filters(&pairs) {
        Ok(f) => f,
        Err(e) => return e.into_response(),
    };
    match list_items(&state, &filters).await {
        Ok((data, total)) => Json(DataTotal { data, total }).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `/v3/catalog/items` for these parameters: the page and its total, or the
/// response a source that cannot answer gives (the search dropdown reads
/// the same grid).
#[allow(clippy::result_large_err)]
pub async fn catalog_items_page(
    state: &MarketplaceState,
    pairs: &[(String, String)],
) -> Result<(Vec<CreditCatalogItem>, i64), Response> {
    require_sources(state).await?;
    let filters = parse_filters(pairs).map_err(IntoResponse::into_response)?;
    let catalog = parse_catalog_items_params(pairs);
    let rate = rate_to_numeric_string(mana_usd_rate(state).await?);
    let (rows, total) = run_items(state, &filters, Some(&rate), Some(&catalog))
        .await
        .map_err(IntoResponse::into_response)?;
    let data = rows
        .iter()
        .map(|r| CreditCatalogItem {
            item: row_to_item(r),
            price_credits: r.try_get::<i64, _>("price_credits").unwrap_or(0),
        })
        .collect();
    Ok((data, total))
}

pub async fn get_catalog_items(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    match catalog_items_page(&state, &pairs).await {
        Ok((data, total)) => Json(DataTotal { data, total }).into_response(),
        Err(r) => r,
    }
}

const TRENDING_DEFAULT_SIZE: i64 = 20;
const SALES_CUT: f64 = 0.6;
const VOLUME_CUT: f64 = 0.4;
pub const TRENDING_SALES_LIMIT: i64 = 1000;
pub const TRENDING_WINDOW_DAYS: i64 = 7;

const SALES_QUERY: &str = r#"
query recentSales($from: BigInt!, $first: Int!) {
  sales(first: $first, orderBy: timestamp, orderDirection: desc, where: { timestamp_gt: $from }) {
    id
    searchItemId
    searchContractAddress
    price
    timestamp
    type
    seller
    buyer
    item { id creator }
  }
}"#;

/// Midnight UTC `days` ago as unix seconds, the anchor upstream derived.
pub fn midnight_days_ago(days: i64) -> i64 {
    let date = Utc::now() - chrono::Duration::days(days);
    date.date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

/// The last `days` of `sales` on every configured collections subgraph, at
/// most [`TRENDING_SALES_LIMIT`] per network, memoized 10 min.
pub async fn recent_sales(state: &MarketplaceState, days: i64) -> Result<Vec<Value>, Response> {
    let urls = readers::require_both_collections(state)?;
    let from = midnight_days_ago(days);
    let mut out = Vec::new();
    for (network, url) in urls {
        let key = format!("sales:{}:{from}", network.db_name());
        let data = cached(state, Ttl::M10, key, || async {
            readers::query(
                state,
                url,
                SALES_QUERY,
                json!({ "from": from.to_string(), "first": TRENDING_SALES_LIMIT }),
            )
            .await
        })
        .await
        .map_err(|e| readers::upstream_failed("/v1/trendings", &e))?;
        out.extend(readers::rows(&data, "sales"));
    }
    Ok(out)
}

fn volume_of(item: &Item, sales_count: i64) -> u128 {
    item.price
        .parse::<u128>()
        .unwrap_or(0)
        .saturating_mul(sales_count.max(0) as u128)
}

/// The old selection: 60% of the slots to the most sold, 40% to the biggest
/// current-price volume among the rest, only items still on sale, then the
/// V8-compatible deterministic shuffle.
pub fn select_trending(
    order: &[(String, String)],
    counts: &HashMap<(String, String), i64>,
    mut fetched: HashMap<(String, String), Item>,
    size: i64,
) -> Vec<Item> {
    let mut owned: Vec<Item> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    for key in order {
        if let Some(it) = fetched.remove(key) {
            index.insert(key.clone(), owned.len());
            owned.push(it);
        }
    }
    let mut by_sales: Vec<(usize, i64)> = order
        .iter()
        .enumerate()
        .map(|(i, k)| (i, counts[k]))
        .collect();
    by_sales.sort_by_key(|b| std::cmp::Reverse(b.1));
    let sales_cap = ((size as f64) * SALES_CUT).floor() as usize;
    let mut chosen_sales: Vec<usize> = Vec::new();
    for (order_i, _) in &by_sales {
        if chosen_sales.len() >= sales_cap {
            break;
        }
        if let Some(idx) = index.get(&order[*order_i]).copied()
            && owned[idx].is_on_sale
        {
            chosen_sales.push(idx);
        }
    }
    let mut by_volume: Vec<(usize, i64)> = order
        .iter()
        .enumerate()
        .map(|(i, k)| (i, counts[k]))
        .collect();
    by_volume.sort_by(|a, b| {
        let va = index
            .get(&order[a.0])
            .map(|i| volume_of(&owned[*i], a.1))
            .unwrap_or(0);
        let vb = index
            .get(&order[b.0])
            .map(|i| volume_of(&owned[*i], b.1))
            .unwrap_or(0);
        vb.cmp(&va)
    });
    let volume_cap = ((size as f64) * VOLUME_CUT).floor() as usize;
    let mut chosen_volume: Vec<usize> = Vec::new();
    for (order_i, _) in &by_volume {
        if chosen_volume.len() >= volume_cap {
            break;
        }
        if let Some(idx) = index.get(&order[*order_i]).copied()
            && owned[idx].is_on_sale
            && !chosen_sales.contains(&idx)
            && !chosen_volume.contains(&idx)
        {
            chosen_volume.push(idx);
        }
    }
    let mut chosen = chosen_sales;
    chosen.extend(chosen_volume);
    let mut wrapped: Vec<Option<Item>> = owned.into_iter().map(Some).collect();
    let mut out: Vec<Item> = Vec::with_capacity(chosen.len());
    for idx in chosen {
        if let Some(it) = wrapped.get_mut(idx).and_then(Option::take) {
            out.push(it);
        }
    }
    seedrandom::det_shuffle(&mut out, |it| it.id.as_str());
    out
}

pub async fn get_trendings(
    State(state): State<SharedState>,
    _headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let p = Params::new(&pairs);
    let size = p
        .get_number("size", None)
        .map(|v| v as i64)
        .unwrap_or(TRENDING_DEFAULT_SIZE);
    let include_social_emotes =
        p.get_string("includeSocialEmotes", None).as_deref() != Some("false");
    if let Err(envelope) = require_sources(&state).await {
        return envelope;
    }
    let cache_control = [(CACHE_CONTROL, "public,max-age=3600,s-maxage=3600")];
    if size <= 0 {
        return (cache_control, Json(json!({ "data": [] }))).into_response();
    }
    let sales = match recent_sales(&state, TRENDING_WINDOW_DAYS).await {
        Ok(s) => s,
        Err(envelope) => return envelope,
    };
    let mut counts: HashMap<(String, String), i64> = HashMap::new();
    let mut order: Vec<(String, String)> = Vec::new();
    for sale in &sales {
        let item_id = str_of(sale, "searchItemId");
        let contract = str_of(sale, "searchContractAddress").to_lowercase();
        if item_id.is_empty() || contract.is_empty() {
            continue;
        }
        let key = (contract, item_id);
        if !counts.contains_key(&key) {
            order.push(key.clone());
        }
        *counts.entry(key).or_insert(0) += 1;
    }
    if counts.is_empty() {
        return (cache_control, Json(json!({ "data": [] }))).into_response();
    }
    let filters = ItemFilters {
        ids: order.iter().map(|(c, i)| format!("{c}-{i}")).collect(),
        first: Some(order.len() as i64),
        include_social_emotes,
        ..Default::default()
    };
    let got = match list_items(&state, &filters).await {
        Ok((items, _)) => items,
        Err(e) => return e.into_response(),
    };
    let fetched: HashMap<(String, String), Item> = got
        .into_iter()
        .map(|it| ((it.contract_address.clone(), it.item_id.clone()), it))
        .collect();
    let data = select_trending(&order, &counts, fetched, size);
    let _ = int_of;
    (cache_control, Json(json!({ "data": data }))).into_response()
}

/// seedrandom's ARC4 first double, and the V8-sort-compatible shuffle the
/// marketplace-server used to spread a rail (`sort(() => seedrandom(a+b)() - 0.5)`).
pub mod seedrandom {
    const WIDTH: u32 = 256;
    const CHUNKS: u32 = 6;
    const MASK: u32 = WIDTH - 1;
    const STARTDENOM: f64 = 281_474_976_710_656.0;
    const SIGNIFICANCE: f64 = 4_503_599_627_370_496.0;
    const OVERFLOW: f64 = 9_007_199_254_740_992.0;

    fn to_int32(v: i64) -> i64 {
        (v as i32) as i64
    }

    fn mixkey(seed: &str) -> Vec<u8> {
        let units: Vec<u16> = seed.encode_utf16().collect();
        let mut key: Vec<Option<i64>> = vec![None; 256];
        let mut smear: i64 = 0;
        let mut max_written: i64 = -1;
        for (j, unit) in units.iter().enumerate() {
            let idx = (MASK as usize) & j;
            let cur = key[idx].unwrap_or(0);
            smear = to_int32(smear ^ to_int32(cur * 19));
            let mixed = to_int32(smear + *unit as i64);
            key[idx] = Some((MASK as i64) & mixed);
            if idx as i64 > max_written {
                max_written = idx as i64;
            }
        }
        if max_written < 0 {
            return Vec::new();
        }
        (0..=max_written as usize)
            .map(|i| key[i].unwrap_or(0) as u8)
            .collect()
    }

    struct Arc4 {
        s: [u32; 256],
        i: u32,
        j: u32,
    }

    impl Arc4 {
        fn new(key: &[u8]) -> Self {
            let key: Vec<u32> = if key.is_empty() {
                vec![0]
            } else {
                key.iter().map(|&b| b as u32).collect()
            };
            let keylen = key.len() as u32;
            let mut s = [0u32; 256];
            for (i, slot) in s.iter_mut().enumerate() {
                *slot = i as u32;
            }
            let mut j: u32 = 0;
            for i in 0..256u32 {
                let t = s[i as usize];
                j = MASK & (j.wrapping_add(key[(i % keylen) as usize]).wrapping_add(t));
                s[i as usize] = s[j as usize];
                s[j as usize] = t;
            }
            let mut arc4 = Arc4 { s, i: 0, j: 0 };
            arc4.g(WIDTH);
            arc4
        }

        fn g(&mut self, count: u32) -> f64 {
            let mut r: f64 = 0.0;
            let mut i = self.i;
            let mut j = self.j;
            for _ in 0..count {
                i = MASK & (i.wrapping_add(1));
                let t = self.s[i as usize];
                j = MASK & (j.wrapping_add(t));
                self.s[i as usize] = self.s[j as usize];
                self.s[j as usize] = t;
                let idx = MASK & (self.s[i as usize].wrapping_add(self.s[j as usize]));
                r = r * (WIDTH as f64) + self.s[idx as usize] as f64;
            }
            self.i = i;
            self.j = j;
            r
        }
    }

    pub fn first_double(seed: &str) -> f64 {
        let key = mixkey(seed);
        let mut arc4 = Arc4::new(&key);
        let mut n = arc4.g(CHUNKS);
        let mut d = STARTDENOM;
        let mut x: f64 = 0.0;
        while n < SIGNIFICANCE {
            n = (n + x) * (WIDTH as f64);
            d *= WIDTH as f64;
            x = arc4.g(1);
        }
        while n >= OVERFLOW {
            n /= 2.0;
            d /= 2.0;
            x = ((x as u32) >> 1) as f64;
        }
        (n + x) / d
    }

    /// V8's binary insertion sort under the seeded comparator.
    pub fn det_shuffle<T, F>(items: &mut [T], id: F)
    where
        F: Fn(&T) -> &str,
    {
        let n = items.len();
        if n < 2 {
            return;
        }
        let cmp = |a: &T, b: &T| -> std::cmp::Ordering {
            let seed = format!("{}{}", id(a), id(b));
            if first_double(&seed) > 0.5 {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        };
        for start in 1..n {
            let mut left = 0usize;
            let mut right = start;
            while left < right {
                let mid = (left + right) >> 1;
                if cmp(&items[start], &items[mid]) == std::cmp::Ordering::Less {
                    right = mid;
                } else {
                    left = mid + 1;
                }
            }
            items[left..=start].rotate_right(1);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn approx(a: f64, b: f64) {
            assert!((a - b).abs() < 1e-12, "expected {b}, got {a}");
        }

        #[test]
        fn known_vectors() {
            approx(first_double("hello"), 0.5463663768140734);
            approx(first_double(""), 0.23144008215179881);
            approx(first_double("0xabc0xdef"), 0.1646894869769447);
            approx(first_double("\u{1F480}emoji"), 0.6852670915645748);
            approx(first_double("long-seed-value-123"), 0.9333971911046186);
        }

        #[test]
        fn det_shuffle_matches_v8_sort() {
            let ids = [
                "0xaaa-1", "0xbbb-2", "0xccc-3", "0xddd-4", "0xeee-5", "0xfff-6", "0xggg-7",
                "0xhhh-8", "0xiii-9", "0xjjj-10", "0xkkk-11", "0xlll-12",
            ];
            let shuffle = |ids: &[&str]| {
                let mut v: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
                det_shuffle(&mut v, |s| s.as_str());
                v
            };
            assert_eq!(
                shuffle(&ids),
                vec![
                    "0xjjj-10", "0xddd-4", "0xccc-3", "0xaaa-1", "0xkkk-11", "0xeee-5", "0xhhh-8",
                    "0xggg-7", "0xlll-12", "0xfff-6", "0xbbb-2", "0xiii-9"
                ]
            );
            assert_eq!(shuffle(&ids[..3]), vec!["0xccc-3", "0xaaa-1", "0xbbb-2"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn relevance_is_the_default_of_a_search_and_nothing_without_one() {
        assert_eq!(ItemSortBy::parse("relevance"), Some(ItemSortBy::Relevance));
        assert_eq!(ShopSortBy::parse("relevance"), Some(ShopSortBy::Relevance));
        assert!(
            ranks_by_relevance(None, Some("hat")) && ranks_by_relevance(Some(true), Some("hat"))
        );
        assert!(
            !ranks_by_relevance(Some(false), Some("hat")),
            "an explicit other sort wins"
        );
        assert!(
            !ranks_by_relevance(Some(true), None) && !ranks_by_relevance(Some(true), Some("  ")),
            "relevance without a search is the default sort"
        );
        assert_eq!(
            search_words("T-Shirt  Máscara!"),
            vec!["t", "shirt", "máscara"]
        );
        let mut b = Builder::new();
        assert!(search_relevance_rank(&mut b, "items.name", " -- ").is_none());
        let rank = search_relevance_rank(&mut b, "items.name", "pirate hat").unwrap();
        assert!(
            rank.starts_with("CASE WHEN") && !rank.contains("pirate"),
            "values are bound: {rank}"
        );
        let (sql, _) = build_items_query(
            &parse_filters(&pairs(&[("search", "hat")])).unwrap(),
            None,
            None,
            chrono::Utc::now().naive_utc(),
            None,
            false,
        );
        assert!(
            sql.contains("ORDER BY CASE WHEN"),
            "a search ranks by relevance by default: {sql}"
        );
        let (sql, _) = build_items_query(
            &parse_filters(&pairs(&[("search", "hat"), ("sortBy", "name")])).unwrap(),
            None,
            None,
            chrono::Utc::now().naive_utc(),
            None,
            false,
        );
        assert!(sql.contains("ORDER BY items.name ASC"), "{sql}");
        let (sql, _) = build_items_query(
            &parse_filters(&pairs(&[("search", "hat")])).unwrap(),
            None,
            None,
            chrono::Utc::now().naive_utc(),
            None,
            true,
        );
        assert!(
            !sql.contains("regexp_split_to_table"),
            "the count binds no rank: {sql}"
        );
    }

    #[tokio::test]
    async fn the_relevance_rank_puts_the_name_that_is_the_query_first() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let mut b = Builder::new();
        let rank = search_relevance_rank(&mut b, "t.name", "hat").unwrap();
        b.push(&format!(
            "SELECT t.name FROM (VALUES ('Hatch'), ('Pirate Hat'), ('Treasure'), ('Hat Pirate'), ('HAT'), ('Red-Hat of Doom')) AS t(name) ORDER BY {rank}, t.name"
        ));
        let names: Vec<String> = sqlx::query_scalar_with(sqlx::AssertSqlSafe(b.sql), b.args)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            names,
            vec![
                "HAT",
                "Hat Pirate",
                "Pirate Hat",
                "Red-Hat of Doom",
                "Hatch",
                "Treasure"
            ]
        );
    }

    #[test]
    fn parse_ether_and_the_price_params() {
        assert_eq!(parse_ether("1").unwrap(), "1000000000000000000");
        assert_eq!(parse_ether("1.5").unwrap(), "1500000000000000000");
        assert_eq!(parse_ether(".5").unwrap(), "500000000000000000");
        assert_eq!(parse_ether("0.000000000000000001").unwrap(), "1");
        assert!(parse_ether("1.1234567890123456789").is_none());
        assert!(
            parse_ether("abc").is_none()
                && parse_ether("1,5").is_none()
                && parse_ether("").is_none()
        );
        assert_eq!(parse_ether("-0").unwrap(), "0");
        let err = parse_filters(&pairs(&[("minPrice", "abc")])).unwrap_err();
        assert_eq!(err.message(), "Invalid minPrice value: abc");
        let f = parse_filters(&pairs(&[
            ("maxPrice", "2"),
            ("sortBy", "cheapest"),
            ("isOnSale", "true"),
            ("includeSocialEmotes", "false"),
        ]))
        .unwrap();
        assert_eq!(f.max_price.as_deref(), Some("2000000000000000000"));
        assert_eq!(f.sort_by, Some(ItemSortBy::Cheapest));
        assert_eq!(f.is_on_sale, Some(true));
        assert!(!f.include_social_emotes);
        let c = parse_catalog_items_params(&pairs(&[
            ("sortBy", "most_expensive"),
            ("minPriceCredits", "2"),
            ("maxPriceCredits", "30"),
        ]));
        assert_eq!(c.sort_by, Some(ShopSortBy::MostExpensive));
        assert_eq!(
            (c.min_price_credits, c.max_price_credits),
            (Some(2.0), Some(30.0))
        );
        assert_eq!(
            parse_catalog_items_params(&pairs(&[("sortBy", "recently_listed")])).sort_by,
            None
        );
        let exact = |t: &str| rate_to_numeric_string(ManaUsdRate::parse_usd_per_mana(t).unwrap());
        assert_eq!(exact("0.02"), "0.02");
        assert_eq!(exact("0.1"), "0.1", "never 0.100000000000000006");
        assert_eq!(exact("0.26960836"), "0.26960836");
    }

    #[test]
    fn the_items_query_keeps_the_lateral_trade_and_the_v3_feed_prices_in_credits() {
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .naive_utc();
        let (sql, _) = build_items_query(
            &ItemFilters {
                item_id: Some("7".into()),
                ..Default::default()
            },
            None,
            None,
            now,
            None,
            false,
        );
        assert!(sql.contains("LEFT JOIN LATERAL"), "{sql}");
        assert!(sql.contains("ORDER BY u.id::text DESC LIMIT 1"), "{sql}");
        assert!(
            !sql.contains("search_is_collection_approved"),
            "/v1/items serves unapproved collections too: {sql}"
        );
        assert!(!sql.contains("price_credits"), "{sql}");
        assert!(
            sql.contains("items.item_id::text = $3"),
            "the clock is $1, the trades floor $2: {sql}"
        );
        let (sql, _) = build_items_query(
            &ItemFilters::default(),
            Some("0.020000000000000000"),
            Some(&CatalogItemsParams {
                sort_by: Some(ShopSortBy::Cheapest),
                min_price_credits: Some(3.0),
                ..Default::default()
            }),
            now,
            None,
            false,
        );
        assert!(
            sql.contains("items.search_is_collection_approved = true"),
            "{sql}"
        );
        assert!(
            sql.contains("ut.received_asset_type = 2"),
            "USD-pegged trades bypass the rate: {sql}"
        );
        assert!(sql.contains("END::bigint AS price_credits"), "{sql}");
        assert!(sql.contains("ORDER BY NULLIF(CASE"), "{sql}");
        assert!(sql.contains(", 0) >= $"), "{sql}");
        let (count, _) = build_items_query(&ItemFilters::default(), None, None, now, None, true);
        assert!(
            count.contains("SELECT COUNT(*)::int8 AS count") && !count.contains("LIMIT $"),
            "{count}"
        );
        let from = &count[count.find("SELECT COUNT").unwrap()..];
        assert!(
            !from.contains("LEFT JOIN"),
            "a count no predicate of which reads the trade or the collection is over items alone (the CTE stays unreferenced): {count}"
        );
        let (count, _) = build_items_query(
            &ItemFilters {
                is_on_sale: Some(true),
                ..Default::default()
            },
            None,
            None,
            now,
            None,
            true,
        );
        assert!(
            count[count.find("SELECT COUNT").unwrap()..].contains("LEFT JOIN LATERAL"),
            "isOnSale reads ut: {count}"
        );
        assert!(
            !sql.contains("COUNT(*) OVER()"),
            "the page carries no window count: {sql}"
        );
        let (sql, _) = build_items_query(
            &ItemFilters {
                search: Some("pirate hat".into()),
                category: Some(NftCategory::Wearable),
                ..Default::default()
            },
            None,
            None,
            now,
            None,
            false,
        );
        assert_eq!(sql.matches("ILIKE").count(), 4, "{sql}");
        assert!(sql.contains("items.item_type = ANY("), "{sql}");
    }

    fn item(id: &str, price: &str, on_sale: bool) -> Item {
        let (c, i) = id.split_once('-').unwrap();
        Item {
            id: id.into(),
            name: id.into(),
            thumbnail: String::new(),
            url: String::new(),
            category: NftCategory::Wearable,
            contract_address: c.into(),
            item_id: i.into(),
            rarity: "epic".into(),
            price: price.into(),
            available: 1,
            is_on_sale: on_sale,
            creator: String::new(),
            beneficiary: None,
            created_at: 0,
            updated_at: 0,
            reviewed_at: 0,
            sold_at: 0,
            data: ItemData::Wearable {
                wearable: WearableData {
                    body_shapes: vec![],
                    category: "hat".into(),
                    description: String::new(),
                    rarity: "epic".into(),
                    is_smart: false,
                },
            },
            network: Network::Matic,
            chain_id: ChainId::MaticMainnet,
            urn: String::new(),
            first_listed_at: None,
            picks: PicksCount::default(),
            utility: None,
            trade_id: None,
            trade_expires_at: None,
            trade_contract_address: None,
        }
    }

    #[test]
    fn trending_takes_the_most_sold_then_the_biggest_volume_and_only_what_is_on_sale() {
        let ids = ["0xa-1", "0xb-2", "0xc-3", "0xd-4"];
        let order: Vec<(String, String)> = ids
            .iter()
            .map(|id| {
                let (c, i) = id.split_once('-').unwrap();
                (c.to_string(), i.to_string())
            })
            .collect();
        let mut counts = HashMap::new();
        counts.insert(order[0].clone(), 5);
        counts.insert(order[1].clone(), 3);
        counts.insert(order[2].clone(), 1);
        counts.insert(order[3].clone(), 9);
        let fetched: HashMap<(String, String), Item> = [
            item("0xa-1", "10", true),
            item("0xb-2", "1000", true),
            item("0xc-3", "5", true),
            item("0xd-4", "1", false),
        ]
        .into_iter()
        .map(|it| ((it.contract_address.clone(), it.item_id.clone()), it))
        .collect();
        let out = select_trending(&order, &counts, fetched, 3);
        let mut ids: Vec<&str> = out.iter().map(|i| i.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["0xa-1", "0xb-2"],
            "one sales slot (0xa, 0xd is not on sale) and one volume slot (0xb: 1000x3)"
        );
        let v = serde_json::to_value(item("0xa-1", "10", true)).unwrap();
        assert!(v.get("firstListedAt").is_none() && v.get("tradeExpiresAt").is_none());
        assert_eq!(
            v["picks"],
            json!({ "count": 0 }),
            "the old crate's PicksCount, not the TypeScript server's {{count, itemId}}"
        );
        // The old crate's serializers (ports/items/mod.rs, ts(type = "number")): numbers, where
        // marketplace-api.decentraland.org emits strings.
        for key in [
            "available",
            "createdAt",
            "updatedAt",
            "reviewedAt",
            "soldAt",
            "chainId",
        ] {
            assert!(v[key].is_number(), "{key} is a number on the old wire: {v}");
        }
        assert!(v["price"].is_string() && v["itemId"].is_string(), "{v}");
        assert_eq!(v["tradeId"], Value::Null);
        assert_eq!(v["category"], "wearable");
    }

    #[tokio::test]
    async fn a_store_mint_prices_its_credits_at_the_exact_rate() {
        // A 1 MANA store mint at $0.10 a MANA is $0.10: one credit. Bound through a
        // float, 0.1 is 0.100000000000000006 and the CEIL makes it two.
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let credits = |rate: String| {
            let pool = pool.clone();
            async move {
                let mut b = Builder::new();
                let rate_p = b.bind_string(rate);
                b.push(&format!(
                    "SELECT CEIL(1000000000000000000::numeric * {rate_p}::numeric / {USD_WEI_PER_CREDIT_SQL})::text"
                ));
                sqlx::query_scalar_with::<_, String, _>(sqlx::AssertSqlSafe(b.sql), b.args)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        let rate = ManaUsdRate::parse_usd_per_mana("0.1").unwrap();
        assert_eq!(credits(rate_to_numeric_string(rate)).await, "1");
        assert_eq!(
            credits(format!("{:.18}", 0.1_f64)).await,
            "2",
            "what the float bind charged"
        );
    }
}
