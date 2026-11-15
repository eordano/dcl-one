//! Camera reel: the player photo gallery behind `camera-reel-service`:
//! uploads (multipart and base64 JSON) with a generated 640x360 thumbnail,
//! per-user galleries and counts under `MAX_IMAGES_PER_USER`, public place
//! and world galleries, the public photos of people wearing an item, owner
//! visibility and deletion, the moderator's review status and delete.
//! Schema `camera_reel` (`schemas/50_create.sql`).
//!
//! What it reuses rather than duplicates: the ONE pool ([`db`]), the ONE
//! blob store (`storage`) for the image and thumbnail bytes by
//! `auth::hash_bytes_v1` hash. The wire URLs retain the upload's UUID and
//! basename under the literal configured `api_url`; reserved filename characters
//! are not URL-escaped. Rows without a retained basename keep hash URLs.
//! Signed fetch goes through the shared verifier under the camera-reel
//! profiles (`shared::signed_fetch::VerificationProfile::camera_reel_read` and
//! `camera_reel_write`: upstream's dcl-crypto-middleware-rs rules, a signature
//! at most 60 s old and never too new, mandatory raw metadata, the 6.x payload
//! followed by the unguarded legacy one) with the process's EIP-1654
//! validator; every chain link must carry a signature string, as the source's
//! parser requires. A write whose metadata names the scene signer
//! `decentraland-kernel-scene` under any spelling, or whose lowercased bytes
//! contain it, is refused (upstream never checks it); a read serves a scene's
//! request as its signer, as upstream does. World names resolve through
//! local `explore.place_indexed` reads ([`db::Database::world_place_ids`]).
//!
//! Every write route goes behind `CREATE_WRITES_ENABLED` ([`writes`];
//! default false, 503 with `Retry-After`, built the way `social::writes`
//! gates the social writes); the reads (galleries, metadata, blobs, the
//! batch place gallery behind `POST /api/places/images`) serve either way.
//! CORS is actix-cors's as upstream configured it ([`http`]); the query and
//! JSON body rejections are actix's plain-text 400/413 ([`http`]); a path
//! or method no route serves is an empty 404.
//!
//! Photo events: camera-reel-service published `photo-taken` and
//! `photo-privacy-changed` to SNS; here the domains call each other in
//! process, with no broker. Each event is recorded, with the upstream body,
//! in the outbox `camera_reel.photo_events` ([`events`]) in the transaction
//! of the change it reports. The community photo counts that
//! social-service-ea derived from `photo-taken` are applied directly in the
//! upload's finalising transaction (`social::communities::ranking::record_photo`);
//! no consumer currently reads the outbox. `photo-privacy-changed` has no
//! consumer.
//!
//! Authorization: an upload's `metadata.userAddress`
//! must be the signer (and the row's CHECK holds the pair together);
//! private rows are listed only to their owner (an unverifiable chain reads
//! as anonymous); delete and visibility are the owner's (403 for anyone
//! else, 404 for an absent row); the admin routes take the bearer
//! `CATALYRST_CAMERA_REEL_ADMIN_TOKEN` (403 unset, 401 wrong); a `rejected`
//! row is omitted from public galleries and its metadata answers 404.
//! Its blobs answer 404 on the protected byte routes of the ONE store:
//! the reel's `GET /api/files/{name}` (from `review_status`), the content server's
//! `/contents/{hash}` at the root and under the worlds nest and the
//! builder's `/contents/{hash}` (from `camera_reel.withheld_blobs`, written
//! in the review's and the delete's transaction and mirrored into the shared
//! in-memory `content::Denylist`, refilled at boot by
//! [`State::load_withheld`]). A hash is
//! newly withheld only when no non-rejected row and no content file or builder
//! upload names it, so an owner's delete cannot hide a deployed file of the
//! same bytes. A later successful upload can release deletion withholding;
//! it does not release rejected hashes or operator and remote denials. A private, non-rejected photo is served to whoever holds its
//! filename URL or hash. Deletion removes the gallery row and applies withholding
//! without unlinking immutable shared blobs. `GET /api/files/{name}` serves only
//! a hash a non-rejected camera row names; arbitrary content files cannot be
//! downloaded through this route. Beside the offset pages: a keyset page
//! (`?cursor=`) over the same order, `nextCursor` on the wire only when
//! asked for.
//!
//! `GET /api/images/{name}` redirects whether or not the object exists.
//! With no bucket origin configured, the target is `/api/files/{name}`,
//! which resolves exact filename aliases and checks the underlying hash's
//! denylist. With a configured bucket origin (an http(s) URL without
//! credentials, query or fragment; anything else falls back to
//! `/api/files`), redirects retain the requested filename without reading the
//! database, and a hash the shared denylist withholds answers 404 instead.
//! Point that origin at `/api/files` or configure its ingress to resolve
//! filenames; a hash-only origin is insufficient. An external origin must
//! enforce its own object availability; this process only controls local
//! bytes. Filename aliases retain the photo's accepted upload MIME
//! for both objects; older rows without it and raw hash URLs sniff stored bytes.
//! The local byte route serves complete GET responses; it
//! does not implement byte ranges or ETag validators, and its HEAD response is 404.
//!
//! Mount: `.nest("/camera-reel-service", camera_reel::router(state))`;
//! the router's own paths are `/api/...`, `/admin/...`, `/health`. It is not
//! merged at the root: `/api/places/{id}/...` is the explore places
//! surface there. A per-vhost proxy for `camera-reel-service.<domain>`
//! forwards `/x` to `/camera-reel-service/x`; a client signs the path it
//! requested, which is what the camera verifier checks
//! (`signed_fetch::candidate_paths`, the ONE path rule: the request's own
//! path, and the `x-original-path` the proxy sets when it ends with the
//! route).
//!
//! `/metrics` exposes this router's request counts and latency histograms.
//! Scrapes require `CAMERA_REEL_METRICS_BEARER_TOKEN`; an unset token fails closed.
//! Documentation and Swagger UI assets are served locally under `/api/docs`.

// The crates this one builds on, under their module names (AGENTS.md).
#[cfg(test)]
pub(crate) use catalyst_builder as builder;
pub(crate) use catalyst_content as content;
pub(crate) use catalyst_jobs as jobs;
pub(crate) use catalyst_shared as shared;
pub(crate) use catalyst_social as social;
pub(crate) use catalyst_storage as storage;
pub(crate) use dcl_auth as auth;

pub mod admin;
pub mod db;
pub mod docs;
pub mod dto;
pub mod events;
pub mod handlers;
pub mod http;
pub mod writes;

#[cfg(test)]
mod http_parity_tests;
#[cfg(test)]
mod tests;

extern crate image_camera as image;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderMap;
use axum::routing::{delete, get, patch, post};
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::content::Denylist;
use crate::shared::signed_fetch::{self, SignedFetchError, VerificationProfile};
use crate::storage::Storage;

use http::ApiError;

pub const DEFAULT_MAX_IMAGES_PER_USER: u64 = 500;
pub const DEFAULT_PLACES_CACHE_TTL_SECONDS: u64 = 300;
pub const DEFAULT_PLACES_CACHE_MAX_SIZE: u64 = 1000;

/// The body cap, applied to the whole router.
pub const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Settings {
    /// Base of the rendered `url`/`thumbnailUrl` (old `API_URL`): the public
    /// URL of this router, `<PUBLIC_URL>/camera-reel-service`.
    pub api_url: String,
    /// Public filename origin. Every requested name is preserved in the redirect.
    /// Without an external origin, image URLs redirect to this service's protected
    /// `/api/files` route, which resolves names to immutable hashes internally.
    pub bucket_url: Option<String>,
    /// Old `MAX_IMAGES_PER_USER` (500).
    pub max_images_per_user: u64,
    /// Old `CATALYRST_CAMERA_REEL_ADMIN_TOKEN`; unset refuses the admin routes.
    pub admin_token: Option<String>,
    /// `CREATE_WRITES_ENABLED`.
    pub writes_enabled: bool,
    /// Protects the service metrics endpoint; an unset token refuses scrapes.
    pub metrics_bearer_token: Option<String>,
    /// Successful world-name lookup cache lifetime (`PLACES_CACHE_TTL_SECONDS`).
    pub places_cache_ttl_seconds: u64,
    /// Maximum world-name lookup cache entries (`PLACES_CACHE_MAX_SIZE`).
    pub places_cache_max_size: u64,
}

impl Settings {
    pub fn new(api_url: impl Into<String>) -> Self {
        Self {
            api_url: api_url.into(),
            bucket_url: None,
            max_images_per_user: DEFAULT_MAX_IMAGES_PER_USER,
            admin_token: None,
            writes_enabled: false,
            metrics_bearer_token: None,
            places_cache_ttl_seconds: DEFAULT_PLACES_CACHE_TTL_SECONDS,
            places_cache_max_size: DEFAULT_PLACES_CACHE_MAX_SIZE,
        }
    }
}

pub struct State {
    pub settings: Settings,
    pub db: db::Database,
    pub storage: Arc<Storage>,
    /// The store's denylist (`storage.denylist()`, the `Arc` the content
    /// state shares): what a rejection or a delete withholds here the store
    /// withholds on every route that reads it by hash, this module's
    /// `/api/images/{hash}` included.
    pub denylist: Arc<Denylist>,
    eip1654: Option<Arc<dyn Eip1654Validator>>,
    metrics: http::Metrics,
    /// Keep a committed camera verdict and its shared in-memory projection in
    /// the same order across upload finalization, deletion and moderation.
    blob_verdict: tokio::sync::Mutex<()>,
}

pub type SharedState = Arc<State>;

pub use crate::shared::signed_fetch::SignedPath;

pub(crate) fn signed_path(original: &axum::http::Uri, local: impl Into<String>) -> SignedPath {
    SignedPath::at(original, local)
}
impl State {
    pub fn new(
        pool: PgPool,
        storage: Arc<Storage>,
        settings: Settings,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        let db = db::Database::new(
            pool,
            settings.api_url.clone(),
            settings.places_cache_ttl_seconds,
            settings.places_cache_max_size,
        );
        let denylist = storage.denylist().clone();
        Self {
            settings,
            db,
            storage,
            denylist,
            eip1654,
            metrics: http::Metrics::default(),
            blob_verdict: tokio::sync::Mutex::new(()),
        }
    }

    /// Fills the shared denylist from `camera_reel.withheld_blobs`; `serve`
    /// runs it once before listening, so a restart keeps every rejected and
    /// deleted photo's bytes off the content routes. Returns the count.
    pub async fn load_withheld(&self) -> Result<usize, sqlx::Error> {
        let hashes = self.db.withheld_hashes().await?;
        for hash in &hashes {
            self.denylist.add(hash);
        }
        Ok(hashes.len())
    }

    /// Mirrors what a review or delete wrote to `withheld_blobs`.
    pub(crate) fn apply_verdict(&self, verdict: &db::BlobVerdict) {
        for hash in &verdict.withheld {
            self.denylist.add(hash);
        }
        for hash in &verdict.released {
            self.denylist.remove(hash);
        }
    }

    async fn verify(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &SignedPath,
        profile: VerificationProfile,
    ) -> Result<String, SignedFetchError> {
        // The source chain parser requires a string signature on every link,
        // including the SIGNER's normally empty field. Shared extraction is
        // more permissive for other services, so enforce Camera's wire shape here.
        let chain = signed_fetch::extract_auth_chain(headers)?;
        if chain.iter().any(|link| link.signature.is_none()) {
            return Err(SignedFetchError::MalformedChain {
                detail: "missing auth link signature".to_owned(),
            });
        }
        let signer = signed_fetch::require_signer_with(
            headers,
            method,
            path,
            &profile,
            self.eip1654.as_deref(),
        )
        .await?;
        Ok(signer.as_str().to_owned())
    }

    /// The verified signer of a write; every refusal is 401, a request a
    /// scene signed included.
    pub async fn require_auth(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &SignedPath,
    ) -> Result<String, ApiError> {
        self.verify(
            headers,
            method,
            path,
            VerificationProfile::camera_reel_write(),
        )
        .await
        .map_err(|e| {
            tracing::debug!(error = %e, "auth chain verification failed");
            ApiError::SignedFetchUnauthorized
        })
    }

    /// The signer of a read that widens for its owner: no chain, or one that
    /// does not verify, reads as anonymous. A scene's request is served as
    /// its signer, as upstream serves it.
    pub async fn optional_auth(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &SignedPath,
    ) -> Option<String> {
        headers.get(format!("{}0", signed_fetch::AUTH_CHAIN_HEADER_PREFIX))?;
        match self
            .verify(
                headers,
                method,
                path,
                VerificationProfile::camera_reel_read(),
            )
            .await
        {
            Ok(signer) => Some(signer),
            Err(e) => {
                tracing::debug!(error = %e, "optional auth chain verification failed");
                None
            }
        }
    }

    pub fn authorize_admin(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        admin::check_admin(self.settings.admin_token.as_deref(), headers)
    }
}

async fn health() -> axum::Json<&'static str> {
    axum::Json("alive")
}

async fn not_found() -> axum::http::StatusCode {
    axum::http::StatusCode::NOT_FOUND
}

/// axum lists the served methods in `Allow` on its method fallback; the 404
/// that fallback answers here carries no such header, as upstream's.
async fn without_allow(mut res: axum::response::Response) -> axum::response::Response {
    // MethodRouter adds this after route middleware, including CORS preflights.
    // The source route guards and CORS responses never emit an Allow header.
    res.headers_mut().remove(axum::http::header::ALLOW);
    res
}

/// The merged `/api` and `/admin` trees plus the executable's `/health`.
/// Each write route's methods sit behind [`writes::route`] with the state's
/// `CREATE_WRITES_ENABLED`; a path or a method no route serves is an empty
/// 404, as upstream's actix default service answers; [`http::cors`] wraps
/// it all, refusals and 404s included.
pub fn router(state: SharedState) -> Router {
    let on = state.settings.writes_enabled;
    let gated = |route| writes::route(route, on);
    let api = Router::new()
        .route("/images", gated(post(handlers::images::upload_image)))
        .route(
            "/images-json",
            gated(post(handlers::images::upload_image_json)),
        )
        .route("/files/{image_id}", get(handlers::images::get_image_bytes))
        .route(
            "/images/{image_id}",
            get(handlers::images::get_image).merge(gated(delete(handlers::images::delete_image))),
        )
        .route(
            "/images/{image_id}/visibility",
            gated(patch(handlers::images::update_image_visibility)),
        )
        .route(
            "/images/{image_id}/metadata",
            get(handlers::images::get_metadata),
        )
        .route("/users/{user_address}", get(handlers::users::get_user_data))
        .route(
            "/users/{user_address}/images",
            get(handlers::users::get_user_images),
        )
        .route(
            "/places/{place_id}/images",
            get(handlers::places::get_place_images),
        )
        .route(
            "/places/images",
            post(handlers::places::get_multiple_places_images),
        )
        .route(
            "/wearables/{item}/images",
            get(handlers::wearables::get_wearable_images),
        )
        .route(
            "/docs/openapi.json",
            get(docs::openapi_json).head(|| async { axum::http::StatusCode::NOT_FOUND }),
        )
        .route(
            "/docs/ui",
            get(docs::swagger_ui).head(|| async { axum::http::StatusCode::NOT_FOUND }),
        )
        .route(
            "/docs/ui/",
            get(docs::swagger_ui).head(|| async { axum::http::StatusCode::NOT_FOUND }),
        )
        .route(
            "/docs/ui/{*rest}",
            get(docs::swagger_ui).head(|| async { axum::http::StatusCode::NOT_FOUND }),
        );

    let admin = Router::new()
        .route(
            "/admin/images/{image_id}",
            gated(delete(handlers::images::admin_delete_image)),
        )
        .route(
            "/admin/images/{image_id}/review",
            gated(patch(handlers::images::admin_update_image_review)),
        );

    let router = Router::new()
        .route("/health", get(health))
        .route("/health/live", get(health))
        .route(
            "/metrics",
            get(http::metrics).head(|| async { axum::http::StatusCode::NOT_FOUND }),
        )
        .nest("/api", api)
        .merge(admin)
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state.clone());
    let router = router
        .layer(axum::middleware::from_fn(http::cors))
        .layer(axum::middleware::from_fn_with_state(state, http::observe));
    Router::new()
        .fallback_service(
            tower::ServiceBuilder::new()
                .map_request(http::routing_request)
                .service(router),
        )
        .layer(axum::middleware::map_response(without_allow))
}
