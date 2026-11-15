//! The shop's settings: the marketplace, the economy, credits and the shop fold.

use clap::Args;

use crate::config::DEFAULT_MANA_PRICE_MAX_STALENESS_SECS;

/// The marketplace's upstreams: the subgraph readers, the trades mirror and the profile CDN.
#[derive(Args, Clone, Debug)]
pub struct MarketUpstreamsConfig {
    // marketplace: subgraph readers, the trades mirror and the profile CDN.
    /// L1 collections subgraph; unset answers owned wearables/emotes unfiltered.
    #[arg(long, env = "ETH_COLLECTIONS_SUBGRAPH_URL")]
    pub eth_collections_subgraph_url: Option<String>,

    /// L2 collections subgraph.
    #[arg(long, env = "MATIC_COLLECTIONS_SUBGRAPH_URL")]
    pub matic_collections_subgraph_url: Option<String>,

    /// L1 marketplace subgraph (names, and LAND before the indexes publish);
    /// `NODE_POSTURE=mirror` sets lamb2's default. Unset, `/lambdas` names
    /// and lands answer the 503 envelope; profiles and outfits claim no name.
    #[arg(long, env = "MARKETPLACE_SUBGRAPH_URL")]
    pub marketplace_subgraph_url: Option<String>,

    /// Land-manager subgraph: parcel operators, permissions and
    /// update-operator lookups before the land-authz index is published;
    /// unset answers the envelope (lamb2's 500 on the permissions route).
    #[arg(long, env = "LAND_SUBGRAPH_URL")]
    pub land_subgraph_url: Option<String>,

    /// The marketplace API `/v1/trades` list to mirror; unset attaches no job.
    #[arg(long, env = "TRADES_SYNC_UPSTREAM_URL")]
    pub trades_sync_upstream_url: Option<String>,

    /// Pause between trades mirror passes (floored to 60).
    #[arg(long, env = "TRADES_SYNC_INTERVAL_SECS", default_value_t = 900)]
    pub trades_sync_interval_secs: u64,

    /// Pages of 100 new trades (one detail fetch each) the trades mirror pays
    /// for per tick, and its page requests per tick when the upstream honours
    /// paging; bounds the upstream traffic of a catch-up. 0 = unbounded.
    #[arg(long, env = "TRADES_SYNC_PAGES_PER_RUN", default_value_t = 50)]
    pub trades_sync_pages_per_run: usize,

    /// Development only: read `shop.trades` before the trades mirror's first
    /// complete pass, and past its freshness budget. The Shop then shows
    /// whatever part of the order book has arrived (prices missing or stale),
    /// which no deployment may serve.
    #[arg(long, env = "TRADES_SERVE_PARTIAL", default_value_t = false, action = clap::ArgAction::Set)]
    pub trades_serve_partial: bool,

    /// Avatar snapshots become `<base>/entities/<id>/{face,body}.png` when
    /// set; unset they point at this node's `/profile-images`.
    #[arg(long, env = "PROFILE_CDN_BASE_URL")]
    pub profile_cdn_base_url: Option<String>,

    /// NFT ownership indexer (`POST <base>/wallets/<owner>/networks/<network>/nfts`)
    /// for an owner's third-party holdings; unset, they are read over each
    /// network's JSON-RPC (`TRADE_RPC_URLS`, `ETH_RPC_URL`, `RPC_ENDPOINT_POLYGON`).
    /// `NODE_POSTURE=mirror` refuses to start with it empty.
    #[arg(long, env = "NFT_WORKER_BASE_URL")]
    pub nft_worker_base_url: Option<String>,

    /// Merge this node's active escrow grants (`shop.usage_grants`) into the
    /// explorer backpack's on-chain items. lamb2 lists chain-owned tokens
    /// only, so the mirror posture turns it off.
    #[arg(long, env = "EXPLORER_ESCROW_GRANTS_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub explorer_escrow_grants_enabled: bool,

    /// Take this node's extended filters on `/lambdas/collections/wearables`
    /// (`collectionType`, `category`, `rarity`, `name`, `orderBy`,
    /// `direction`). lamb2 ignores them, so the mirror posture turns it off.
    #[arg(long, env = "LAMBDAS_CATALOG_EXTENSIONS_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub lambdas_catalog_extensions_enabled: bool,
}

/// The marketplace: the collections and items mirrors, the trade ownership RPCs, the
/// operator bearer, the catalog cache and the chain indexes.
#[derive(Args, Clone, Debug)]
pub struct MarketplaceConfig {
    // marketplace (src/marketplace, the old catalyrst-market surface): the
    // collections and items mirrors, the ownership RPCs of POST /v1/trades,
    // the operator bearer, the catalog page cache and the MANA/USD fallback.
    /// Run the collections and items mirrors over the configured collections subgraphs.
    #[arg(long, env = "MARKETPLACE_MIRRORS_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub marketplace_mirrors_enabled: bool,

    #[arg(long, env = "MARKETPLACE_MIRROR_INTERVAL_SECS", default_value_t = 300)]
    pub marketplace_mirror_interval_secs: u64,

    /// Pages of 1,000 items one mirror tick walks per network; the catch-up backpressure.
    #[arg(long, env = "MARKETPLACE_MIRROR_PAGES_PER_TICK", default_value_t = 50)]
    pub marketplace_mirror_pages_per_tick: usize,

    /// `137=https://polygon...,1=https://eth...`: the per-chain JSON-RPC the
    /// signed-trade ownership check calls; https to a public host, never this
    /// node. Gateway keys sit in the paths, so TRADE_RPC_URLS_FILE is the
    /// intended form (the RPC_ENDPOINT_POLYGON rule).
    #[arg(long, env = "TRADE_RPC_URLS", hide_env_values = true)]
    pub trade_rpc_urls: Option<String>,

    /// Bearer of /v1/admin/* (moderation, disputes, force-cancel); unset refuses them.
    #[arg(long, env = "MARKET_ADMIN_TOKEN", hide_env_values = true)]
    pub market_admin_token: Option<String>,

    /// 0 disables the catalog page cache.
    #[arg(long, env = "MARKETPLACE_CATALOG_CACHE_TTL_SECS", default_value_t = 30)]
    pub marketplace_catalog_cache_ttl_secs: u64,

    // The marketplace chain indexes (src/marketplace/{chain_walk,nft_index,
    // trade_events}): eth_getLogs over the TRADE_RPC_URLS entry of each chain
    // (1=, 137=; ETH_RPC_URL for Ethereum when the list has none), the item
    // neighbours they feed and the coupon state reader.
    /// Walk the collections' transfers, the names, the legacy orders and the LAND data.
    #[arg(long, env = "MARKETPLACE_NFT_INDEX_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub marketplace_nft_index_enabled: bool,

    /// Walk the off-chain marketplaces' Traded, cancellation and index logs.
    #[arg(long, env = "MARKETPLACE_TRADE_EVENTS_INDEX_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub marketplace_trade_events_index_enabled: bool,

    #[arg(long, env = "MARKETPLACE_MANA_TRANSFERS_INDEX_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub marketplace_mana_transfers_index_enabled: bool,

    /// The smallest MANA transfer (wei) a tip forward notifies; smaller ones are refused.
    #[arg(
        long,
        env = "MARKETPLACE_TIP_MIN_AMOUNT_WEI",
        default_value = "1000000000000000000"
    )]
    pub marketplace_tip_min_amount_wei: String,

    /// The oldest transfer (block time before the forward) a tip forward notifies.
    #[arg(long, env = "MARKETPLACE_TIP_MAX_AGE_SECS", default_value_t = 86_400)]
    pub marketplace_tip_max_age_secs: u64,

    /// Tip forwards waiting for the MANA transfer index, across every sender.
    #[arg(long, env = "MARKETPLACE_TIP_MAX_PENDING", default_value_t = 2_000)]
    pub marketplace_tip_max_pending: u64,
    /// Walk the store's primary mints and the ERC721BidV2 bids on Polygon.
    #[arg(long, env = "MARKETPLACE_SALES_INDEX_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub marketplace_sales_index_enabled: bool,

    #[arg(long, env = "MARKETPLACE_INDEX_INTERVAL_SECS", default_value_t = 5)]
    pub marketplace_index_interval_secs: u64,

    /// eth_getLogs calls one network's pass may make; the catch-up backpressure.
    #[arg(
        long,
        env = "MARKETPLACE_INDEX_REQUESTS_PER_PASS",
        default_value_t = 100
    )]
    pub marketplace_index_requests_per_pass: usize,

    /// The widest window asked for (the strictest public backend takes 10,000).
    #[arg(
        long,
        env = "MARKETPLACE_INDEX_WINDOW_BLOCKS",
        default_value_t = 10_000
    )]
    pub marketplace_index_window_blocks: u64,

    /// Addresses per eth_getLogs; a refusal of the count halves it for the pass.
    #[arg(long, env = "MARKETPLACE_INDEX_ADDRESS_CHUNK", default_value_t = 1_000)]
    pub marketplace_index_address_chunk: usize,

    /// Development/measurement bound: a fresh cursor starts here (never published).
    #[arg(long, env = "MARKETPLACE_INDEX_START_BLOCK_ETHEREUM")]
    pub marketplace_index_start_block_ethereum: Option<u64>,

    #[arg(long, env = "MARKETPLACE_INDEX_START_BLOCK_MATIC")]
    pub marketplace_index_start_block_matic: Option<u64>,

    /// Development/measurement bound: index no further (never published).
    #[arg(long, env = "MARKETPLACE_INDEX_STOP_BLOCK_ETHEREUM")]
    pub marketplace_index_stop_block_ethereum: Option<u64>,

    #[arg(long, env = "MARKETPLACE_INDEX_STOP_BLOCK_MATIC")]
    pub marketplace_index_stop_block_matic: Option<u64>,

    /// The chain the trade-events walk of each network reads: its mainnet, or
    /// its testnet (11155111, 80002) on a stack that trades there.
    #[arg(long, env = "MARKETPLACE_INDEX_CHAIN_ID_ETHEREUM", default_value_t = 1)]
    pub marketplace_index_chain_id_ethereum: i64,

    #[arg(long, env = "MARKETPLACE_INDEX_CHAIN_ID_MATIC", default_value_t = 137)]
    pub marketplace_index_chain_id_matic: i64,

    /// Between two rebuilds of the co-ownership neighbours (min 600).
    #[arg(
        long,
        env = "MARKETPLACE_NEIGHBOURS_INTERVAL_SECS",
        default_value_t = 21_600
    )]
    pub marketplace_neighbours_interval_secs: u64,

    /// Between two CouponManager state reads of the live coupons (min 30).
    #[arg(
        long,
        env = "MARKETPLACE_COUPON_REFRESH_INTERVAL_SECS",
        default_value_t = 300
    )]
    pub marketplace_coupon_refresh_interval_secs: u64,
}

/// The economy (`shop::evm`): the meta-transaction relay, the broker and escrow signers and
/// the NAME custody chain.
#[derive(Args, Clone, Debug)]
pub struct EconomyConfig {
    // economy (src/shop/evm, the old catalyrst-economy env table): the
    // meta-transaction relay policy, the broadcast paths, the broker and
    // escrow signers and the NAME custody chain.
    #[arg(
        long,
        env = "MIN_SALE_VALUE_IN_WEI",
        default_value = "1000000000000000000"
    )]
    pub min_sale_value_in_wei: String,

    #[arg(long, env = "MAX_TRANSACTIONS_PER_DAY", default_value_t = 10)]
    pub max_transactions_per_day: i64,

    /// Free meta-transactions (no sale, no credits spend) the relayer pays for per day across every address; 0 is no limit. Sales are never held back by it.
    #[arg(
        long,
        env = "MAX_RELAYED_TRANSACTIONS_PER_DAY",
        default_value_t = 20_000
    )]
    pub max_relayed_transactions_per_day: i64,

    /// The dApps' contract-addresses JSON (public https); unset, only the collections mirror admits.
    #[arg(long, env = "CONTRACT_ADDRESSES_URL")]
    pub contract_addresses_url: Option<String>,

    #[arg(long, env = "CONTRACT_ADDRESSES_CHAIN_KEY", default_value = "matic")]
    pub contract_addresses_chain_key: String,

    #[arg(long, env = "COLLECTIONS_CHAIN_ID", default_value_t = 137)]
    pub collections_chain_id: u64,

    #[arg(
        long,
        env = "COLLECTIONS_FETCH_INTERVAL_MS",
        default_value_t = 3_600_000
    )]
    pub collections_fetch_interval_ms: u64,

    /// The collections chain's JSON-RPC (the old RPC_URL; public https, or
    /// RPC_ENDPOINT_POLYGON_FILE since gateway keys sit in the path); the
    /// content validator's L2 deploy access checks read it too (https only).
    /// Unset or empty: https://rpc.decentraland.org/<polygon|amoy>?project=catalyst-content
    /// by ETH_NETWORK, except under NODE_POSTURE island and journey.
    #[arg(long, env = "RPC_ENDPOINT_POLYGON", hide_env_values = true)]
    pub rpc_endpoint_polygon: Option<String>,

    #[arg(long, env = "MAX_GAS_PRICE_ALLOWED_IN_WEI")]
    pub max_gas_price_allowed_in_wei: Option<u128>,

    #[arg(long, env = "MAX_GAS_LIMIT", default_value_t = 1_500_000)]
    pub max_gas_limit: u64,

    /// The highest fee per gas, in wei, the direct relay signer signs: EIP-1559
    /// `max_fee_per_gas`, a legacy `gas_price`. Unset, no cap; required under CREDITS_TRIAL_MODE.
    #[arg(long, env = "RELAYER_MAX_FEE_PER_GAS_WEI")]
    pub relayer_max_fee_per_gas_wei: Option<u128>,

    #[arg(long, env = "OZ_RELAYER_URL")]
    pub oz_relayer_url: Option<String>,

    #[arg(long, env = "OZ_RELAYER_ID")]
    pub oz_relayer_id: Option<String>,

    #[arg(long, env = "OZ_RELAYER_API_KEY", hide_env_values = true)]
    pub oz_relayer_api_key: Option<String>,

    #[arg(long, env = "OZ_RELAYER_SPEED", default_value = "fast")]
    pub oz_relayer_speed: String,

    #[arg(long, env = "OZ_MAX_STATUS_CHECKS", default_value_t = 150)]
    pub oz_max_status_checks: u32,

    #[arg(long, env = "OZ_SLEEP_TIME_BETWEEN_CHECKS_MS", default_value_t = 800)]
    pub oz_sleep_time_between_checks_ms: u64,

    /// Arms the direct signers (RELAYER_PRIVATE_KEY over RPC_ENDPOINT_POLYGON).
    #[arg(long, env = "META_TX_BROADCAST_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub meta_tx_broadcast_enabled: bool,

    /// The relayer EOA's key; RELAYER_PRIVATE_KEY_FILE is the intended form.
    #[arg(long, env = "RELAYER_PRIVATE_KEY", hide_env_values = true)]
    pub relayer_private_key: Option<String>,

    /// The upstream transactions-api the relay forwards to when it has no signer of its own (public https).
    #[arg(long, env = "TRANSACTIONS_UPSTREAM_URL")]
    pub transactions_upstream_url: Option<String>,

    #[arg(
        long,
        env = "TRANSACTIONS_UPSTREAM_TIMEOUT_MS",
        default_value_t = 30_000
    )]
    pub transactions_upstream_timeout_ms: u64,

    /// Bearer of the money routes (broker, escrow, admin, payments verify); unset refuses them.
    #[arg(long, env = "CATALYRST_ECONOMY_ADMIN_TOKEN", hide_env_values = true)]
    pub catalyrst_economy_admin_token: Option<String>,

    #[arg(long, env = "LANDILER_ESCROW_ADDRESS")]
    pub landiler_escrow_address: Option<String>,

    #[arg(long, env = "NAMES_CHAIN_ID", default_value_t = 1)]
    pub names_chain_id: u64,

    /// Decimal wei; unset leaves the NAME purchases uncapped.
    #[arg(long, env = "NAMES_MAX_PRICE_WEI")]
    pub names_max_price_wei: Option<String>,

    #[arg(long, env = "RECEIPT_POLL_INTERVAL_MS", default_value_t = 3_000)]
    pub receipt_poll_interval_ms: u64,

    #[arg(long, env = "RECEIPT_TIMEOUT_MS", default_value_t = 180_000)]
    pub receipt_timeout_ms: u64,

    #[arg(long, env = "BROKER_RECONCILE_INTERVAL_MS", default_value_t = 60_000)]
    pub broker_reconcile_interval_ms: u64,

    /// The Chainlink MANA/USD aggregator; unset is the Polygon mainnet one.
    #[arg(long, env = "MANA_USD_AGGREGATOR_ADDRESS")]
    pub mana_usd_aggregator_address: Option<String>,

    #[arg(long, env = "USD_PEGGED_ORACLE_MAX_AGE_SECS", default_value_t = 60)]
    pub usd_pegged_oracle_max_age_secs: u64,

    #[arg(long, env = "USD_PEGGED_SLIPPAGE_BPS", default_value_t = 100)]
    pub usd_pegged_slippage_bps: u64,
}

/// The shop: credits (Stripe, hCaptcha, the checkout worker, the authorize signer), the
/// shop-server fold and the CreditUsed index.
#[derive(Args, Clone, Debug)]
pub struct ShopConfig {
    // credits (the old catalyrst-credits): the operator bearer, Stripe,
    // hCaptcha, the checkout worker and the on-chain authorize signer.
    /// The credits operator bearer (or `_FILE`); unset refuses the admin routes.
    #[arg(long, env = "CATALYRST_CREDITS_ADMIN_TOKEN", hide_env_values = true)]
    pub catalyrst_credits_admin_token: Option<String>,

    /// The hCaptcha secret (or `_FILE`); unset skips the token check.
    #[arg(long, env = "CREDITS_CAPTCHA_SECRET", hide_env_values = true)]
    pub credits_captcha_secret: Option<String>,

    #[arg(
        long,
        env = "CREDITS_CAPTCHA_VERIFY_URL",
        default_value = "https://hcaptcha.com/siteverify"
    )]
    pub credits_captcha_verify_url: String,

    /// Enables the Stripe client (or `_FILE`); unset, the card routes answer 501/503.
    #[arg(long, env = "STRIPE_SECRET_KEY", hide_env_values = true)]
    pub stripe_secret_key: Option<String>,

    /// The Stripe webhook signature secret (or `_FILE`).
    #[arg(long, env = "STRIPE_WEBHOOK_SECRET", hide_env_values = true)]
    pub stripe_webhook_secret: Option<String>,

    /// Must be a public https endpoint when STRIPE_SECRET_KEY is set.
    #[arg(
        long,
        env = "STRIPE_API_BASE",
        default_value = "https://api.stripe.com"
    )]
    pub stripe_api_base: String,

    #[arg(long, env = "CREDITS_CURRENCY", default_value = "usd")]
    pub credits_currency: String,

    #[arg(long, env = "MARKETPLACE_MARKUP_BPS", default_value_t = 2500)]
    pub marketplace_markup_bps: i64,

    /// How old the newest MANA/USD snapshot may be before every credits price
    /// read answers 503. Age runs from CoinGecko's `source_updated_at`, which
    /// trails the poll by about 90 s, so the budget covers one poll interval
    /// (300 s), that lag and one missed poll: 900. At 300 (the old default)
    /// the lane answered "oracle is stale" for part of every cycle.
    #[arg(long, env = "MANA_PRICE_MAX_STALENESS_SECS", default_value_t = DEFAULT_MANA_PRICE_MAX_STALENESS_SECS)]
    pub mana_price_max_staleness_secs: i64,

    /// The oldest round of a marketplace's MANA/USD feed a credit for a
    /// USD-pegged trade is sized at; the marketplace's own tolerance applies
    /// too, and the stricter wins. Past it the authorization is refused (503).
    /// It also bounds the Shop's own rate (`shop::mana::feed_rate`): the
    /// catalog, item, import and credit prices show the settlement chain's
    /// feed while its last good round is younger than this, then answer
    /// unavailable; a purchase quote is cut at the marketplace's tolerance.
    #[arg(
        long,
        env = "CREDITS_USD_PEGGED_FEED_MAX_AGE_SECS",
        default_value_t = 3600
    )]
    pub credits_usd_pegged_feed_max_age_secs: u64,

    /// primary (the legacy secondary | auto run as primary: credits pay for
    /// CollectionStore mints only, SHOP.md).
    #[arg(long, env = "CHECKOUT_FULFILLMENT_MODE", default_value = "primary")]
    pub checkout_fulfillment_mode: String,

    /// POST /checkout requires the EIP-712 purchase intent.
    #[arg(long, env = "CREDITS_REQUIRE_PURCHASE_INTENT", default_value_t = true, action = clap::ArgAction::Set)]
    pub credits_require_purchase_intent: bool,

    #[arg(long, env = "CHECKOUT_WORKER_INTERVAL_SECS", default_value_t = 30)]
    pub checkout_worker_interval_secs: u64,

    #[arg(long, env = "CHECKOUT_MAX_ATTEMPTS", default_value_t = 5)]
    pub checkout_max_attempts: i32,

    #[arg(long, env = "ESCROW_LOCK_DAYS", default_value_t = 15)]
    pub escrow_lock_days: i32,

    /// Deliver checkouts off-chain without the economy broker (development).
    #[arg(long, env = "CREDITS_MOCK_FULFILLMENT", default_value_t = false, action = clap::ArgAction::Set)]
    pub credits_mock_fulfillment: bool,

    /// The mock card purchase and top-up routes (development).
    #[arg(long, env = "CREDITS_MOCK_CARD", default_value_t = false, action = clap::ArgAction::Set)]
    pub credits_mock_card: bool,

    /// The treasury signer of POST /credits/authorize (or `_FILE`; the old CREDITS_SIGNER_PRIVATE_KEY).
    #[arg(long, env = "CREDITS_SIGNER_KEY", hide_env_values = true)]
    pub credits_signer_key: Option<String>,

    #[arg(long, env = "CREDITS_MANAGER_CONTRACT")]
    pub credits_manager_contract: Option<String>,

    #[arg(long, env = "CREDITS_ONCHAIN_AUTHORIZE_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub credits_onchain_authorize_enabled: bool,

    /// How long a signed credit stays spendable, in seconds (upstream USD_CREDIT_TTL_SECONDS).
    #[arg(long, env = "CREDITS_TTL_SECONDS", default_value_t = 300)]
    pub credits_ttl_seconds: u64,

    /// The signed value over the group's quoted MANA need, in percent, 100..=150 (upstream USD_CREDIT_CAP_HEADROOM_PCT).
    #[arg(long, env = "CREDITS_CAP_HEADROOM_PCT", default_value_t = 100)]
    pub credits_cap_headroom_pct: u32,

    /// The oracle buffer inside the credit sizing, in basis points, at most 1000 (upstream USD_CREDIT_BUFFER_BPS).
    #[arg(long, env = "CREDITS_ORACLE_BUFFER_BPS", default_value_t = 200)]
    pub credits_oracle_buffer_bps: u32,

    /// The CREDITS_REVOKER_ROLE signer of revokeCredits (`_FILE` only); unset, revocations queue in credits.revocations.
    #[arg(long, env = "CREDITS_REVOKER_KEY", hide_env_values = true)]
    pub credits_revoker_key: Option<String>,

    /// A closed trial of on-chain credits: only CREDITS_TRIAL_BUYERS get new credit
    /// authorizations, the MANA top-up routes refuse, and the node refuses to start with a card
    /// rail, a mock card, a relay fallback, an uncapped relay or no revoker. The relay's writes
    /// then open with OPS_WRITES_ENABLED, the credits switch, instead of DATA_WRITES_ENABLED.
    #[arg(long, env = "CREDITS_TRIAL_MODE", default_value_t = false, action = clap::ArgAction::Set)]
    pub credits_trial_mode: bool,

    /// The trial's buyers, comma-separated addresses (or `_FILE`): the root signer of a request's
    /// auth chain must be one of them. Empty or unset under CREDITS_TRIAL_MODE: no new credit is
    /// authorized.
    #[arg(long, env = "CREDITS_TRIAL_BUYERS", hide_env_values = true)]
    pub credits_trial_buyers: Option<String>,

    /// Empty: the Stripe Checkout order flow answers 503.
    #[arg(long, env = "CREDITS_CHECKOUT_SUCCESS_URL", default_value = "")]
    pub credits_checkout_success_url: String,

    #[arg(long, env = "CREDITS_CHECKOUT_CANCEL_URL", default_value = "")]
    pub credits_checkout_cancel_url: String,

    // The shop-server fold (src/shop/{outfits,subscriptions,treasury}).
    /// The outfit authoring allowlist (comma or newline separated wallets, the signed-fetch signer compared lowercase); empty refuses every outfit write.
    #[arg(long, env = "SHOP_OUTFIT_CREATORS", default_value = "")]
    pub shop_outfit_creators: String,

    /// The Shop's page base the availability alert deep-links (the item page and the opt-out page).
    #[arg(
        long,
        env = "SHOP_BASE_URL",
        default_value = "https://decentraland.org/shop"
    )]
    pub shop_base_url: String,

    /// Credits' feature service; empty uses explicit node overrides only.
    #[arg(long, env = "CREDITS_FEATURE_FLAGS_URL", default_value = "")]
    pub credits_feature_flags_url: String,
    /// Credits' own Referer context, normalized with https when no scheme is supplied.
    #[arg(long, env = "CREDITS_SERVER_URL", default_value = "")]
    pub credits_server_url: String,
    #[arg(long, env = "CREDITS_FEATURE_FLAGS_REFRESH_SECS", default_value_t = 60)]
    pub credits_feature_flags_refresh_secs: u64,

    /// The feature-flags service the Shop reads decentraland.org's `dapps` rollout flags from (as sites'
    /// `FEATURE_FLAGS_URL`); empty reads only this node's local overrides (`explore_api.feature_flags_local`).
    #[arg(long, env = "SHOP_FEATURE_FLAGS_URL", default_value = "")]
    pub shop_feature_flags_url: String,

    /// The site the flags are asked for: the service answers by Referer (as sites' `FEATURE_FLAGS_ORIGIN`).
    /// The mirror posture asks as decentraland.org; empty asks as the origin of `SHOP_BASE_URL`.
    #[arg(long, env = "SHOP_FEATURE_FLAGS_ORIGIN", default_value = "")]
    pub shop_feature_flags_origin: String,

    #[arg(long, env = "SHOP_FEATURE_FLAGS_REFRESH_SECS", default_value_t = 60)]
    pub shop_feature_flags_refresh_secs: u64,

    /// The availability alert outbox worker's cadence.
    #[arg(long, env = "SHOP_NOTIFY_DELIVERY_INTERVAL_SECS", default_value_t = 30)]
    pub shop_notify_delivery_interval_secs: u64,

    /// How often the watcher compares the waited-for items against the items mirror.
    #[arg(long, env = "SHOP_NOTIFY_WATCH_INTERVAL_SECS", default_value_t = 60)]
    pub shop_notify_watch_interval_secs: u64,

    /// Where a checkout the builder opened returns (both set, or the Shop pair).
    #[arg(long, env = "CREDITS_CHECKOUT_SUCCESS_URL_BUILDER")]
    pub credits_checkout_success_url_builder: Option<String>,

    #[arg(long, env = "CREDITS_CHECKOUT_CANCEL_URL_BUILDER")]
    pub credits_checkout_cancel_url_builder: Option<String>,

    /// Attach the CreditUsed index (credits.usages, over RPC_ENDPOINT_POLYGON
    /// and CREDITS_MANAGER_CONTRACT); POST /credits/authorize mints only while it is ready.
    #[arg(long, env = "CREDITS_USAGE_INDEX_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub credits_usage_index_enabled: bool,

    /// The first block a full replay reads; unset, the chain's known start (137: 70459461, 80002: 20612932).
    #[arg(long, env = "CREDITS_USAGE_START_BLOCK")]
    pub credits_usage_start_block: Option<u64>,

    /// MANA top-ups mined before this block are refused; unset, any block.
    #[arg(long, env = "CREDITS_TOPUP_START_BLOCK")]
    pub credits_topup_start_block: Option<u64>,

    /// Blocks a MANA top-up transfer needs on top of it before it is credited.
    #[arg(long, env = "CREDITS_TOPUP_MIN_CONFIRMATIONS", default_value_t = 64)]
    pub credits_topup_min_confirmations: u64,

    #[arg(long, env = "CREDITS_USAGE_INDEX_INTERVAL_SECS", default_value_t = 30)]
    pub credits_usage_index_interval_secs: u64,

    /// eth_getLogs requests one pass may make before it yields with its cursor persisted.
    #[arg(
        long,
        env = "CREDITS_USAGE_INDEX_REQUESTS_PER_PASS",
        default_value_t = 50
    )]
    pub credits_usage_index_requests_per_pass: usize,

    /// Rewind the index durably once at the first pass (the start block for a full repair).
    #[arg(long, env = "CREDITS_USAGE_REPLAY_FROM_BLOCK")]
    pub credits_usage_replay_from_block: Option<u64>,

    /// Index no further (a development bound; an index held short is never published).
    #[arg(long, env = "CREDITS_USAGE_STOP_BLOCK")]
    pub credits_usage_stop_block: Option<u64>,

    /// Walk the Rentals contract's logs on Ethereum (shop.rental_event, over TRADE_RPC_URLS 1=
    /// or, on mainnet, ETH_RPC_URL): listing status sync and the rental notifications.
    #[arg(long, env = "SHOP_RENTALS_INDEX_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub shop_rentals_index_enabled: bool,
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        Config::parse_for_tests(args)
    }

    #[test]
    fn data_plane_defaults_are_the_old_binaries_values_and_writes_are_off() {
        let cli = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        let c = cli;
        assert!(
            !c.marketplace.marketplace_mirrors_enabled
                && c.marketplace.trade_rpc_urls.is_none()
                && c.marketplace.market_admin_token.is_none()
        );
        assert_eq!(
            (
                c.marketplace.marketplace_mirror_interval_secs,
                c.marketplace.marketplace_mirror_pages_per_tick,
                c.marketplace.marketplace_catalog_cache_ttl_secs
            ),
            (300, 50, 30)
        );
        assert_eq!(
            (
                c.economy.min_sale_value_in_wei.as_str(),
                c.economy.max_transactions_per_day
            ),
            ("1000000000000000000", 10)
        );
        assert!(
            c.economy.contract_addresses_url.is_none()
                && c.economy.rpc_endpoint_polygon.is_none()
                && c.economy.max_gas_price_allowed_in_wei.is_none()
        );
        assert_eq!(
            (
                c.economy.contract_addresses_chain_key.as_str(),
                c.economy.collections_chain_id,
                c.economy.collections_fetch_interval_ms,
                c.economy.max_gas_limit
            ),
            ("matic", 137, 3_600_000, 1_500_000)
        );
        assert!(
            c.economy.oz_relayer_url.is_none()
                && c.economy.oz_relayer_id.is_none()
                && c.economy.oz_relayer_api_key.is_none()
        );
        assert_eq!(
            (
                c.economy.oz_relayer_speed.as_str(),
                c.economy.oz_max_status_checks,
                c.economy.oz_sleep_time_between_checks_ms
            ),
            ("fast", 150, 800)
        );
        assert!(
            !c.economy.meta_tx_broadcast_enabled
                && c.economy.relayer_private_key.is_none()
                && c.economy.transactions_upstream_url.is_none()
        );
        assert_eq!(c.economy.transactions_upstream_timeout_ms, 30_000);
        assert!(
            c.economy.catalyrst_economy_admin_token.is_none()
                && c.economy.landiler_escrow_address.is_none()
                && c.economy.names_max_price_wei.is_none()
        );
        assert_eq!(
            (
                c.economy.names_chain_id,
                c.economy.receipt_poll_interval_ms,
                c.economy.receipt_timeout_ms,
                c.economy.broker_reconcile_interval_ms
            ),
            (1, 3_000, 180_000, 60_000)
        );
        assert!(c.economy.mana_usd_aggregator_address.is_none());
        assert_eq!(
            (
                c.economy.usd_pegged_oracle_max_age_secs,
                c.economy.usd_pegged_slippage_bps
            ),
            (60, 100)
        );
        assert!(
            !c.marketplace.marketplace_nft_index_enabled
                && !c.marketplace.marketplace_trade_events_index_enabled
                && !c.marketplace.marketplace_sales_index_enabled,
            "the chain indexes are opt-in"
        );
        assert_eq!(
            (
                c.marketplace.marketplace_index_interval_secs,
                c.marketplace.marketplace_index_requests_per_pass,
                c.marketplace.marketplace_index_window_blocks,
                c.marketplace.marketplace_index_address_chunk
            ),
            (5, 100, 10_000, 1_000)
        );
        assert!(
            c.marketplace
                .marketplace_index_start_block_ethereum
                .is_none()
                && c.marketplace.marketplace_index_start_block_matic.is_none()
        );
        assert!(
            c.marketplace
                .marketplace_index_stop_block_ethereum
                .is_none()
                && c.marketplace.marketplace_index_stop_block_matic.is_none()
        );
        assert_eq!(
            (
                c.marketplace.marketplace_index_chain_id_ethereum,
                c.marketplace.marketplace_index_chain_id_matic
            ),
            (1, 137)
        );
        assert_eq!(
            (
                c.marketplace.marketplace_neighbours_interval_secs,
                c.marketplace.marketplace_coupon_refresh_interval_secs
            ),
            (21_600, 300)
        );

        let cli = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--data-writes-enabled",
            "true",
            "--marketplace-mirrors-enabled",
            "true",
            "--price-poll-enabled",
            "true",
            "--land-authz-index-enabled",
            "true",
            "--land-authz-stop-block",
            "4950000",
            "--presence-retention-days",
            "30",
            "--trade-rpc-urls",
            "137=https://rpc.example/polygon",
        ])
        .unwrap();
        let c = cli;
        assert_eq!(
            c.marketplace.trade_rpc_urls.as_deref(),
            Some("137=https://rpc.example/polygon")
        );
    }

    #[test]
    fn ops_defaults_are_the_old_binaries_values_and_writes_are_off() {
        let c = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        assert!(
            c.shop.catalyrst_credits_admin_token.is_none()
                && c.shop.credits_captcha_secret.is_none()
                && c.shop.stripe_secret_key.is_none()
                && c.shop.stripe_webhook_secret.is_none()
        );
        assert_eq!(
            (
                c.shop.credits_captcha_verify_url.as_str(),
                c.shop.stripe_api_base.as_str(),
                c.shop.credits_currency.as_str()
            ),
            (
                "https://hcaptcha.com/siteverify",
                "https://api.stripe.com",
                "usd"
            )
        );
        // two defaults moved on purpose: 300 s staled the lane for part of every poll cycle;
        // "secondary" was the forbidden fulfilment (credits buying resale listings, SHOP.md)
        assert_eq!(
            (
                c.shop.marketplace_markup_bps,
                c.shop.mana_price_max_staleness_secs,
                c.shop.checkout_fulfillment_mode.as_str()
            ),
            (2500, 900, "primary")
        );
        assert_eq!(
            c.shop.checkout_fulfillment_mode,
            crate::config::DEFAULT_CHECKOUT_FULFILLMENT_MODE
        );
        assert!(
            c.shop.credits_require_purchase_intent
                && !c.shop.credits_mock_fulfillment
                && !c.shop.credits_mock_card
                && !c.shop.credits_onchain_authorize_enabled
        );
        assert_eq!(
            (
                c.shop.checkout_worker_interval_secs,
                c.shop.checkout_max_attempts,
                c.shop.escrow_lock_days
            ),
            (30, 5, 15)
        );
        assert!(c.shop.credits_signer_key.is_none() && c.shop.credits_manager_contract.is_none());
        assert!(
            c.shop.credits_checkout_success_url.is_empty()
                && c.shop.credits_checkout_cancel_url.is_empty()
        );
        assert!(
            !c.shop.shop_rentals_index_enabled,
            "the rentals index is opt-in"
        );
        assert!(
            !c.shop.credits_usage_index_enabled
                && c.shop.credits_usage_start_block.is_none()
                && c.shop.credits_usage_replay_from_block.is_none()
        );
        assert_eq!(
            (
                c.shop.credits_usage_index_interval_secs,
                c.shop.credits_usage_index_requests_per_pass
            ),
            (30, 50)
        );
    }

    #[test]
    fn the_credit_cap_adds_no_headroom_by_default_and_keeps_an_explicit_one() {
        let c = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        assert_eq!(
            (
                c.shop.credits_cap_headroom_pct,
                c.shop.credits_oracle_buffer_bps
            ),
            (100, 200)
        );
        let c = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--credits-cap-headroom-pct",
            "150",
        ])
        .unwrap();
        assert_eq!(c.shop.credits_cap_headroom_pct, 150);
    }
}
