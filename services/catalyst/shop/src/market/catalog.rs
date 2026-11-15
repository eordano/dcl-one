//! `GET /v1/catalog` and `GET /v2/catalog` (the explorer boot corpus row 45,
//! `?first=34&id=<contract>-<item>`) over the `items` mirror. The old route
//! read `squid_marketplace.item` with its metadata joins and, for v2, the
//! `mv_trades` view; here the item and its wearable/emote leg are one
//! `shop.items` row, the collection's minters come from
//! `shop.collections` (the v3-minter flag the squid precomputed), and
//! the v2 off-chain legs come from `shop.trades` at read time: an
//! open trade is one not expired, effective, and absent from
//! `trade_cancellations` (no `Traded` event source exists in v2, so an
//! executed trade still reads open until the `trade_events` indexer lands).
//! The nft-order legs map a token to its
//! item by the collection layout `item_id = token_id >> 216`. What v2 does
//! not have: the legacy on-chain `orders` (a squid table) that fed v1's
//! listing counts, so `/v1/catalog` serves the store-minter half alone and
//! `listings`/`minListingPrice`/`maxListingPrice` on v1 are the no-orders
//! shape; the `owners` count and the `suggested` sort (per-item nft owners,
//! `wearable_last_seen`) are deferred with the nft indexer, `owners` reads
//! null and `sortBy=suggested` orders as `newest`. Both routes answer the
//! unavailable envelope until the items mirror has a complete pass for
//! every configured network, and v2 until the trades mirror has one too
//! (`trades::require_ready`): its `price`, `isOnSale`, `tradeId` and listing
//! counts come from `shop.trades`. Both carry prices, so a copy of the
//! items past its money budget answers the envelope too
//! (`mirrors::require_priced`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Serialize;
use serde::ser::{SerializeSeq, SerializeStruct, Serializer};
use sqlx::Arguments;
use sqlx::Row;
use sqlx::postgres::PgArguments;

use crate::shared::cache::TtlMap;

use super::lists::{PickStats, picks_stats};
use super::mirrors::require_priced;
use super::signed::optional_signer;
use super::wire::{
    ApiError, ChainId, MAX_PAGE_LIMIT, NETWORK_VALUES, NFT_CATEGORY_VALUES, Network, NftCategory,
    Params, clamp_first, clamp_skip,
};
use super::{MarketplaceState, SharedState};

pub const FRAGMENT_WEARABLE_V1: &str = "wearable_v1";
pub const FRAGMENT_WEARABLE_V2: &str = "wearable_v2";
pub const FRAGMENT_SMART_WEARABLE_V1: &str = "smart_wearable_v1";
pub const FRAGMENT_EMOTE_V1: &str = "emote_v1";
pub const WEARABLE_ITEM_TYPES: [&str; 3] = [
    FRAGMENT_WEARABLE_V1,
    FRAGMENT_WEARABLE_V2,
    FRAGMENT_SMART_WEARABLE_V1,
];

/// uint256 max: what the squid wrote into `price` for "no price set", and
/// the `LEAST` sentinel of the old price CASEs. It never reaches the wire:
/// `min_max_price_cases` maps it to NULL, so an item with no open leg
/// carries no `minPrice`/`minListingPrice` (production emits none either).
pub const MAX_NUMERIC_NUMBER: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";

/// 2^216: a collection token id is `item_id << 216 | issued_id`, the rule
/// that gives an nft-order leg its item without an nft table.
pub use crate::shared::collections::ITEM_SHIFT as ITEM_ID_SHIFT;

/// The off-chain marketplace contracts (v2 and v3, both mainnets and
/// testnets): a collection listing one of them as a minter is what the squid
/// flagged `search_is_marketplace_v3_minter`.
pub const OFFCHAIN_MARKETPLACE_MINTERS: [&str; 6] = [
    "0x1b67d0e31eeb6b52d8eeed71d3616c2f5b33b8e7",
    "0xa40b1d129b8906888720686f3a01921ddf37716f",
    "0x0f11d0d1671519683bd48abf3dbe779e300941cd",
    "0xe38ef22abe871513555cba89adfe45ab4f548ada",
    "0x257db44ac97789c16ab277eae87dcde0c246cc9f",
    "0x36fd1434a6c4b8ade80c9847c1d15033ce34488c",
];

pub const HEAD_CATEGORIES: [&str; 5] = ["eyebrows", "eyes", "facial_hair", "hair", "mouth"];
pub const ACCESSORY_CATEGORIES: [&str; 7] = [
    "earring", "eyewear", "hat", "helmet", "mask", "tiara", "top_head",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogSortBy {
    Newest,
    RecentlyListed,
    RecentlySold,
    Cheapest,
    MostExpensive,
    Suggested,
}

impl CatalogSortBy {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "newest" => Self::Newest,
            "recently_listed" => Self::RecentlyListed,
            "recently_sold" => Self::RecentlySold,
            "cheapest" => Self::Cheapest,
            "most_expensive" => Self::MostExpensive,
            "suggested" => Self::Suggested,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogSortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CatalogFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub sort_by: Option<CatalogSortBy>,
    pub sort_direction: Option<CatalogSortDirection>,
    pub only_listing: bool,
    pub only_minting: bool,
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
    pub include_social_emotes: Option<bool>,
}

const DEFAULT_PAGE_SIZE: i64 = 20;
pub const WEARABLE_CATEGORY_VALUES: &[&str] = &[
    "eyebrows",
    "eyes",
    "facial_hair",
    "hair",
    "body_shape",
    "mouth",
    "upper_body",
    "lower_body",
    "feet",
    "earring",
    "eyewear",
    "hat",
    "helmet",
    "mask",
    "tiara",
    "top_head",
    "skin",
    "hands_wear",
];
pub const EMOTE_CATEGORY_VALUES: &[&str] = &[
    "dance",
    "stunt",
    "greetings",
    "fun",
    "poses",
    "reactions",
    "horror",
    "miscellaneous",
];
pub const RARITY_VALUES: &[&str] = &[
    "unique",
    "mythic",
    "exotic",
    "legendary",
    "epic",
    "rare",
    "uncommon",
    "common",
];
pub const GENDER_VALUES: &[&str] = &["male", "female", "unisex"];
pub const EMOTE_PLAY_MODE_VALUES: &[&str] = &["simple", "loop"];
pub const EMOTE_OUTCOME_VALUES: &[&str] = &["so", "mo", "ro"];
const SORT_BY_VALUES: &[&str] = &[
    "newest",
    "recently_sold",
    "cheapest",
    "most_expensive",
    "recently_listed",
    "suggested",
];

pub fn parse_catalog_filters(pairs: &[(String, String)]) -> CatalogFilters {
    let p = Params::new(pairs);
    let is_on_sale = if p.get_boolean("isOnSale") {
        Some(p.get_string("isOnSale", None).as_deref() == Some("true"))
    } else {
        None
    };
    CatalogFilters {
        first: p
            .get_number("first", Some(DEFAULT_PAGE_SIZE as f64))
            .map(|n| n as i64),
        skip: p.get_number("skip", Some(0.0)).map(|n| n as i64),
        sort_by: p
            .get_value("sortBy", SORT_BY_VALUES, Some("cheapest"))
            .and_then(|s| CatalogSortBy::parse(&s)),
        sort_direction: p
            .get_value("sortDirection", &["asc", "desc"], Some("asc"))
            .map(|s| {
                if s == "desc" {
                    CatalogSortDirection::Desc
                } else {
                    CatalogSortDirection::Asc
                }
            }),
        only_listing: p.get_boolean("onlyListing"),
        only_minting: p.get_boolean("onlyMinting"),
        category: p
            .get_value("category", NFT_CATEGORY_VALUES, None)
            .and_then(|s| NftCategory::parse(&s)),
        creator: p.get_list("creator", &[]),
        is_sold_out: p.get_boolean("isSoldOut"),
        is_on_sale,
        search: p.get_string("search", None),
        is_wearable_head: p.get_boolean("isWearableHead"),
        is_wearable_accessory: p.get_boolean("isWearableAccessory"),
        is_wearable_smart: p.get_boolean("isWearableSmart"),
        wearable_category: p.get_value("wearableCategory", WEARABLE_CATEGORY_VALUES, None),
        rarities: p.get_list("rarity", RARITY_VALUES),
        wearable_genders: p.get_list("wearableGender", GENDER_VALUES),
        emote_category: p.get_value("emoteCategory", EMOTE_CATEGORY_VALUES, None),
        emote_genders: p.get_list("emoteGender", GENDER_VALUES),
        emote_play_mode: p.get_list("emotePlayMode", EMOTE_PLAY_MODE_VALUES),
        emote_has_geometry: p.get_boolean("emoteHasGeometry"),
        emote_has_sound: p.get_boolean("emoteHasSound"),
        emote_outcome_type: p.get_value("emoteOutcomeType", EMOTE_OUTCOME_VALUES, None),
        contract_addresses: p.get_address_list("contractAddress", false),
        item_id: p.get_string("itemId", None),
        network: p
            .get_value("network", NETWORK_VALUES, None)
            .and_then(|s| Network::parse(&s)),
        max_price: p
            .get_string("maxPrice", None)
            .filter(|s| !s.trim().is_empty()),
        min_price: p
            .get_string("minPrice", None)
            .filter(|s| !s.trim().is_empty()),
        urns: p.get_list("urn", &[]),
        ids: p.get_list("id", &[]),
        include_social_emotes: Some(
            p.get_string("includeSocialEmotes", None).as_deref() != Some("false"),
        ),
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WearableData {
    pub description: Option<String>,
    pub category: Option<String>,
    #[serde(rename = "bodyShapes")]
    pub body_shapes: Vec<String>,
    pub rarity: String,
    #[serde(rename = "isSmart")]
    pub is_smart: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ItemData {
    Wearable { wearable: WearableData },
    Emote { emote: serde_json::Value },
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct CatalogItem {
    pub id: String,
    pub beneficiary: Option<String>,
    #[serde(rename = "itemId")]
    pub item_id: String,
    pub name: String,
    pub thumbnail: String,
    pub url: String,
    pub urn: String,
    pub category: &'static str,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    pub rarity: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available: i64,
    #[serde(rename = "isOnSale")]
    pub is_on_sale: bool,
    /// Present iff the price came from an open v3 item trade (marketplace-server #387).
    #[serde(rename = "tradeId", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub trade_id: Option<String>,
    pub creator: String,
    #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
    pub data: ItemData,
    pub network: Network,
    #[serde(rename = "chainId")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    pub price: String,
    #[serde(rename = "createdAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub updated_at: i64,
    #[serde(rename = "reviewedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub reviewed_at: i64,
    #[serde(rename = "firstListedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub first_listed_at: Option<i64>,
    #[serde(rename = "soldAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub sold_at: i64,
    #[serde(rename = "minPrice", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub min_price: Option<String>,
    #[serde(rename = "maxListingPrice")]
    pub max_listing_price: Option<String>,
    #[serde(rename = "minListingPrice")]
    pub min_listing_price: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub listings: Option<i64>,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub owners: Option<i64>,
    pub picks: Option<PickStats>,
}

/// `{"data": [...], "total": N}` with the caller's picks folded in; only the
/// items carrying picks are cloned off the cached page.
pub struct CatalogPage {
    page: Arc<(Vec<CatalogItem>, i64)>,
    picks: Vec<Option<PickStats>>,
}

impl Serialize for CatalogPage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Data<'a>(&'a CatalogPage);
        impl Serialize for Data<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let items = &self.0.page.0;
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for (item, pick) in items.iter().zip(&self.0.picks) {
                    if pick.is_none() && item.picks.is_none() {
                        seq.serialize_element(item)?;
                    } else {
                        let mut owned = item.clone();
                        owned.picks = pick.clone();
                        seq.serialize_element(&owned)?;
                    }
                }
                seq.end()
            }
        }
        let mut s = serializer.serialize_struct("DataTotal", 2)?;
        s.serialize_field("data", &Data(self))?;
        s.serialize_field("total", &self.page.1)?;
        s.end()
    }
}

type Page = Arc<(Vec<CatalogItem>, i64)>;

/// Pages keyed by `(is_v2, filters)` and anonymous pick stats keyed by the
/// page's ids, under `MARKETPLACE_CATALOG_CACHE_TTL_SECS` (0 disables).
pub struct Cache {
    enabled: bool,
    pages: TtlMap<(bool, CatalogFilters), Page>,
    picks: TtlMap<Vec<String>, Arc<Vec<PickStats>>>,
}

const CACHE_ENTRIES: usize = 256;

impl Cache {
    pub fn new(ttl: Duration) -> Self {
        let ttl_or_min = if ttl.is_zero() {
            Duration::from_secs(1)
        } else {
            ttl
        };
        Self {
            enabled: !ttl.is_zero(),
            pages: TtlMap::bounded(ttl_or_min, CACHE_ENTRIES),
            picks: TtlMap::bounded(ttl_or_min, CACHE_ENTRIES),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// A local pick write happened: every cached page's counts are stale.
    pub fn invalidate(&self) {
        self.pages.clear();
        self.picks.clear();
    }

    async fn page_or_fetch<F, Fut>(
        &self,
        key: (bool, CatalogFilters),
        fetch: F,
    ) -> Result<Page, ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Page, ApiError>>,
    {
        if !self.enabled {
            return fetch().await;
        }
        self.pages.get_or_fetch(key, fetch).await
    }

    async fn picks_or_fetch<F, Fut>(
        &self,
        ids: Vec<String>,
        fetch: F,
    ) -> Result<Arc<Vec<PickStats>>, ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Arc<Vec<PickStats>>, ApiError>>,
    {
        if !self.enabled {
            return fetch().await;
        }
        self.picks.get_or_fetch(ids, fetch).await
    }
}

pub struct Builder {
    pub sql: String,
    pub args: PgArguments,
    next_index: usize,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self {
            sql: String::new(),
            args: PgArguments::default(),
            next_index: 1,
        }
    }
    pub fn push(&mut self, s: &str) -> &mut Self {
        self.sql.push_str(s);
        self
    }
    pub fn bind_string(&mut self, v: String) -> String {
        self.args.add(v).expect("add string arg");
        self.placeholder()
    }
    pub fn bind_strings(&mut self, v: &[String]) -> String {
        self.args.add(v.to_vec()).expect("add string[] arg");
        self.placeholder()
    }
    pub fn bind_bool(&mut self, v: bool) -> String {
        self.args.add(v).expect("add bool arg");
        self.placeholder()
    }
    pub fn bind_i64(&mut self, v: i64) -> String {
        self.args.add(v).expect("add i64 arg");
        self.placeholder()
    }
    pub fn bind_time(&mut self, v: chrono::NaiveDateTime) -> String {
        self.args.add(v).expect("add timestamp arg");
        self.placeholder()
    }
    pub fn bind_opt_time(&mut self, v: Option<chrono::NaiveDateTime>) -> String {
        self.args.add(v).expect("add timestamp arg");
        self.placeholder()
    }
    fn placeholder(&mut self) -> String {
        let p = format!("${}", self.next_index);
        self.next_index += 1;
        p
    }
}

/// The chains this node's trade readers serve, installed once by the binary.
static SERVED: std::sync::OnceLock<(i64, i64)> = std::sync::OnceLock::new();

/// Installs the served chains (`MARKETPLACE_INDEX_CHAIN_ID_*`) for the
/// process; the first call wins. Without it the readers serve mainnet (1, 137).
pub fn install_served_chains(indexes: &super::chain_walk::IndexSettings) {
    use super::chain_walk::ChainNet;
    let _ = SERVED.set((
        indexes.chain_id(ChainNet::Ethereum),
        indexes.chain_id(ChainNet::Matic),
    ));
}

/// `(ethereum, matic)`.
pub fn served_chains() -> (i64, i64) {
    SERVED.get().copied().unwrap_or((1, 137))
}

/// `alias`'s trade is on the chain this node serves for its network.
pub fn served_chain_sql(alias: &str) -> String {
    let (eth, matic) = served_chains();
    format!(
        "{alias}.chain_id = CASE lower({alias}.network) WHEN 'ethereum' THEN {eth} ELSE {matic} END"
    )
}

/// `alias`'s trade is one this node serves: on its chain for the network, and
/// an item order only when the item's creator signed it.
pub fn served_trade_sql(alias: &str) -> String {
    format!(
        "{served} AND ({alias}.type <> 'public_item_order' OR EXISTS ( \
           SELECT 1 FROM shop.trade_assets sa \
           JOIN shop.trade_assets_item si ON si.asset_id = sa.id \
           JOIN shop.items ci ON ci.network = lower({alias}.network) \
             AND ci.contract_address = lower(sa.contract_address) AND ci.item_id = si.item_id::numeric \
           WHERE sa.trade_id = {alias}.id AND sa.direction = 'sent' AND lower(ci.creator) = lower({alias}.signer)))",
        served = served_chain_sql(alias),
    )
}

/// `unified_trades`: the open off-chain listings of `shop.trades` as
/// one row per trade with its sent asset resolved to `(contract, item_id,
/// token_id)` and its received amount. Open = effective, not expired at
/// `now_p` (a bound timestamp, never `now()`), not in `trade_cancellations`,
/// and not closed on chain (`trade_events`: sold or cancelled there) while
/// that network's trade-events index is published; and counted at
/// `since_p` (a bound nullable timestamp, `trades::mirrored_since`).
pub fn unified_trades_cte(now_p: &str, since_p: &str) -> String {
    format!(
        "unified_trades AS ( \
           SELECT t.id, t.type::text AS type, t.signer, t.created_at, t.expires_at, t.network, t.chain_id, t.contract AS trade_contract, \
                  sent.contract_address AS contract_address_sent, \
                  COALESCE(sent_item.item_id::numeric, \
                           CASE WHEN sent_erc721.token_id IS NOT NULL THEN div(sent_erc721.token_id::numeric, {shift}::numeric) END) AS item_id, \
                  sent_erc721.token_id AS token_id, \
                  recv.asset_type::int4 AS received_asset_type, \
                  recv_erc20.amount AS amount_received, \
                  recv.beneficiary AS beneficiary \
           FROM shop.trades t \
           JOIN shop.trade_assets sent ON sent.trade_id = t.id AND sent.direction = 'sent' \
           LEFT JOIN shop.trade_assets_item sent_item ON sent_item.asset_id = sent.id \
           LEFT JOIN shop.trade_assets_erc721 sent_erc721 ON sent_erc721.asset_id = sent.id \
           JOIN shop.trade_assets recv ON recv.trade_id = t.id AND recv.direction = 'received' AND recv.asset_type IN (1, 2) \
           JOIN shop.trade_assets_erc20 recv_erc20 ON recv_erc20.asset_id = recv.id \
           WHERE t.type IN ('public_item_order', 'public_nft_order') \
             AND t.expires_at > {now_p} AND t.effective_since <= {now_p} \
             AND NOT EXISTS (SELECT 1 FROM shop.trade_cancellations c WHERE c.target_hashed_signature = t.hashed_signature) \
             AND NOT {closed} \
             AND {counted} \
             AND {served} \
             AND (t.type <> 'public_item_order' OR EXISTS ( \
                   SELECT 1 FROM shop.items ci \
                   WHERE ci.network = lower(t.network) AND ci.contract_address = lower(sent.contract_address) \
                     AND ci.item_id = sent_item.item_id::numeric AND lower(ci.creator) = lower(t.signer))) \
         )",
        served = served_chain_sql("t"),
        shift = ITEM_ID_SHIFT,
        closed = super::trade_events::closed_on_chain_sql("t", now_p),
        counted = super::trades::counted_sql("t.", since_p)
    )
}

/// The `items` alias' v3-minter flag: the collection lists an off-chain marketplace as a minter.
pub fn v3_minter_expr() -> String {
    let list: Vec<String> = OFFCHAIN_MARKETPLACE_MINTERS
        .iter()
        .map(|a| format!("'{a}'"))
        .collect();
    format!("(c.minters && ARRAY[{}]::text[])", list.join(", "))
}

fn build_offchain_orders_cte(b: &mut Builder, f: &CatalogFilters) {
    b.push(
        ", offchain_orders AS ( \
           SELECT contract_address_sent, item_id, \
                  COUNT(*) AS count, \
                  COUNT(*) FILTER (WHERE type = 'public_nft_order') AS nfts_listings_count, \
                  COUNT(*) FILTER (WHERE type = 'public_item_order') AS items_listings_count, \
                  MIN(amount_received) FILTER (WHERE type = 'public_nft_order') AS min_order_amount_received, \
                  MAX(amount_received) FILTER (WHERE type = 'public_nft_order') AS max_order_amount_received, \
                  MAX(created_at) AS max_created_at, \
                  MAX(id::text) FILTER (WHERE type = 'public_item_order') AS open_item_trade_id, \
                  MAX(amount_received) FILTER (WHERE type = 'public_item_order') AS open_item_trade_price, \
                  MIN(created_at) FILTER (WHERE type = 'public_item_order') AS item_first_listed_at \
           FROM unified_trades WHERE item_id IS NOT NULL",
    );
    if f.only_minting {
        b.push(" AND type = 'public_item_order'");
    }
    if let Some(mn) = &f.min_price {
        let p = b.bind_string(mn.clone());
        b.push(&format!(" AND amount_received >= {p}::numeric"));
    }
    if let Some(mx) = &f.max_price {
        let p = b.bind_string(mx.clone());
        b.push(&format!(" AND amount_received <= {p}::numeric"));
    }
    b.push(" GROUP BY contract_address_sent, item_id )");
}

fn head_or_accessory_list(list: &[&str]) -> String {
    list.iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The item-level predicates every catalog shape applies (old `build_collections_where`).
fn build_where(b: &mut Builder, f: &CatalogFilters, is_v2: bool) {
    let mut parts: Vec<String> = vec!["items.search_is_collection_approved = true".to_string()];
    if f.include_social_emotes == Some(false) {
        parts.push("items.emote_outcome_type IS NULL".to_string());
    }
    if let Some(cat) = f.category {
        match cat {
            NftCategory::Wearable => {
                if f.is_wearable_smart {
                    parts.push(format!("items.item_type = '{FRAGMENT_SMART_WEARABLE_V1}'"));
                } else {
                    parts.push(format!(
                        "items.item_type IN ({})",
                        WEARABLE_ITEM_TYPES
                            .iter()
                            .map(|t| format!("'{t}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            NftCategory::Emote => parts.push(format!("items.item_type = '{FRAGMENT_EMOTE_V1}'")),
            _ => parts.push("TRUE".to_string()),
        }
    }
    if !f.rarities.is_empty() {
        let p = b.bind_strings(&f.rarities);
        parts.push(format!("items.rarity = ANY({p})"));
    }
    if !f.creator.is_empty() {
        let lower: Vec<String> = f.creator.iter().map(|c| c.to_lowercase()).collect();
        let p = b.bind_strings(&lower);
        parts.push(format!("items.creator = ANY({p})"));
    }
    if f.is_sold_out {
        parts.push("items.available = 0".to_string());
    }
    if let Some(on_sale) = f.is_on_sale {
        let store = "(items.search_is_store_minter = true AND items.available > 0)";
        if is_v2 {
            let trade_mint = format!(
                "({} AND offchain_orders.items_listings_count > 0 AND items.available > 0)",
                v3_minter_expr()
            );
            let resale = "COALESCE(offchain_orders.nfts_listings_count, 0) > 0";
            if on_sale {
                if f.only_minting {
                    parts.push(format!("({store} OR {trade_mint})"));
                } else {
                    parts.push(format!("({store} OR {trade_mint} OR {resale})"));
                }
            } else {
                parts.push(format!(
                    "(NOT {store} AND NOT {trade_mint} AND NOT {resale})"
                ));
            }
        } else if on_sale {
            parts.push(store.to_string());
        } else {
            parts.push(format!("NOT {store}"));
        }
    }
    if f.is_wearable_head {
        parts.push(format!(
            "items.category IN ({})",
            head_or_accessory_list(&HEAD_CATEGORIES)
        ));
    }
    if f.is_wearable_accessory {
        parts.push(format!(
            "items.category IN ({})",
            head_or_accessory_list(&ACCESSORY_CATEGORIES)
        ));
    }
    if let Some(c) = &f.wearable_category {
        let p = b.bind_string(c.clone());
        parts.push(format!(
            "items.category = {p} AND items.item_type <> '{FRAGMENT_EMOTE_V1}'"
        ));
    }
    if !f.wearable_genders.is_empty() {
        let mut shapes: Vec<String> = Vec::new();
        for g in &f.wearable_genders {
            match g.as_str() {
                "female" => shapes.push("BaseFemale".to_string()),
                "male" => shapes.push("BaseMale".to_string()),
                _ => {}
            }
        }
        if !shapes.is_empty() {
            let p = b.bind_strings(&shapes);
            parts.push(format!("items.body_shapes @> {p}"));
        }
    }
    if let Some(c) = &f.emote_category {
        let p = b.bind_string(c.clone());
        parts.push(format!(
            "items.category = {p} AND items.item_type = '{FRAGMENT_EMOTE_V1}'"
        ));
    }
    if f.emote_play_mode.len() == 1 {
        let p = b.bind_bool(f.emote_play_mode[0] == "loop");
        parts.push(format!("items.emote_loop = {p}"));
    }
    if !f.contract_addresses.is_empty() {
        let lower: Vec<String> = f
            .contract_addresses
            .iter()
            .map(|c| c.to_lowercase())
            .collect();
        let p = b.bind_strings(&lower);
        parts.push(format!("items.contract_address = ANY({p})"));
    }
    if let Some(id) = f.item_id.as_deref().filter(|s| !s.is_empty()) {
        let p = b.bind_string(id.to_string());
        parts.push(format!("items.item_id::text = {p}"));
    }
    if let Some(mn) = &f.min_price {
        let p = b.bind_string(mn.clone());
        let minting = format!(
            "(items.price >= {p}::numeric AND items.available > 0 AND (items.search_is_store_minter = true OR {}))",
            v3_minter_expr()
        );
        if f.only_minting {
            parts.push(format!("(items.price >= {p}::numeric AND items.price IS DISTINCT FROM '{MAX_NUMERIC_NUMBER}'::numeric)"));
        } else if f.only_listing && is_v2 {
            parts.push(format!(
                "offchain_orders.min_order_amount_received >= {p}::numeric"
            ));
        } else if is_v2 {
            parts.push(format!("({minting} OR offchain_orders.min_order_amount_received >= {p}::numeric OR offchain_orders.open_item_trade_price >= {p}::numeric)"));
        } else {
            parts.push(minting);
        }
    }
    if let Some(mx) = &f.max_price {
        let p = b.bind_string(mx.clone());
        let minting = format!(
            "(items.price <= {p}::numeric AND items.available > 0 AND (items.search_is_store_minter = true OR {}))",
            v3_minter_expr()
        );
        if f.only_minting {
            parts.push(format!("items.price <= {p}::numeric"));
        } else if f.only_listing && is_v2 {
            parts.push(format!(
                "offchain_orders.max_order_amount_received <= {p}::numeric"
            ));
        } else if is_v2 {
            parts.push(format!("({minting} OR offchain_orders.max_order_amount_received <= {p}::numeric OR offchain_orders.open_item_trade_price <= {p}::numeric)"));
        } else {
            parts.push(minting);
        }
    }
    if f.only_listing {
        if is_v2 {
            parts.push(format!(
                "((items.search_is_store_minter = false AND NOT {v3}) OR (items.search_is_store_minter = true AND items.available = 0) OR ({v3} AND COALESCE(offchain_orders.items_listings_count, 0) = 0)) AND COALESCE(offchain_orders.nfts_listings_count, 0) > 0",
                v3 = v3_minter_expr()
            ));
        } else {
            parts.push("FALSE".to_string());
        }
    }
    if f.only_minting {
        if is_v2 {
            parts.push(format!("((items.search_is_store_minter = true OR ({} AND offchain_orders.items_listings_count > 0)) AND items.available > 0)", v3_minter_expr()));
        } else {
            parts.push("items.search_is_store_minter = true AND items.available > 0".to_string());
        }
    }
    if !f.ids.is_empty() {
        let p = b.bind_strings(&f.ids);
        parts.push(format!(
            "(items.contract_address || '-' || items.item_id::text) = ANY({p})"
        ));
    }
    if f.emote_has_sound {
        parts.push("items.emote_has_sound = true".to_string());
    }
    if f.emote_has_geometry {
        parts.push("items.emote_has_geometry = true".to_string());
    }
    if f.emote_outcome_type.is_some() {
        parts.push("items.emote_outcome_type IS NOT NULL".to_string());
    }
    if !f.urns.is_empty() {
        let p = b.bind_strings(&expand_urn_network_forms(&f.urns));
        parts.push(format!("items.urn = ANY({p})"));
    }
    if let Some(n) = f.network {
        let p = b.bind_string(n.db_name().to_string());
        parts.push(format!("items.network = {p}"));
    }
    if let Some(term) = f.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        for word in term.split_whitespace().take(8) {
            let p = b.bind_string(format!("%{}%", escape_like(word)));
            parts.push(format!("(items.name ILIKE {p} OR c.name ILIKE {p})"));
        }
    }
    b.push(&format!(" WHERE {} ", parts.join(" AND ")));
}

pub fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn expand_urn_network_forms(urns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(urns.len());
    let mut seen = std::collections::HashSet::new();
    for u in urns {
        for v in [
            u.clone(),
            u.replace(":mainnet:", ":ethereum:"),
            u.replace(":ethereum:", ":mainnet:"),
        ] {
            if seen.insert(v.clone()) {
                out.push(v);
            }
        }
    }
    out
}

fn build_order_by(b: &mut Builder, f: &CatalogFilters, is_v2: bool) {
    let sort_by = f.sort_by.unwrap_or(CatalogSortBy::Newest);
    if f.is_on_sale == Some(false) && sort_by != CatalogSortBy::Newest {
        b.push(" ORDER BY items.contract_address, items.item_id ");
        return;
    }
    b.push(" ORDER BY ");
    if f.search.is_some() && !f.ids.is_empty() {
        let p = b.bind_strings(&f.ids);
        b.push(&format!(
            "array_position({p}::text[], items.contract_address || '-' || items.item_id::text), "
        ));
    }
    let clause = match (sort_by, is_v2) {
        (CatalogSortBy::Newest | CatalogSortBy::Suggested, true) => {
            "GREATEST(COALESCE(ROUND(EXTRACT(EPOCH FROM offchain_orders.item_first_listed_at)), 0), items.first_listed_at) DESC NULLS LAST"
        }
        (CatalogSortBy::Newest | CatalogSortBy::Suggested, false) => {
            "items.first_listed_at DESC NULLS LAST"
        }
        (CatalogSortBy::MostExpensive, _) => "max_price_num DESC NULLS LAST",
        (CatalogSortBy::RecentlyListed, true) => {
            "GREATEST(COALESCE(ROUND(EXTRACT(EPOCH FROM offchain_orders.max_created_at)), 0), items.first_listed_at) DESC NULLS LAST"
        }
        (CatalogSortBy::RecentlyListed, false) => "items.first_listed_at DESC NULLS LAST",
        (CatalogSortBy::RecentlySold, _) => "items.sold_at DESC",
        (CatalogSortBy::Cheapest, _) => "min_price_num ASC NULLS LAST, items.first_listed_at DESC",
    };
    b.push(clause);
    b.push(", items.contract_address ASC, items.item_id ASC ");
}

fn min_max_price_cases(b: &mut Builder, f: &CatalogFilters, is_v2: bool) {
    let v3 = v3_minter_expr();
    let n = MAX_NUMERIC_NUMBER;
    let mut min_when = format!(
        " (CASE WHEN items.available > 0 AND (items.search_is_store_minter = true OR {v3}) "
    );
    if let Some(mn) = &f.min_price {
        let p = b.bind_string(mn.clone());
        min_when.push_str(&format!(" AND items.price >= {p}::numeric"));
    }
    let min_expr = if is_v2 {
        if f.only_minting {
            format!(
                "{min_when} THEN LEAST(COALESCE(items.price, '{n}'::numeric), COALESCE(offchain_orders.open_item_trade_price, '{n}'::numeric)) ELSE COALESCE(offchain_orders.open_item_trade_price, '{n}'::numeric) END)"
            )
        } else {
            format!(
                "{min_when} THEN LEAST(COALESCE(items.price, '{n}'::numeric), COALESCE(offchain_orders.min_order_amount_received, '{n}'::numeric), COALESCE(offchain_orders.open_item_trade_price, '{n}'::numeric)) ELSE LEAST(COALESCE(offchain_orders.min_order_amount_received, '{n}'::numeric), COALESCE(offchain_orders.open_item_trade_price, '{n}'::numeric)) END)"
            )
        }
    } else {
        format!("{min_when} THEN items.price ELSE NULL::numeric END)")
    };
    let mut max_when = format!(
        " (CASE WHEN items.available > 0 AND (items.search_is_store_minter = true OR {v3}) "
    );
    if let Some(mx) = &f.max_price {
        let p = b.bind_string(mx.clone());
        max_when.push_str(&format!(" AND items.price <= {p}::numeric"));
    }
    let max_expr = if is_v2 {
        if f.only_minting {
            format!(
                "{max_when} THEN GREATEST(items.price, offchain_orders.open_item_trade_price) ELSE offchain_orders.open_item_trade_price END)"
            )
        } else {
            format!(
                "{max_when} THEN GREATEST(items.price, offchain_orders.max_order_amount_received, offchain_orders.open_item_trade_price) ELSE GREATEST(offchain_orders.max_order_amount_received, offchain_orders.open_item_trade_price) END)"
            )
        }
    } else {
        format!("{max_when} THEN items.price ELSE NULL::numeric END)")
    };
    let min_expr = format!("NULLIF({min_expr}, '{n}'::numeric)");
    let max_expr = format!("NULLIF({max_expr}, '{n}'::numeric)");
    b.push(&format!(" {min_expr} AS min_price_num, {min_expr}::text AS min_price, {max_expr} AS max_price_num, {max_expr}::text AS max_price, "));
}

/// The page query: `is_v2` joins the off-chain legs, `count` swaps the
/// projection for `COUNT(*)` (the total when the page is empty past the end).
pub fn build_catalog_query(
    f: &CatalogFilters,
    is_v2: bool,
    now: chrono::NaiveDateTime,
    since: Option<chrono::NaiveDateTime>,
    count: bool,
) -> (String, PgArguments) {
    let mut b = Builder::new();
    if is_v2 {
        let now_p = b.bind_time(now);
        let since_p = b.bind_opt_time(since);
        b.push("WITH ");
        b.push(&unified_trades_cte(&now_p, &since_p));
        build_offchain_orders_cte(&mut b, f);
        b.push(" ");
    }
    if count {
        b.push("SELECT COUNT(*)::int8 AS total ");
    } else {
        b.push(
            "SELECT items.contract_address, items.item_id::text AS item_id, items.item_type, items.urn, items.creator, items.beneficiary, \
                    items.rarity, items.available::text AS available, items.price::text AS price, items.image, items.name, items.description, \
                    items.category, items.body_shapes, items.emote_loop, items.emote_has_sound, items.emote_has_geometry, items.emote_outcome_type, \
                    items.search_is_store_minter, items.network, items.created_at, items.updated_at, items.reviewed_at, items.sold_at, ",
        );
        b.push(&format!(
            "COALESCE({}, false) AS v3_minter, ",
            v3_minter_expr()
        ));
        if is_v2 {
            b.push(
                " offchain_orders.open_item_trade_id::text AS open_item_trade_id, offchain_orders.open_item_trade_price::text AS open_item_trade_price, \
                  offchain_orders.min_order_amount_received::text AS min_listing_price, offchain_orders.max_order_amount_received::text AS max_listing_price, \
                  COALESCE(offchain_orders.nfts_listings_count, 0)::int8 AS listings_count, ",
            );
            if f.is_on_sale == Some(true) {
                b.push(" LEAST(items.first_listed_at, ROUND(EXTRACT(EPOCH FROM offchain_orders.item_first_listed_at))::bigint) AS first_listed_at, ");
            } else {
                b.push(" items.first_listed_at, ");
            }
        } else {
            b.push(" NULL::text AS open_item_trade_id, NULL::text AS open_item_trade_price, NULL::text AS min_listing_price, NULL::text AS max_listing_price, 0::int8 AS listings_count, items.first_listed_at, ");
        }
        min_max_price_cases(&mut b, f, is_v2);
        b.push(" COUNT(*) OVER()::int8 AS total ");
    }
    b.push(" FROM shop.items items LEFT JOIN shop.collections c ON c.network = items.network AND c.contract_address = items.contract_address ");
    if is_v2 {
        b.push(" LEFT JOIN offchain_orders ON offchain_orders.contract_address_sent = items.contract_address AND offchain_orders.item_id = items.item_id ");
    }
    build_where(&mut b, f, is_v2);
    if !count {
        build_order_by(&mut b, f, is_v2);
        if f.first.is_some() || f.skip.is_some() {
            let li = b.bind_i64(clamp_first(f.first, MAX_PAGE_LIMIT));
            let oi = b.bind_i64(clamp_skip(f.skip));
            b.push(&format!(" LIMIT {li} OFFSET {oi}"));
        }
    }
    (b.sql, b.args)
}

/// The old `fix_thumbnail`: the L2 thumbnail URN gets its `0x` and item id.
pub fn fix_thumbnail(thumbnail: &str, blockchain_id: &str) -> String {
    if thumbnail.is_empty() {
        return String::new();
    }
    let t = thumbnail
        .replace("polygon", "matic")
        .replace("mainnet", "ethereum");
    if t.contains("ethereum") {
        return t;
    }
    let mut parts: Vec<String> = t.split(':').map(String::from).collect();
    if parts.len() <= 5 {
        return t;
    }
    if !parts[5].starts_with("0x") {
        let with_prefix = format!("0x{}", parts[5]);
        parts[5] = with_prefix.replace("/thumbnail", &format!(":{blockchain_id}/thumbnail"));
    }
    parts.join(":")
}

fn row_to_item(r: &sqlx::postgres::PgRow) -> CatalogItem {
    let contract: String = r.get("contract_address");
    let item_id: String = r.get("item_id");
    let item_type: String = r.get("item_type");
    let rarity: String = r.get("rarity");
    let available: i64 = r.get::<String, _>("available").parse().unwrap_or(0);
    let store_minter: bool = r.get("search_is_store_minter");
    let v3_minter: bool = r.get("v3_minter");
    let open_item_trade_id: Option<String> = r.get("open_item_trade_id");
    let open_item_trade_price: Option<String> = r.get("open_item_trade_price");
    let priced_by_trade = available > 0 && open_item_trade_id.is_some() && v3_minter;
    let price: String = if available > 0 {
        if priced_by_trade {
            open_item_trade_price.clone().unwrap_or_else(|| "0".into())
        } else if store_minter {
            r.get("price")
        } else {
            "0".into()
        }
    } else {
        "0".into()
    };
    let description: Option<String> = r.get("description");
    let category: String = r.get("category");
    let body_shapes: Vec<String> = r.get("body_shapes");
    let (name, wire_category, data): (String, &'static str, ItemData) = if item_type
        == FRAGMENT_EMOTE_V1
    {
        (
            r.get("name"),
            "emote",
            ItemData::Emote {
                emote: serde_json::json!({
                    "description": description,
                    "category": category.to_lowercase(),
                    "bodyShapes": body_shapes,
                    "rarity": rarity,
                    "loop": r.get::<Option<bool>, _>("emote_loop").unwrap_or(false),
                    "hasGeometry": r.get::<Option<bool>, _>("emote_has_geometry").unwrap_or(false),
                    "hasSound": r.get::<Option<bool>, _>("emote_has_sound").unwrap_or(false),
                    "outcomeType": r.get::<Option<String>, _>("emote_outcome_type"),
                }),
            },
        )
    } else {
        (
            r.get("name"),
            "wearable",
            ItemData::Wearable {
                wearable: WearableData {
                    description,
                    category: Some(category),
                    body_shapes,
                    rarity: rarity.clone(),
                    is_smart: item_type == FRAGMENT_SMART_WEARABLE_V1,
                },
            },
        )
    };
    let (network, chain_id) = super::wire::network_and_chain(&r.get::<String, _>("network"));
    let listings: Option<i64> = r.get("listings_count");
    CatalogItem {
        id: format!("{contract}-{item_id}"),
        beneficiary: r.get("beneficiary"),
        item_id: item_id.clone(),
        name,
        thumbnail: fix_thumbnail(
            &r.get::<Option<String>, _>("image").unwrap_or_default(),
            &item_id,
        ),
        url: format!("/contracts/{contract}/items/{item_id}"),
        urn: r.get("urn"),
        category: wire_category,
        contract_address: contract,
        rarity,
        available,
        is_on_sale: (store_minter || (open_item_trade_id.is_some() && v3_minter)) && available > 0,
        trade_id: if priced_by_trade {
            open_item_trade_id
        } else {
            None
        },
        creator: r.get("creator"),
        data,
        network,
        chain_id,
        price,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        reviewed_at: r.get("reviewed_at"),
        first_listed_at: r.get("first_listed_at"),
        sold_at: r.get("sold_at"),
        min_price: r.get("min_price"),
        max_listing_price: r.get("max_listing_price"),
        min_listing_price: r.get("min_listing_price"),
        listings,
        owners: None,
        picks: None,
    }
}

pub async fn fetch_page(
    state: &MarketplaceState,
    f: &CatalogFilters,
    is_v2: bool,
) -> Result<(Vec<CatalogItem>, i64), ApiError> {
    let now = Utc::now().naive_utc();
    let since = super::trades::mirrored_since(&state.settings, now);
    let (sql, args) = build_catalog_query(f, is_v2, now, since, false);
    let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "catalog query failed");
            ApiError::bad_request("Couldn't fetch the catalog with the filters provided")
        })?;
    let total: i64 = match rows.first() {
        Some(r) => r.get("total"),
        None if clamp_skip(f.skip) > 0 => {
            let (sql, args) = build_catalog_query(f, is_v2, now, since, true);
            sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_one(&state.pool)
                .await
                .map(|r| r.get::<i64, _>("total"))
                .unwrap_or(0)
        }
        None => 0,
    };
    Ok((rows.iter().map(row_to_item).collect(), total))
}

async fn catalog_inner(
    state: SharedState,
    headers: HeaderMap,
    pairs: Vec<(String, String)>,
    is_v2: bool,
) -> Response {
    // Every row carries the item's minter price: the money rule.
    if let Err(envelope) = require_priced(&state).await {
        return envelope;
    }
    // The v2 legs (price, isOnSale, tradeId, the listing counts) are computed
    // from shop.trades: a partial mirror is not the order book.
    if is_v2 && let Err(envelope) = super::trades::require_ready(&state).await {
        return envelope;
    }
    let filters = parse_catalog_filters(&pairs);
    let route = if is_v2 { "/v2/catalog" } else { "/v1/catalog" };
    let picked_by = match optional_signer(&headers, "get", route).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let key = (is_v2, filters.clone());
    let page = match state
        .catalog_cache
        .page_or_fetch(key, || async {
            fetch_page(&state, &filters, is_v2).await.map(Arc::new)
        })
        .await
    {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let ids: Vec<String> = page.0.iter().map(|i| i.id.clone()).collect();
    let stats: Vec<PickStats> = match picked_by.as_deref() {
        Some(user) => match picks_stats(&state.pool, &ids, Some(user)).await {
            Ok(s) => s,
            Err(e) => return e.into_response(),
        },
        None => match state
            .catalog_cache
            .picks_or_fetch(ids.clone(), || async {
                picks_stats(&state.pool, &ids, None).await.map(Arc::new)
            })
            .await
        {
            Ok(s) => s.as_ref().clone(),
            Err(e) => return e.into_response(),
        },
    };
    let mut by_id: HashMap<String, PickStats> =
        stats.into_iter().map(|s| (s.item_id.clone(), s)).collect();
    let picks = page.0.iter().map(|i| by_id.remove(&i.id)).collect();
    Json(CatalogPage { page, picks }).into_response()
}

pub async fn get_catalog_v1(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    catalog_inner(state, headers, pairs, false).await
}

pub async fn get_catalog_v2(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    catalog_inner(state, headers, pairs, true).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn wearable_item() -> CatalogItem {
        CatalogItem {
            id: "0xc-1".into(),
            beneficiary: Some("0xbeef".into()),
            item_id: "1".into(),
            name: "hat".into(),
            thumbnail: "thumb".into(),
            url: "/contracts/0xc/items/1".into(),
            urn: "urn:x".into(),
            category: "wearable",
            contract_address: "0xc".into(),
            rarity: "epic".into(),
            available: 2,
            is_on_sale: true,
            trade_id: Some("t-1".into()),
            creator: "0xcr".into(),
            data: ItemData::Wearable {
                wearable: WearableData {
                    description: Some("d".into()),
                    category: Some("hat".into()),
                    body_shapes: vec!["BaseMale".into()],
                    rarity: "epic".into(),
                    is_smart: false,
                },
            },
            network: Network::Ethereum,
            chain_id: ChainId::EthereumMainnet,
            price: "10".into(),
            created_at: 1,
            updated_at: 2,
            reviewed_at: 3,
            first_listed_at: Some(99),
            sold_at: 4,
            min_price: Some("7".into()),
            max_listing_price: Some("20".into()),
            min_listing_price: Some("7".into()),
            listings: Some(3),
            owners: Some(5),
            picks: None,
        }
    }

    fn emote_item() -> CatalogItem {
        CatalogItem {
            id: "0xe-3".into(),
            beneficiary: None,
            item_id: "3".into(),
            name: "dance".into(),
            thumbnail: "et".into(),
            url: "/contracts/0xe/items/3".into(),
            urn: "urn:e".into(),
            category: "emote",
            contract_address: "0xe".into(),
            rarity: "rare".into(),
            available: 1,
            is_on_sale: false,
            trade_id: None,
            creator: "0xec".into(),
            data: ItemData::Emote {
                emote: json!({ "description": "edesc", "category": "dance", "bodyShapes": ["BaseFemale"], "rarity": "rare", "loop": true, "hasGeometry": false, "hasSound": true, "outcomeType": "win" }),
            },
            network: Network::Matic,
            chain_id: ChainId::MaticMainnet,
            price: "0".into(),
            created_at: 10,
            updated_at: 20,
            reviewed_at: 30,
            first_listed_at: None,
            sold_at: 40,
            min_price: None,
            max_listing_price: None,
            min_listing_price: None,
            listings: None,
            owners: None,
            picks: None,
        }
    }

    #[test]
    fn catalog_page_serializes_byte_identical_to_data_total() {
        let stats = PickStats {
            count: 8,
            item_id: "0xc-1".into(),
            picked_by_user: Some(true),
        };
        for picks in [
            vec![None, None],
            vec![Some(stats.clone()), None],
            vec![None, Some(stats.clone())],
        ] {
            let items = vec![wearable_item(), emote_item()];
            let mut expected = items.clone();
            for (item, pick) in expected.iter_mut().zip(&picks) {
                item.picks = pick.clone();
            }
            let expected = serde_json::to_string(&super::super::wire::DataTotal {
                data: expected,
                total: 7,
            })
            .unwrap();
            let got = serde_json::to_string(&CatalogPage {
                page: Arc::new((items, 7)),
                picks,
            })
            .unwrap();
            assert_eq!(got, expected);
        }
    }

    /// The pinned upstream `/v1/catalog?first=1` answer (tests/fixtures/upstream/catalog-first1.json
    /// of the old crate): every key, in kind.
    #[test]
    fn a_catalog_item_carries_every_upstream_key() {
        let v = serde_json::to_value(wearable_item()).unwrap();
        for key in [
            "id",
            "beneficiary",
            "itemId",
            "name",
            "thumbnail",
            "url",
            "urn",
            "category",
            "contractAddress",
            "rarity",
            "available",
            "isOnSale",
            "creator",
            "data",
            "network",
            "chainId",
            "price",
            "createdAt",
            "updatedAt",
            "reviewedAt",
            "firstListedAt",
            "soldAt",
            "minPrice",
            "maxListingPrice",
            "minListingPrice",
            "listings",
            "owners",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["data"]["wearable"]["isSmart"], false);
        assert_eq!(v["network"], "ETHEREUM");
        assert_eq!(v["chainId"], 1);
        assert_eq!(v["tradeId"], "t-1");
        let e = serde_json::to_value(emote_item()).unwrap();
        assert!(
            e.get("tradeId").is_none() && e.get("minPrice").is_none(),
            "absent optionals are omitted"
        );
        assert_eq!(e["maxListingPrice"], serde_json::Value::Null);
        assert_eq!(e["data"]["emote"]["loop"], true);
    }

    #[test]
    fn filters_parse_with_upstream_defaults() {
        let f = parse_catalog_filters(&pairs(&[
            ("first", "34"),
            ("id", "0x7dd2c14796d6a411439f3ed465071fb1626084a8-0"),
            ("id", "0x0bf152a83a6fc55066c2b664b164ca2916ad38f5-2"),
        ]));
        assert_eq!(f.first, Some(34));
        assert_eq!(f.skip, Some(0));
        assert_eq!(
            f.sort_by,
            Some(CatalogSortBy::Cheapest),
            "cheapest is the default sort"
        );
        assert_eq!(f.ids.len(), 2);
        assert_eq!(f.include_social_emotes, Some(true));
        let f = parse_catalog_filters(&pairs(&[
            ("isOnSale", "false"),
            ("rarity", "epic"),
            ("rarity", "bogus"),
            ("wearableGender", "male"),
            ("includeSocialEmotes", "false"),
            ("network", "MATIC"),
        ]));
        assert_eq!(f.is_on_sale, Some(false));
        assert_eq!(f.rarities, vec!["epic"]);
        assert_eq!(f.network, Some(Network::Matic));
        assert_eq!(f.include_social_emotes, Some(false));
        let f = parse_catalog_filters(&pairs(&[]));
        assert_eq!((f.first, f.skip, f.is_on_sale), (Some(20), Some(0), None));
    }

    #[test]
    fn the_v2_query_joins_the_open_trades_and_the_v1_query_does_not() {
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .naive_utc();
        let f = CatalogFilters {
            first: Some(24),
            is_on_sale: Some(true),
            min_price: Some("1".into()),
            ..Default::default()
        };
        let (v2, _) = build_catalog_query(&f, true, now, None, false);
        assert!(v2.contains("WITH unified_trades AS"), "{v2}");
        assert!(
            v2.contains("NOT EXISTS (SELECT 1 FROM shop.trade_cancellations"),
            "{v2}"
        );
        assert!(
            v2.contains("t.expires_at > $1 AND t.effective_since <= $1"),
            "the clock is a bound parameter: {v2}"
        );
        assert!(
            v2.contains(ITEM_ID_SHIFT),
            "nft-order legs map token -> item by the layout: {v2}"
        );
        assert!(v2.contains("items.search_is_collection_approved = true"));
        assert!(v2.contains("LIMIT $"), "{v2}");
        assert!(
            !v2.contains("now()"),
            "no moving clock in the predicate: {v2}"
        );
        assert_eq!(
            v2.matches(&"NULLIF(".to_string()).count(),
            4,
            "the uint256-max sentinel of the LEAST arms never reaches the wire: {v2}"
        );
        assert!(
            v2.contains(&format!(
                "'{MAX_NUMERIC_NUMBER}'::numeric) AS min_price_num"
            )),
            "{v2}"
        );
        let (v1, _) = build_catalog_query(&f, false, now, None, false);
        assert!(!v1.contains("unified_trades"), "{v1}");
        assert!(v1.contains("NULL::text AS open_item_trade_id"), "{v1}");
        let (count, _) = build_catalog_query(&f, true, now, None, true);
        assert!(
            count.starts_with("WITH unified_trades")
                && count.contains("SELECT COUNT(*)::int8 AS total")
                && !count.contains("LIMIT"),
            "{count}"
        );
        let ids = CatalogFilters {
            ids: vec!["0xc-1".into()],
            search: Some("hat pirate".into()),
            ..Default::default()
        };
        let (sql, _) = build_catalog_query(&ids, true, now, None, false);
        assert!(
            sql.contains("(items.contract_address || '-' || items.item_id::text) = ANY("),
            "{sql}"
        );
        assert!(
            sql.contains("array_position("),
            "a search with ids keeps the ids' order: {sql}"
        );
        assert_eq!(
            sql.matches("ILIKE").count(),
            4,
            "one name-or-collection arm per word: {sql}"
        );
        let sorted = CatalogFilters {
            sort_by: Some(CatalogSortBy::Suggested),
            ..Default::default()
        };
        let (sql, _) = build_catalog_query(&sorted, false, now, None, false);
        assert!(
            sql.contains("ORDER BY items.first_listed_at DESC NULLS LAST"),
            "suggested falls back to newest: {sql}"
        );
    }

    #[test]
    fn thumbnails_and_urns_normalize_as_before() {
        assert_eq!(
            fix_thumbnail(
                "https://peer.decentraland.org/lambdas/collections/contents/urn:decentraland:polygon:collections-v2:c5aaaa47d4ed16932d9970352afbd5e0df5a4925/thumbnail",
                "0"
            ),
            "https://peer.decentraland.org/lambdas/collections/contents/urn:decentraland:matic:collections-v2:0xc5aaaa47d4ed16932d9970352afbd5e0df5a4925:0/thumbnail"
        );
        assert_eq!(fix_thumbnail("", "1"), "");
        assert_eq!(
            fix_thumbnail(
                "https://x/urn:decentraland:mainnet:collections-v1:atari_launch:hat/thumbnail",
                "1"
            ),
            "https://x/urn:decentraland:ethereum:collections-v1:atari_launch:hat/thumbnail"
        );
        let out = expand_urn_network_forms(&[
            "urn:decentraland:ethereum:collections-v1:0xabc:0".to_string()
        ]);
        assert!(out.contains(&"urn:decentraland:mainnet:collections-v1:0xabc:0".to_string()));
        assert_eq!(escape_like("50%_off\\"), "50\\%\\_off\\\\");
    }
}
