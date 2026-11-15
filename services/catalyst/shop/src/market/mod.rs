//! Marketplace and explorer-facing lambdas without a squid database: the
//! off-chain trades mirror (`trades`), the chain-derived readers the routes
//! need (`graph`: the collections and marketplace subgraphs; `contracts`: the
//! catalyst registry over JSON-RPC) and the routes over them (`profiles`,
//! `user_items`). Every upstream call goes through the process's one HTTP
//! client, the SSRF guard, the retry ladder and the body cap in `crate::http`.
//!
//! The old handlers preferred the squid owner mirror (`mv.*`,
//! `squid_marketplace.*`) and had no subgraph path for owned items; in v2 the
//! subgraph readers ARE the path, cached in memory. The old routes answered
//! empty with `squid_pool: None`; here a route whose reader is not configured
//! answers the unavailable envelope (the stage-5 rule below), and a route
//! without a network filter needs both collections subgraphs
//! (`readers::require_both_collections`), since it answers for the whole
//! marketplace.
//!
//! Stage 5 (the market half of the data plane): the old
//! `catalyrst-market` read 169 places from the squid indexer, and v2 has no
//! squid database, so every marketplace route names one of four sources:
//! a **reader** (a subgraph call with a bounded page and a TTL), a **mirror**
//! (`shop.collections`, `shop.items`, `shop.trades`,
//! filled by a job with a cursor and a pass marker in `mirror_state`), the
//! **land_authz index**, or **deferred** (an nft indexer, the marketplace
//! contract's trade events). A route whose source is unset or whose mirror
//! has no complete pass answers [`unavailable`], the documented 503
//! envelope, never an empty page presented as the truth. Local writes (the
//! signed trades, lists and picks, cancellations, operator moderation, and
//! the economy, signatures and presence writes) sit behind
//! `DATA_WRITES_ENABLED` ([`DataWrites`], the social switch's twin).
//!
//! The market half as landed (2026-09-25): [`market_router`] serves the 18
//! market route groups and is nested under `/marketplace-api` only, because
//! `/v1/items` and `/v1/collections` collide with the builder's root
//! routes. Mirrors: `collections` and `items` (both whole walks by id every
//! tick, `mirrors::{collections,items}`: the subgraph does not bump
//! `Item.updatedAt` on mints) behind
//! `MARKETPLACE_MIRRORS_ENABLED`, gating the catalog, the shop feeds,
//! `/v1/items`, `/v1/contracts`, `/v1/collections` and `/v1/prices` on a
//! complete pass per configured network. Readers (`readers.rs`: first ≤ 100,
//! skip ≤ 5000, id-keyset counts capped at 5000, per-route TTL buckets):
//! `/v1/nfts`, the legacy legs of `/v1/orders` and `/v1/bids`, `/v1/sales`,
//! `/v1/owners`, `/v1/accounts`, `/v1/rankings`, `/v1/volume`, the sales
//! signal of the trending and creators rails, and the `/v1/users` routes
//! over `graph.rs`. Local: `POST /v1/trades` (`listings.rs`, the old EIP-712
//! policy, `ownerOf` on `TRADE_RPC_URLS`), lists and picks (`lists.rs`), the
//! operator moderation (`admin.rs`, `MARKET_ADMIN_TOKEN`), a wallet's
//! `/v1/activity` over the readers and the local trades (`activity.rs`),
//! the `/v1/nfts` rental legs over the local rentals listings (`rentals.rs`),
//! the market federation's signed-action log and its feeds
//! (`federation.rs`: `/v1/federation/*`, `/federation/market/*`).

pub use crate::chain_index::source::{Source, unavailable};
pub use crate::chain_index::{graph, land_graph};

pub mod accounts;
pub mod activity;
pub mod admin;
pub mod base_wearables;
pub mod bids;
pub mod catalog;
pub mod chain_walk;
pub mod collections;
pub mod contracts;
pub mod definitions;
pub mod features;
pub mod federation;
pub mod items;
pub mod lambdas_catalog;
pub mod lambdas_crypto;
pub mod lambdas_explorer;
pub mod lambdas_http;
pub mod lambdas_land;
pub mod listings;
pub mod lists;
pub mod mana_transfers;
pub mod mirrors;
pub mod neighbours;
pub mod nft_index;
pub mod nft_reads;
pub mod nfts;
pub mod notice_cursor;
pub mod orders;
pub mod owners;
pub mod prices;
pub mod profiles;
pub mod rankings;
pub mod readers;
pub mod rentals;
pub mod royalty_notices;
pub mod sale_notices;
pub mod sales;
pub mod sales_index;
pub mod search;
pub mod shop;
pub mod signed;
pub mod third_party;
pub mod tips;
pub mod trade_events;
pub mod trades;
pub mod transfer_notices;
pub mod user_items;
pub mod users;
pub mod wire;

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod lambdas_tests;
#[cfg(test)]
mod market_tests;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{any, get, post};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower_http::timeout::TimeoutLayer;

use crate::chain_index::{ChainIndex, IndexSettings};
use crate::jobs::{Jobs, PeriodicCfg};
use crate::shared::cache::TtlMap;
use crate::shared::{cors, nul_guard};

/// The explicit upstream endpoints this module reads. Each is an `Option`
/// because every one is per-deployment infrastructure with deliberately no
/// default (the historical defaults were production Decentraland services);
/// unset means the dependent route answers 503 ([`unavailable`], naming the
/// source and its variable), never an empty page, and the dependent job is
/// not attached. `main.rs` fills it from `Config`.
#[derive(Clone, Debug, Default)]
pub struct Upstreams {
    /// `ETH_COLLECTIONS_SUBGRAPH_URL`: the L1 collections subgraph (wearables v1/v2 on Ethereum).
    pub eth_collections_subgraph_url: Option<String>,
    /// `MATIC_COLLECTIONS_SUBGRAPH_URL`: the L2 collections subgraph (wearables/emotes v2 on Polygon).
    pub matic_collections_subgraph_url: Option<String>,
    /// `MARKETPLACE_SUBGRAPH_URL`: the L1 marketplace subgraph (ENS names).
    pub marketplace_subgraph_url: Option<String>,
    /// `LAND_SUBGRAPH_URL`: the land-manager subgraph (parcel operators and
    /// update-operator lookups before the land-authz index is published).
    pub land_subgraph_url: Option<String>,
    /// `TRADES_SYNC_UPSTREAM_URL`: the marketplace API `/v1/trades` list to mirror.
    pub trades_sync_upstream_url: Option<String>,
    /// `TRADES_SYNC_INTERVAL_SECS` (900): pause between two mirror passes.
    pub trades_sync_interval_secs: u64,
    /// `TRADES_SYNC_PAGES_PER_RUN` (50): pages of 100 new trades (detail
    /// fetches) one tick pays for, and the page requests per tick when the
    /// upstream honours paging; 0 = unbounded.
    pub trades_sync_pages_per_run: usize,
    /// `TRADES_SERVE_PARTIAL` (development only; every posture pins it off).
    pub trades_serve_partial: bool,
    /// `PROFILE_CDN_BASE_URL`: when set, avatar snapshots point at
    /// `<base>/entities/<id>/{face,body}.png` as the old lambdas did;
    /// unset, at this node's `profile-images/entities/<id>/{face,body}.png`.
    pub profile_cdn_base_url: Option<String>,
    /// `NFT_WORKER_BASE_URL`: the NFT ownership indexer the old lambdas asked
    /// for an owner's third-party holdings; unset, they are read from the chain.
    pub nft_worker_base_url: Option<String>,
    /// `EXPLORER_ESCROW_GRANTS_ENABLED`: the explorer lists escrow grants.
    pub explorer_escrow_grants_enabled: bool,
    /// `LAMBDAS_CATALOG_EXTENSIONS_ENABLED`: the wearables catalogue's extended filters.
    pub lambdas_catalog_extensions_enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub version: String,
    pub commit_hash: String,
    pub eth_network: String,
    pub content_public_url: String,
    pub profile_images_public_url: String,
    pub profile_cdn_base_url: Option<String>,
    pub eth_collections_subgraph_url: Option<String>,
    pub matic_collections_subgraph_url: Option<String>,
    pub marketplace_subgraph_url: Option<String>,
    pub land_subgraph_url: Option<String>,
    /// `Config::eth_rpc_url()`: the contracts registry is read over the same
    /// TLS JSON-RPC endpoint the EIP-1654 validator uses.
    pub eth_rpc_url: Option<String>,
    /// `RPC_ENDPOINT_POLYGON`: the POI list, and third-party ownership on
    /// the L2 of `ETH_NETWORK` (its `TRADE_RPC_URLS` entry otherwise).
    pub polygon_rpc_url: Option<String>,
    /// `THIRD_PARTY_REGISTRY_L2_SUBGRAPH_URL`: the approved third-party providers.
    pub third_party_registry_subgraph_url: Option<String>,
    /// `NFT_WORKER_BASE_URL`: the owner's third-party holdings (`third_party`).
    pub nft_worker_base_url: Option<String>,
    /// `EXPLORER_ESCROW_GRANTS_ENABLED`: the explorer backpack's on-chain
    /// items include this node's active escrow grants (`shop.usage_grants`);
    /// off under the mirror posture, where lamb2 lists chain-owned tokens only.
    pub explorer_escrow_grants: bool,
    /// `LAMBDAS_CATALOG_EXTENSIONS_ENABLED`: `/lambdas/collections/wearables`
    /// takes `collectionType`, `category`, `rarity`, `name`, `orderBy` and
    /// `direction`; off under the mirror posture, where lamb2 ignores them.
    pub lambdas_catalog_extensions: bool,
    /// `LAND_IMAGE_BASE_URL`: the map service `/lambdas/users/{a}/lands` points its images at.
    pub land_image_base_url: String,
    pub trades_sync_upstream_url: Option<String>,
    pub trades_sync_interval: Duration,
    pub trades_sync_pages_per_run: usize,
    /// `TRADES_SERVE_PARTIAL` (development only): the trade reads skip the
    /// pass gate ([`trades::ready`]) while an upstream is configured.
    pub trades_serve_partial: bool,
    pub request_timeout: Option<Duration>,
    /// `DATA_WRITES_ENABLED`: whether this node accepts the data plane's
    /// local writes (signed trades, lists and picks, cancellations,
    /// moderation; the economy, signatures and presence writes read it too).
    pub data_writes_enabled: bool,
    /// The stage-5 market surface's own knobs (`MarketSettings`); `main.rs`
    /// fills them with [`Settings::with_market`], the defaults keep the
    /// mirrors off and the admin routes disabled.
    pub market: MarketSettings,
}

/// The market half of the data plane's configuration, every value with a
/// default that leaves the feature off: the two subgraph mirrors
/// (`MARKETPLACE_MIRRORS_ENABLED`, `MARKETPLACE_MIRROR_INTERVAL_SECS`,
/// `MARKETPLACE_MIRROR_PAGES_PER_TICK`), the per-chain JSON-RPC endpoints
/// `POST /v1/trades` checks ownership on (`TRADE_RPC_URLS`, `137=https://...,1=https://...`),
/// the operator bearer of `/v1/admin/*` (`MARKET_ADMIN_TOKEN`, or
/// `MARKET_ADMIN_TOKEN_FILE`) and the catalog page cache
/// (`MARKETPLACE_CATALOG_CACHE_TTL_SECS`, 0 disables). The credit-priced
/// `/v3/catalog` feeds take their MANA/USD rate from `economy.price_snapshots`
/// alone (`items::mana_usd_rate`); the `MANA_USD_FALLBACK_RATE` the slice
/// first shipped is gone, since a price from a constant is a wrong amount.
#[derive(Clone)]
pub struct MarketSettings {
    pub mirrors_enabled: bool,
    pub mirror_interval: Duration,
    pub mirror_pages_per_tick: usize,
    pub trade_rpc_urls: listings::RpcEndpoints,
    pub admin_token: Option<String>,
    pub catalog_cache_ttl: Duration,
    /// `MARKETPLACE_BASE_URL`: the site links the trade notification events point at.
    pub marketplace_base_url: String,
    /// The chain indexes (`nft`, `trade_events`), the neighbours job and the
    /// coupon state reader (`chain_walk::IndexSettings`; every default off).
    pub indexes: chain_walk::IndexSettings,
    /// The tip forward's thresholds and pending cap.
    pub tips: tips::TipSettings,
    /// `HTTP_TRUSTED_PROXY_CIDRS`: peers whose `X-Real-IP` names the client
    /// (the tip forward's and the trade post's per-address limits).
    pub trusted_proxy_cidrs: Vec<ipnet::IpNet>,
}

/// No derived `Debug`: `admin_token` is a bearer. Prints whether one is set,
/// the `DirectSigner`/`Rpc` pattern.
impl std::fmt::Debug for MarketSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarketSettings")
            .field("mirrors_enabled", &self.mirrors_enabled)
            .field("mirror_interval", &self.mirror_interval)
            .field("mirror_pages_per_tick", &self.mirror_pages_per_tick)
            .field(
                "trade_rpc_chains",
                &self
                    .trade_rpc_urls
                    .iter()
                    .map(|(chain, _)| chain)
                    .collect::<Vec<_>>(),
            )
            .field("admin_token", &self.admin_token.is_some())
            .field("catalog_cache_ttl", &self.catalog_cache_ttl)
            .field("marketplace_base_url", &self.marketplace_base_url)
            .field("indexes", &self.indexes)
            .field("tips", &self.tips)
            .finish_non_exhaustive()
    }
}

impl Default for MarketSettings {
    fn default() -> Self {
        Self {
            mirrors_enabled: false,
            mirror_interval: Duration::from_secs(mirrors::DEFAULT_INTERVAL_SECS),
            mirror_pages_per_tick: mirrors::DEFAULT_PAGES_PER_TICK,
            trade_rpc_urls: listings::RpcEndpoints::default(),
            admin_token: None,
            catalog_cache_ttl: Duration::from_secs(30),
            marketplace_base_url: "https://decentraland.org/marketplace".to_string(),
            indexes: chain_walk::IndexSettings::default(),
            tips: tips::TipSettings::default(),
            trusted_proxy_cidrs: Vec::new(),
        }
    }
}

impl MarketSettings {
    /// The values as the environment spells them; every endpoint in
    /// `TRADE_RPC_URLS` must pass [`check_upstream`] (a private RPC is a
    /// deployment fault, refused at startup and never echoed).
    #[allow(
        clippy::too_many_arguments,
        reason = "one argument per environment variable it validates"
    )]
    pub fn parse(
        mirrors_enabled: bool,
        mirror_interval_secs: u64,
        mirror_pages_per_tick: usize,
        trade_rpc_urls: Option<&str>,
        admin_token: Option<String>,
        catalog_cache_ttl_secs: u64,
        marketplace_base_url: &str,
        self_hosts: &[String],
    ) -> anyhow::Result<Self> {
        let trade_rpc_urls = listings::RpcEndpoints::from_env(trade_rpc_urls);
        for (chain, url) in trade_rpc_urls.iter() {
            check_upstream("TRADE_RPC_URLS", url, self_hosts)
                .with_context(|| format!("TRADE_RPC_URLS entry for chain {chain}"))?;
            if !url.starts_with("https://") {
                anyhow::bail!(
                    "TRADE_RPC_URLS entry for chain {chain} is not https: ownership checks over plaintext are refused"
                );
            }
        }
        Ok(Self {
            mirrors_enabled,
            mirror_interval: Duration::from_secs(mirror_interval_secs.max(60)),
            mirror_pages_per_tick,
            trade_rpc_urls,
            admin_token: admin_token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            catalog_cache_ttl: Duration::from_secs(catalog_cache_ttl_secs),
            marketplace_base_url: marketplace_base_url.trim_end_matches('/').to_string(),
            indexes: chain_walk::IndexSettings::default(),
            tips: tips::TipSettings::default(),
            trusted_proxy_cidrs: Vec::new(),
        })
    }

    /// The chain indexes' knobs (`main.rs` fills them from `Config`).
    pub fn with_indexes(mut self, indexes: chain_walk::IndexSettings) -> Self {
        self.indexes = indexes;
        self
    }

    /// The tip forward's knobs (`main.rs` fills them from `Config`).
    pub fn with_tips(mut self, tips: tips::TipSettings) -> Self {
        self.tips = tips;
        self
    }

    pub fn with_trusted_proxies(mut self, cidrs: Vec<ipnet::IpNet>) -> Self {
        self.trusted_proxy_cidrs = cidrs;
        self
    }
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.trim_end_matches('/').to_string())
}

impl Settings {
    pub fn from_config(
        config: &crate::config::Config,
        upstreams: &Upstreams,
    ) -> anyhow::Result<Self> {
        let content = crate::content::Settings::from_config(config);
        let hosts = crate::content::own_hosts(config);
        let checked = |var: &str, value: &Option<String>| -> anyhow::Result<Option<String>> {
            match non_empty(value) {
                Some(url) => {
                    check_upstream(var, &url, &hosts)?;
                    Ok(Some(url))
                }
                None => Ok(None),
            }
        };
        Ok(Self {
            version: content.version.clone(),
            commit_hash: content
                .commit_hash
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            eth_network: content.eth_network.clone(),
            content_public_url: content.content_public_url.clone(),
            profile_images_public_url: content.profile_images_public_url.clone(),
            profile_cdn_base_url: non_empty(&upstreams.profile_cdn_base_url),
            eth_collections_subgraph_url: checked(
                "ETH_COLLECTIONS_SUBGRAPH_URL",
                &upstreams.eth_collections_subgraph_url,
            )?,
            matic_collections_subgraph_url: checked(
                "MATIC_COLLECTIONS_SUBGRAPH_URL",
                &upstreams.matic_collections_subgraph_url,
            )?,
            marketplace_subgraph_url: checked(
                "MARKETPLACE_SUBGRAPH_URL",
                &upstreams.marketplace_subgraph_url,
            )?,
            land_subgraph_url: checked("LAND_SUBGRAPH_URL", &upstreams.land_subgraph_url)?,
            eth_rpc_url: config.content.eth_rpc_url().context("ETH_RPC_URL")?,
            polygon_rpc_url: non_empty(&config.economy.rpc_endpoint_polygon),
            third_party_registry_subgraph_url: checked(
                "THIRD_PARTY_REGISTRY_L2_SUBGRAPH_URL",
                &config.validator.third_party_registry_l2_subgraph_url,
            )?
            .map(|url| {
                if url.starts_with("https://") {
                    Ok(url)
                } else {
                    anyhow::bail!(
                        "THIRD_PARTY_REGISTRY_L2_SUBGRAPH_URL is not https: the approved \
                         third-party providers are refused over plaintext"
                    )
                }
            })
            .transpose()?,
            nft_worker_base_url: checked("NFT_WORKER_BASE_URL", &upstreams.nft_worker_base_url)?,
            explorer_escrow_grants: upstreams.explorer_escrow_grants_enabled,
            lambdas_catalog_extensions: upstreams.lambdas_catalog_extensions_enabled,
            land_image_base_url: config.map_urls().land_image_base,
            trades_sync_upstream_url: checked(
                "TRADES_SYNC_UPSTREAM_URL",
                &upstreams.trades_sync_upstream_url,
            )?,
            trades_sync_interval: Duration::from_secs(upstreams.trades_sync_interval_secs.max(60)),
            trades_sync_pages_per_run: upstreams.trades_sync_pages_per_run,
            trades_serve_partial: upstreams.trades_serve_partial,
            request_timeout: content.request_timeout,
            data_writes_enabled: config.writes.data_writes_enabled,
            market: MarketSettings::default(),
        })
    }

    pub fn with_market(mut self, market: MarketSettings) -> Self {
        self.market = market;
        self
    }

    /// The chain index's share of the settings.
    pub fn index_settings(&self) -> IndexSettings {
        IndexSettings {
            eth_collections_subgraph_url: self.eth_collections_subgraph_url.clone(),
            matic_collections_subgraph_url: self.matic_collections_subgraph_url.clone(),
            marketplace_subgraph_url: self.marketplace_subgraph_url.clone(),
            land_subgraph_url: self.land_subgraph_url.clone(),
            eth_network: self.eth_network.clone(),
        }
    }

    /// The collections subgraphs configured, in the order the mirrors walk them.
    pub fn collection_networks(&self) -> Vec<graph::Network> {
        let mut out = Vec::new();
        if self.eth_collections_subgraph_url.is_some() {
            out.push(graph::Network::Ethereum);
        }
        if self.matic_collections_subgraph_url.is_some() {
            out.push(graph::Network::Matic);
        }
        out
    }

    pub fn for_tests() -> Self {
        Self {
            version: "test".to_string(),
            commit_hash: "unknown".to_string(),
            eth_network: "mainnet".to_string(),
            content_public_url: "http://localhost/content/".to_string(),
            profile_images_public_url: "http://localhost/profile-images/".to_string(),
            profile_cdn_base_url: None,
            eth_collections_subgraph_url: None,
            matic_collections_subgraph_url: None,
            marketplace_subgraph_url: None,
            land_subgraph_url: None,
            eth_rpc_url: None,
            polygon_rpc_url: None,
            third_party_registry_subgraph_url: None,
            nft_worker_base_url: None,
            explorer_escrow_grants: true,
            lambdas_catalog_extensions: true,
            land_image_base_url: "https://api.decentraland.org".to_string(),
            trades_sync_upstream_url: None,
            trades_sync_interval: Duration::from_secs(900),
            trades_sync_pages_per_run: trades::MAX_PAGES_PER_RUN,
            trades_serve_partial: false,
            request_timeout: None,
            data_writes_enabled: false,
            market: MarketSettings::default(),
        }
    }
}

pub struct MarketplaceState {
    pub pool: PgPool,
    pub http: crate::http::Outbound,
    pub settings: Settings,
    /// The subgraph readers (`chain_index`), shared with the other domains.
    pub index: Arc<ChainIndex>,
    pub(crate) profile_cache: TtlMap<String, Value>,
    pub(crate) by_owner_cache: TtlMap<(String, String, bool), Value>,
    pub(crate) contracts: contracts::Caches,
    /// The readers' memo, one map per freshness budget (`readers::Ttl`).
    pub readers: readers::Caches,
    /// `mirror_state.last_complete_pass_at` per mirror, cached 10 s.
    pub(crate) mirror_ready: TtlMap<mirrors::Mirror, Option<chrono::NaiveDateTime>>,
    /// `trades_mirror.completed_at` for the configured upstream, cached 10 s
    /// (`trades::require_ready`).
    pub(crate) trades_ready: TtlMap<(), Option<chrono::NaiveDateTime>>,
    /// The upstream trades list of the pass in progress, held between ticks
    /// so a pass that spans several ticks downloads it once (`trades::PassList`).
    pub(crate) trades_list: std::sync::Mutex<Option<Arc<trades::PassList>>>,
    /// Catalog pages and totals (`MARKETPLACE_CATALOG_CACHE_TTL_SECS`).
    pub(crate) catalog_cache: catalog::Cache,
    /// The lambdas catalogue's memo (`lambdas_catalog`).
    pub(crate) lambdas: lambdas_catalog::Caches,
    /// The third-party readers' memo (`third_party`).
    pub(crate) third_party: third_party::Caches,
    /// The explorer backpack, per owner and filter set, 30 s.
    pub(crate) explorer_cache: TtlMap<lambdas_explorer::ExplorerKey, Value>,
    /// The ONE EIP-1654 validator (`/lambdas/crypto/validate-signature`).
    pub(crate) eip1654: Option<Arc<dyn crate::auth::eip1654::Eip1654Validator>>,
    /// `READ_THROUGH_ENABLED`: a profile this node does not hold is asked of the sync sources.
    pub(crate) read_through: Option<Arc<crate::content::read_through::ReadThrough>>,
    /// The store's moderation projection (`content::Denylist`): every read of
    /// active entities here leaves the listed ones out, as lamb2 reading the
    /// content server's filtered `/entities/active` does.
    pub(crate) denylist: Arc<crate::content::Denylist>,
    /// `/profile-images`, attached once the social states exist: the faces a
    /// response points at are warmed there.
    pub(crate) profile_images: std::sync::OnceLock<crate::social::profile_images::SharedState>,
    /// decentraland.org's `dapps` rollout flags as this node enforces them (`features`); every flag off until read.
    pub(crate) dapps_flags: Arc<features::DappsFlags>,
    /// Where `dapps_flags` is read from (`SHOP_FEATURE_FLAGS_*`).
    pub(crate) flags_source: features::Source,
    /// The chain index's loopback switch: a test that flips it admits a
    /// loopback fake upstream for the shop's calls and the subgraph readers.
    #[cfg(test)]
    pub(crate) test_allow_loopback: Arc<std::sync::atomic::AtomicBool>,
}

pub type SharedState = Arc<MarketplaceState>;

impl MarketplaceState {
    pub fn new(pool: PgPool, http: crate::http::Outbound, settings: Settings) -> Self {
        let index = Arc::new(ChainIndex::new(http.clone(), settings.index_settings()));
        Self {
            pool,
            http,
            #[cfg(test)]
            test_allow_loopback: index.loopback_switch_for_tests(),
            index,
            profile_cache: TtlMap::bounded(
                profiles::PROFILE_CACHE_TTL,
                profiles::PROFILE_CACHE_MAX_ENTRIES,
            ),
            by_owner_cache: TtlMap::bounded(Duration::from_secs(30), 50_000),
            contracts: contracts::Caches::default(),
            readers: readers::Caches::default(),
            mirror_ready: TtlMap::bounded(mirrors::READY_TTL, 8),
            trades_ready: TtlMap::bounded(mirrors::READY_TTL, 1),
            trades_list: std::sync::Mutex::new(None),
            catalog_cache: catalog::Cache::new(settings.market.catalog_cache_ttl),
            lambdas: lambdas_catalog::Caches::default(),
            third_party: third_party::Caches::default(),
            explorer_cache: lambdas_explorer::explorer_cache(),
            eip1654: None,
            read_through: None,
            denylist: Arc::default(),
            profile_images: std::sync::OnceLock::new(),
            dapps_flags: Arc::new(features::DappsFlags::new()),
            flags_source: features::Source::default(),
            settings,
        }
    }

    /// Attaches the ONE EIP-1654 validator (contract-wallet links).
    pub fn with_eip1654(
        mut self,
        eip1654: Option<Arc<dyn crate::auth::eip1654::Eip1654Validator>>,
    ) -> Self {
        self.eip1654 = eip1654;
        self
    }

    /// Attaches the store's denylist (`Storage::denylist`), which the active
    /// entity reads honour; without it nothing is hidden.
    pub fn with_denylist(mut self, denylist: Arc<crate::content::Denylist>) -> Self {
        self.denylist = denylist;
        self
    }

    /// Attaches the content read-through the profile reads fill their misses with.
    pub fn with_read_through(
        mut self,
        read_through: Option<Arc<crate::content::read_through::ReadThrough>>,
    ) -> Self {
        self.read_through = read_through;
        self
    }

    /// Attaches this node's `/profile-images`, whose faces the profile
    /// responses warm; `false` when one is already attached.
    pub fn attach_profile_images(
        &self,
        images: crate::social::profile_images::SharedState,
    ) -> bool {
        self.profile_images.set(images).is_ok()
    }

    /// Whether a face the profile never uploaded is served: by the CDN, or by
    /// this node's `/profile-images` when it has an origin or a renderer.
    pub(crate) fn derives_faces(&self) -> bool {
        self.settings.profile_cdn_base_url.is_some()
            || self
                .profile_images
                .get()
                .is_some_and(|images| images.origin.is_some() || images.render_queue.is_some())
    }

    /// Warms the faces of these profile entities when they point at this
    /// node's `/profile-images` (no `PROFILE_CDN_BASE_URL`).
    pub(crate) fn warm_faces(&self, entities: impl IntoIterator<Item = String>) {
        if self.settings.profile_cdn_base_url.is_some() {
            return;
        }
        if let Some(images) = self.profile_images.get() {
            images.warm(entities);
        }
    }

    /// The SSRF guard every upstream call passes: a publicly routable http(s)
    /// URL. Tests may admit a loopback fake upstream.
    pub(crate) fn upstream_allowed(&self, url: &str) -> bool {
        self.index.upstream_allowed(url)
    }

    /// Attaches the trades mirror when `TRADES_SYNC_UPSTREAM_URL` is set
    /// (one periodic job with a 30 s first-fire jitter and a 1 h error cap)
    /// and, with `MARKETPLACE_MIRRORS_ENABLED`, the `collections-mirror` and
    /// `items-mirror` jobs over every configured collections subgraph
    /// (`mirrors`): one pass per network per tick, the cursor persisted per
    /// page, the pass marker the mirror-served routes require.
    pub fn spawn_index_jobs(self: &Arc<Self>, jobs: &Jobs) {
        match self.settings.trades_sync_upstream_url.clone() {
            Some(upstream) => {
                let state = self.clone();
                jobs.spawn_periodic_leased(
                    "trades-mirror",
                    PeriodicCfg::every(self.settings.trades_sync_interval)
                        .with_jitter(Duration::from_secs(30))
                        .with_error_backoff(Duration::from_secs(3600)),
                    move |ctx| {
                        let (state, upstream) = (state.clone(), upstream.clone());
                        async move { trades::run_pass(&state, &upstream, &ctx).await }
                    },
                );
            }
            None => tracing::info!(
                "TRADES_SYNC_UPSTREAM_URL is unset: the off-chain trades mirror is off; \
                 shop.trades stays at its last mirrored state"
            ),
        }
        mirrors::spawn_jobs(self, jobs);
        spawn_index_jobs(self, jobs);
        neighbours::spawn_job(self, jobs);
    }

    pub fn spawn_api_jobs(self: &Arc<Self>, jobs: &Jobs) {
        features::spawn_job(self, jobs);
        crate::coupons::spawn_job(self, jobs);
        federation::spawn_job(self, jobs);
        tips::spawn_job(self, jobs);
        sale_notices::spawn(self, jobs);
        royalty_notices::spawn(
            self.pool.clone(),
            self.settings.market.marketplace_base_url.clone(),
            self.settings.market.indexes.nft_enabled,
            jobs,
        );
        transfer_notices::spawn(self, jobs);
        crate::rentals::notices::spawn(self, jobs);
    }

    /// The JSON-RPC endpoint the chain indexes and the coupon reader use on
    /// `net`: the `TRADE_RPC_URLS` entry of its chain, and for Ethereum
    /// `ETH_RPC_URL` when that list has none.
    pub fn chain_rpc_url(&self, net: chain_walk::ChainNet) -> Option<String> {
        self.rpc_url_for(net, net.chain_id())
    }

    /// The endpoint the settlement venues of `chain_id` (1, 137, or the
    /// testnet book's 80002) are read over. Their addresses are mainnet's, so
    /// off mainnet `ETH_RPC_URL`, which serves `ETH_NETWORK`, does not stand
    /// in for chain 1. Amoy reads only its own `TRADE_RPC_URLS` entry: no
    /// other chain's endpoint stands in for it.
    pub fn venue_rpc_url(&self, chain_id: u64) -> Option<String> {
        let net = match chain_id {
            1 => chain_walk::ChainNet::Ethereum,
            137 => chain_walk::ChainNet::Matic,
            80002 => {
                return self
                    .settings
                    .market
                    .trade_rpc_urls
                    .for_chain(80002)
                    .map(str::to_string);
            }
            _ => return None,
        };
        if self.settings.eth_network.trim() != "mainnet" {
            return self
                .settings
                .market
                .trade_rpc_urls
                .for_chain(chain_id as i64)
                .map(str::to_string);
        }
        self.chain_rpc_url(net)
    }

    /// The endpoint the trade-events walk of `net` reads: the
    /// `TRADE_RPC_URLS` entry of the chain it indexes
    /// (`MARKETPLACE_INDEX_CHAIN_ID_*`), and for Ethereum `ETH_RPC_URL`.
    pub fn trade_events_rpc_url(&self, net: chain_walk::ChainNet) -> Option<String> {
        self.rpc_url_for(net, self.settings.market.indexes.chain_id(net))
    }

    fn rpc_url_for(&self, net: chain_walk::ChainNet, chain_id: i64) -> Option<String> {
        self.settings
            .market
            .trade_rpc_urls
            .for_chain(chain_id)
            .map(str::to_string)
            .or_else(|| {
                (net == chain_walk::ChainNet::Ethereum)
                    .then(|| self.settings.eth_rpc_url.clone())
                    .flatten()
            })
    }
}

/// Refuses to walk an endpoint that serves another chain than `expected`:
/// the walk would read another network's logs into this one's rows.
pub(crate) async fn ensure_chain(
    rpc: &crate::chain_index::rpc::Rpc<'_>,
    expected: i64,
) -> anyhow::Result<()> {
    let v = rpc.call("eth_chainId", serde_json::json!([])).await?;
    let served = v
        .get("result")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| i64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| anyhow::anyhow!("eth_chainId returned {v}"))?;
    anyhow::ensure!(
        served == expected,
        "the endpoint serves chain {served}, not {expected}: refusing to index it"
    );
    Ok(())
}

/// Attaches `nft-index`, `trade-events-index` and `mana-transfers-index`
/// when their switches are on: one periodic job each, a pass walking both
/// networks in turn (the budget is per network), the nft walk of a network
/// waiting for the collections mirror's first complete pass (its address
/// set); then `sales-index` (`sales_index::spawn_job`, Polygon only).
fn spawn_index_jobs(state: &Arc<MarketplaceState>, jobs: &Jobs) {
    sales_index::spawn_job(state, jobs);
    let settings = state.settings.market.indexes.clone();
    for (enabled, name) in [
        (
            settings.trade_events_enabled,
            chain_walk::INDEXER_TRADE_EVENTS,
        ),
        (settings.nft_enabled, chain_walk::INDEXER_NFT),
        (settings.mana_transfers_enabled, chain_walk::INDEXER_MANA),
    ] {
        let job = if name == chain_walk::INDEXER_NFT {
            "nft-index"
        } else if name == chain_walk::INDEXER_MANA {
            "mana-transfers-index"
        } else {
            "trade-events-index"
        };
        if !enabled {
            tracing::info!(
                job,
                "marketplace chain index off: its routes answer the unavailable envelope"
            );
            continue;
        }
        let trade_events = name == chain_walk::INDEXER_TRADE_EVENTS;
        let rpc_url = move |state: &MarketplaceState, net: chain_walk::ChainNet| {
            if trade_events {
                state.trade_events_rpc_url(net)
            } else {
                state.chain_rpc_url(net)
            }
        };
        let nets: Vec<chain_walk::ChainNet> = chain_walk::ChainNet::ALL
            .into_iter()
            .filter(|n| rpc_url(state, *n).is_some())
            .collect();
        if nets.is_empty() {
            tracing::warn!(
                job,
                "marketplace chain index enabled but no JSON-RPC endpoint is configured (TRADE_RPC_URLS 1= / 137=, or ETH_RPC_URL): it stays off"
            );
            continue;
        }
        let state = state.clone();
        let nft_indexes: Arc<Vec<nft_index::NftIndex>> =
            Arc::new(nets.iter().map(|n| nft_index::NftIndex::new(*n)).collect());
        jobs.spawn_periodic_leased(
            job,
            PeriodicCfg::every(settings.interval).with_jitter(Duration::from_secs(1)).with_error_backoff(Duration::from_secs(30)),
            move |ctx| {
                let (state, nets, nft_indexes) = (state.clone(), nets.clone(), nft_indexes.clone());
                async move {
                    let outcomes = futures::future::join_all(nets.iter().enumerate().map(|(i, net)| {
                        let (state, nft_indexes, ctx) = (&state, &nft_indexes, &ctx);
                        async move {
                            let Some(url) = rpc_url(state, *net) else { return Ok(()) };
                            let rpc = crate::chain_index::rpc::Rpc::at(&state.http, &url);
                            let progress = |m: String| ctx.progress(m);
                            let outcome = if name == chain_walk::INDEXER_NFT {
                                if !nft_index::mirror_complete(&state.pool, *net).await {
                                    ctx.progress(format!("nft {}: waiting for the collections mirror's first complete pass", net.db_name()));
                                    return Ok(());
                                }
                                chain_walk::run_pass(&state.pool, &rpc, &nft_indexes[i], &state.settings.market.indexes, Some(&ctx.cancel), &progress).await
                            } else if name == chain_walk::INDEXER_MANA {
                                chain_walk::run_pass(&state.pool, &rpc, &mana_transfers::ManaTransfersIndex { net: *net }, &state.settings.market.indexes, Some(&ctx.cancel), &progress).await
                            } else {
                                let chain_id = state.settings.market.indexes.chain_id(*net);
                                match ensure_chain(&rpc, chain_id).await {
                                    Ok(()) => chain_walk::run_pass(&state.pool, &rpc, &trade_events::TradeEventsIndex { net: *net, chain_id }, &state.settings.market.indexes, Some(&ctx.cancel), &progress).await,
                                    Err(e) => Err(e),
                                }
                            };
                            match outcome {
                                Ok(o) => {
                                    tracing::info!(indexer = name, summary = %o.summary(), "marketplace chain index pass");
                                    ctx.progress(o.summary());
                                    Ok(())
                                }
                                Err(e) => Err(format!("{}: {e:#}", net.db_name())),
                            }
                        }
                    })).await;
                    let errors: Vec<String> = outcomes.into_iter().filter_map(Result::err).collect();
                    if errors.is_empty() { Ok(()) } else { Err(anyhow::anyhow!(errors.join("; "))) }
                }
            },
        );
    }
}

/// The canonical operator paths (`/shop-api/admin/*`) of the moderation
/// handlers `market_router` serves at `/v1/admin/*`
/// ([`crate::paths::ALIASES`]).
pub fn shop_admin_router(state: SharedState) -> Router {
    let writes_enabled = state.settings.data_writes_enabled;
    let app = Router::new()
        .route("/admin/moderation/flags", get(admin::list_flags))
        .route(
            "/admin/moderation/{kind}/{hash}/flag",
            post(admin::set_flag).delete(admin::clear_flag),
        )
        .route("/admin/disputes", get(admin::list_disputes))
        .route(
            "/admin/disputes/{trade_hash}/open",
            post(admin::open_dispute),
        )
        .route(
            "/admin/disputes/{trade_hash}/resolve",
            post(admin::resolve_dispute),
        )
        .route(
            "/admin/listings/{kind}/{hash}/force-cancel",
            post(admin::force_cancel),
        )
        .route("/admin/audit", get(admin::list_audit))
        .merge(super::admin_extensions())
        .layer(axum::middleware::from_fn(nul_guard::nul_guard_middleware))
        .layer(axum::middleware::from_fn(cors::cors_middleware))
        .with_state(state);
    DataWrites::guard(app, writes_enabled)
}

/// Chain-derived identity: the network, the contract and the token or item
/// id, never the wire's `contract-token` string alone (DATABASE.md, schema
/// and ownership rule 1). `render` gives the wire id back.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AssetKey {
    /// `ethereum` | `matic` (the subgraphs' spelling; `mainnet` normalizes to `ethereum`).
    pub network: String,
    pub contract_address: String,
    /// The token id (an NFT) or the collection's `blockchainId` (an item), decimal.
    pub id: String,
}

impl AssetKey {
    pub fn new(network: &str, contract_address: &str, id: &str) -> Self {
        let network = match network {
            "mainnet" | "ETHEREUM" | "MAINNET" => "ethereum",
            "polygon" | "MATIC" | "POLYGON" => "matic",
            other => other,
        };
        Self {
            network: network.to_string(),
            contract_address: contract_address.to_ascii_lowercase(),
            id: id.to_string(),
        }
    }

    /// The wire id the old routes emitted and accept (`?id=`, `nftId`, `itemId`).
    pub fn render(&self) -> String {
        format!("{}-{}", self.contract_address, self.id)
    }

    /// Parses `<contract>-<id>`; the network is the caller's (a URN or a query parameter).
    pub fn parse(network: &str, wire: &str) -> Option<Self> {
        let (contract, id) = wire.trim().split_once('-')?;
        let contract = contract.to_ascii_lowercase();
        let is_addr = contract
            .strip_prefix("0x")
            .is_some_and(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        let is_id = !id.is_empty() && id.len() <= 78 && id.bytes().all(|b| b.is_ascii_digit());
        (is_addr && is_id).then(|| Self::new(network, &contract, id))
    }
}

pub use crate::shared::http_util::{
    bad_request, internal_server_error, lambdas_error, lambdas_internal_error, not_found,
    service_unavailable,
};
pub use crate::shared::identity::is_valid_eth_address;
pub use crate::shared::upstream::check_upstream;
pub use crate::shared::writes::DataWrites;

/// `GET /v1/indexes`: the chain indexes' cursors, readiness and owed
/// backfills, and the neighbours build (the operator's freshness view).
async fn get_index_status(
    axum::extract::State(state): axum::extract::State<SharedState>,
) -> Json<Value> {
    Json(json!({
        "nft": nft_index::status(&state.pool).await,
        "tradeEvents": trade_events::status(&state.pool).await,
        "manaTransfers": mana_transfers::status(&state.pool).await,
        "sales": sales_index::status(&state.pool).await,
        "neighbours": neighbours::status(&state.pool).await,
    }))
}

async fn lambdas_status(
    axum::extract::State(state): axum::extract::State<SharedState>,
) -> Json<Value> {
    Json(json!({
        "version": state.settings.version,
        "currentTime": chrono::Utc::now().timestamp_millis(),
        "commitHash": state.settings.commit_hash,
    }))
}

/// The routes this module serves, with lamb2's CORS and the gateway's 404
/// under `/lambdas/` ([`lambdas_http`]), the NUL guard and the request
/// timeout: `main.rs` merges it into the content router, whose layers cover
/// only the routes it built itself.
pub fn router(state: SharedState) -> Router {
    let request_timeout = state.settings.request_timeout;
    let mut app = Router::new()
        .route(
            "/lambdas/profiles",
            post(profiles::profiles).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/lambdas/profiles/metadata",
            post(profiles::profiles_metadata).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/lambdas/profiles/{id}", get(profiles::profile_by_id))
        .route("/lambdas/profile/{id}", get(profiles::profile_alias))
        .route(
            "/lambdas/users/{address}/wearables",
            get(user_items::user_wearables),
        )
        .route(
            "/lambdas/users/{address}/emotes",
            get(user_items::user_emotes),
        )
        .route(
            "/lambdas/users/{address}/names",
            get(user_items::user_names),
        )
        .route(
            "/lambdas/collections/wearables-by-owner/{owner}",
            get(user_items::wearables_by_owner),
        )
        .route(
            "/lambdas/collections/emotes-by-owner/{owner}",
            get(user_items::emotes_by_owner),
        )
        .route(
            "/lambdas/contracts/servers",
            get(contracts::contracts_servers),
        )
        .route("/lambdas/contracts/pois", get(contracts::contracts_pois))
        .route(
            "/lambdas/contracts/denylisted-names",
            get(contracts::contracts_denylisted_names),
        )
        .route(
            "/lambdas/third-party-integrations",
            get(third_party::third_party_integrations),
        )
        .route(
            "/lambdas/users/{address}/third-party-wearables",
            get(third_party::user_third_party_wearables),
        )
        .route(
            "/lambdas/users/{address}/third-party-wearables/{collectionId}",
            get(third_party::user_third_party_collection_wearables),
        )
        .route(
            "/lambdas/users/{address}/lands",
            get(lambdas_land::user_lands),
        )
        .route(
            "/lambdas/users/{address}/genesis-parcels",
            get(lambdas_land::user_genesis_parcels),
        )
        .route("/lambdas/names/{name}/owner", get(lambdas_land::name_owner))
        .route(
            "/lambdas/collections",
            get(lambdas_catalog::nfts_collections),
        )
        .route(
            "/lambdas/nfts/collections",
            get(lambdas_catalog::nfts_collections),
        )
        .route(
            "/lambdas/collections/wearables",
            get(lambdas_catalog::collections_wearables_catalog),
        )
        .route(
            "/lambdas/collections/emotes",
            get(lambdas_catalog::collections_emotes_catalog),
        )
        .route("/lambdas/outfits/{id}", get(lambdas_catalog::outfits))
        .route("/outfits/{id}", get(lambdas_catalog::outfits))
        .route(
            "/lambdas/explorer/{address}/wearables",
            get(lambdas_explorer::explorer_wearables),
        )
        .route(
            "/lambdas/explorer/{address}/emotes",
            get(lambdas_explorer::explorer_emotes),
        )
        .route(
            "/explorer/{address}/wearables",
            get(lambdas_explorer::explorer_wearables),
        )
        .route(
            "/explorer/{address}/emotes",
            get(lambdas_explorer::explorer_emotes),
        )
        .route(
            "/lambdas/crypto/validate-signature",
            post(lambdas_crypto::validate_signature).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/lambdas/status", get(lambdas_status))
        .route("/lambdas/{*rest}", any(lambdas_http::not_found))
        .route("/v1/trades", get(trades::get_trades))
        .route("/v1/trades-mirror", get(trades::get_mirror_status))
        .route("/v1/trades/{id}", get(trades::get_trade))
        .layer(axum::middleware::from_fn(mirrors::stamp_copies))
        .layer(axum::middleware::from_fn(nul_guard::nul_guard_middleware))
        .layer(axum::middleware::from_fn(lambdas_http::cors_middleware));
    if let Some(timeout) = request_timeout {
        app = app.layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ));
    }
    app.with_state(state)
}

/// The marketplace-server surface (the old `catalyrst-market` crate's
/// routes), for the `/shop-api` nest and its `/marketplace-api` alias: its
/// `/v1/items`, `/v1/collections` and `/v1/contracts` collide with the
/// builder's at the root, where `router` keeps the lambdas and the trades
/// reads. Every mutating route sits behind `DATA_WRITES_ENABLED`
/// ([`DataWrites::guard`]).
pub fn market_router(state: SharedState) -> Router {
    let request_timeout = state.settings.request_timeout;
    let writes_enabled = state.settings.data_writes_enabled;
    let app = Router::new()
        .route("/v1/contracts", get(collections::get_contracts))
        .route("/v1/collections", get(collections::get_collections))
        .route("/v1/accounts", get(accounts::get_accounts))
        .route("/v1/owners", get(owners::get_owners))
        .route("/v1/catalog", get(catalog::get_catalog_v1))
        .route("/v2/catalog", get(catalog::get_catalog_v2))
        .route("/v3/catalog/shop", get(shop::get_shop_catalog))
        .route("/v3/catalog/legacy", get(shop::get_legacy_catalog))
        .route("/v3/catalog/unified", get(shop::get_unified_catalog))
        .route("/v3/catalog/items", get(items::get_catalog_items))
        .route("/v3/catalog/related", get(shop::get_related_catalog))
        .route("/v3/catalog/trending", get(shop::get_trending_catalog))
        .route("/v3/catalog/suggested", get(shop::get_suggested_catalog))
        .route("/v3/catalog/creators", get(shop::get_top_creators))
        .route(
            "/v3/catalog/creators/search",
            get(search::get_creator_search),
        )
        .route("/v3/catalog/suggest", get(search::get_search_suggest))
        .route("/v3/catalog/importable", get(shop::get_importable_listings))
        .route("/v1/nfts", get(nfts::get_nfts))
        .route("/v1/items", get(items::get_items))
        .route(
            "/v1/users/{address}/wearables",
            get(users::get_user_wearables),
        )
        .route(
            "/v1/users/{address}/wearables/grouped",
            get(users::get_user_grouped_wearables),
        )
        .route(
            "/v1/users/{address}/wearables/urn-token",
            get(users::get_user_wearables_urn_token),
        )
        .route("/v1/users/{address}/emotes", get(users::get_user_emotes))
        .route(
            "/v1/users/{address}/emotes/grouped",
            get(users::get_user_grouped_emotes),
        )
        .route(
            "/v1/users/{address}/emotes/urn-token",
            get(users::get_user_emotes_urn_token),
        )
        .route("/v1/users/{address}/names", get(users::get_user_names))
        .route(
            "/v1/users/{address}/names/names-only",
            get(users::get_user_names_only),
        )
        .route("/v1/orders", get(orders::get_orders))
        .route(
            "/v1/orders/open-by-items",
            get(orders::get_open_orders_by_items),
        )
        .route("/v1/bids", get(bids::get_bids))
        .route("/v1/sales/summary", get(sales::get_sales_summary))
        .route("/v1/sales", get(sales::get_sales))
        .route("/v1/prices", get(prices::get_prices))
        .route("/v1/trendings", get(items::get_trendings))
        .route(
            "/v1/rankings/{entity}/{timeframe}",
            get(rankings::get_rankings),
        )
        .route("/v1/stats/{category}/{stat}", get(rankings::get_stats))
        .route("/v1/volume/{timeframe}", get(rankings::get_volume))
        .route(
            "/v1/trades",
            get(trades::get_trades)
                .post(listings::post_trade)
                .layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/v1/trades-mirror", get(trades::get_mirror_status))
        .route("/v1/trades/{id}", get(trades::get_trade))
        .route(
            "/v1/trades/{hashed_signature}/accept",
            get(trade_events::get_trade_accepted_event),
        )
        .route("/v1/lists", get(lists::get_lists).post(lists::create_list))
        .route(
            "/v1/lists/{id}",
            get(lists::get_list_route)
                .put(lists::update_list_route)
                .delete(lists::delete_list_route),
        )
        .route(
            "/v1/lists/{id}/picks",
            get(lists::get_list_picks).post(lists::add_pick_to_list_route),
        )
        .route(
            "/v1/lists/{id}/picks/{item_id}",
            axum::routing::delete(lists::delete_pick_in_list_route),
        )
        .route(
            "/v1/lists/{id}/access",
            post(lists::create_access_route).delete(lists::delete_access_route),
        )
        .route("/v1/picks/stats", get(lists::get_picks_stats))
        .route("/v1/picks/{item_id}/stats", get(lists::get_item_pick_stats))
        .route(
            "/v1/picks/{item_id}",
            get(lists::get_picks_by_item)
                .post(lists::pick_unpick_in_bulk)
                .delete(lists::unpick_everywhere),
        )
        .route(
            "/v1/coupons",
            get(crate::coupons::get_coupons)
                .post(crate::coupons::add_coupon)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/v1/coupons/{id}", get(crate::coupons::get_coupon))
        .route("/v1/indexes", get(get_index_status))
        .route("/v1/mana/transfers", get(mana_transfers::history))
        .route("/v1/admin/moderation/flags", get(admin::list_flags))
        .route("/v1/federation/bid", post(federation::place_bid))
        .route("/v1/federation/bid/cancel", post(federation::cancel_bid))
        .route("/v1/federation/bid/accept", post(federation::accept_bid))
        .route("/v1/federation/order", post(federation::create_order))
        .route(
            "/v1/federation/order/cancel",
            post(federation::cancel_order),
        )
        .route("/v1/federation/trade", post(federation::record_trade))
        .route("/v1/federation/bids", get(federation::list_bids))
        .route("/v1/federation/orders", get(federation::list_orders))
        .route("/v1/federation/trades", get(federation::list_trades))
        .route("/federation/market/snapshot", get(federation::snapshot))
        .route("/federation/market/changes", get(federation::changes))
        .route(
            "/v1/admin/moderation/{kind}/{hash}/flag",
            post(admin::set_flag).delete(admin::clear_flag),
        )
        .route("/v1/admin/disputes", get(admin::list_disputes))
        .route(
            "/v1/admin/disputes/{trade_hash}/open",
            post(admin::open_dispute),
        )
        .route(
            "/v1/admin/disputes/{trade_hash}/resolve",
            post(admin::resolve_dispute),
        )
        .route(
            "/v1/admin/listings/{kind}/{hash}/force-cancel",
            post(admin::force_cancel),
        )
        .route("/v1/admin/audit", get(admin::list_audit))
        .route("/v1/activity", get(activity::get_activity))
        .merge(super::market_extensions())
        .layer(axum::middleware::from_fn(mirrors::stamp_copies))
        .layer(axum::middleware::from_fn(signed::mounted_path))
        .layer(axum::middleware::from_fn(nul_guard::nul_guard_middleware))
        .layer(axum::middleware::from_fn(cors::cors_middleware));
    let app = if let Some(timeout) = request_timeout {
        app.layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
    } else {
        app
    };
    DataWrites::guard(app.with_state(state), writes_enabled)
}
