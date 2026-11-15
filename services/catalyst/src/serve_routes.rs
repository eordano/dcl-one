//! Root HTTP server routes.
use super::{settings::write_switches, states::OpsStates};
use crate::{
    builder, camera_reel, comms, config::Config, content, explore, land_authz, ops, presence,
    price, shared, shop, social, worlds,
};
use std::sync::Arc;

/// The old transactions-server surface: the relay (`economy`) plus the
/// Shop's checkout and treasury routes over the same state, at the root
/// and under `/transactions-api`.
pub(super) fn economy_routes(state: &Arc<shop::evm::State>) -> axum::Router {
    shop::evm::router(state.clone())
        .merge(shop::checkout::router(state.clone()))
        .merge(shop::treasury::router(state.clone()))
}

/// The live presence's two routes where the worlds core routers held them:
/// at the root and under `/worlds-content-server`, each behind the worlds
/// writes gate (`WORLDS_WRITES_ENABLED`) as those routers were, applied
/// before nesting so the gate sees the route-local path.
pub(crate) fn live_presence_routes(
    presence: Arc<comms::presence::Presence>,
    writes_enabled: bool,
) -> axum::Router {
    axum::Router::new()
        .merge(worlds::writes::guard(
            comms::presence::router(presence.clone()),
            writes_enabled,
        ))
        .nest(
            "/worlds-content-server",
            worlds::writes::guard(comms::presence::router(presence), writes_enabled),
        )
}

/// Catalyst's own half of `/asset-bundle-registry` (profiles); the Registry
/// gateway's reads join it under the same prefix through `explore::api::router`.
pub(super) fn registry_profiles(market: shop::market::SharedState) -> axum::Router {
    axum::Router::new().nest(
        "/asset-bundle-registry",
        shop::market::profiles::registry::router(market),
    )
}

pub(super) struct Routes<'a> {
    pub(super) content: Arc<content::ContentState>,
    pub(super) market: shop::market::SharedState,
    pub(super) stage5_ops: OpsStates,
    pub(super) economy_state: Arc<shop::evm::State>,
    pub(super) signatures: Arc<shop::rentals::State>,
    pub(super) outfits: Arc<shop::outfits::State>,
    pub(super) subscriptions: Arc<shop::subscriptions::State>,
    pub(super) mana: Arc<shop::mana::State>,
    pub(super) land: Arc<land_authz::LandAuthzState>,
    pub(super) price: Arc<price::State>,
    pub(super) presence: presence::SharedState,
    pub(super) explore: explore::SharedState,
    pub(super) worlds: worlds::SharedState,
    pub(super) stage5: &'a worlds::Stage5,
    pub(super) live_presence: Arc<comms::presence::Presence>,
    pub(super) explorer_api: explore::api::Shared,
    pub(super) archipelago_state: comms::archipelago::AppState,
    pub(super) gatekeeper: &'a comms::gatekeeper::SharedState,
    pub(super) social: &'a social::States,
    pub(super) builder: builder::SharedState,
    pub(super) camera_reel: camera_reel::SharedState,
    pub(super) ops_admin: ops::admin::SharedState,
    pub(super) snapshot_gen: Option<Arc<content::snapshot_gen::SnapshotGenerator>>,
}

pub(super) fn build_routes(
    config: &Config,
    prepared: crate::metrics_matcher::PreparedStats,
    primary: bool,
    routes: Routes<'_>,
) -> axum::Router {
    let Routes {
        content,
        market,
        stage5_ops,
        economy_state,
        signatures,
        outfits,
        subscriptions,
        mana,
        land,
        price,
        presence,
        explore,
        worlds,
        stage5,
        live_presence,
        explorer_api,
        archipelago_state,
        gatekeeper,
        social,
        builder,
        camera_reel,
        ops_admin,
        snapshot_gen,
    } = routes;
    // The old gateway's location table (catalyrst/nixos/web-gateway.nix): the
    // content routes and /lambdas at the root, every other service under the
    // prefix nginx used to strip; the same routes also at the paths their own
    // vhost served, so a per-vhost proxy needs no rewrite. Anything else falls
    // through to the content 404. The canonical Shop tree (`shop::router`:
    // the market surface, the credits resources, the rentals store, the
    // MANA top-up reads, the operator routes) is under /shop-api; the
    // market surface alone answers again under /marketplace-api, its legacy
    // alias: its /v1/items and /v1/collections collide with the builder's at
    // the root, and the lambdas stay at the root. The economy routes answer at the root
    // (the old web tier's /v1/transactions location) and under
    // /transactions-api (the gateway's strip prefix, with the old /health and
    // /ping there only); the price feed at the root (/api/v3/simple/price,
    // as web.nix served it) and under /price; the rentals store at the root
    // and under /signatures-api; the LAND routes at their /lambdas paths; the
    // presence archive under /presence, the paths the sites call. The
    // connector answers at the root and under /archipelago-ea-stats; its
    // socket routes only on the primary.
    let connector_lifecycle = archipelago_state.clone();
    let (statistics_settings, statistics_matcher) = prepared.into_parts();
    let archipelago = crate::comms::archipelago::mounts(
        archipelago_state,
        primary,
        statistics_settings,
        statistics_matcher,
        content.denylist.clone(),
    );
    let statistics = archipelago.statistics.clone();
    let blobs = content.clone();
    let app = content::build_router(content)
        .merge(shop::market::router(market.clone()))
        .nest(
            "/shop-api",
            shop::router(shop::States {
                market: market.clone(),
                credits: stage5_ops.credits.clone(),
                economy: economy_state.clone(),
                signatures: signatures.clone(),
                outfits: outfits.clone(),
                subscriptions: subscriptions.clone(),
                mana,
            }),
        )
        .merge(shop::campaigns::builder_alias_router(market.clone()))
        // events-notifier.<domain> reaches /events-notifier (gateway subdomain rule).
        .nest(
            "/events-notifier",
            shop::market::tips::router(market.clone()),
        )
        .merge(registry_profiles(market.clone()))
        .nest("/marketplace-api", shop::market::market_router(market))
        .merge(land_authz::router(land))
        .merge(economy_routes(&economy_state))
        .merge(shop::evm::contracts_document_router())
        .nest(
            "/transactions-api",
            economy_routes(&economy_state).merge(shop::evm::health_router(economy_state)),
        )
        .merge(price::router(price.clone()))
        .nest("/price", price::router(price))
        .merge(shop::rentals::router(signatures.clone()))
        .nest("/signatures-api", shop::rentals::router(signatures))
        .nest("/presence", presence::router(presence))
        .merge(explore::router(explore.clone()))
        .nest("/places", explore::router(explore.clone()))
        .nest("/events", explore::router(explore))
        .merge(worlds::app_router(worlds, blobs, stage5))
        .merge(live_presence_routes(live_presence, stage5.writes_enabled))
        .merge(if primary {
            comms::archipelago::socket_router(connector_lifecycle.clone())
        } else {
            axum::Router::new()
        })
        .merge(explore::api::router(explorer_api))
        // The gatekeeper's mounts, where `social::router` held them: the Pulse
        // room authority, the world comms mints and the LiveKit webhook (the
        // worlds content server's root path) at the root, the mints and the
        // webhook under `/worlds-content-server`, and the gatekeeper's routes
        // with social's MLS routes under `/comms-gatekeeper` behind the
        // social writes switch.
        .merge(comms::gatekeeper::room_authority_router(gatekeeper))
        .merge(comms::gatekeeper::admission::world_mint::router(
            gatekeeper.clone(),
        ))
        .merge(social::writes::guard(
            comms::gatekeeper::webhook_router(gatekeeper.clone()),
            config.writes.social_writes_enabled,
        ))
        .nest(
            "/worlds-content-server",
            comms::gatekeeper::admission::world_mint::router(gatekeeper.clone()).merge(
                social::writes::guard(
                    comms::gatekeeper::webhook_router(gatekeeper.clone()),
                    config.writes.social_writes_enabled,
                ),
            ),
        )
        .nest(
            "/comms-gatekeeper",
            social::writes::guard(
                comms::gatekeeper::router(gatekeeper.clone())
                    .merge(social::messaging::router(social.messaging.clone())),
                config.writes.social_writes_enabled,
            ),
        )
        .merge(social::router(social, config.writes.social_writes_enabled))
        .merge(builder::root_router(builder.clone()))
        .nest("/builder-api", builder::router(builder))
        .nest("/camera-reel-service", camera_reel::router(camera_reel));
    // The operator plane under the old gateway's prefixes: credits under
    // /credits only (its /users, /cart and /checkout would collide at the root; the
    // handlers verify a signed fetch over the request path or a matching
    // x-original-path), the relay under /rpc (rpc.<domain>/x -> /rpc/x),
    // the imposter zips under /bvimposters (the prefix baked into the explorer
    // wasm), the console API at the root (/admin/auth, /admin/api/*).
    let app = app
        .nest("/credits", shop::credits::router(stage5_ops.credits))
        .nest("/rpc", ops::rpc::router(stage5_ops.rpc))
        .nest(
            "/bvimposters",
            explore::imposters::router(stage5_ops.imposters),
        )
        .merge(ops::admin::content::router(ops_admin.clone()))
        .merge(ops::admin::snapshots::router(
            ops_admin.clone(),
            snapshot_gen,
        ))
        .merge(ops::admin::router(ops_admin))
        .merge(shared::writes::status_router(write_switches(config)));
    // A request a gate, an auth check or a handler answers before reading
    // its body is drained, not reset: nginx writes a buffered upload after
    // the answer, and a reset fails that write, answers 502 and counts the
    // process down. Up to the largest body a route admits (a deployment).
    // Forwarding headers name the client only from a loopback or listed proxy.
    let app =
        shared::client_ip::pin_client_headers(app, config.node.http_trusted_proxy_cidrs.clone());
    let app = shared::drain::drain_unread(
        app,
        content::create_entity::MAX_DEPLOYMENT_BYTES
            .max(worlds::core::deploy::MAX_UPLOAD_WIRE_SIZE_BYTES) as u64,
    );
    // Upstream's router under /content/: static segments matched whatever
    // their case, and its `Allow` list on a 405 (also under
    // /worlds-content-server/). Other paths pass untouched.
    let app = content::routing::upstream_routing(app);
    comms::stats::http::mount_services(app, statistics, archipelago.stats, archipelago.connector)
}
