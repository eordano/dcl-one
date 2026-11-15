//! The Shop's public paths as data; `tests` requests every row against the
//! trees mounted where `serve.rs` mounts them.
//!
//! The backend answers under [`PREFIX`]. The marketplace-server paths keep
//! their subpaths there and answer again under `/marketplace-api`. The
//! credits paths take resource names ([`ALIASES`]: a pack purchase is a
//! `pack-order`, a merchandise purchase a `checkout`, a credit signature a
//! `credit-authorization`, a price read a `credit-quote`, the user reads
//! live under `/v1/users/{address}`) and keep their old paths under
//! `/credits`. The relay, broker and payments routes answer at the root and
//! under `/transactions-api`, the rentals store at the root and under
//! `/signatures-api`. Operator routes live under `/shop-api/admin`,
//! development mocks under `/shop-api/dev`, and a path the UI calls that no
//! handler serves yet answers the unavailable envelope
//! ([`super::pending_router`]). shop-server's service-to-service routes are
//! in-process calls and outbox jobs, never public routes.
//!
//! A legacy alias is the same handler with the same wire shape: a signed
//! request is verified over the method and path it was signed for, never
//! redirected or rewritten. `library/shop/api/endpoints.ts` is the client's
//! table of the same paths.

/// The canonical backend prefix.
pub const PREFIX: &str = "/shop-api";

/// Every renamed Shop route and every route outside the marketplace surface,
/// one per row: `METHOD canonical legacy...`. The rest of the marketplace
/// surface answers under `/marketplace-api` with the same subpath.
/// `POST /credits/credits/checkout` is credits-server's pack purchase and
/// `POST /credits/checkout` the merchandise checkout; the canonical tree never
/// says a bare "checkout". Only at their old paths: `GET /credits/packs` (the
/// web `PackOut` shape), `GET /credits/wallet/{address}/balance` and
/// `POST /v1/payments/verify`.
pub const ALIASES: &[&str] = &[
    "POST /shop-api/v1/users /credits/users",
    "DELETE /shop-api/v1/users/{address} /credits/users/{address}",
    "GET /shop-api/v1/users/{address}/status /credits/users/{address}/status",
    "GET /shop-api/v1/users/{address}/progress /credits/users/{address}/progress",
    "GET /shop-api/v1/users/{address}/credits /credits/users/{address}/credits",
    "GET /shop-api/v1/users/{address}/purchases /credits/users/{address}/purchases",
    "GET /shop-api/v1/users/{address}/credit-orders /credits/users/{address}/credit-orders",
    "GET /shop-api/v1/packs /credits/credits/packs",
    "POST /shop-api/v1/packs/{sku}/payment-intents /credits/packs/{sku}/intent",
    "POST /shop-api/v1/pack-orders /credits/credits/checkout",
    "GET /shop-api/v1/pack-orders/{id} /credits/credits/orders/{id}",
    "POST /shop-api/v1/pack-orders/{id}/cancel /credits/credits/orders/{id}/cancel",
    "POST /shop-api/v1/pack-orders/{id}/resume /credits/credits/orders/{id}/resume",
    "GET /shop-api/v1/pack-orders/{id}/cancel-return /credits/credits/orders/{id}/cancel-return",
    "POST /shop-api/v1/credit-authorizations /credits/credits/authorize",
    "POST /shop-api/v1/credit-authorizations/batch /credits/credits/authorize/batch",
    "POST /shop-api/v1/credit-authorizations/cancel /credits/credits/authorize/cancel",
    "POST /shop-api/v1/credit-authorizations/submitted /credits/credits/authorize/submitted",
    "GET /shop-api/v1/names/credit-route /credits/credits-name-route",
    "GET /shop-api/v1/credit-usage-index/status /credits/usage-index/status",
    "POST /shop-api/v1/credit-topups/mana /credits/topup/mana",
    "GET /shop-api/v1/credit-topups/mana/quote /credits/topup/mana/quote",
    "GET /shop-api/v1/credit-topups/mana/config /v1/payments/config /transactions-api/v1/payments/config",
    "GET /shop-api/v1/credit-topups/mana/nonce/{address} /v1/payments/nonce/{address} /transactions-api/v1/payments/nonce/{address}",
    "GET /shop-api/v1/credit-topups/mana/balance/{address} /v1/payments/balance/{address} /transactions-api/v1/payments/balance/{address}",
    // Both old webhook paths stay registered with Stripe until rotated.
    "POST /shop-api/v1/stripe/webhook /credits/stripe/webhook /credits/webhook",
    "GET /shop-api/v1/cart /credits/cart",
    "POST /shop-api/v1/cart/items /credits/cart/items",
    "DELETE /shop-api/v1/cart/items/{collection}/{item_id} /credits/cart/items/{collection}/{item_id}",
    "POST /shop-api/v1/checkouts /credits/checkout",
    "GET /shop-api/v1/checkouts/{id} /credits/checkout/{id}",
    "POST /shop-api/v1/credit-quotes /credits/prices/quote",
    "GET /shop-api/v1/credit-program/seasons /credits/seasons",
    "GET /shop-api/v1/credit-program/season /credits/season",
    "GET /shop-api/v1/captcha /credits/captcha",
    "POST /shop-api/v1/captcha /credits/captcha",
    "POST /shop-api/dev/packs/{sku}/mock-purchase /credits/packs/{sku}/mock-purchase",
    "POST /shop-api/dev/topup/mock-card /credits/topup/mock-card",
    "POST /shop-api/admin/credits/grant /credits/admin/credits/grant",
    "POST /shop-api/admin/credits/revoke /credits/admin/credits/revoke",
    "POST /shop-api/admin/users/{address}/block /credits/admin/users/{address}/block",
    "GET /shop-api/admin/packs /credits/admin/packs",
    "POST /shop-api/admin/packs /credits/admin/packs",
    "PUT /shop-api/admin/packs/{sku} /credits/admin/packs/{sku}",
    "DELETE /shop-api/admin/packs/{sku} /credits/admin/packs/{sku}",
    "GET /shop-api/admin/purchases /credits/admin/purchases",
    "GET /shop-api/admin/checkouts /credits/admin/checkouts",
    "GET /shop-api/admin/credits/ledger /credits/admin/ledger",
    "POST /shop-api/admin/checkouts/{id}/refund /credits/admin/checkouts/{id}/refund",
    "POST /shop-api/admin/checkouts/{id}/force-fulfill /credits/admin/checkouts/{id}/force-fulfill",
    "POST /shop-api/admin/grants/{escrow_ref}/reclaim /credits/admin/grants/{escrow_ref}/reclaim",
    "POST /shop-api/admin/grants/{escrow_ref}/release /credits/admin/grants/{escrow_ref}/release",
    "GET /shop-api/admin/credits/reconcile /credits/admin/reconcile",
    "POST /shop-api/admin/broker/approve-mana /v1/admin/collections/approve-mana /transactions-api/v1/admin/collections/approve-mana",
    "POST /shop-api/admin/names/approve-mana /v1/admin/names/approve-mana /transactions-api/v1/admin/names/approve-mana",
    "GET /shop-api/admin/moderation/flags /marketplace-api/v1/admin/moderation/flags",
    "POST /shop-api/admin/moderation/{kind}/{hash}/flag /marketplace-api/v1/admin/moderation/{kind}/{hash}/flag",
    "DELETE /shop-api/admin/moderation/{kind}/{hash}/flag /marketplace-api/v1/admin/moderation/{kind}/{hash}/flag",
    "GET /shop-api/admin/disputes /marketplace-api/v1/admin/disputes",
    "POST /shop-api/admin/disputes/{trade_hash}/open /marketplace-api/v1/admin/disputes/{trade_hash}/open",
    "POST /shop-api/admin/disputes/{trade_hash}/resolve /marketplace-api/v1/admin/disputes/{trade_hash}/resolve",
    "POST /shop-api/admin/listings/{kind}/{hash}/force-cancel /marketplace-api/v1/admin/listings/{kind}/{hash}/force-cancel",
    "GET /shop-api/admin/audit /marketplace-api/v1/admin/audit",
    "PUT /shop-api/admin/campaigns/{id} /marketplace-api/v1/admin/campaigns/{id}",
    "DELETE /shop-api/admin/campaigns/{id} /marketplace-api/v1/admin/campaigns/{id}",
    "PUT /shop-api/admin/campaigns/{id}/collections /marketplace-api/v1/admin/campaigns/{id}/collections",
    "GET /shop-api/v1/rentals-listings /v1/rentals-listings /signatures-api/v1/rentals-listings",
    "POST /shop-api/v1/rentals-listings /v1/rentals-listings /signatures-api/v1/rentals-listings",
    "PATCH /shop-api/v1/rentals-listings/{id} /v1/rentals-listings/{id} /signatures-api/v1/rentals-listings/{id}",
    "GET /shop-api/v1/rental-listings/prices /v1/rental-listings/prices /signatures-api/v1/rental-listings/prices",
];

/// A public path that prices or spends credits, and the guard that refuses
/// LAND and resale lines on it before any ledger row, signature or
/// broadcast.
pub struct CreditPath {
    pub method: &'static str,
    /// The canonical path first, then its aliases.
    pub paths: &'static [&'static str],
    pub guard: &'static str,
}

const fn credit(
    method: &'static str,
    paths: &'static [&'static str],
    guard: &'static str,
) -> CreditPath {
    CreditPath {
        method,
        paths,
        guard,
    }
}

const CATALOG_FEED: &str = "guards::catalog::credit_price: no credit price on a resale row";

/// Every path through which credits could be spent on LAND or a resale, or
/// pay a resale seller. A refusal is one wire shape, `guards::Refused`, and
/// never re-routes a line to another payment method; the
/// `credits-fulfillment` job runs `guard_spend` again on every line it
/// settles, admin force-fulfil included.
pub const CREDIT_PATHS: &[CreditPath] = &[
    credit(
        "POST",
        &["/shop-api/v1/purchase-offers"],
        "policy::eligibility, then policy::quote per offered method (mana::quote)",
    ),
    credit(
        "POST",
        &["/shop-api/v1/credit-quotes", "/credits/prices/quote"],
        "guards::credits::Line::guard_quote: policy::guard_method(Credits)",
    ),
    credit(
        "POST",
        &["/shop-api/v1/cart/items", "/credits/cart/items"],
        "guards::credits::Line::guard_spend with LedgerBroker",
    ),
    credit(
        "POST",
        &["/shop-api/v1/checkouts", "/credits/checkout"],
        "guards::credits::Line::guard_spend with LedgerBroker",
    ),
    credit(
        "POST",
        &[
            "/shop-api/admin/checkouts/{id}/force-fulfill",
            "/credits/admin/checkouts/{id}/force-fulfill",
        ],
        "guards::credits::Line::guard_spend with LedgerBroker, in the job it re-arms",
    ),
    credit(
        "POST",
        &[
            "/shop-api/v1/credit-authorizations",
            "/credits/credits/authorize",
        ],
        "policy::guard_group_credit_spend with AuthorizeCredit (credits::batch)",
    ),
    credit(
        "POST",
        &[
            "/shop-api/v1/credit-authorizations/batch",
            "/credits/credits/authorize/batch",
        ],
        "policy::guard_group_credit_spend with AuthorizeCredit (credits::batch)",
    ),
    credit(
        "POST",
        &[
            "/shop-api/v1/import-checks",
            "/marketplace-api/v1/import-checks",
        ],
        "policy::guard_credit_spend with CreditListing (import)",
    ),
    credit(
        "POST",
        &["/v1/broker/buy", "/transactions-api/v1/broker/buy"],
        "guards::broker::guard_buy with LedgerBroker",
    ),
    credit(
        "POST",
        &[
            "/v1/broker/names/buy",
            "/transactions-api/v1/broker/names/buy",
        ],
        "guards::broker::guard_name_buy with LedgerBroker",
    ),
    credit(
        "POST",
        &["/v1/transactions", "/transactions-api/v1/transactions"],
        "guards::relay::guard_meta_transaction with UseCredits, decoded from the calldata",
    ),
    credit(
        "GET",
        &[
            "/shop-api/v3/catalog/shop",
            "/marketplace-api/v3/catalog/shop",
        ],
        CATALOG_FEED,
    ),
    credit(
        "GET",
        &[
            "/shop-api/v3/catalog/unified",
            "/marketplace-api/v3/catalog/unified",
        ],
        CATALOG_FEED,
    ),
    credit(
        "GET",
        &[
            "/shop-api/v3/catalog/related",
            "/marketplace-api/v3/catalog/related",
        ],
        CATALOG_FEED,
    ),
    credit(
        "GET",
        &[
            "/shop-api/v3/catalog/trending",
            "/marketplace-api/v3/catalog/trending",
        ],
        CATALOG_FEED,
    ),
    credit(
        "GET",
        &[
            "/shop-api/v3/catalog/suggested",
            "/marketplace-api/v3/catalog/suggested",
        ],
        CATALOG_FEED,
    ),
];

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{Method, StatusCode};
    use axum::middleware::{Next, from_fn};
    use tower::ServiceExt;

    use super::*;
    use crate::http::Outbound;
    use crate::{checkout, credits, evm, mana, market, outfits, rentals, subscriptions, treasury};

    fn lazy_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
            .unwrap()
    }

    /// The Shop's trees where `serve.rs` mounts them, over states whose pool
    /// never connects. A matched route answers 204 from the outer route layer
    /// before any handler runs, so 404 is a path and 405 a method no router
    /// serves.
    async fn mounted(storage_root: &std::path::Path) -> Router {
        let pool = lazy_pool();
        let http = Outbound::for_tests();
        let credits = Arc::new(credits::State::new(
            pool.clone(),
            http.clone(),
            credits::Settings::for_tests(),
            credits::Sources {
                market: None,
                price: None,
                economy: None,
            },
            None,
        ));
        mounted_with_credits(storage_root, credits)
            .await
            .route_layer(from_fn(|_: Request, _: Next| async {
                StatusCode::NO_CONTENT
            }))
            // `route_layer` also wraps each path's wrong-method fallback; this
            // puts a bare 405 back in its place.
            .method_not_allowed_fallback(|| async { StatusCode::METHOD_NOT_ALLOWED })
    }

    /// Build the whole production Shop tree with real Credits handlers.
    pub(crate) async fn mounted_with_credits(
        storage_root: &std::path::Path,
        credits: credits::SharedState,
    ) -> Router {
        let pool = credits.credits.pool.clone();
        let http = Outbound::for_tests();
        let market = market::tests::support::state_without_db();
        let economy = Arc::new(
            evm::State::new(pool.clone(), http.clone(), evm::Settings::for_tests()).unwrap(),
        );
        let signatures = Arc::new(rentals::State::new(
            pool.clone(),
            Some(market.clone()),
            rentals::Settings {
                writes_enabled: true,
            },
            None,
        ));
        let storage = crate::storage::Storage::from_config(storage_root)
            .await
            .unwrap();
        let outfits = Arc::new(outfits::State::new(
            pool.clone(),
            Arc::new(storage),
            outfits::Settings {
                creators: Vec::new(),
                writes_enabled: true,
            },
            None,
        ));
        let mailer =
            crate::social::notifications::email::EmailSender::new(Default::default(), http.clone());
        let subscriptions = Arc::new(subscriptions::State::new(
            pool.clone(),
            Arc::new(mailer),
            subscriptions::Settings {
                shop_base_url: String::new(),
                delivery_interval: std::time::Duration::from_secs(60),
                watch_interval: std::time::Duration::from_secs(60),
                writes_enabled: true,
            },
            None,
        ));
        let mana = Arc::new(
            mana::State::new(
                pool,
                http,
                market.clone(),
                signatures.clone(),
                mana::Settings::default(),
                None,
            )
            .expect("the production book"),
        );
        let economy_routes = || {
            evm::router(economy.clone())
                .merge(checkout::router(economy.clone()))
                .merge(treasury::router(economy.clone()))
        };
        Router::new()
            .nest(
                PREFIX,
                crate::router(crate::States {
                    market: market.clone(),
                    credits: credits.clone(),
                    economy: economy.clone(),
                    outfits,
                    subscriptions,
                    signatures: signatures.clone(),
                    mana,
                }),
            )
            .nest("/marketplace-api", market::market_router(market))
            .merge(economy_routes())
            .nest("/transactions-api", economy_routes())
            .merge(rentals::router(signatures.clone()))
            .nest("/signatures-api", rentals::router(signatures))
            .nest("/credits", credits::router(credits))
    }

    /// `method path` for every request the mounted trees do not route.
    async fn unrouted<'a>(
        app: &Router,
        requests: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for (method, template) in requests {
            let uri = template.replace(['{', '}'], "");
            let req = Request::builder()
                .method(Method::from_bytes(method.as_bytes()).unwrap())
                .uri(&uri)
                .body(Body::empty())
                .unwrap();
            let status = app.clone().oneshot(req).await.unwrap().status();
            if status != StatusCode::NO_CONTENT {
                out.push(format!("{method} {template} -> {status}"));
            }
        }
        out
    }

    #[tokio::test]
    async fn every_listed_path_is_routed_where_serve_mounts_the_shop() {
        let root =
            std::env::temp_dir().join(format!("catalyst-shop-paths-{}", uuid::Uuid::new_v4()));
        let app = mounted(&root).await;

        assert_eq!(
            unrouted(&app, [("GET", "/shop-api/v1/no-such-route")]).await,
            ["GET /shop-api/v1/no-such-route -> 404 Not Found"],
            "an unknown path is told apart"
        );
        assert_eq!(
            unrouted(&app, [("GET", "/shop-api/v1/credit-quotes")]).await,
            ["GET /shop-api/v1/credit-quotes -> 405 Method Not Allowed"],
            "a wrong method is told apart"
        );

        for row in ALIASES {
            let words: Vec<&str> = row.split_whitespace().collect();
            assert!(
                words.len() >= 3 && words[1].starts_with(PREFIX),
                "`METHOD canonical legacy...`: {row}"
            );
        }
        let aliases = ALIASES.iter().flat_map(|row| {
            let mut words = row.split_whitespace();
            let method = words.next().unwrap();
            words.map(move |path| (method, path))
        });
        let credit_paths = CREDIT_PATHS
            .iter()
            .flat_map(|c| c.paths.iter().map(move |path| (c.method, *path)));
        let missing = unrouted(&app, aliases.chain(credit_paths)).await;
        let _ = std::fs::remove_dir_all(&root);
        assert!(missing.is_empty(), "not routed: {missing:#?}");
    }
}
