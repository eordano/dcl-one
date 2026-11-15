//! Credits: the Marketplace Credits program (schema `credits`,
//! `schemas/60_ops.sql`).
//!
//! The old `catalyrst-credits` single (`credits.decentraland.org`, the data
//! bundle's :5146 behind the gateway's `/credits` prefix) as one module over
//! the ONE pool, the ONE outbound client (Stripe and hCaptcha through the
//! SSRF guard) and `jobs` for the two loops. Routes, mounted by `main.rs`
//! under `/credits`: `/users`, `/users/{wallet}/{progress,credits}`,
//! `/wallet/{wallet}/balance`, `/packs`, `/credits/packs`,
//! `/packs/{sku}/intent`, `/packs/{sku}/mock-purchase`, `/topup/mock-card`,
//! `/topup/mana[/quote]`, `/credits/checkout`, `/credits/orders/{id}`,
//! `/credits/authorize[/cancel]`, `/stripe/webhook`, `/cart`,
//! `/cart/items[/{collection}/{item}]`, `/checkout[/{id}]`, `/prices/quote`,
//! `/seasons`, seasonal photo goals and `/captcha`, and
//! the operator routes `/admin/credits/{grant,revoke}`,
//! `/admin/users/{a}/block`, `/admin/packs[/{sku}]`, `/admin/purchases`,
//! `/admin/checkouts[/{id}/{refund,force-fulfill}]`, `/admin/ledger`,
//! `/admin/grants/{ref}/{reclaim,release}`, `/admin/reconcile` behind
//! `CATALYRST_CREDITS_ADMIN_TOKEN` (`admin.rs`).
//!
//! Where each route reads and writes:
//! - the wallet routes, the cart and the checkout: local (`credits.wallets`,
//!   `ledger`, `idempotency`, `carts`, `cart_items`, `checkouts`,
//!   `fulfillment_outbox`); the item prices come from the marketplace items
//!   mirror and the open-orders readers, the MANA spot from `price::State`
//!   (`pricing::LocalMarket`; a source that is unset or not ready answers 503,
//!   a quote miss prices to `null`);
//! - the packs, intents, orders and the Stripe webhook: local
//!   (`credits.packs`, `purchases`, `payment_reversals`, `stripe_events`) plus
//!   Stripe's API;
//! - the MANA top-up, the fulfilment and the escrow ops: the economy module
//!   (`escrow::EconomyBroker`, the old HTTP hops to `/v1/payments/verify`,
//!   `/v1/broker/buy`, `/v1/escrow/{reclaim,release}` as handler calls with
//!   the economy bearer this process holds), the escrow leases in
//!   `shop.usage_grants` (the old `USAGE_GRANTS_PG_CONNECTION_STRING` side
//!   pool is the ONE database);
//! - `/credits/authorize`: local (`credits.authorizations`), off unless
//!   `CREDITS_ONCHAIN_AUTHORIZE_ENABLED` with `CREDITS_SIGNER_KEY(_FILE)` and
//!   `CREDITS_MANAGER_CONTRACT`, and minting only while the CreditUsed index
//!   (`usage.rs`, `credits.usages`) is ready: 503 naming
//!   `credits-usage-index` otherwise; the index consumes the pledges;
//! - `/usage-index/status`: the index's cursor, readiness and lag.
//!
//! Money: every balance and amount is `numeric` in SQL and a decimal string
//! in Rust, never a float (`money::CreditAmount` is the one gate);
//! `credits.wallets.available` and `earned_available` are projections of
//! `credits.ledger` written in the ledger insert's transaction under the
//! wallet row lock, and the reconciler asserts the signed sums equal them.
//! The three old idempotency tables are `credits.idempotency (scope, key)`.
//! Authorization contracts kept exactly: the Stripe webhook verifies
//! `Stripe-Signature` (HMAC over `t.body`, 5 minute tolerance) before any row
//! is touched and grants from the purchase row, never from event metadata;
//! the claim flow consumes a stored slider challenge and then the hCaptcha
//! token when `CREDITS_CAPTCHA_SECRET(_FILE)` is set; signed fetch binds the
//! wallet on every user route (over the public path or the route-local one);
//! the operator bearer is compared in constant time and fails closed.
//!
//! Every write is behind `OPS_WRITES_ENABLED` (`writes.rs`, the social
//! switch's twin; `POST /prices/quote` stays open, a read). Jobs:
//! `credits-fulfillment` (the outbox, every `CHECKOUT_WORKER_INTERVAL_SECS`
//! and on every checkout write), `credits-escrow-release` (leases past
//! their unlock, when the broker is configured) and `credits-usage-index`
//! (with `CREDITS_USAGE_INDEX_ENABLED`, `RPC_ENDPOINT_POLYGON` and
//! `CREDITS_MANAGER_CONTRACT`; it debits the ledger it folds, as the old
//! reconciler did, so it is local authority too: it runs in `serve` and
//! folds only with `OPS_WRITES_ENABLED` and fresh restore stamps, indexing
//! the log either way). Not carried: the `ts-rs`
//! bindings, the per-service `/health`, `/ping` (the content server owns
//! them), `CREDITS_PG_CONNECTION_STRING`, `MARKET_BASE_URL`,
//! `PRICE_BASE_URL`, `ECONOMY_BASE_URL`, `CATALYRST_ECONOMY_ADMIN_TOKEN` and
//! `LANDILER_ESCROW_ADDRESS` (the economy module's own), and
//! `USAGE_GRANTS_PG_CONNECTION_STRING`.

pub mod admin;
pub mod admin_store;
pub mod authorize;
pub mod batch;
pub mod captcha;
pub mod checkout;
pub mod digest;
pub mod dto;
pub mod errors;
pub mod escrow;
pub mod feed;
pub mod fulfillment;
pub mod goals;
pub mod handlers;
pub mod history;
pub mod money;
pub mod orders;
pub mod packs;
pub mod pricing;
pub mod purchase_intent;
pub mod reconcile;
pub mod settlement;
pub mod store;
pub mod stripe;
pub mod trial;
pub mod usage;
pub mod wallet;
pub mod wallet_flags;
pub mod writes;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod usage_tests;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{delete, get, post};
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::http::Outbound;
use crate::jobs::{Jobs, PeriodicCfg};

pub use errors::ApiError;
pub use store::Credits;

pub const DEFAULT_CAPTCHA_VERIFY_URL: &str = "https://hcaptcha.com/siteverify";
pub const DEFAULT_CREDITS_CURRENCY: &str = "usd";
pub const DEFAULT_MARKETPLACE_MARKUP_BPS: i64 = 2500;
// Defined in config, which parses them from the environment.
pub use crate::config::{DEFAULT_CHECKOUT_FULFILLMENT_MODE, DEFAULT_MANA_PRICE_MAX_STALENESS_SECS};
/// Only authorization expiry is time-bound (10 min TTL); checkout writes kick the worker.
pub const DEFAULT_CHECKOUT_WORKER_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_CHECKOUT_MAX_ATTEMPTS: i32 = 5;
pub const DEFAULT_ESCROW_LOCK_DAYS: i32 = 15;
pub const DEFAULT_REQUIRE_PURCHASE_INTENT: bool = true;
pub const DEFAULT_TOPUP_MIN_CONFIRMATIONS: u64 = 64;
const CHAIN_SEPOLIA: i64 = 11_155_111;

/// The raw values `main.rs` reads from `Config` (the old env names; the
/// secrets through `<NAME>_FILE`). No `Debug`: five fields are credentials.
#[derive(Clone)]
pub struct RawSettings {
    pub goal_flags_source: goals::flags::Source,
    /// `OPS_WRITES_ENABLED` (false).
    pub writes_enabled: bool,
    /// `CATALYRST_CREDITS_ADMIN_TOKEN` (or `_FILE`): the operator bearer; unset refuses the admin routes.
    pub admin_token: Option<String>,
    /// `CREDITS_CAPTCHA_SECRET` (or `_FILE`): the hCaptcha secret; unset skips the token check.
    pub captcha_secret: Option<String>,
    /// `CREDITS_CAPTCHA_VERIFY_URL` (`https://hcaptcha.com/siteverify`).
    pub captcha_verify_url: String,
    /// `STRIPE_SECRET_KEY` (or `_FILE`): enables the Stripe client.
    pub stripe_secret_key: Option<String>,
    /// `STRIPE_WEBHOOK_SECRET` (or `_FILE`): the webhook signature secret.
    pub stripe_webhook_secret: Option<String>,
    /// `STRIPE_API_BASE` (`https://api.stripe.com`).
    pub stripe_api_base: String,
    /// `CREDITS_CURRENCY` (usd).
    pub credits_currency: String,
    /// `MARKETPLACE_MARKUP_BPS` (2500).
    pub marketplace_markup_bps: i64,
    /// `MANA_PRICE_MAX_STALENESS_SECS` (300).
    pub mana_price_max_staleness_secs: i64,
    /// `CREDITS_USD_PEGGED_FEED_MAX_AGE_SECS` (3600).
    pub credits_usd_pegged_feed_max_age_secs: u64,
    /// `CHECKOUT_FULFILLMENT_MODE` (primary; the legacy secondary | auto run as primary).
    pub checkout_fulfillment_mode: String,
    /// `CREDITS_REQUIRE_PURCHASE_INTENT` (true).
    pub require_purchase_intent: bool,
    /// `CHECKOUT_WORKER_INTERVAL_SECS` (30).
    pub checkout_worker_interval_secs: u64,
    /// `CHECKOUT_MAX_ATTEMPTS` (5).
    pub checkout_max_attempts: i32,
    /// `ESCROW_LOCK_DAYS` (15).
    pub escrow_lock_days: i32,
    /// `CREDITS_MOCK_FULFILLMENT` (false): deliver off-chain, no broker call.
    pub mock_fulfillment: bool,
    /// `CREDITS_MOCK_CARD` (false): the mock purchase and top-up routes.
    pub mock_card: bool,
    /// `CREDITS_SIGNER_KEY_FILE` (the plain variable is refused): the treasury signer of `/credits/authorize`.
    pub credits_signer_key: Option<String>,
    /// `CREDITS_MANAGER_CONTRACT`.
    pub credits_manager_contract: Option<String>,
    /// `CREDITS_ONCHAIN_AUTHORIZE_ENABLED` (false).
    pub credits_onchain_authorize_enabled: bool,
    /// `CREDITS_CHECKOUT_SUCCESS_URL`, `CREDITS_CHECKOUT_CANCEL_URL` (empty: the order flow answers 503).
    pub checkout_success_url: String,
    pub checkout_cancel_url: String,
    /// `CREDITS_CHECKOUT_SUCCESS_URL_BUILDER`, `CREDITS_CHECKOUT_CANCEL_URL_BUILDER`:
    /// where a checkout the builder opened (`source: "builder"`) returns; unset,
    /// it returns to the Shop pair like every other surface.
    pub checkout_success_url_builder: Option<String>,
    pub checkout_cancel_url_builder: Option<String>,
    /// `CREDITS_USAGE_INDEX_ENABLED` (false): attach the CreditUsed index.
    pub usage_index_enabled: bool,
    /// `RPC_ENDPOINT_POLYGON` (or `_FILE`): the collections chain's JSON-RPC the index reads.
    pub usage_rpc_url: Option<String>,
    /// `COLLECTIONS_CHAIN_ID` (137).
    pub usage_chain_id: i64,
    /// `CREDITS_USAGE_START_BLOCK`: unset, the chain's known start (`usage::Settings::default_start_block`).
    pub usage_start_block: Option<u64>,
    /// `CREDITS_TOPUP_START_BLOCK`: a MANA top-up mined before it is refused.
    pub topup_start_block: Option<u64>,
    /// MANA top-ups can verify payments: the economy bearer, broadcasting and a relayer key.
    pub topups_enabled: bool,
    /// `CREDITS_TOPUP_MIN_CONFIRMATIONS` (64).
    pub topup_min_confirmations: u64,
    /// `CREDITS_USAGE_INDEX_INTERVAL_SECS` (30).
    pub usage_interval_secs: u64,
    /// `CREDITS_USAGE_INDEX_REQUESTS_PER_PASS` (50).
    pub usage_requests_per_pass: usize,
    /// `CREDITS_USAGE_REPLAY_FROM_BLOCK`.
    pub usage_replay_from_block: Option<u64>,
    /// `CREDITS_USAGE_STOP_BLOCK`.
    pub usage_stop_block: Option<u64>,
    /// `CREDITS_TTL_SECONDS` (300): how long a signed credit stays spendable (upstream `USD_CREDIT_TTL_SECONDS`).
    pub credit_ttl_seconds: u64,
    /// `CREDITS_CAP_HEADROOM_PCT` (100): the signed value over the quoted MANA need, 100..=150 (upstream `USD_CREDIT_CAP_HEADROOM_PCT`).
    pub credit_cap_headroom_pct: u32,
    /// `CREDITS_ORACLE_BUFFER_BPS` (200): the oracle buffer inside the sizing, at most 1000 (upstream `USD_CREDIT_BUFFER_BPS`).
    pub credit_buffer_bps: u32,
    /// `CREDITS_REVOKER_KEY_FILE` (the plain variable is refused): the `CREDITS_REVOKER_ROLE` signer of `revokeCredits`; unset, revocations queue and wait.
    pub credits_revoker_key: Option<String>,
    /// `CREDITS_TRIAL_MODE` (false): a closed trial (`trial.rs`).
    pub trial_mode: bool,
    /// `CREDITS_TRIAL_BUYERS` (or `_FILE`): the trial's buyers.
    pub trial_buyers: Option<String>,
    /// `SHOP_BASE_URL`: the grant notification's link (the old `${DECENTRALAND_URL}/shop`).
    pub shop_base_url: String,
}

impl Default for RawSettings {
    fn default() -> Self {
        Self {
            goal_flags_source: goals::flags::Source::default(),
            writes_enabled: false,
            admin_token: None,
            captcha_secret: None,
            captcha_verify_url: DEFAULT_CAPTCHA_VERIFY_URL.to_string(),
            stripe_secret_key: None,
            stripe_webhook_secret: None,
            stripe_api_base: stripe::DEFAULT_STRIPE_API_BASE.to_string(),
            credits_currency: DEFAULT_CREDITS_CURRENCY.to_string(),
            marketplace_markup_bps: DEFAULT_MARKETPLACE_MARKUP_BPS,
            mana_price_max_staleness_secs: DEFAULT_MANA_PRICE_MAX_STALENESS_SECS,
            credits_usd_pegged_feed_max_age_secs: 3_600,
            checkout_fulfillment_mode: DEFAULT_CHECKOUT_FULFILLMENT_MODE.to_string(),
            require_purchase_intent: DEFAULT_REQUIRE_PURCHASE_INTENT,
            checkout_worker_interval_secs: DEFAULT_CHECKOUT_WORKER_INTERVAL_SECS,
            checkout_max_attempts: DEFAULT_CHECKOUT_MAX_ATTEMPTS,
            escrow_lock_days: DEFAULT_ESCROW_LOCK_DAYS,
            mock_fulfillment: false,
            mock_card: false,
            credits_signer_key: None,
            credits_manager_contract: None,
            credits_onchain_authorize_enabled: false,
            checkout_success_url: String::new(),
            checkout_cancel_url: String::new(),
            checkout_success_url_builder: None,
            checkout_cancel_url_builder: None,
            usage_index_enabled: false,
            usage_rpc_url: None,
            usage_chain_id: usage::CHAIN_POLYGON,
            usage_start_block: None,
            topup_start_block: None,
            topups_enabled: false,
            topup_min_confirmations: DEFAULT_TOPUP_MIN_CONFIRMATIONS,
            usage_interval_secs: 30,
            usage_requests_per_pass: 50,
            usage_replay_from_block: None,
            usage_stop_block: None,
            credit_ttl_seconds: batch::DEFAULT_TTL_SECONDS,
            credit_cap_headroom_pct: batch::DEFAULT_CAP_HEADROOM_PCT,
            credit_buffer_bps: batch::DEFAULT_ORACLE_BUFFER_BPS,
            credits_revoker_key: None,
            trial_mode: false,
            trial_buyers: None,
            shop_base_url: crate::subscriptions::DEFAULT_SHOP_BASE_URL.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub goal_flags_source: goals::flags::Source,
    pub writes_enabled: bool,
    pub admin_token: Option<String>,
    pub captcha_secret: Option<String>,
    pub captcha_verify_url: String,
    pub stripe_secret_key: Option<String>,
    pub stripe_webhook_secret: Option<String>,
    pub stripe_api_base: String,
    pub credits_currency: String,
    pub marketplace_markup_bps: i64,
    pub mana_price_max_staleness_secs: i64,
    pub credits_usd_pegged_feed_max_age_secs: u64,
    pub checkout_fulfillment_mode: String,
    pub require_purchase_intent: bool,
    pub checkout_worker_interval: Duration,
    pub checkout_max_attempts: i32,
    pub escrow_lock_days: i32,
    pub mock_fulfillment: bool,
    pub mock_card: bool,
    pub credits_signer_key: Option<String>,
    pub credits_manager_contract: Option<String>,
    pub credits_onchain_authorize_enabled: bool,
    pub checkout_success_url: String,
    pub checkout_cancel_url: String,
    /// Both set or neither (a half-configured pair falls back to the Shop's).
    pub checkout_builder_urls: Option<(String, String)>,
    /// The CreditUsed index (`usage.rs`); its contract is `credits_manager_contract`.
    pub usage: usage::Settings,
    /// Which MANA top-up transfers are credited (`handlers::topup`).
    pub topup: handlers::topup::TopupPolicy,
    pub credit_ttl_seconds: u64,
    pub credit_cap_headroom_pct: u32,
    pub credit_buffer_bps: u32,
    pub credits_revoker_key: Option<String>,
    /// `Some` under `CREDITS_TRIAL_MODE`: only these buyers get new credit authorizations, and
    /// the MANA top-up routes refuse.
    pub trial: Option<trial::TrialBuyers>,
    pub shop_base_url: String,
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn parse_fulfillment_mode(raw: &str) -> anyhow::Result<String> {
    let mode = raw.trim().to_ascii_lowercase();
    match mode.as_str() {
        "primary" => Ok(mode),
        "secondary" | "auto" => {
            tracing::warn!(
                requested = %mode,
                "CHECKOUT_FULFILLMENT_MODE={mode} is not a fulfilment source any more: credits pay for CollectionStore mints only (SHOP.md; shop::guards refuses a resale line at every entry); running as primary"
            );
            Ok(DEFAULT_CHECKOUT_FULFILLMENT_MODE.to_string())
        }
        other => anyhow::bail!(
            "invalid CHECKOUT_FULFILLMENT_MODE {other:?} (expected \"primary\"; the legacy \"secondary\" and \"auto\" run as primary)"
        ),
    }
}

impl Settings {
    /// Names the variable in every refusal, never a value. The two external
    /// endpoints (Stripe, hCaptcha) must be public https: both carry a secret.
    pub fn parse(raw: RawSettings) -> anyhow::Result<Self> {
        let goal_flags_source = raw.goal_flags_source.normalize()?;
        let stripe_api_base = raw.stripe_api_base.trim().trim_end_matches('/').to_string();
        let stripe_api_base = if stripe_api_base.is_empty() {
            stripe::DEFAULT_STRIPE_API_BASE.to_string()
        } else {
            stripe_api_base
        };
        let captcha_verify_url = raw.captcha_verify_url.trim().to_string();
        let captcha_verify_url = if captcha_verify_url.is_empty() {
            DEFAULT_CAPTCHA_VERIFY_URL.to_string()
        } else {
            captcha_verify_url
        };
        let stripe_secret_key = non_empty(raw.stripe_secret_key);
        let captcha_secret = non_empty(raw.captcha_secret);
        if stripe_secret_key.is_some()
            && !(stripe_api_base.starts_with("https://")
                && crate::http::is_safe_http_url(&stripe_api_base))
        {
            anyhow::bail!("STRIPE_API_BASE must be a public https:// endpoint");
        }
        if captcha_secret.is_some()
            && !(captcha_verify_url.starts_with("https://")
                && crate::http::is_safe_http_url(&captcha_verify_url))
        {
            anyhow::bail!("CREDITS_CAPTCHA_VERIFY_URL must be a public https:// endpoint");
        }
        let credits_signer_key = non_empty(raw.credits_signer_key);
        let credits_manager_contract = match non_empty(raw.credits_manager_contract) {
            Some(raw) => Some(usage::normalize_address(&raw).ok_or_else(|| {
                anyhow::anyhow!("CREDITS_MANAGER_CONTRACT must be a 0x-prefixed 20-byte address")
            })?),
            None => None,
        };
        let usage_rpc_url = non_empty(raw.usage_rpc_url);
        if raw.usage_index_enabled
            && let Some(url) = &usage_rpc_url
            && !(url.starts_with("https://") && crate::http::is_safe_http_url(url))
        {
            anyhow::bail!(
                "RPC_ENDPOINT_POLYGON must be a public https:// endpoint for the credits usage index"
            );
        }
        if raw.usage_index_enabled
            && (usage_rpc_url.is_none() || credits_manager_contract.is_none())
        {
            tracing::warn!(
                "CREDITS_USAGE_INDEX_ENABLED is true but RPC_ENDPOINT_POLYGON or CREDITS_MANAGER_CONTRACT is unset: the CreditUsed index stays off and POST /credits/authorize answers 503"
            );
        }
        let usage = usage::Settings {
            enabled: raw.usage_index_enabled,
            rpc_url: usage_rpc_url,
            chain_id: raw.usage_chain_id,
            contract: credits_manager_contract.clone(),
            start_block: raw
                .usage_start_block
                .unwrap_or_else(|| usage::Settings::default_start_block(raw.usage_chain_id)),
            interval: Duration::from_secs(raw.usage_interval_secs.max(5)),
            requests_per_pass: raw.usage_requests_per_pass.max(1),
            replay_from_block: raw.usage_replay_from_block,
            stop_block: raw.usage_stop_block,
        };
        if credits_signer_key.is_some() && !raw.credits_onchain_authorize_enabled {
            tracing::warn!(
                "CREDITS_SIGNER_KEY is set but CREDITS_ONCHAIN_AUTHORIZE_ENABLED is false: POST /credits/authorize refuses to mint treasury-claim signatures (503)"
            );
        }
        if raw.credits_onchain_authorize_enabled
            && (credits_signer_key.is_none() || credits_manager_contract.is_none())
        {
            tracing::warn!(
                "CREDITS_ONCHAIN_AUTHORIZE_ENABLED is true but CREDITS_SIGNER_KEY or CREDITS_MANAGER_CONTRACT is unset: POST /credits/authorize stays unavailable (503)"
            );
        }
        // The mock card grants credits for nothing. On a chain where credits are worth something, the
        // on-chain authorization, or any later spending path, would turn those into real MANA paid by the
        // treasury, so the mock routes run only where the credits chain is a testnet (Amoy).
        // Without a start block, every earlier MANA transfer to the relayer could be claimed.
        if raw.topups_enabled
            && !raw.trial_mode
            && raw.topup_start_block.is_none()
            && !matches!(raw.usage_chain_id, usage::CHAIN_AMOY | CHAIN_SEPOLIA)
        {
            anyhow::bail!(
                "CREDITS_TOPUP_START_BLOCK is required on chain {} while MANA top-ups are enabled: set it to the block at which top-ups start for the relayer address",
                raw.usage_chain_id
            );
        }
        if raw.mock_card && raw.usage_chain_id != usage::CHAIN_AMOY {
            anyhow::bail!(
                "CREDITS_MOCK_CARD grants credits without a charge and runs only on Amoy ({}); COLLECTIONS_CHAIN_ID is {}",
                usage::CHAIN_AMOY,
                raw.usage_chain_id
            );
        }
        if stripe_secret_key.is_none() {
            tracing::warn!(
                "STRIPE_SECRET_KEY unset: card-purchase routes are disabled (POST /packs/{{sku}}/intent answers 501, /credits/checkout 503)"
            );
        }
        if !(100..=crate::types::CreditSizing::MAX_HEADROOM_PCT)
            .contains(&raw.credit_cap_headroom_pct)
        {
            anyhow::bail!(
                "CREDITS_CAP_HEADROOM_PCT must be between 100 and {} (docs/shop/CONTRACTS.md, \"No spendable remainder\")",
                crate::types::CreditSizing::MAX_HEADROOM_PCT
            );
        }
        if raw.credits_usd_pegged_feed_max_age_secs == 0
            || raw.credits_usd_pegged_feed_max_age_secs > i64::MAX as u64
        {
            anyhow::bail!(
                "CREDITS_USD_PEGGED_FEED_MAX_AGE_SECS must be between 1 and {}: 0 refuses every USD-pegged credit, and a larger value cannot be compared with a timestamp",
                i64::MAX
            );
        }
        if raw.credit_buffer_bps > crate::types::CreditSizing::MAX_BUFFER_BPS {
            anyhow::bail!(
                "CREDITS_ORACLE_BUFFER_BPS must be at most {}",
                crate::types::CreditSizing::MAX_BUFFER_BPS
            );
        }
        if !(30..=3600).contains(&raw.credit_ttl_seconds) {
            anyhow::bail!("CREDITS_TTL_SECONDS must be between 30 and 3600");
        }
        let credits_revoker_key = non_empty(raw.credits_revoker_key);
        if credits_revoker_key.is_some() && !usage.is_configured() {
            tracing::warn!(
                "CREDITS_REVOKER_KEY is set but the CreditUsed index is not configured: revocations queue and are never sent"
            );
        }
        // A trial issues credits through the on-chain authorization alone, to the buyers it
        // names, and revokes each credit it sees consumed: no card rail, no mock card, and a
        // revoker, or it does not start. An unreadable CREDITS_TRIAL_BUYERS_FILE already failed
        // the start (Config::resolve_secret_files) and can't reach here as an empty list.
        let trial = if raw.trial_mode {
            if !raw.writes_enabled {
                anyhow::bail!(
                    "CREDITS_TRIAL_MODE needs OPS_WRITES_ENABLED=true: with the switch off every credit write, an authorization or an admin grant, answers 503"
                );
            }
            if stripe_secret_key.is_some() {
                anyhow::bail!("CREDITS_TRIAL_MODE takes no card rail: unset STRIPE_SECRET_KEY");
            }
            // The webhook alone still credits a card payment started before the trial.
            if non_empty(raw.stripe_webhook_secret.clone()).is_some() {
                anyhow::bail!("CREDITS_TRIAL_MODE takes no card rail: unset STRIPE_WEBHOOK_SECRET");
            }
            // Closed and complete: a trial that would answer every authorization with 503 does
            // not start either.
            if !(raw.credits_onchain_authorize_enabled
                && credits_signer_key.is_some()
                && credits_manager_contract.is_some()
                && usage.is_configured())
            {
                anyhow::bail!(
                    "CREDITS_TRIAL_MODE needs on-chain authorization: CREDITS_ONCHAIN_AUTHORIZE_ENABLED=true, CREDITS_SIGNER_KEY_FILE, CREDITS_MANAGER_CONTRACT and the CreditUsed index (CREDITS_USAGE_INDEX_ENABLED, RPC_ENDPOINT_POLYGON)"
                );
            }
            if raw.mock_card {
                anyhow::bail!("CREDITS_TRIAL_MODE takes no mock card: unset CREDITS_MOCK_CARD");
            }
            // Parsed as the revoker's signer parses it: a key that would fail there (malformed, or
            // a 32-byte scalar off the curve such as zero) would leave the node issuing without a
            // revoker, since a revoker that can't be built is only logged.
            match credits_revoker_key.as_deref() {
                None => anyhow::bail!(
                    "CREDITS_TRIAL_MODE needs CREDITS_REVOKER_KEY_FILE: every consumed credit is revoked"
                ),
                Some(key) if crate::evm::signer::parse_signing_key(key).is_err() => anyhow::bail!(
                    "CREDITS_TRIAL_MODE needs a valid CREDITS_REVOKER_KEY_FILE: not a 32-byte secp256k1 key"
                ),
                Some(_) => {}
            }
            let buyers = trial::TrialBuyers::parse(raw.trial_buyers.as_deref())?;
            if buyers.is_empty() {
                tracing::warn!(
                    "CREDITS_TRIAL_MODE is on and CREDITS_TRIAL_BUYERS names no one: no new credit is authorized"
                );
            } else {
                tracing::info!(
                    buyers = buyers.len(),
                    "CREDITS_TRIAL_MODE: new credit authorizations only for the trial's buyers; MANA top-ups refuse"
                );
            }
            Some(buyers)
        } else {
            if non_empty(raw.trial_buyers).is_some() {
                tracing::warn!(
                    "CREDITS_TRIAL_BUYERS is set but CREDITS_TRIAL_MODE is false: ignored"
                );
            }
            None
        };
        Ok(Self {
            goal_flags_source,
            writes_enabled: raw.writes_enabled,
            admin_token: non_empty(raw.admin_token),
            captcha_secret,
            captcha_verify_url,
            stripe_secret_key,
            stripe_webhook_secret: non_empty(raw.stripe_webhook_secret),
            stripe_api_base,
            credits_currency: if raw.credits_currency.trim().is_empty() {
                DEFAULT_CREDITS_CURRENCY.to_string()
            } else {
                raw.credits_currency.trim().to_string()
            },
            marketplace_markup_bps: raw.marketplace_markup_bps,
            mana_price_max_staleness_secs: raw.mana_price_max_staleness_secs,
            credits_usd_pegged_feed_max_age_secs: raw.credits_usd_pegged_feed_max_age_secs,
            checkout_fulfillment_mode: parse_fulfillment_mode(&raw.checkout_fulfillment_mode)?,
            require_purchase_intent: raw.require_purchase_intent,
            checkout_worker_interval: Duration::from_secs(raw.checkout_worker_interval_secs.max(1)),
            checkout_max_attempts: raw.checkout_max_attempts.max(1),
            escrow_lock_days: raw.escrow_lock_days,
            mock_fulfillment: raw.mock_fulfillment,
            mock_card: raw.mock_card,
            credits_signer_key,
            credits_manager_contract,
            credits_onchain_authorize_enabled: raw.credits_onchain_authorize_enabled,
            checkout_success_url: raw.checkout_success_url.trim().to_string(),
            checkout_cancel_url: raw.checkout_cancel_url.trim().to_string(),
            checkout_builder_urls: non_empty(raw.checkout_success_url_builder)
                .zip(non_empty(raw.checkout_cancel_url_builder)),
            usage,
            topup: handlers::topup::TopupPolicy {
                start_block: raw.topup_start_block,
                min_confirmations: raw.topup_min_confirmations,
            },
            credit_ttl_seconds: raw.credit_ttl_seconds,
            credit_cap_headroom_pct: raw.credit_cap_headroom_pct,
            credit_buffer_bps: raw.credit_buffer_bps,
            credits_revoker_key,
            trial,
            shop_base_url: non_empty(Some(raw.shop_base_url))
                .unwrap_or_else(|| crate::subscriptions::DEFAULT_SHOP_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
        })
    }

    /// Where Stripe returns the buyer, per surface: server-owned (a URL in the
    /// body would be an open redirect), the builder's pair only for a builder
    /// checkout on a node that configured one.
    pub fn checkout_return_urls(&self, source: Option<&str>) -> (&str, &str) {
        match (&self.checkout_builder_urls, source) {
            (Some((success, cancel)), Some("builder")) => (success, cancel),
            _ => (&self.checkout_success_url, &self.checkout_cancel_url),
        }
    }

    pub fn for_tests() -> Self {
        Self::parse(RawSettings {
            writes_enabled: true,
            require_purchase_intent: false,
            mana_price_max_staleness_secs: 3600,
            credits_usd_pegged_feed_max_age_secs: 3_600,
            ..RawSettings::default()
        })
        .expect("the defaults parse")
    }
}

/// The sources a credits state is built over: the marketplace and price
/// modules (pricing) and the economy module (the broker), each optional so
/// a test or a node without the data plane can leave them out.
pub struct Sources {
    pub market: Option<Arc<crate::market::MarketplaceState>>,
    pub price: Option<Arc<crate::price::State>>,
    pub economy: Option<Arc<crate::evm::State>>,
}

pub struct State {
    pub settings: Settings,
    pub credits: Credits,
    pub pricing: Arc<pricing::Pricing>,
    pub broker: Arc<dyn escrow::Broker>,
    pub stripe: Option<stripe::StripeClient>,
    pub captcha: Option<captcha::CaptchaProvider>,
    pub quote_cache: handlers::prices::QuoteCache,
    pub eip1654: Option<Arc<dyn Eip1654Validator>>,
    /// The CreditUsed index: the authorize lane's readiness and the fold.
    pub usage: Arc<usage::UsageIndex>,
    /// The issuance gate over the configured CreditsManager (`settlement.rs`).
    pub gate: Arc<dyn settlement::IssuanceGate>,
    /// The `revokeCredits` sender, with `CREDITS_REVOKER_KEY_FILE` over the index's endpoint.
    pub revoker: Option<Arc<settlement::Revoker>>,
    /// The MANA/USD feed a USD-pegged group's marketplace settles at (`feed.rs`).
    pub feed: Arc<dyn feed::MarketplaceFeed>,
    /// Existing chain transport for the seasonal program paused() view.
    pub goals_chain: Option<Arc<crate::evm::chain::ChainComponent>>,
    pub goal_flags: goals::flags::Flags,
    /// Rotating in-process scan hint; receipts alone acknowledge source events.
    pub(crate) goal_scan_after: std::sync::atomic::AtomicI64,
    /// Fixed upper bound for the current round, so arrivals cannot postpone retries.
    pub(crate) goal_scan_horizon: std::sync::atomic::AtomicI64,
    pub(crate) goal_scan_reconcile_at_ms: std::sync::atomic::AtomicI64,
    /// An event failed since the last round started: the next round starts
    /// from the beginning without waiting for the reconcile interval.
    pub(crate) goal_scan_retry: std::sync::atomic::AtomicBool,
}

pub type SharedState = Arc<State>;

impl State {
    pub fn new(
        pool: PgPool,
        http: Outbound,
        settings: Settings,
        sources: Sources,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        let market: Arc<dyn pricing::Market> = Arc::new(pricing::LocalMarket {
            pool: pool.clone(),
            market: sources.market,
            price: sources.price,
            max_staleness_secs: settings.mana_price_max_staleness_secs,
        });
        let broker: Arc<dyn escrow::Broker> = match sources.economy {
            Some(economy) => Arc::new(escrow::EconomyBroker::new(economy)),
            None => Arc::new(escrow::NoBroker),
        };
        Self::with_sources(pool, http, settings, market, broker, eip1654)
    }

    /// The constructor the tests use with a fake market and broker.
    pub fn with_sources(
        pool: PgPool,
        http: Outbound,
        settings: Settings,
        market: Arc<dyn pricing::Market>,
        broker: Arc<dyn escrow::Broker>,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        let stripe = settings.stripe_secret_key.clone().map(|secret| {
            stripe::StripeClient::new(secret, settings.stripe_api_base.clone(), http.clone())
        });
        let captcha = settings.captcha_secret.clone().map(|secret| {
            captcha::CaptchaProvider::new(secret, settings.captcha_verify_url.clone(), http.clone())
        });
        let pricing = Arc::new(pricing::Pricing::new(
            market,
            settings.marketplace_markup_bps,
        ));
        let mut credits = Credits::new(pool).with_shop_base_url(&settings.shop_base_url);
        if settings.usage.is_configured() {
            credits = credits.with_usage_index(settings.usage.chain_id);
        }
        let usage = Arc::new(usage::UsageIndex::new(
            credits.clone(),
            http.clone(),
            settings.usage.clone(),
        ));
        let chain = settings
            .usage
            .is_configured()
            .then_some(settings.usage.rpc_url.as_deref())
            .flatten()
            .map(|url| {
                Arc::new(crate::evm::chain::ChainComponent::new(
                    http.clone(),
                    url,
                    std::time::Duration::from_secs(10),
                ))
            });
        let gate: Arc<dyn settlement::IssuanceGate> = match &chain {
            Some(chain) => Arc::new(settlement::ChainGate::new(
                chain.clone(),
                settings.usage.chain_id as u64,
            )),
            None => Arc::new(settlement::ClosedGate),
        };
        let feed: Arc<dyn feed::MarketplaceFeed> = match &chain {
            Some(chain) => Arc::new(feed::ChainFeed::new(
                chain.clone(),
                settings.credits_usd_pegged_feed_max_age_secs,
            )),
            None => Arc::new(feed::NoFeed),
        };
        let revoker = match (&chain, settings.credits_revoker_key.as_deref()) {
            (Some(chain), Some(key)) => match settlement::Revoker::new(
                credits.clone(),
                chain.clone(),
                key,
                settings.usage.chain_id,
            ) {
                Ok(revoker) => Some(Arc::new(revoker)),
                Err(e) => {
                    tracing::error!("{e}: revocations queue and are never sent");
                    None
                }
            },
            _ => None,
        };
        let goal_flags = goals::flags::Flags::new(settings.goal_flags_source.clone(), http);
        Self {
            goal_flags,
            goal_scan_after: std::sync::atomic::AtomicI64::new(0),
            goal_scan_horizon: std::sync::atomic::AtomicI64::new(0),
            goal_scan_reconcile_at_ms: std::sync::atomic::AtomicI64::new(0),
            goal_scan_retry: std::sync::atomic::AtomicBool::new(false),
            credits,
            usage,
            pricing,
            broker,
            stripe,
            captcha,
            quote_cache: handlers::prices::QuoteCache::default(),
            eip1654,
            settings,
            gate,
            revoker,
            feed,
            goals_chain: chain,
        }
    }

    /// The tests' gate in place of the chain read.
    pub fn with_gate(mut self, gate: Arc<dyn settlement::IssuanceGate>) -> Self {
        self.gate = gate;
        self
    }

    /// The tests' MANA/USD feed in place of the chain read.
    pub fn with_feed(mut self, feed: Arc<dyn feed::MarketplaceFeed>) -> Self {
        self.feed = feed;
        self
    }

    /// The fulfilment worker (always: it idles without a broker), the
    /// escrow release worker (only with one), the settlement job, the
    /// wallet flags listener and the CreditUsed index, all under the shared
    /// lifecycle. `serve` runs them: the usage index's fold debits the
    /// ledger, so it runs beside the other credits writers, under the
    /// restore stamps `serve` installs before its jobs start.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs) {
        fulfillment::OutboxWorker {
            credits: self.credits.clone(),
            pricing: self.pricing.clone(),
            broker: self.broker.clone(),
            max_attempts: self.settings.checkout_max_attempts,
            escrow_lock_days: self.settings.escrow_lock_days,
            mock_fulfillment: self.settings.mock_fulfillment,
        }
        .spawn(jobs, self.settings.checkout_worker_interval);
        escrow::ReleaseWorker {
            credits: self.credits.clone(),
            broker: self.broker.clone(),
        }
        .spawn(jobs, self.settings.checkout_worker_interval);
        settlement::spawn_settlement_job(
            jobs,
            self.credits.clone(),
            self.revoker.clone(),
            self.settings.writes_enabled,
            self.settings.checkout_worker_interval,
        );
        wallet_flags::spawn(jobs, self.credits.clone(), self.settings.writes_enabled);
        self.spawn_usage_index(jobs);
        if self.settings.writes_enabled {
            tracing::warn!(
                "OPS_WRITES_ENABLED=true: this node is the authority for the credits ledger, checkouts and authorizations"
            );
        } else {
            tracing::info!(
                "OPS_WRITES_ENABLED=false: credits routes serve reads and quotes; every write answers 503"
            );
        }
    }

    /// The `credits-usage-index` job, when `CREDITS_USAGE_INDEX_ENABLED`, the
    /// RPC and the contract are set: one pass per interval with a 5 s
    /// first-fire jitter, a failing pass backing off to ten minutes at most
    /// (the authorize lane closes once the fold is 10 minutes old). The fold
    /// debits wallets, so with `OPS_WRITES_ENABLED` off, or a pass the
    /// restore stamps hold, the job indexes the log (regenerable from the
    /// chain) and publishes nothing: the lane stays closed. Private: only
    /// [`Self::spawn_jobs`] starts it, in a process whose restore stamps are
    /// installed.
    fn spawn_usage_index(self: &Arc<Self>, jobs: &Jobs) {
        let settings = &self.settings.usage;
        if !settings.enabled {
            tracing::info!(
                "CREDITS_USAGE_INDEX_ENABLED=false: the CreditUsed index is off; POST /credits/authorize answers 503"
            );
            return;
        }
        if !settings.is_configured() {
            return;
        }
        let index = self.usage.clone();
        let writes_enabled = self.settings.writes_enabled;
        let gate = Arc::new(crate::shared::writes::PassGate::new(
            usage::JOB_NAME,
            crate::shared::writes::ops_refusal,
        ));
        let interval = settings.interval;
        tracing::info!(chain_id = settings.chain_id, contract = ?settings.contract, start_block = settings.start_block, "credits usage index started");
        jobs.spawn_periodic_leased(
            usage::JOB_NAME,
            PeriodicCfg::every(interval)
                .with_jitter(Duration::from_secs(5))
                .with_error_backoff(Duration::from_secs(600).max(interval)),
            move |ctx| {
                let index = index.clone();
                let gate = gate.clone();
                async move {
                    index
                        .gated_pass(Some(&ctx), &gate, writes_enabled)
                        .await
                        .map(|_| ())
                }
            },
        );
    }

    /// `/admin/reconcile`: the ledger invariants plus the CreditUsed pass
    /// (the full fold over every indexed usage, retrying a debit the job
    /// could not make); `ok` is false while a used credit stays unpaid.
    pub async fn reconcile_report(&self) -> Result<reconcile::ReconcileReport, ApiError> {
        let mut report = self.credits.reconcile().await?;
        let settings = &self.usage.settings;
        let (Some(contract), true) = (settings.contract.as_deref(), settings.is_configured())
        else {
            return Ok(report);
        };
        if !self.settings.writes_enabled {
            report.credit_usage_index = "writes_disabled";
            return Ok(report);
        }
        let ready = self.usage.ready_at(chrono::Utc::now()).await.is_some();
        report.credit_usage_index = if ready { "ready" } else { "not_ready" };
        let Some(cursor) = usage::cursor(&self.credits.pool, settings.chain_id).await? else {
            return Ok(report);
        };
        let diffs = usage::fold(
            &self.credits,
            settings.chain_id,
            contract,
            -1,
            cursor.last_block,
        )
        .await?;
        let unpaid: Vec<&str> = diffs
            .iter()
            .filter(|d| !d.consumed)
            .map(|d| d.credit_id.as_str())
            .collect();
        if !unpaid.is_empty() {
            tracing::error!(
                invariant = "credit_used==debited",
                count = unpaid.len(),
                credit_ids = ?unpaid,
                "RECONCILE ALERT: on-chain CreditUsed events could not debit the wallet balance"
            );
            report.ok = false;
        }
        report.credit_usages_reconciled = diffs;
        Ok(report)
    }

    /// The fulfilment worker as a value, for a test driving `run_once` by hand.
    pub fn outbox_worker(&self) -> fulfillment::OutboxWorker {
        fulfillment::OutboxWorker {
            credits: self.credits.clone(),
            pricing: self.pricing.clone(),
            broker: self.broker.clone(),
            max_attempts: self.settings.checkout_max_attempts,
            escrow_lock_days: self.settings.escrow_lock_days,
            mock_fulfillment: self.settings.mock_fulfillment,
        }
    }
}

/// The old `api_router` (every user route, the admin routes, the 64 KiB body
/// cap) behind the write gate; `main.rs` nests it under `/credits`, the
/// legacy alias of [`shop_router`]: same handlers, exact old paths, a signed
/// request verified over the path it was signed for. `/webhook` is the
/// upstream credits-server Stripe path beside v2's `/stripe/webhook`; both
/// stay registered with Stripe until rotated ([`crate::paths::ALIASES`]).
pub fn router(state: SharedState) -> Router {
    let writes_enabled = state.settings.writes_enabled;
    let api = Router::new()
        .route("/users", post(handlers::users::enroll))
        .route("/users/{wallet_id}", delete(handlers::users::unregister))
        .route("/users/{wallet_id}/status", get(handlers::users::status))
        .route(
            "/users/{wallet_id}/progress",
            get(handlers::users::progress),
        )
        .route(
            "/users/{wallet_id}/credits",
            get(handlers::users::user_credits),
        )
        .route("/wallet/{wallet_id}/balance", get(handlers::users::balance))
        .route("/packs", get(handlers::packs::list_packs))
        .route("/credits/packs", get(handlers::packs::unity_list_packs))
        .route("/packs/{sku}/intent", post(handlers::packs::create_intent))
        .route("/credits/checkout", post(handlers::orders::create_checkout))
        .route(
            "/credits/orders/{order_id}",
            get(handlers::orders::order_status),
        )
        .route("/credits/authorize", post(handlers::authorize::authorize))
        .route(
            "/credits/authorize/batch",
            post(handlers::authorize::authorize_batch),
        )
        .route(
            "/credits/authorize/cancel",
            post(handlers::authorize::cancel),
        )
        .route(
            "/credits/authorize/submitted",
            post(handlers::authorize::submitted),
        )
        .route(
            "/credits/orders/{order_id}/cancel",
            post(handlers::orders::cancel_order),
        )
        .route(
            "/credits/orders/{order_id}/resume",
            post(handlers::orders::resume_order),
        )
        .route(
            "/credits/orders/{order_id}/cancel-return",
            get(handlers::orders::cancel_return),
        )
        .route(
            "/users/{wallet_id}/purchases",
            get(handlers::history::purchases),
        )
        .route(
            "/users/{wallet_id}/credit-orders",
            get(handlers::history::credit_orders),
        )
        .route(
            "/credits-name-route",
            get(handlers::authorize::name_credit_route),
        )
        .route(
            "/packs/{sku}/mock-purchase",
            post(handlers::packs::mock_purchase),
        )
        .route("/topup/mock-card", post(handlers::packs::mock_topup))
        .route("/topup/mana", post(handlers::topup::mana_topup))
        .route("/topup/mana/quote", get(handlers::topup::mana_topup_quote))
        .route("/stripe/webhook", post(handlers::stripe::webhook))
        .route("/webhook", post(handlers::stripe::webhook))
        .route("/cart", get(handlers::cart::get_cart))
        .route("/cart/items", post(handlers::cart::add_item))
        .route(
            "/cart/items/{collection}/{item_id}",
            delete(handlers::cart::remove_item),
        )
        .route("/checkout", post(handlers::cart::checkout))
        .route("/checkout/{id}", get(handlers::cart::get_checkout))
        .route("/prices/quote", post(handlers::prices::quote))
        .route("/seasons", get(handlers::users::seasons))
        .route("/season", get(handlers::users::season))
        .route(
            "/captcha",
            get(handlers::captcha::generate).post(handlers::captcha::claim),
        )
        .route(
            "/usage-index/status",
            get(handlers::authorize::usage_index_status),
        )
        .merge(handlers::admin::router())
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(axum::middleware::from_fn(
            crate::shared::cors::cors_middleware,
        ))
        .with_state(state);
    writes::guard(api, writes_enabled)
}

/// The canonical Shop paths of the same handlers ([`crate::paths::ALIASES`]
/// pairs each with its legacy path), for the `/shop-api` nest: a pack purchase is a `pack-order`, a merchandise purchase a
/// `checkout`, a credit signature a `credit-authorization`, a price read a
/// `credit-quote`; the user-scoped reads live under `/v1/users/{address}`,
/// the operator routes under `/admin`, the mocks under `/dev` (501 unless
/// `CREDITS_MOCK_CARD`). Behind the same write gate, with the canonical
/// read `POST /v1/credit-quotes` open like its alias.
pub fn shop_router(state: SharedState) -> Router {
    let writes_enabled = state.settings.writes_enabled;
    let api = Router::new()
        .route("/v1/users", post(handlers::users::enroll))
        .route("/v1/users/{wallet_id}", delete(handlers::users::unregister))
        .route("/v1/users/{wallet_id}/status", get(handlers::users::status))
        .route(
            "/v1/users/{wallet_id}/progress",
            get(handlers::users::progress),
        )
        .route(
            "/v1/users/{wallet_id}/credits",
            get(handlers::users::user_credits),
        )
        .route("/v1/packs", get(handlers::packs::unity_list_packs))
        .route(
            "/v1/packs/{sku}/payment-intents",
            post(handlers::packs::create_intent),
        )
        .route("/v1/pack-orders", post(handlers::orders::create_checkout))
        .route(
            "/v1/pack-orders/{order_id}",
            get(handlers::orders::order_status),
        )
        .route(
            "/v1/credit-authorizations",
            post(handlers::authorize::authorize),
        )
        .route(
            "/v1/credit-authorizations/batch",
            post(handlers::authorize::authorize_batch),
        )
        .route(
            "/v1/credit-authorizations/cancel",
            post(handlers::authorize::cancel),
        )
        .route(
            "/v1/credit-authorizations/submitted",
            post(handlers::authorize::submitted),
        )
        .route(
            "/v1/pack-orders/{order_id}/cancel",
            post(handlers::orders::cancel_order),
        )
        .route(
            "/v1/pack-orders/{order_id}/resume",
            post(handlers::orders::resume_order),
        )
        .route(
            "/v1/pack-orders/{order_id}/cancel-return",
            get(handlers::orders::cancel_return),
        )
        .route(
            "/v1/users/{wallet_id}/purchases",
            get(handlers::history::purchases),
        )
        .route(
            "/v1/users/{wallet_id}/credit-orders",
            get(handlers::history::credit_orders),
        )
        .route(
            "/v1/names/credit-route",
            get(handlers::authorize::name_credit_route),
        )
        .route(
            "/v1/credit-usage-index/status",
            get(handlers::authorize::usage_index_status),
        )
        .route("/v1/credit-topups/mana", post(handlers::topup::mana_topup))
        .route(
            "/v1/credit-topups/mana/quote",
            get(handlers::topup::mana_topup_quote),
        )
        .route("/v1/stripe/webhook", post(handlers::stripe::webhook))
        .route("/v1/cart", get(handlers::cart::get_cart))
        .route("/v1/cart/items", post(handlers::cart::add_item))
        .route(
            "/v1/cart/items/{collection}/{item_id}",
            delete(handlers::cart::remove_item),
        )
        .route("/v1/checkouts", post(handlers::cart::checkout))
        .route("/v1/checkouts/{id}", get(handlers::cart::get_checkout))
        .route("/v1/credit-quotes", post(handlers::prices::quote))
        .route("/v1/credit-program/seasons", get(handlers::users::seasons))
        .route("/v1/credit-program/season", get(handlers::users::season))
        .route(
            "/v1/captcha",
            get(handlers::captcha::generate).post(handlers::captcha::claim),
        )
        .route(
            "/dev/packs/{sku}/mock-purchase",
            post(handlers::packs::mock_purchase),
        )
        .route("/dev/topup/mock-card", post(handlers::packs::mock_topup))
        .merge(handlers::admin::shop_router())
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(axum::middleware::from_fn(
            crate::shared::cors::cors_middleware,
        ))
        .with_state(state);
    writes::guard(api, writes_enabled)
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn fulfillment_mode_parses_exactly_the_three_modes() {
        assert_eq!(DEFAULT_CHECKOUT_FULFILLMENT_MODE, "primary");
        assert_eq!(
            parse_fulfillment_mode("secondary").unwrap(),
            "primary",
            "the legacy resale source runs as primary"
        );
        assert_eq!(parse_fulfillment_mode("primary").unwrap(), "primary");
        assert_eq!(parse_fulfillment_mode(" AUTO ").unwrap(), "primary");
        assert!(parse_fulfillment_mode("tertiary").is_err());
        assert!(parse_fulfillment_mode("").is_err());
    }

    #[test]
    fn settings_refuse_a_private_or_plaintext_secret_endpoint_and_default_off() {
        let s = Settings::parse(RawSettings::default()).unwrap();
        assert!(
            !s.writes_enabled
                && !s.mock_card
                && !s.mock_fulfillment
                && !s.credits_onchain_authorize_enabled
        );
        assert!(
            s.require_purchase_intent,
            "intent enforcement is the default"
        );
        assert_eq!(s.checkout_worker_interval, Duration::from_secs(30));
        assert_eq!(
            (
                s.checkout_max_attempts,
                s.escrow_lock_days,
                s.marketplace_markup_bps
            ),
            (5, 15, 2500)
        );
        let err = Settings::parse(RawSettings {
            stripe_secret_key: Some("sk".into()),
            stripe_api_base: "http://127.0.0.1:1".into(),
            ..RawSettings::default()
        })
        .err()
        .expect("a plaintext loopback Stripe base is refused")
        .to_string();
        assert!(
            err.contains("STRIPE_API_BASE") && !err.contains("sk"),
            "{err}"
        );
        let err = Settings::parse(RawSettings {
            captcha_secret: Some("cs".into()),
            captcha_verify_url: "https://10.0.0.1/siteverify".into(),
            ..RawSettings::default()
        })
        .err()
        .expect("a private hCaptcha endpoint is refused")
        .to_string();
        assert!(err.contains("CREDITS_CAPTCHA_VERIFY_URL"), "{err}");
        assert!(
            Settings::parse(RawSettings {
                stripe_api_base: "http://127.0.0.1:1".into(),
                ..RawSettings::default()
            })
            .is_ok(),
            "no secret, no endpoint check"
        );
    }

    #[test]
    fn the_mock_card_runs_only_on_amoy() {
        for chain in [usage::CHAIN_POLYGON, 1, 0] {
            let err = Settings::parse(RawSettings {
                mock_card: true,
                usage_chain_id: chain,
                ..RawSettings::default()
            })
            .err()
            .expect("free credits are refused where credits are worth something")
            .to_string();
            assert!(err.contains("CREDITS_MOCK_CARD"), "{err}");
        }
        // Also with the treasury signing on chain: the pair that would pay real MANA for free credits.
        assert!(
            Settings::parse(RawSettings {
                mock_card: true,
                credits_onchain_authorize_enabled: true,
                credits_signer_key: Some(format!("0x{}", "11".repeat(32))),
                credits_manager_contract: Some(format!("0x{}", "22".repeat(20))),
                ..RawSettings::default()
            })
            .is_err()
        );
        let amoy = Settings::parse(RawSettings {
            mock_card: true,
            usage_chain_id: usage::CHAIN_AMOY,
            ..RawSettings::default()
        })
        .expect("the testnet keeps its mock card");
        assert!(amoy.mock_card);
        assert!(
            Settings::parse(RawSettings::default()).is_ok(),
            "off by default, so mainnet parses"
        );
    }

    #[test]
    fn the_schema_carries_every_credits_table_this_module_names() {
        for table in [
            "wallets",
            "ledger",
            "idempotency",
            "packs",
            "stripe_events",
            "purchases",
            "payment_reversals",
            "carts",
            "cart_items",
            "checkouts",
            "fulfillment_outbox",
            "authorizations",
            "authorization_lines",
            "usage_settlements",
            "revocations",
            "usages",
            "usage_index_cursor",
            "captcha_challenges",
        ] {
            assert!(
                crate::db::SCHEMA_SQL.contains(&format!("CREATE TABLE credits.{table} (")),
                "{table}"
            );
        }
        assert!(crate::db::SCHEMA_SQL.contains("CREATE TABLE shop.usage_grants ("));
        assert!(crate::db::SCHEMA_SQL.contains("CREATE TABLE ops.admin_audit ("));
        assert!(
            crate::db::SCHEMA_SQL
                .contains("('credits_usd_5',  'Starter',  50,   500, 'usd', true, 1)"),
            "the four tiers are seeded"
        );
    }
}
