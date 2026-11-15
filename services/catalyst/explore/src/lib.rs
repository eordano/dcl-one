//! The explore domain: places, the worlds catalogue, events, the map and the
//! explorer-facing `/api/destinations` feed, over the `explore.*` schema.
//!
//! Reads are served from the mirrored catalogues (`explore.place`,
//! `explore.event`) plus the local overlays; the mirrors run as periodic jobs
//! ([`ExploreState::spawn_jobs`]) against explicit upstream endpoints
//! ([`MirrorSource`], which refuses a source that is this node or a private
//! address), each persisting its cursor and pass markers so a restart resumes
//! and `GET /api/status` reports how fresh each catalogue is. Writes (favorites,
//! likes, RSVPs, event/schedule creation, reports) arrive with stage 5.

// The crates this one builds on, under their module names (AGENTS.md).
pub(crate) use catalyst_config::config;
pub(crate) use catalyst_content as content;
#[cfg(test)]
pub(crate) use catalyst_db as db;
pub(crate) use catalyst_jobs as jobs;
pub(crate) use catalyst_land_authz as land_authz;
pub(crate) use catalyst_shared as shared;
pub(crate) use catalyst_storage as storage;
pub(crate) use dcl_auth as auth;
pub(crate) use dcl_http as http;

pub mod api;
pub mod copies;
pub mod cors;
pub mod events;
pub mod fed;
pub mod filecache;
pub mod imposters;
pub(crate) mod json_date;
mod keyed_locks;
pub mod mirror_state;
pub mod moderation_hooks;
pub mod opensea;
pub mod places;
pub mod rate;
pub mod sanitize;
pub mod schema_functions;
pub mod validate;

use std::sync::Arc;

use crate::shared::signed_fetch;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Json;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, patch, post, put};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::shared::errors::ApiError;
use crate::shared::openapi;
use events::EventsComponent;
use events::schedules::SchedulesComponent;
use places::PlacesComponent;

/// An upstream catalogue endpoint a mirror reads from. Built once at startup:
/// the URL must be http(s) to a publicly routable host (the SSRF guard of
/// [`crate::http`]), and must not name this node itself, which would make the
/// mirror read its own copy back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorSource {
    base: String,
}

impl MirrorSource {
    /// `local_urls`: every URL this node answers at (PUBLIC_URL, the bind
    /// address); a source whose host is one of them is a loop.
    pub fn checked(url: &str, local_urls: &[&str]) -> anyhow::Result<Self> {
        let trimmed = url.trim().trim_end_matches('/');
        let parsed = url::Url::parse(trimmed).context("mirror source is not a URL (the value is not echoed: a hosted endpoint may carry a token)")?;
        let Some(host) = parsed.host_str() else {
            bail!("mirror source has no host");
        };
        if !crate::http::is_safe_http_url(trimmed) {
            bail!(
                "mirror source host {host} is not a public http(s) endpoint; a mirror reads an \
                 explicit upstream, never a local or private address"
            );
        }
        for local in local_urls {
            if let Ok(local) = url::Url::parse(local)
                && local
                    .host_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case(host))
                && local.port_or_known_default() == parsed.port_or_known_default()
            {
                bail!(
                    "mirror source {trimmed:?} is this node itself ({local}); refusing a self-referential mirror"
                );
            }
        }
        Ok(Self {
            base: trimmed.to_string(),
        })
    }

    /// No guard: for tests against a stub upstream on the loopback.
    pub fn unchecked(url: &str) -> Self {
        Self {
            base: url.trim().trim_end_matches('/').to_string(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }
}

/// The subset of the process configuration the explore domain reads. The old
/// env names: PLACES_MIRROR_UPSTREAM/PLACES_UPSTREAM_URL/PLACES_MIRROR_INTERVAL_SECS,
/// WORLDS_MIRROR_UPSTREAM/WORLDS_UPSTREAM_URL/WORLDS_MIRROR_INTERVAL_SECS,
/// EVENTS_MIRROR_UPSTREAM/EVENTS_UPSTREAM_URL/EVENTS_MIRROR_INTERVAL_SECS,
/// CATALYRST_EVENTS_ADMIN_TOKEN, CONTENT_PUBLIC_URL and HTTP_BASE_URL (the
/// asset rewrite domain, derived from PUBLIC_URL here).
#[derive(Clone)]
pub struct Settings {
    /// The node's content base URL; thumbnails from it pass the image filter.
    pub content_public_url: String,
    /// The deployment's own domain, swapped into mirrored upstream poster
    /// URLs; `None` (a decentraland.org node, or no PUBLIC_URL) keeps them.
    pub asset_rewrite_domain: Option<String>,
    /// Bearer for the events moderation routes; unset refuses them.
    pub events_admin_token: Option<String>,
    /// Optional moderation webhook, dispatched only with operator writes enabled.
    pub events_slack_webhook: Option<String>,
    /// Opt-in for the one Slack notice per upcoming occurrence, queued with
    /// its reminders; off by default.
    pub events_upcoming_summaries_enabled: bool,
    pub places_upstream: Option<MirrorSource>,
    pub places_interval: Duration,
    pub worlds_upstream: Option<MirrorSource>,
    pub worlds_interval: Duration,
    pub events_upstream: Option<MirrorSource>,
    pub events_interval: Duration,
    /// `EVENTS_MIRROR_ADOPT`: adopt the rows the events mirror wrote as this
    /// node's own, once per database (`events::mirror::adopt_upstream`).
    pub events_adopt: bool,
    /// Which scenes the `places-derive` job turns into place rows
    /// (`places::derive`); `None` attaches no job (tests).
    pub places_derive: Option<places::derive::DeriveScope>,
    pub places_derive_interval: Duration,
    /// `DATA_WRITES_ENABLED`: the users' own writes (favorites, likes, RSVPs,
    /// their events, place reports) are local authority behind the data
    /// switch; off, each answers 503 with `Retry-After` and the reads serve.
    pub user_writes_enabled: bool,
    /// `OPS_WRITES_ENABLED`: the places admin routes (moderation, rankings,
    /// POIs, the reports queue's decisions) are operator writes.
    pub operator_writes_enabled: bool,
    /// `PLACES_ADMIN_AUTH_TOKEN`: the places admin bearer; unset refuses it.
    pub places_admin_token: Option<String>,
    /// `DATA_TEAM_AUTH_TOKEN`: the automated-ranking bearer.
    pub data_team_token: Option<String>,
    /// `PLACES_ADMIN_ADDRESSES`: wallets whose signed fetch may moderate.
    pub places_admin_addresses: Vec<String>,
    /// `PLACES_BASE_URL`: the places site the social cards' canonical URLs
    /// and the moderation notices point at (`places::social`,
    /// `places::moderation`).
    pub places_base_url: String,
    /// `PLACES_MODERATION_WEBHOOK`: the places moderation notices' webhook,
    /// captured and dispatched only with operator writes enabled.
    pub places_moderation_webhook: Option<String>,
    /// `SNAPSHOT_SCORE_URL`: the Snapshot score API the favorite and like
    /// writes ask for the signer's voting power (`places::voting_power`);
    /// `None` asks none (every voter scores 0).
    pub snapshot_score_url: Option<String>,
    /// `EVENTS_BASE_URL`: the events site the sitemaps point at.
    pub events_base_url: String,
    /// `JUMP_IN_SITE_URL`: the jump-in site the event notifications and the
    /// Slack notices link to (`Notifications/index.ts:18`, `Slack/utils.ts:13`).
    pub jump_in_site_url: String,
    /// Backend asset mount derived from the node public URL, never the events website.
    pub events_asset_base_url: String,
    /// `PUBLIC_URL`: this node's own origin; issued URLs (report uploads)
    /// are built on it, never on request headers.
    pub public_url: String,
    /// `HTTP_TRUSTED_PROXY_CIDRS`: peers whose `X-Real-IP` names the client.
    pub trusted_proxy_cidrs: Vec<ipnet::IpNet>,
    /// This node's realm name, the `serverName` a place's `realms_detail`
    /// carries.
    pub realm_name: String,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("content_public_url", &self.content_public_url)
            .field("user_writes_enabled", &self.user_writes_enabled)
            .field("operator_writes_enabled", &self.operator_writes_enabled)
            .field(
                "events_slack_webhook",
                &self.events_slack_webhook.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "places_moderation_webhook",
                &self
                    .places_moderation_webhook
                    .as_ref()
                    .map(|_| "[REDACTED]"),
            )
            .finish_non_exhaustive()
    }
}

impl Settings {
    /// The old `asset_rewrite_domain` rule: the public host unless it is
    /// decentraland.org or a subdomain. Only a domain can carry the
    /// `events-assets-*` subdomain, so an IP address, `localhost` or another
    /// single-label host (a local development stack) keeps the upstream URLs.
    pub fn asset_rewrite_domain_for(public_url: Option<&str>) -> Option<String> {
        public_url
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| match u.host() {
                Some(url::Host::Domain(h)) => Some(h.to_string()),
                _ => None,
            })
            .filter(|h| h.contains('.'))
            .filter(|h| h != "decentraland.org" && !h.ends_with(".decentraland.org"))
    }

    pub fn events_asset_base_for(public_url: &str) -> String {
        format!("{}/events", public_url.trim_end_matches('/'))
    }

    pub fn for_tests() -> Self {
        Self {
            content_public_url: "http://localhost/content/".to_string(),
            asset_rewrite_domain: None,
            events_admin_token: None,
            events_slack_webhook: None,
            events_upcoming_summaries_enabled: false,
            places_upstream: None,
            places_interval: Duration::from_secs(3600),
            worlds_upstream: None,
            worlds_interval: Duration::from_secs(3600),
            events_upstream: None,
            events_interval: Duration::from_secs(3600),
            events_adopt: false,
            places_derive: None,
            places_derive_interval: places::derive::DEFAULT_INTERVAL,
            user_writes_enabled: false,
            operator_writes_enabled: false,
            places_admin_token: None,
            data_team_token: None,
            places_admin_addresses: Vec::new(),
            places_base_url: "https://decentraland.org/places".to_string(),
            places_moderation_webhook: None,
            snapshot_score_url: None,
            events_base_url: "https://events.decentraland.org".to_string(),
            jump_in_site_url: "https://decentraland.org/jump".to_string(),
            events_asset_base_url: Self::events_asset_base_for("http://localhost"),
            public_url: "http://localhost".to_string(),
            trusted_proxy_cidrs: Vec::new(),
            realm_name: "main".to_string(),
        }
    }
}

/// The 503 message of an operator write while `OPS_WRITES_ENABLED` is off.
pub const OPERATOR_WRITES_DISABLED: &str = "operator writes are disabled on this node (OPS_WRITES_ENABLED=false): a backup restore has to be demonstrated before this node accepts operator writes";

/// The 503 message of a user write while `DATA_WRITES_ENABLED` is off.
pub const USER_WRITES_DISABLED: &str = "explore writes are disabled on this node (DATA_WRITES_ENABLED=false): a backup restore has to be demonstrated before this node accepts user writes";

pub struct ExploreState {
    pub pool: PgPool,
    pub settings: Settings,
    pub places: PlacesComponent,
    pub events: EventsComponent,
    pub schedules: SchedulesComponent,
    /// Contract-wallet (EIP-1654) signature check for signed-fetch chains;
    /// `None` reads such chains as anonymous on the optional path.
    pub eip1654: Option<Arc<dyn Eip1654Validator>>,
    /// The ONE blob store, for the event poster uploads; `None` answers them 503.
    pub storage: Option<Arc<crate::storage::Storage>>,
    /// The destinations' live headcounts and connected wallets
    /// (`places::live`; comms attaches its rooms once it is up).
    pub live: Arc<places::live::Live>,
    /// The mirrors' pass markers, the `as_of` of their reads (`copies`).
    pub pass_markers: copies::Markers,
    /// Whether each catalogue holds rows of this node's own (`copies`).
    pub local_rows: copies::LocalRows,
    /// The `/api` CORS allow-lists (`cors`); upstream's by default.
    pub cors: Arc<cors::Policy>,
    /// The Snapshot voting-power client the favorite and like writes score
    /// their signer with (`places::voting_power`).
    pub voting_power: places::voting_power::Client,
}

pub type SharedState = Arc<ExploreState>;

impl ExploreState {
    pub fn new(
        pool: PgPool,
        settings: Settings,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        Self {
            voting_power: places::voting_power::Client::new(settings.snapshot_score_url.clone()),
            places: PlacesComponent::new(pool.clone(), &settings.content_public_url)
                .with_worlds_mirror(settings.worlds_upstream.is_some()),
            events: EventsComponent::new(
                pool.clone(),
                settings.asset_rewrite_domain.clone(),
                settings.events_asset_base_url.clone(),
            ),
            schedules: SchedulesComponent::new(pool.clone()),
            live: Arc::new(places::live::Live::new(pool.clone())),
            pass_markers: copies::Markers::new(copies::MARKER_TTL),
            local_rows: copies::LocalRows::new(copies::MARKER_TTL),
            pool,
            settings,
            eip1654,
            storage: None,
            cors: Arc::new(cors::Policy::upstream()),
        }
    }

    /// The blob store the poster uploads land in.
    pub fn with_storage(mut self, storage: Arc<crate::storage::Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// A node's own CORS allow-lists in place of upstream's.
    pub fn with_cors(mut self, policy: cors::Policy) -> Self {
        self.cors = Arc::new(policy);
        self
    }

    /// The outbound HTTP client the voting-power lookups leave through (the
    /// process's shared guarded client in `serve`; a loopback-admitting one
    /// in tests with a stub score API).
    pub fn with_outbound(mut self, client: crate::http::Outbound) -> Self {
        self.voting_power = self.voting_power.with_http(client);
        self
    }

    /// A user write may proceed: `DATA_WRITES_ENABLED` is on and the
    /// restore stamps vouch for this node.
    pub fn user_writes(&self) -> Result<(), ApiError> {
        match crate::shared::writes::refusal(self.settings.user_writes_enabled) {
            None => Ok(()),
            Some(r) => Err(ApiError::WritesDisabled(
                r.message("explore", USER_WRITES_DISABLED),
            )),
        }
    }

    /// An operator write may proceed: `OPS_WRITES_ENABLED` is on and the
    /// restore stamps vouch for this node.
    pub fn operator_writes(&self) -> Result<(), ApiError> {
        match crate::shared::writes::ops_refusal(self.settings.operator_writes_enabled) {
            None => Ok(()),
            Some(r) => Err(ApiError::WritesDisabled(
                r.message("operator", OPERATOR_WRITES_DISABLED),
            )),
        }
    }

    /// The lowercased signer a write is attributed to: a verified signed
    /// fetch over `path` (or the `x-original-path` a prefix-stripping proxy
    /// forwards), else the events service's 401 "Unauthorized" (gatsby
    /// `auth()`, `Auth/middleware.js:35-39`, which hides the reason).
    pub async fn signer(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
    ) -> Result<String, ApiError> {
        self.verify_signer(headers, method, path)
            .await
            .map_err(|e| {
                tracing::debug!(error = %e, "signed-fetch verification failed");
                ApiError::unauthorized("Unauthorized")
            })
    }

    /// [`Self::signer`] for the places service's wkc routes (`withAuth`,
    /// gatsby `Auth/routes/withDecentralandAuth.js:12-49`), which answer the
    /// `@dcl/crypto-middleware` refusal itself: no or short chain → 400
    /// "Invalid Auth Chain" (`internal-verification.js:93-95`), a malformed
    /// link → 400 "Invalid chain format: …" (`:82-91`), a bad timestamp →
    /// 400 "Invalid chain timestamp: …" (`:201`), an old one → 401 "Expired
    /// signature: …" (`:229-234`), a bad signature → 401 "Invalid signature:
    /// …" (`:109`, `:183`). Capture `places.patch_favorites.no_auth`.
    pub async fn wkc_signer(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
    ) -> Result<String, ApiError> {
        use signed_fetch::SignedFetchError as E;
        self.verify_signer(headers, method, path).await.map_err(|e| {
            tracing::debug!(error = %e, "signed-fetch verification failed");
            match e {
                E::InsufficientLinks => ApiError::bad_request("Invalid Auth Chain"),
                E::MalformedChain { detail } => {
                    ApiError::bad_request(format!("Invalid chain format: {detail}"))
                }
                E::InvalidTimestamp(raw) => {
                    ApiError::bad_request(format!("Invalid chain timestamp: {raw}"))
                }
                E::Expired {
                    signed_at,
                    now,
                    window_secs,
                } => ApiError::unauthorized(format!(
                    "Expired signature: signature timestamp: {}, timestamp expiration: {}, local timestamp: {}",
                    signed_at.saturating_mul(1000),
                    signed_at.saturating_add(window_secs).saturating_mul(1000),
                    now.saturating_mul(1000)
                )),
                E::InvalidSignature(detail) => {
                    ApiError::unauthorized(format!("Invalid signature: {detail}"))
                }
                E::EipNotImplemented => ApiError::unauthorized(format!("Invalid signature: {e}")),
                E::CatalystUnavailable(_) => {
                    ApiError::service_unavailable("Signature verification unavailable")
                }
            }
        })
    }

    async fn verify_signer(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
    ) -> Result<String, signed_fetch::SignedFetchError> {
        signed_fetch::require_signer(
            headers,
            method,
            path,
            signed_fetch::PayloadPolicy::V6ThenLegacy,
            self.eip1654.as_deref(),
        )
        .await
        .map(|s| s.into_string())
    }

    /// The lowercased signer of a signed-fetch request, or `None` for an
    /// anonymous or unverifiable one.
    pub async fn viewer(&self, headers: &HeaderMap, method: &str, path: &str) -> Option<String> {
        signed_fetch::optional_signer(
            headers,
            method,
            path,
            signed_fetch::PayloadPolicy::V6ThenLegacy,
            self.eip1654.as_deref(),
        )
        .await
        .map(|s| s.into_string())
    }

    /// Attaches the configured mirrors as periodic jobs: the first pass right
    /// away (jittered), then every interval, doubling the wait after a failed
    /// pass up to an hour.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs, client: crate::http::Outbound) {
        if self.settings.operator_writes_enabled {
            moderation_hooks::spawn(
                self.pool.clone(),
                client.clone(),
                moderation_hooks::Endpoints {
                    events: self.settings.events_slack_webhook.clone(),
                    places: self.settings.places_moderation_webhook.clone(),
                },
                events::hooks::Links {
                    base: self.settings.events_base_url.clone(),
                    jump_in_site_url: self.settings.jump_in_site_url.clone(),
                    asset_base: self.settings.events_asset_base_url.clone(),
                },
                jobs,
            );
        }
        let cfg = |interval: Duration| {
            PeriodicCfg::every(interval)
                .with_jitter(Duration::from_secs(30))
                .with_error_backoff(Duration::from_secs(3600))
        };
        {
            let pool = self.pool.clone();
            let cfg = PeriodicCfg::every(Duration::from_secs(300))
                .with_jitter(Duration::from_secs(30))
                .with_error_backoff(Duration::from_secs(900));
            jobs.spawn_periodic_leased("explore-nonce-sweep", cfg, move |_ctx| {
                let pool = pool.clone();
                async move {
                    catalyst_fed::sweep_nonces(&pool, fed::NONCES, chrono::Utc::now().timestamp())
                        .await?;
                    Ok(())
                }
            });
        }
        events::recurrence::spawn(self.pool.clone(), jobs);
        if let Some(scope) = self.settings.places_derive {
            places::derive::spawn_job(
                jobs,
                self.pool.clone(),
                scope,
                self.settings.places_derive_interval,
                self.settings.content_public_url.clone(),
                self.storage.clone(),
            );
        }
        if let Some(source) = self.settings.places_upstream.clone() {
            let (client, pool, interval) = (
                client.clone(),
                self.pool.clone(),
                self.settings.places_interval,
            );
            tracing::info!(
                upstream = source.base(),
                interval_secs = interval.as_secs(),
                "place catalogue: mirroring from upstream"
            );
            jobs.spawn_periodic_leased("places-mirror", cfg(interval), move |ctx| {
                let (client, pool, source) = (client.clone(), pool.clone(), source.clone());
                async move {
                    let out = places::mirror::run_places_pass(
                        &client,
                        &pool,
                        &source,
                        interval,
                        &ctx.cancel,
                    )
                    .await?;
                    ctx.progress(format!(
                        "upserted {} retired {} swept {} complete {} resumed_from {:?}",
                        out.upserted, out.retired, out.swept, out.complete, out.resumed_from
                    ));
                    tracing::info!(
                        upserted = out.upserted,
                        retired = out.retired,
                        swept = out.swept,
                        complete = out.complete,
                        cancelled = out.cancelled,
                        "place catalogue mirrored from upstream"
                    );
                    Ok(())
                }
            });
        }
        if let Some(source) = self.settings.worlds_upstream.clone() {
            let (client, pool, interval) = (
                client.clone(),
                self.pool.clone(),
                self.settings.worlds_interval,
            );
            tracing::info!(
                upstream = source.base(),
                interval_secs = interval.as_secs(),
                "world catalogue: mirroring from upstream"
            );
            jobs.spawn_periodic_leased("worlds-mirror", cfg(interval), move |ctx| {
                let (client, pool, source) = (client.clone(), pool.clone(), source.clone());
                async move {
                    let out = places::mirror::run_worlds_pass(
                        &client,
                        &pool,
                        &source,
                        interval,
                        &ctx.cancel,
                    )
                    .await?;
                    ctx.progress(format!(
                        "upserted {} skipped {} failed_pages {} swept {} complete {}",
                        out.upserted, out.skipped_rows, out.failed_pages, out.swept, out.complete
                    ));
                    tracing::info!(
                        upserted = out.upserted,
                        skipped_rows = out.skipped_rows,
                        failed_rows = out.failed_rows,
                        failed_pages = out.failed_pages,
                        swept = out.swept,
                        complete = out.complete,
                        cancelled = out.cancelled,
                        "world catalogue mirrored from upstream"
                    );
                    if !out.complete && !out.cancelled {
                        anyhow::bail!(
                            "worlds pass incomplete: {} failed pages, {} failed rows",
                            out.failed_pages,
                            out.failed_rows
                        );
                    }
                    Ok(())
                }
            });
        }
        if let Some(source) = self.settings.events_upstream.clone() {
            let (client, pool, interval) = (
                client.clone(),
                self.pool.clone(),
                self.settings.events_interval,
            );
            tracing::info!(
                upstream = source.base(),
                interval_secs = interval.as_secs(),
                "event catalogue: mirroring from upstream"
            );
            jobs.spawn_periodic_leased("events-mirror", cfg(interval), move |ctx| {
                let (client, pool, source) = (client.clone(), pool.clone(), source.clone());
                async move {
                    let out =
                        events::mirror::run_pass(&client, &pool, &source, interval, &ctx.cancel)
                            .await?;
                    if out.adopted {
                        ctx.progress(
                            "adopted: the mirror writes nothing; unset EVENTS_MIRROR_UPSTREAM",
                        );
                        return Ok(());
                    }
                    ctx.progress(format!(
                        "upserted {} failed_pages {} swept {} unlisted gone {} complete {}",
                        out.upserted, out.failed_pages, out.swept, out.unlisted_gone, out.complete
                    ));
                    tracing::info!(
                        upserted = out.upserted,
                        skipped_rows = out.skipped_rows,
                        failed_rows = out.failed_rows,
                        failed_pages = out.failed_pages,
                        swept = out.swept,
                        unlisted_refreshed = out.unlisted_refreshed,
                        unlisted_gone = out.unlisted_gone,
                        unlisted_unconfirmed = out.unlisted_unconfirmed,
                        complete = out.complete,
                        cancelled = out.cancelled,
                        "event catalogue mirrored from upstream"
                    );
                    if !out.complete && !out.cancelled {
                        anyhow::bail!(
                            "events pass incomplete: {} failed pages, {} failed rows",
                            out.failed_pages,
                            out.failed_rows
                        );
                    }
                    Ok(())
                }
            });
        } else if self.settings.events_adopt {
            // The cut-over (`events::mirror::adopt_upstream`) under the
            // mirror's lease: it waits for a replica still mirroring, runs
            // once per database and finishes.
            let pool = self.pool.clone();
            jobs.spawn_leased("events-mirror", move |ctx| {
                let pool = pool.clone();
                async move {
                    match events::mirror::adopt_upstream(&pool).await? {
                        Some(rows) => ctx.progress(format!("adopted {rows} mirrored events")),
                        None => ctx.progress("mirrored events already adopted"),
                    }
                    Ok(())
                }
            });
        }
    }
}

/// `GET /openapi.json`: the events document under `/events`, the places one elsewhere.
async fn openapi_json(
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> axum::response::Response {
    if uri.path().starts_with("/events/") {
        openapi::document(openapi::EVENTS)
    } else {
        openapi::document(openapi::PLACES)
    }
}

#[derive(Debug, Serialize)]
struct CatalogueCounts {
    places: i64,
    worlds: i64,
    local_worlds: i64,
    events: i64,
    local_events: i64,
    /// The newest `fetched_at` on the mirrored rows, per catalogue.
    places_fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    events_fetched_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn catalogue_counts(pool: &PgPool) -> Result<CatalogueCounts, ApiError> {
    let (places, worlds, local_worlds, events, local_events, places_fetched_at, events_fetched_at): (
        i64,
        i64,
        i64,
        i64,
        i64,
        Option<chrono::NaiveDateTime>,
        Option<chrono::NaiveDateTime>,
    ) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM explore.place WHERE world IS FALSE), \
                (SELECT count(*) FROM explore.place WHERE world IS TRUE), \
                (SELECT count(*) FROM explore.place_world_local), \
                (SELECT count(*) FROM explore.event), \
                (SELECT count(*) FROM explore.event WHERE origin = 'local'), \
                (SELECT max(fetched_at) FROM explore.place WHERE origin = 'upstream'), \
                (SELECT max(fetched_at) FROM explore.event WHERE origin = 'upstream')",
    )
    .fetch_one(pool)
    .await?;
    Ok(CatalogueCounts {
        places,
        worlds,
        local_worlds,
        events,
        local_events,
        places_fetched_at: places_fetched_at.map(|t| t.and_utc()),
        events_fetched_at: events_fetched_at.map(|t| t.and_utc()),
    })
}

/// `GET /api/status`: the old places status envelope plus, honestly, what the
/// mirrors have done: each configured mirror's last complete pass and its lag
/// (or `null` when none finished yet), an in-flight cursor, the last error,
/// and the row counts behind the routes. A node that mirrors nothing says so.
async fn status(State(state): State<SharedState>) -> Result<Json<Value>, ApiError> {
    let mirrors = mirror_state::status(&state.pool).await?;
    let counts = catalogue_counts(&state.pool).await?;
    let configured = json!({
        "places": state.settings.places_upstream.as_ref().map(MirrorSource::base),
        "worlds": state.settings.worlds_upstream.as_ref().map(MirrorSource::base),
        "events": state.settings.events_upstream.as_ref().map(MirrorSource::base),
    });
    Ok(Json(json!({
        "ok": true,
        "data": {
            "image": concat!("catalyst/", env!("CARGO_PKG_VERSION")),
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "version": env!("CARGO_PKG_VERSION"),
            "mirrors": mirrors,
            "configured_mirrors": configured,
            "catalogue": counts,
        }
    })))
}

/// Whether a successful `method` on the explore path `path` answered 201
/// upstream: the places wkc router turned every PUT and POST into `Created`
/// (gatsby `Route/wkc/routes/utils.js:13-24`), the events `handle` every
/// POST, PUT and PATCH (`Route/handle/utils.js:4-15`). Only upstream's `/api`
/// paths map; the dcl-one-only ones keep their 200.
pub fn answers_created(method: &Method, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    if !path.starts_with("/api/") {
        return false;
    }
    let dcl_one_only = path.starts_with("/api/report/upload/")
        || path.starts_with("/api/pois")
        || path.starts_with("/api/reports")
        || path == "/api/events/moderation";
    if dcl_one_only {
        return false;
    }
    let events_side = path.starts_with("/api/events")
        || path.starts_with("/api/schedules")
        || path.starts_with("/api/profiles")
        || path.starts_with("/api/poster");
    if events_side {
        matches!(*method, Method::POST | Method::PUT | Method::PATCH)
    } else {
        matches!(*method, Method::POST | Method::PUT)
    }
}

/// The router layer behind [`answers_created`]: a handler's 200 becomes the
/// framework's 201 where upstream's did; every other status (4xx, 5xx, 410,
/// the copies gate's 503) stays. Fixtures `places.post_places.ids`,
/// `events.post_search.*`, the writes fixtures' success entries.
pub async fn created_status(request: Request, next: Next) -> Response {
    let created = answers_created(request.method(), request.uri().path());
    let mut response = next.run(request).await;
    if created && response.status() == StatusCode::OK {
        *response.status_mut() = StatusCode::CREATED;
    }
    response
}

/// The `/api` catch-all both services ended their routers with
/// (`places/src/server.ts:84-86`, `events/src/server.ts:70-72`):
/// `{"ok":false,"error":"NotFound","code":"not_found"}`. Fixtures
/// `places.get_unknown_route`, `events.get_unknown_route`. A route, not the
/// router's fallback: this router is merged into one that has its own.
pub async fn api_not_found() -> ApiError {
    ApiError::not_found("NotFound")
}

/// The explore routes with the old services' envelopes, status codes and
/// CORS. Mount beside the content router:
/// `content::build_router(content).merge(explore::router(explore))`.
pub fn router(state: SharedState) -> Router {
    use events::handlers as ev;
    use events::profiles as evp;
    use events::writes as evw;
    use places::admin as pa;
    use places::destinations;
    use places::handlers as pl;
    use places::interactions as pi;
    use places::reports as pr;
    use places::v1 as pv1;
    Router::new()
        .route(
            "/api/places",
            get(pl::get_place_list).post(pl::post_place_list_by_id),
        )
        .route("/api/places/status", post(pl::post_place_status_list_by_id))
        .route("/api/places/{place_id}", get(pl::get_place))
        .route(
            "/api/places/{place_id}/categories",
            get(pl::get_place_categories),
        )
        .route(
            "/api/places/{place_id}/favorites",
            patch(pi::patch_place_favorites),
        )
        .route("/api/places/{place_id}/likes", patch(pi::patch_place_likes))
        .route("/api/places/{place_id}/rating", put(pa::put_place_rating))
        .route("/api/places/{place_id}/ranking", put(pa::put_place_ranking))
        .route(
            "/api/places/{place_id}/ranking-exclusion",
            put(pa::put_place_ranking_exclusion).delete(pa::delete_place_ranking_exclusion),
        )
        .route(
            "/api/places/{place_id}/highlight",
            put(pa::put_place_highlight),
        )
        .route(
            "/api/places/{place_id}/disable",
            put(pa::put_place_disable).patch(pa::patch_place_disable),
        )
        .route(
            "/api/places/{place_id}/featured",
            put(pa::put_place_featured).delete(pa::delete_place_featured),
        )
        .route("/api/categories", get(pl::get_categories))
        .route("/api/map", get(pl::get_map_places))
        .route("/api/map/places", get(pl::get_all_places_list))
        .route(
            "/api/destinations",
            get(destinations::get_destinations_list)
                .post(destinations::post_destinations_list_by_id),
        )
        .route("/api/worlds", get(pl::get_world_list))
        .route("/api/worlds/{world_id}", get(pl::get_world))
        .route(
            "/api/worlds/{world_id}/scenes",
            get(places::world_scenes::get_world_scene_places),
        )
        .route(
            "/api/worlds/{world_id}/favorites",
            patch(pi::patch_world_favorites),
        )
        .route("/api/worlds/{world_id}/likes", patch(pi::patch_world_likes))
        .route("/api/worlds/{world_id}/rating", put(pa::put_world_rating))
        .route("/api/worlds/{world_id}/ranking", put(pa::put_world_ranking))
        .route(
            "/api/worlds/{world_id}/ranking-exclusion",
            put(pa::put_world_ranking_exclusion).delete(pa::delete_world_ranking_exclusion),
        )
        .route(
            "/api/worlds/{world_id}/highlight",
            put(pa::put_world_highlight),
        )
        .route(
            "/api/worlds/{world_id}/featured",
            put(pa::put_world_featured).delete(pa::delete_world_featured),
        )
        .route(
            "/api/destinations/ranking",
            put(pa::put_destinations_ranking),
        )
        .route("/places/place", get(places::social::inject_place_metadata))
        .route("/places/world", get(places::social::inject_world_metadata))
        .route("/api/pois", get(pa::get_pois).post(pa::post_poi))
        .route(
            "/api/pois/{position}",
            patch(pa::patch_poi).delete(pa::delete_poi),
        )
        .route("/api/reports", get(pr::get_reports))
        .route("/api/reports/{id}", patch(pr::patch_report))
        .route(
            "/api/report",
            post(pr::post_report)
                .layer(axum::extract::DefaultBodyLimit::max(pr::REPORT_BODY_LIMIT)),
        )
        .route(
            "/api/report/upload/{filename}",
            put(pr::put_report_upload)
                .layer(axum::extract::DefaultBodyLimit::max(pr::UPLOAD_BODY_LIMIT)),
        )
        .route(
            "/v1/destinations",
            get(pv1::get_v1_destinations_list).post(destinations::post_v1_destinations_list),
        )
        .route("/v1/destinations/{id}", get(pv1::get_v1_destination))
        .route(
            "/v1/destinations/{id}/favorites",
            put(pi::put_v1_destination_favorites).delete(pi::delete_v1_destination_favorites),
        )
        .route(
            "/v1/destinations/{id}/likes",
            put(pi::put_v1_destination_likes).delete(pi::delete_v1_destination_likes),
        )
        .route("/v1/categories", get(pv1::get_v1_categories))
        .route("/api/world_names", get(pl::get_world_names_list))
        .route(
            "/api/events",
            get(ev::get_event_list).post(evw::create_event),
        )
        .route("/api/events/search", post(ev::post_event_search))
        .route("/api/events/attending", get(ev::get_attending_event_list))
        .route("/api/events/moderation", get(ev::get_moderation_list))
        .route("/api/events/categories", get(ev::get_event_category_list))
        .route(
            "/api/events/{event_id}",
            get(ev::get_event)
                .patch(evw::patch_event)
                .delete(evw::delete_event),
        )
        .route(
            "/api/events/{event_id}/attendees",
            get(ev::get_event_attendees)
                .post(evw::create_event_attendee)
                .delete(evw::delete_event_attendee),
        )
        .route(
            "/api/schedules",
            get(ev::get_schedule_list).post(evp::create_schedule),
        )
        .route(
            "/api/schedules/{schedule_id}",
            get(ev::get_schedule_by_id).patch(evp::patch_schedule),
        )
        .route("/api/profiles/settings", get(evp::list_profile_settings))
        .route(
            "/api/profiles/me/settings",
            get(evp::get_my_profile_settings).patch(evp::update_my_profile_settings),
        )
        .route(
            "/api/profiles/{profile_id}/settings",
            get(evp::get_profile_settings).patch(evp::update_profile_settings),
        )
        .route(
            "/api/profiles/subscriptions",
            get(evp::get_profile_subscriptions)
                .post(evp::create_profile_subscription)
                .delete(evp::delete_profile_subscription),
        )
        .route("/api/poster", post(events::posters::upload_poster))
        .route(
            events::posters::DEFAULT_EVENT_IMAGE,
            get(events::posters::default_image),
        )
        .route(
            "/api/poster-vertical",
            post(events::posters::upload_poster_vertical),
        )
        .route("/v1/events", get(ev::get_v1_event_list))
        .route("/v1/events/{event_id}", get(events::v1::get_v1_event))
        .route(
            "/v1/destinations/{id}/events",
            get(events::v1::get_v1_destination_events),
        )
        .route("/events/sitemap.xml", get(events::sitemap::sitemap_index))
        .route(
            "/events/sitemap.static.xml",
            get(events::sitemap::sitemap_static),
        )
        .route(
            "/events/sitemap.events.xml",
            get(events::sitemap::sitemap_events),
        )
        .route(
            "/events/sitemap.schedules.xml",
            get(events::sitemap::sitemap_schedules),
        )
        .route("/api/profile/{user}/face.png", get(evp::avatar_face))
        .route("/api/profile/{user}/body.png", get(evp::avatar_body))
        .route("/federation/places/snapshot", get(fed::places_snapshot))
        .route("/federation/places/changes", get(fed::places_changes))
        .route("/federation/v1/events/feed", get(fed::events_feed))
        .route(
            "/federation/v1/events/{event_id}/attendance",
            get(fed::events_attendance),
        )
        .route("/api/status", get(status))
        .route("/openapi.json", get(openapi_json))
        .route("/api", axum::routing::any(api_not_found))
        .route("/api/{*rest}", axum::routing::any(api_not_found))
        // express fell through to the catch-all on a known path with another
        // method; axum's 405 answers the same JSON 404 (a per-route default).
        .method_not_allowed_fallback(api_not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            copies::gate,
        ))
        .layer(axum::middleware::from_fn(created_status))
        .layer(axum::middleware::from_fn(
            crate::shared::errors::strict_envelope,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            cors::explore_cors,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    #[test]
    fn settings_debug_redacts_the_optional_webhook_and_other_credentials() {
        let mut settings = super::Settings::for_tests();
        settings.events_slack_webhook = Some("https://example.org/secret-webhook".into());
        settings.events_admin_token = Some("secret-admin".into());
        let debug = format!("{settings:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret-webhook"));
        assert!(!debug.contains("secret-admin"));
    }

    #[test]
    fn event_assets_use_the_node_mount_independently_of_the_website() {
        assert_eq!(
            super::Settings::events_asset_base_for("http://localhost:8080/"),
            "http://localhost:8080/events"
        );
        assert_eq!(
            super::Settings::events_asset_base_for("https://node.example.org/prefix/"),
            "https://node.example.org/prefix/events"
        );
        assert_ne!(
            super::Settings::for_tests().events_asset_base_url,
            super::Settings::for_tests().events_base_url
        );
    }

    use super::*;

    #[test]
    fn a_mirror_source_must_be_a_public_upstream_that_is_not_this_node() {
        let ok = MirrorSource::checked(
            "https://places.decentraland.org/",
            &["https://peer.example.org", "http://127.0.0.1:5210"],
        )
        .unwrap();
        assert_eq!(ok.base(), "https://places.decentraland.org");
        for bad in [
            "http://127.0.0.1:5143",
            "http://localhost:5143",
            "http://10.0.0.1/",
            "http://169.254.169.254/",
            "ftp://places.decentraland.org",
            "not a url",
            "",
        ] {
            assert!(
                MirrorSource::checked(bad, &[]).is_err(),
                "{bad:?} must be refused"
            );
        }
        let err = MirrorSource::checked(
            "https://peer.example.org/",
            &["https://peer.example.org:443/content"],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("self-referential"), "{err}");
        assert!(
            MirrorSource::checked(
                "https://peer.example.org:8443/",
                &["https://peer.example.org/"]
            )
            .is_ok()
        );
        assert_eq!(
            MirrorSource::unchecked("http://127.0.0.1:9/").base(),
            "http://127.0.0.1:9"
        );
    }

    #[test]
    fn the_asset_rewrite_domain_is_the_public_host_unless_it_is_decentraland() {
        assert_eq!(
            Settings::asset_rewrite_domain_for(Some("https://interconnected.online/")).as_deref(),
            Some("interconnected.online")
        );
        assert_eq!(
            Settings::asset_rewrite_domain_for(Some("https://events.decentraland.org")),
            None
        );
        assert_eq!(
            Settings::asset_rewrite_domain_for(Some("https://decentraland.org")),
            None
        );
        assert_eq!(Settings::asset_rewrite_domain_for(None), None);
        for local in [
            "http://127.0.0.1:5212",
            "http://localhost:5212/",
            "http://[::1]:5212",
            "http://devbox:5212",
        ] {
            assert_eq!(
                Settings::asset_rewrite_domain_for(Some(local)),
                None,
                "{local}"
            );
        }
        assert_eq!(Settings::asset_rewrite_domain_for(Some("garbage")), None);
    }

    #[test]
    fn created_follows_the_two_upstream_frameworks_on_upstream_paths_only() {
        let (get, post, put, patch, delete) = (
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        );
        // wkc (places): PUT and POST
        for path in [
            "/api/places",
            "/api/places/status",
            "/api/destinations",
            "/api/places/x/rating",
            "/api/places/x/featured",
            "/api/destinations/ranking",
            "/api/report",
        ] {
            assert!(
                answers_created(&post, path) || answers_created(&put, path),
                "{path}"
            );
            assert!(!answers_created(&get, path), "{path}");
            assert!(!answers_created(&delete, path), "{path}");
        }
        assert!(answers_created(&put, "/api/places/x/featured"));
        assert!(
            !answers_created(&patch, "/api/places/x/favorites"),
            "wkc PATCH stays 200"
        );
        assert!(!answers_created(&patch, "/api/places/x/disable"));
        // gatsby handle (events): POST, PUT and PATCH
        for path in [
            "/api/events",
            "/api/events/search",
            "/api/events/x",
            "/api/events/x/attendees",
            "/api/poster",
            "/api/poster-vertical",
            "/api/schedules",
            "/api/schedules/x",
            "/api/profiles/me/settings",
            "/api/profiles/x/settings",
        ] {
            assert!(answers_created(&post, path), "{path}");
            assert!(answers_created(&patch, path), "{path}");
            assert!(!answers_created(&get, path), "{path}");
            assert!(!answers_created(&delete, path), "{path}");
        }
        // dcl-one-only paths keep their 200
        for path in [
            "/api/report/upload/abc.json",
            "/api/pois",
            "/api/pois/1,1",
            "/api/reports",
            "/api/reports/7",
            "/api/events/moderation",
            "/v1/destinations/x/favorites",
            "/federation/places/snapshot",
        ] {
            assert!(!answers_created(&post, path), "{path}");
            assert!(!answers_created(&put, path), "{path}");
            assert!(!answers_created(&patch, path), "{path}");
        }
    }

    /// A pool nothing answers: the routes under test refuse before any
    /// query, and the copies gate serves on a marker read failure.
    fn offline_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
            .unwrap()
    }

    fn offline_state() -> SharedState {
        Arc::new(ExploreState::new(
            offline_pool(),
            Settings::for_tests(),
            None,
        ))
    }

    async fn send(
        app: &Router,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        use axum::body::Body;
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Captures `places.get_unknown_route`, `events.get_unknown_route`: the
    /// JSON 404 under `/api` on every mount, for an unknown method too.
    #[tokio::test]
    async fn unknown_api_routes_answer_the_upstream_json_404() {
        let app = router(offline_state());
        let nested = Router::new()
            .nest("/places", router(offline_state()))
            .nest("/events", router(offline_state()));
        let expected = json!({"ok": false, "error": "NotFound", "code": "not_found"});
        for (app, uri) in [
            (&app, "/api/nope"),
            (&app, "/api/places/x/nope/deeper"),
            (&app, "/api"),
            (&nested, "/places/api/nope"),
            (&nested, "/events/api/nope"),
        ] {
            let (status, headers, body) = send(app, Method::GET, uri, &[]).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(body, expected, "{uri}");
            assert_eq!(headers["content-type"], "application/json", "{uri}");
        }
        let (status, _, body) = send(&app, Method::DELETE, "/api/places", &[]).await;
        assert_eq!((status, body), (StatusCode::NOT_FOUND, expected));
        let (status, _, _) = send(&app, Method::GET, "/nope", &[]).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "outside /api axum's plain 404 stands"
        );
    }

    /// Captures `places.get_worlds.invalid_limit`, `places.get_place.invalid_uuid`,
    /// `events.get_events.unknown_param`, `events.get_event.invalid_uuid`: the
    /// validators answer before any lookup, in upstream's envelope (no
    /// `message`, no `stack`), and the public events reads carry `*`.
    #[tokio::test]
    async fn validation_400s_render_upstream_envelopes_before_any_lookup() {
        let app = router(offline_state());
        let (status, _, body) = send(&app, Method::GET, "/api/worlds?limit=abc", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            "Error validating input:\n- limit must match format \"uint\" {\"format\":\"uint\"}"
        );
        assert_eq!(body["code"], "bad_request");
        assert_eq!(
            body["data"]["messages"][0],
            "limit must match format \"uint\" {\"format\":\"uint\"}"
        );
        assert_eq!(body["data"]["body"]["limit"], "abc");
        assert!(body.get("message").is_none() && body.get("stack").is_none());

        let (status, _, body) = send(&app, Method::GET, "/api/places/not-a-uuid", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["data"]["errors"][0]["instancePath"], "/place_id");
        let (status, _, _) =
            send(&app, Method::GET, "/api/places/not-a-uuid/categories", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, body) = send(&app, Method::GET, "/api/destinations?pointer=abc", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["data"]["errors"][0]["instancePath"], "/pointer/0");
        let (status, _, body) = send(&app, Method::GET, "/api/worlds?owner=bad", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["data"]["errors"][0]["keyword"], "pattern");
        let (status, _, body) = send(
            &app,
            Method::GET,
            "/api/map/places?creator_address=nope",
            &[],
        )
        .await;
        assert_eq!(
            (status, body["data"]["errors"][0]["instancePath"].clone()),
            (StatusCode::BAD_REQUEST, json!("/creator_address"))
        );

        let (status, headers, body) =
            send(&app, Method::GET, "/api/events?foo=bar&limit=1", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Invalid data was sent to the server");
        assert_eq!(
            body["data"],
            json!({
                "errors": [{
                    "instancePath": "",
                    "schemaPath": "#/additionalProperties",
                    "keyword": "additionalProperties",
                    "params": {"additionalProperty": "foo"},
                    "message": "must NOT have additional properties"
                }],
                "messages": ["must NOT have additional properties {\"additionalProperty\":\"foo\"}"],
                "body": {"foo": "bar", "limit": "1", "places_ids": []}
            })
        );
        assert_eq!(headers["access-control-allow-origin"], "*");
        let (status, _, body) = send(&app, Method::GET, "/api/events/not-a-uuid", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["data"]["body"], json!({"event_id": "not-a-uuid"}));
        let (status, _, body) = send(&app, Method::GET, "/api/events?only_attendee=", &[]).await;
        assert_eq!(
            (status, body["data"]["errors"][0]["keyword"].clone()),
            (StatusCode::BAD_REQUEST, json!("enum"))
        );
        let (status, _, body) =
            send(&app, Method::GET, "/api/events?only_attendee=true", &[]).await;
        assert_eq!(
            (status, body["error"].clone()),
            (
                StatusCode::UNAUTHORIZED,
                json!("only_attendee filter requieres autentication")
            )
        );
        let (status, _, body) =
            send(&app, Method::GET, "/api/events?positions=0,0&limit=2", &[]).await;
        assert_eq!(
            (status, body["data"]["messages"][0].clone()),
            (
                StatusCode::BAD_REQUEST,
                json!("positions must be array {\"type\":\"array\"}")
            )
        );
    }

    /// The events `adminBearer` middleware (`adminBearer.ts:18-64`): fixtures
    /// `events.get_events.bearer_wrong`, `.bearer_unconfigured`,
    /// `events.get_event.bearer_non_bearer_scheme_is_anonymous`; the places
    /// bearer routes: `places.put_featured.no_bearer`, `.wrong_bearer`,
    /// `.invalid_uuid_wrong_bearer`, `places.put_destinations_ranking.*`; the
    /// wkc wallet routes: `places.put_rating.no_auth`, `.admin_bearer_refused`.
    #[tokio::test]
    async fn auth_refusals_follow_upstream_statuses_and_messages() {
        let app = router(offline_state());
        let (status, _, body) = send(
            &app,
            Method::GET,
            "/api/events",
            &[("authorization", "Bearer wrong")],
        )
        .await;
        assert_eq!(
            (status, body),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"ok": false, "error": events::handlers::ADMIN_TOKEN_UNCONFIGURED, "code": "service_unavailable"})
            )
        );
        let configured = Arc::new(ExploreState::new(
            offline_pool(),
            Settings {
                events_admin_token: Some("events-admin".into()),
                places_admin_token: Some("places-admin".into()),
                data_team_token: Some("data-team".into()),
                // the write switches answer before the credentials; open them
                operator_writes_enabled: true,
                user_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        ));
        let app = router(configured);
        for uri in [
            "/api/events",
            "/api/events/3ca25728-5b41-48f6-8e24-5cb0e3f2bb5d",
        ] {
            let (status, _, body) =
                send(&app, Method::GET, uri, &[("authorization", "Bearer wrong")]).await;
            assert_eq!(
                (status, body),
                (
                    StatusCode::UNAUTHORIZED,
                    json!({"ok": false, "error": "Unauthorized", "code": "unauthorized"})
                ),
                "{uri}"
            );
        }
        let (status, _, _) = send(
            &app,
            Method::GET,
            "/api/events/not-a-uuid",
            &[("authorization", "Bearer wrong")],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the bearer middleware runs before the params"
        );
        let (status, _, body) = send(
            &app,
            Method::GET,
            "/api/events/not-a-uuid",
            &[("authorization", "Token abc")],
        )
        .await;
        assert_eq!(
            (status, body["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("bad_request")),
            "another scheme is anonymous"
        );

        let place = "/api/places/3ca25728-5b41-48f6-8e24-5cb0e3f2bb5d";
        let world = "/api/worlds/metadynelabs.dcl.eth";
        let (ranking, rating, highlight, disable, entries) = (
            Some(json!({"ranking": 1})),
            Some(json!({"content_rating": "E"})),
            Some(json!({"highlighted": true})),
            Some(json!({"disabled": true})),
            Some(json!({"entries": []})),
        );
        let refusals = [
            (
                format!("{place}/featured"),
                Method::PUT,
                None,
                None,
                401,
                "Missing Authorization",
            ),
            (
                format!("{place}/featured"),
                Method::DELETE,
                None,
                None,
                401,
                "Missing Authorization",
            ),
            (
                format!("{place}/featured"),
                Method::PUT,
                Some("Bearer wrong"),
                None,
                403,
                "Invalid Bearer Token",
            ),
            (
                "/api/places/not-a-uuid/featured".to_string(),
                Method::PUT,
                Some("Bearer wrong"),
                None,
                403,
                "Invalid Bearer Token",
            ),
            (
                format!("{place}/featured"),
                Method::PUT,
                Some("Bearer data-team"),
                None,
                403,
                "Invalid Bearer Token",
            ),
            (
                format!("{place}/featured"),
                Method::PUT,
                Some("Basic abc"),
                None,
                400,
                "Invalid Authorization",
            ),
            (
                format!("{place}/ranking-exclusion"),
                Method::PUT,
                None,
                None,
                401,
                "Missing Authorization",
            ),
            (
                format!("{world}/featured"),
                Method::PUT,
                Some("Bearer wrong"),
                None,
                403,
                "Invalid Bearer Token",
            ),
            (
                format!("{world}/ranking-exclusion"),
                Method::PUT,
                None,
                None,
                401,
                "Missing Authorization",
            ),
            (
                format!("{place}/ranking"),
                Method::PUT,
                None,
                ranking.clone(),
                401,
                "Missing Authorization",
            ),
            (
                format!("{place}/ranking"),
                Method::PUT,
                Some("Bearer wrong"),
                ranking.clone(),
                403,
                "Invalid Bearer Token",
            ),
            (
                format!("{world}/ranking"),
                Method::PUT,
                None,
                ranking.clone(),
                401,
                "Missing Authorization",
            ),
            (
                "/api/destinations/ranking".to_string(),
                Method::PUT,
                None,
                entries.clone(),
                401,
                "Missing Authorization",
            ),
            (
                "/api/destinations/ranking".to_string(),
                Method::PUT,
                Some("Bearer wrong"),
                entries.clone(),
                403,
                "Invalid Bearer Token",
            ),
            (
                format!("{place}/rating"),
                Method::PUT,
                None,
                rating.clone(),
                400,
                "Invalid Auth Chain",
            ),
            (
                format!("{place}/rating"),
                Method::PUT,
                Some("Bearer places-admin"),
                rating.clone(),
                400,
                "Invalid Auth Chain",
            ),
            (
                format!("{place}/highlight"),
                Method::PUT,
                None,
                highlight.clone(),
                400,
                "Invalid Auth Chain",
            ),
            (
                format!("{place}/highlight"),
                Method::PUT,
                Some("Bearer places-admin"),
                highlight.clone(),
                400,
                "Invalid Auth Chain",
            ),
            (
                format!("{place}/disable"),
                Method::PUT,
                None,
                disable.clone(),
                400,
                "Invalid Auth Chain",
            ),
            (
                format!("{world}/rating"),
                Method::PUT,
                None,
                rating.clone(),
                400,
                "Invalid Auth Chain",
            ),
            (
                format!("{world}/highlight"),
                Method::PUT,
                None,
                highlight.clone(),
                400,
                "Invalid Auth Chain",
            ),
            (
                "/api/events/attending".to_string(),
                Method::GET,
                None,
                None,
                401,
                "Unauthorized",
            ),
            (
                "/api/events/attending".to_string(),
                Method::GET,
                Some("Bearer events-admin"),
                None,
                401,
                "Unauthorized",
            ),
        ];
        for (uri, method, auth, body, status, message) in refusals {
            use axum::body::Body;
            use tower::ServiceExt;
            let mut req = axum::http::Request::builder()
                .method(method.clone())
                .uri(&uri);
            if let Some(auth) = auth {
                req = req.header("authorization", auth);
            }
            let req = match &body {
                Some(b) => req
                    .header("content-type", "application/json")
                    .body(Body::from(b.to_string()))
                    .unwrap(),
                None => req.body(Body::empty()).unwrap(),
            };
            let resp = app.clone().oneshot(req).await.unwrap();
            let got = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            assert_eq!(
                (got.as_u16(), body["error"].clone(), body["code"].clone()),
                (
                    status,
                    json!(message),
                    json!(crate::shared::errors::code_for_status(status))
                ),
                "{method} {uri} {auth:?}: {body}"
            );
        }
    }

    /// A batch read without a JSON body (no
    /// content-type, malformed JSON) answers upstream's "not an array" 400
    /// envelope, not the framework's text rejection; an undecodable social
    /// card query answers the fallback card.
    #[tokio::test]
    async fn extractor_rejections_keep_the_upstream_envelope() {
        use axum::body::Body;
        use tower::ServiceExt;
        let app = router(offline_state());
        for uri in ["/api/places", "/api/places/status"] {
            for (content_type, body) in [
                (None, ""),
                (Some("text/plain"), "[\"a\"]"),
                (Some("application/json"), "{not json"),
                (Some("application/json"), "{\"ids\": []}"),
            ] {
                let mut req = axum::http::Request::builder().method(Method::POST).uri(uri);
                if let Some(ct) = content_type {
                    req = req.header("content-type", ct);
                }
                let resp = app
                    .clone()
                    .oneshot(req.body(Body::from(body)).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    resp.status(),
                    StatusCode::BAD_REQUEST,
                    "{uri} {content_type:?} {body}"
                );
                let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
                    .await
                    .unwrap();
                let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    json["error"], "Invalid request body. Expected an array of place IDs.",
                    "{uri} {content_type:?} {body}: {json}"
                );
            }
        }
        let (status, headers, _) =
            send(&app, Method::GET, "/places/place?position=%zz&id=%", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "{headers:?}"
        );
    }

    /// Captures `places.cors.allowed_origin`, `.other_origin`, `.get_other_origin`,
    /// `.social_origin`, `events.cors.*`.
    #[tokio::test]
    async fn cors_follows_the_upstream_allow_lists() {
        let app = router(offline_state());
        let preflight = [
            ("origin", "https://decentraland.org"),
            ("access-control-request-method", "GET"),
            ("access-control-request-headers", "x-identity-auth-chain-0"),
        ];
        for uri in ["/api/places", "/api/events"] {
            let (status, h, _) = send(&app, Method::OPTIONS, uri, &preflight).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{uri}");
            assert_eq!(h["access-control-allow-origin"], "https://decentraland.org");
            assert_eq!(
                h["access-control-allow-methods"],
                "GET,HEAD,PUT,PATCH,POST,DELETE"
            );
            assert_eq!(h["access-control-allow-headers"], "*");
            assert_eq!(h["vary"], "Origin");
            assert!(h.get("access-control-allow-credentials").is_none());
            let (status, h, _) = send(
                &app,
                Method::OPTIONS,
                uri,
                &[
                    ("origin", "https://example.com"),
                    ("access-control-request-method", "GET"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert!(
                h.get("access-control-allow-origin").is_none(),
                "{uri}: {h:?}"
            );
            assert_eq!(
                h["access-control-allow-methods"],
                "GET,HEAD,PUT,PATCH,POST,DELETE"
            );
        }
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/events/categories",
            &[("origin", "https://example.com")],
        )
        .await;
        assert_eq!(h["access-control-allow-origin"], "*");
        assert_eq!(h["vary"], "Origin");
        let (_, h, _) = send(&app, Method::GET, "/api/events/categories", &[]).await;
        assert_eq!(
            h["access-control-allow-origin"], "*",
            "the public reads carry * without an Origin"
        );
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/events?limit=1",
            &[("origin", "https://decentraland.org")],
        )
        .await;
        assert_eq!(h["access-control-allow-origin"], "*");
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/schedules/nope",
            &[("origin", "https://example.com")],
        )
        .await;
        assert!(h.get("access-control-allow-origin").is_none());
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/schedules/nope",
            &[("origin", "https://mvfw.org")],
        )
        .await;
        assert_eq!(h["access-control-allow-origin"], "https://mvfw.org");
        // the places reads echo listed origins only (the lookup fails offline; the headers are set anyway)
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/places/not-a-uuid",
            &[("origin", "https://example.com")],
        )
        .await;
        assert!(h.get("access-control-allow-origin").is_none());
        assert_eq!(h["vary"], "Origin");
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/places/not-a-uuid",
            &[("origin", "https://play.decentraland.zone")],
        )
        .await;
        assert_eq!(
            h["access-control-allow-origin"],
            "https://play.decentraland.zone"
        );
        assert_eq!(
            h["access-control-expose-headers"],
            "ETag,Cache-Control,Content-Language,Content-Type,Expires,Last-Modified,Pragma"
        );
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/places/not-a-uuid",
            &[("origin", "https://foo.pages.dev")],
        )
        .await;
        assert_eq!(
            h["access-control-allow-origin"], "https://foo.pages.dev",
            "the place router's broader list"
        );
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/api/categories",
            &[("origin", "https://foo.pages.dev")],
        )
        .await;
        assert!(
            h.get("access-control-allow-origin").is_none(),
            "categories meet the app list only"
        );
        // the social cards: no CORS; the dcl-one-only paths: the shared reflecting CORS
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/places/place?position=0,0",
            &[("origin", "https://decentraland.org")],
        )
        .await;
        assert!(h.get("access-control-allow-origin").is_none() && h.get("vary").is_none());
        let (_, h, _) = send(
            &app,
            Method::GET,
            "/v1/categories",
            &[("origin", "https://example.com")],
        )
        .await;
        assert_eq!(h["access-control-allow-origin"], "https://example.com");
    }

    /// The explore writes' two switches wait on the restore stamps too: a
    /// stale drill stamp answers the 503 naming the stamp and its age.
    #[tokio::test]
    async fn a_stale_restore_stamp_holds_explore_writes_and_names_it() {
        use crate::shared::writes::restore::{
            Stamp,
            lab::{DAY, StampLab},
            scoped,
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
            .unwrap();
        let state = ExploreState::new(
            pool,
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        );
        let lab = StampLab::fresh();
        scoped(lab.stamps.clone(), async {
            assert!(state.user_writes().is_ok());
            assert!(state.operator_writes().is_ok());
            lab.write_aged(Stamp::Drill, 8 * DAY + DAY / 2);
            let held = |r: Result<(), ApiError>| match r {
                Err(ApiError::WritesDisabled(m)) => m.into_owned(),
                other => panic!("{other:?}"),
            };
            assert_eq!(
                held(state.user_writes()),
                "explore writes are held on this node: the last passing restore drill (drill.json) is 8 d 12 h old, past WRITES_DRILL_MAX_AGE_DAYS=8"
            );
            assert!(
                held(state.operator_writes()).starts_with("operator writes are held on this node: ")
            );
            let res = axum::response::IntoResponse::into_response(state.user_writes().unwrap_err());
            assert_eq!(res.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(res.headers()[axum::http::header::RETRY_AFTER], "3600");
        })
        .await;
    }
}
