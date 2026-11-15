//! Shop: the unified Shop that absorbs the Marketplace, the credits checkout
//! and treasury (SHOP.md, "One implementation"). One module tree, one
//! canonical router ([`router`], nested at `/shop-api`), the legacy prefixes
//! (`/marketplace-api`, `/credits`, `/transactions-api`, `/signatures-api`,
//! the root aliases) mounted by `main.rs` onto the same handlers with their
//! exact old paths, a signed request always verified over the path it was
//! signed for (never redirected or rewritten):
//!
//! | module | what it holds | tables |
//! | --- | --- | --- |
//! | [`market`] | the marketplace-server surface (catalog, items, nfts, orders, bids, sales, trades, lists, picks, moderation, federation), the mirrors, the chain indexes, the subgraph readers, and the Catalyst lambdas that share its state | `shop.*` (schemas/30_shop.sql) |
//! | [`coupons`] | creator discount coupons (`/v1/coupons`) and the CouponManager state reader, over the market's state | `shop.coupons`, `shop.coupon_state` |
//! | [`credits`] | the credits program: wallets, ledger, packs, Stripe, the cart checkout, authorizations, the CreditUsed index | `credits.*` (the accounting namespace, schemas/60_ops.sql) |
//! | [`checkout`] | the broker, escrow and NAME custody settlement the node executes itself, over `crate::evm`'s signers | `shop.broker_purchases`, `shop.escrow_actions`, `shop.name_transfers`, `shop.usage_grants` |
//! | [`treasury`] | what the relayer account holds and receives (the MANA payment readers and verifier) | none yet |
//! | [`outfits`] | the curated shoppable looks of shop-server (authoring behind the creator allowlist, thumbnails in the blob store) | `shop.outfits`, `shop.outfit_thumbnails` |
//! | [`subscriptions`] | the "notify me when available" waitlist of shop-server (signed subscriptions, the availability watcher, the delivery outbox) | `shop.item_notify_requests`, `shop.item_notify_outbox`, `shop.item_availability_watch` |
//! | [`rentals`] | the LAND/estate rentals listings store (the old signatures service) and the `rentals` chain index that moves a listing's status | `signatures.*`, `shop.rental_event` |
//! | [`mana`] | the MANA purchases: quotes with the rule applied, seller/owner actions, tracked executions | `shop.executions` |
//! | [`types`], [`policy`], [`policy_entry`], [`guards`] | the canonical identities, money units, THE payment rule, the readers that turn an entry's inputs into its facts, and the guard at every entry | none |
//!
//! Every public path that prices or spends credits, with the guard that
//! closes it, is a row of [`paths::CREDIT_PATHS`]; [`paths::ALIASES`] pairs
//! each renamed route with its legacy paths.

// The crates this one builds on, under their module names (AGENTS.md).
pub(crate) use catalyst_chain_index as chain_index;
pub(crate) use catalyst_comms as comms;
pub(crate) use catalyst_config::config;
pub(crate) use catalyst_content as content;
#[cfg(test)]
pub(crate) use catalyst_db as db;
pub(crate) use catalyst_explore as explore;
pub(crate) use catalyst_fed as fed;
pub(crate) use catalyst_jobs as jobs;
pub(crate) use catalyst_land_authz as land_authz;
pub(crate) use catalyst_price as price;
pub(crate) use catalyst_shared as shared;
pub(crate) use catalyst_social as social;
pub(crate) use catalyst_storage as storage;
pub(crate) use dcl_auth as auth;
pub(crate) use dcl_http as http;

pub mod campaigns;
pub mod checkout;
pub mod coupons;
pub mod credits;
pub mod evm;
pub mod guards;
pub mod import;
pub mod mana;
pub mod market;
pub mod outfits;
pub mod paths;
pub mod policy;
pub mod policy_entry;
pub mod proceeds;
pub mod rentals;
pub mod stores;
pub mod subscriptions;
pub mod treasury;
pub mod types;

#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod test_db;

use std::sync::Arc;

use axum::Router;
use axum::routing::{MethodRouter, any};

pub use policy::{
    Authoritative, CartLine, Eligibility, Facts, GroupTotal, Quote, Reason, Refusal,
    SettlementGroup, WalletGuidance, eligibility, guard_credit_spend, guard_group_credit_spend,
    guard_method, partition, quote,
};
pub use types::{
    AssetId, AssetKind, ChainId, ContractRef, CreditSizing, EvmAddress, ManaUsdRate, ManaWei,
    PaymentMethod, SaleKind, Session, SettlementCall, SettlementGroupKey, SettlementOperation,
    TokenRef, UsdCents,
};

/// The market routes the slice-5 modules add to `market::market_router`
/// (so they answer under `/shop-api` and its `/marketplace-api` alias
/// alike, behind the same layers and the same write gate): the listing
/// import check, the campaigns, the stores, the proceeds.
pub(crate) fn market_extensions() -> Router<market::SharedState> {
    Router::new()
        .merge(import::router())
        .merge(campaigns::router())
        .merge(stores::router())
        .merge(proceeds::router())
}

/// The operator routes those modules add under the canonical `/shop-api/admin`
/// (`market::shop_admin_router`: the same bearer, the same write gate).
pub(crate) fn admin_extensions() -> Router<market::SharedState> {
    Router::new().merge(campaigns::admin_router())
}

/// The states the canonical router serves over: each module keeps its own.
pub struct States {
    pub market: Arc<market::MarketplaceState>,
    pub credits: Arc<credits::State>,
    pub economy: Arc<crate::evm::State>,
    pub outfits: Arc<outfits::State>,
    pub subscriptions: Arc<subscriptions::State>,
    /// The rentals listings store (`rentals`; the field keeps the name
    /// `main.rs` spells).
    pub signatures: Arc<rentals::State>,
    /// The MANA purchase quotes, actions and executions (`mana`).
    pub mana: Arc<mana::State>,
}

/// The canonical `/shop-api` tree ([`paths`]; `library/shop/api/endpoints.ts`
/// is the client's table of it):
/// the marketplace-server paths as they are, the credits paths under their
/// resource names, the rentals store's `/v1/rentals-listings`, the MANA
/// top-up reads, and the operator routes under `/admin`. Every route is a
/// module's own handler behind that module's write gate; a path the UI
/// calls that no handler serves yet answers the documented unavailable
/// envelope naming the missing piece ([`pending_router`]), never a 404 that
/// reads as "not a Shop route" or an empty body that reads as an answer.
pub fn router(states: States) -> Router {
    Router::new()
        .merge(market::market_router(states.market.clone()))
        .merge(market::shop_admin_router(states.market))
        .merge(credits::shop_router(states.credits))
        .merge(checkout::shop_router(states.economy.clone()))
        .merge(treasury::shop_router(states.economy))
        .merge(rentals::router(states.signatures))
        .merge(mana::shop_router(states.mana))
        .merge(outfits::shop_router(states.outfits))
        .merge(subscriptions::shop_router(states.subscriptions))
        .merge(pending_router())
}

/// `503` with the lambdas envelope, `Retry-After` and `source` naming what
/// is missing, as every deferred market route answers (`market::unavailable`).
fn pending(source: &'static str) -> MethodRouter {
    any(move || async move {
        market::unavailable(
            market::Source::Deferred(source),
            "this Shop route is not served by this node yet: shop::pending_router lists it as implementation work",
        )
    })
}

/// The canonical paths the Shop UI codes against whose handlers are still
/// to be written, one source label per feature: the credits-server user histories, pack order
/// resume/cancel, batch authorization and submission reporting, the NAME
/// credit route, IAP and the card
/// on-ramps. Mounted after the served routes, so a path a handler owns is
/// never shadowed.
pub fn pending_router() -> Router {
    Router::new()
        .route(
            "/v1/users/{address}/credits/stream",
            pending("shop-credits-users"),
        )
        .route(
            "/v2/credit-program/seasons",
            pending("shop-credits-program"),
        )
        .route(
            "/v1/credit-authorizations/publication",
            pending("shop-credit-authorizations"),
        )
        .route("/v1/iap/quote", pending("shop-credits-iap"))
        .route("/v1/iap/register", pending("shop-credits-iap"))
        .route("/v1/iap/verify", pending("shop-credits-iap"))
        .route("/v1/transak/widget-url", pending("shop-onramp"))
        .route("/v1/transak/orders/{id}", pending("shop-onramp"))
        .route("/v1/wert/sign", pending("shop-onramp"))
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn a_pending_shop_route_answers_the_unavailable_envelope() {
        let app = Router::new().nest("/shop-api", pending_router());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/shop-api/v1/iap/quote")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get("retry-after").unwrap(), "60");
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1 << 16)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["source"], "shop-credits-iap");
        assert_eq!(body["error"], "Service Unavailable");
        assert!(
            body.get("configure").is_none(),
            "nothing to configure: it is unwritten"
        );
    }
}
