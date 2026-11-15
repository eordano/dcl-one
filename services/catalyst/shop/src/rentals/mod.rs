//! Rentals: the LAND/Estate rental-listing signature store (the old
//! `catalyrst-signatures`, itself the Rust port of
//! decentraland/signatures-server, moved here from `src/signatures` as
//! SHOP.md slice 3 asked), over the ONE pool (`signatures.*`,
//! `schemas/32_signatures.sql`) and the stage-4 marketplace subgraph reader.
//!
//! Where each route reads (the stage-5 data-plane rule: every route names its
//! source, and a route whose source is unset answers the unavailable
//! envelope, never an empty page presented as the truth):
//! - `GET /v1/rentals-listings`, `GET /v1/rental-listings/prices`: local
//!   `signatures.rentals` / `rental_listings` / `rental_periods` /
//!   `rental_assets`. These are this node's own signed offers, so an empty
//!   answer IS the truth and the reads always serve.
//! - `POST /v1/rentals-listings`: local authority behind
//!   `DATA_WRITES_ENABLED` and signed fetch (ADR-44, the 6.x payload then the
//!   legacy one, as the old crate accepted); the asset's owner, category and
//!   LAND search columns come from the marketplace subgraph reader
//!   ([`nft`], `nfts(where: {contractAddress, tokenId})`, 30 s TTL), which
//!   replaces the crate's `squid.nft` probe; without
//!   `MARKETPLACE_SUBGRAPH_URL` the route answers
//!   `marketplace::unavailable(Source::MarketplaceSubgraph)` (the old 503
//!   "NFT ownership verification is unavailable").
//! - `PATCH /v1/rentals-listings/{id}`: the same reader refreshes the asset
//!   row; behind the write switch; the same envelope without a reader (the
//!   old crate answered the stored row unchanged, which presents stale
//!   metadata as a refresh).
//! - Rental status sync (`open -> executed | cancelled | claimed`): the
//!   `rentals` chain index ([`index`], `shop.rental_event`, the Rentals
//!   contract's own logs instead of the rentals subgraph the old crate
//!   configured and never ran), on whenever a chain-1 endpoint is configured
//!   unless `SHOP_RENTALS_INDEX_ENABLED=false`;
//!   `GET /v1/rentals-index` reports its cursor. Until it publishes, a
//!   listing keeps the status it was posted with and the rent quote
//!   (`shop::mana`) reads the nonces on chain.
//! - The rent, claim and cancel calls ([`calls`]) are pure builders the
//!   `shop::mana` quote and action routes serve over the chain readers.
//!
//! Not carried: the per-service `/ping`, `/health`, `/health/live` (the
//! content server owns `/status` and `/health` at the root),
//! `AUTH_EXPIRATION_SECONDS` (the shared verifier's window is five minutes
//! for every surface), `DAPPS_PG_COMPONENT_PSQL_*` (no squid),
//! `SIGNATURES_PG_CONNECTION_STRING` (the ONE pool), the `updates` table and
//! the `update_type` enum (nothing read or wrote them).
//!
//! Mount: `.merge(shop::rentals::router(state))` at the root (the old
//! `signatures-api.` vhost served its root there; `/v1/rentals-listings` and
//! `/v1/rental-listings/prices` collide with nothing) and
//! `.nest("/signatures-api", shop::rentals::router(state))` for a per-vhost
//! proxy; the create checks the signature over the request's own path first
//! and the route-local one second.

pub mod calls;
pub mod db;
pub mod handlers;
pub mod http;
pub mod index;
pub mod nft;
pub mod notices;
pub mod signature;
pub mod types;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderMap;
use axum::routing::{get, patch};
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::market::MarketplaceState;
use crate::shared::cache::TtlMap;
use crate::shared::signed_fetch::{self, PayloadPolicy, SignedFetchError};
use crate::shared::writes::DataWrites;

use http::ApiError;

/// The old crate's body cap for the creation JSON.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct Settings {
    /// `DATA_WRITES_ENABLED`.
    pub writes_enabled: bool,
}

pub struct State {
    pub settings: Settings,
    pub db: db::Database,
    /// The stage-4 marketplace state whose `MARKETPLACE_SUBGRAPH_URL` reader
    /// answers the asset lookups; `None` (or one without that URL) means the
    /// writes answer the envelope.
    market: Option<Arc<MarketplaceState>>,
    eip1654: Option<Arc<dyn Eip1654Validator>>,
    nft_cache: TtlMap<(String, String), Option<Arc<nft::LandNft>>>,
}

pub type SharedState = Arc<State>;

pub use crate::shared::signed_fetch::SignedPath;
pub use index::spawn_job;

pub(crate) fn signed_path(original: &axum::http::Uri, local: impl Into<String>) -> SignedPath {
    SignedPath::at(original, local)
}
/// The 401 bodies the old service emitted: the malformed-chain and the
/// invalid-signature details survive, every other refusal is its `Display`.
pub fn wire_message(err: &SignedFetchError) -> String {
    match err {
        SignedFetchError::MalformedChain { detail } => format!("Invalid Auth Chain: {detail}"),
        SignedFetchError::InvalidSignature(detail) => format!("Invalid signature: {detail}"),
        other => other.to_string(),
    }
}

impl State {
    pub fn new(
        pool: PgPool,
        market: Option<Arc<MarketplaceState>>,
        settings: Settings,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        Self {
            settings,
            db: db::Database::new(pool),
            market,
            eip1654,
            nft_cache: TtlMap::bounded(nft::NFT_TTL, nft::NFT_CACHE_MAX_ENTRIES),
        }
    }

    /// Whether the asset reader is configured (the create and refresh
    /// routes' source).
    pub fn reader_configured(&self) -> bool {
        self.market
            .as_ref()
            .is_some_and(|m| m.settings.marketplace_subgraph_url.is_some())
    }

    pub(crate) async fn nft(
        &self,
        contract_address: &str,
        token_id: &str,
    ) -> Result<Option<Arc<nft::LandNft>>, nft::NftReadError> {
        nft::nft_by_contract_token(
            self.market.as_ref(),
            &self.nft_cache,
            contract_address,
            token_id,
        )
        .await
    }

    async fn verify(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &SignedPath,
    ) -> Result<String, SignedFetchError> {
        signed_fetch::require_signer_at(
            headers,
            method,
            path,
            PayloadPolicy::V6ThenLegacy,
            self.eip1654.as_deref(),
        )
        .await
        .map(|signer| signer.into_string())
    }

    /// A rental listing is the Marketplace's: refused while its rollout is off
    /// (`market::features`), and when no marketplace state is there to say.
    pub(crate) fn require_marketplace(&self) -> Result<(), ApiError> {
        if self
            .market
            .as_ref()
            .is_some_and(|m| m.marketplace_features())
        {
            return Ok(());
        }
        Err(ApiError::forbidden(crate::market::features::refusal()))
    }

    /// The lowercased signer of a write; every refusal is the old 401 with
    /// [`wire_message`].
    pub async fn require_signer(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &SignedPath,
    ) -> Result<String, ApiError> {
        self.verify(headers, method, path).await.map_err(|e| {
            tracing::debug!(error = %e, "auth chain verification failed on a rentals route");
            ApiError::unauthorized(wire_message(&e))
        })
    }
}

/// The `/v1` rental-listings routes, wrapped in `DataWrites::guard` with the
/// state's `DATA_WRITES_ENABLED`.
pub fn router(state: SharedState) -> Router {
    let writes_enabled = state.settings.writes_enabled;
    let v1 = Router::new()
        .route(
            "/rentals-listings",
            axum::routing::post(handlers::create_rentals_listing)
                .get(handlers::get_rentals_listings),
        )
        .route(
            "/rentals-listings/{id}",
            patch(handlers::refresh_rentals_listing),
        )
        .route(
            "/rental-listings/prices",
            get(handlers::get_rental_listings_prices),
        )
        .route("/rentals-index", get(handlers::get_rentals_index));
    let router = Router::new()
        .nest("/v1", v1)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn(
            crate::shared::cors::cors_middleware,
        ))
        .with_state(state);
    DataWrites::guard(router, writes_enabled)
}
