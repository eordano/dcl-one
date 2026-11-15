//! Price: `GET /api/v3/simple/price` in api.coingecko.com's shape (the old
//! `catalyrst-price`, the "price-feed" the credits checkout and the
//! marketplace UI read) over the MANA spot archive in the ONE database
//! (`economy.price_snapshots`, `economy.price_overrides`,
//! `economy.price_override_audit`: `schemas/31_economy.sql`, the economy
//! schema because the broker is their neighbour and the relay reads the
//! same spot).
//!
//! Where each route reads (the stage-5 data-plane rule: every route names
//! its source, and a route whose source is unset answers the unavailable
//! envelope, never an empty document presented as the truth):
//! - `GET /api/v3/simple/price`: **mirror**. The `mana-price-poll` job
//!   ([`poller`], `PRICE_POLL_ENABLED`, every `PRICE_POLL_INTERVAL_SECS` =
//!   300 s) appends one CoinGecko `/simple/price` row per pass through the
//!   ONE client (`COINGECKO_URL` is checked at startup to be a public https
//!   endpoint that is not this node), or, when CoinGecko fails, one row from
//!   the first `PRICE_FALLBACK_SOURCES` exchange that answers (same checks);
//!   the route serves the newest row of any source (from memory once the
//!   poller published one, the row otherwise), overlaid by `economy.price_overrides` for the requested
//!   `(token_id, vs_currency)` pairs. Freshness: one row per 300 s, and a
//!   budget (`store::SNAPSHOT_MAX_AGE`, 30 min = six default polls): a
//!   newer row is served with an `Age` header (its age on this node) and the
//!   wire's `last_updated_at` (the provider's quote time) on request; past
//!   the budget the route answers [`unavailable`] naming the age, so a dead
//!   poller never degrades into a weeks-old quote under the 30 s cache
//!   header, and the credit-priced catalog feeds (`marketplace::items::
//!   mana_usd_rate`) apply the same budget. A request for MANA with no
//!   snapshot on this node and no override answers [`unavailable`] (`503`,
//!   `source: mana-price-poll`, `configure: PRICE_POLL_ENABLED`) where the
//!   old service answered `{"decentraland": {}}`.
//! - `GET /admin/api/price/overrides`, `PUT`/`DELETE
//!   /admin/api/price/overrides/{token}/{vs}`: local; every one takes the
//!   console's bearer `PRICE_ADMIN_TOKEN` (`_FILE`; unset fails closed with
//!   403), the listing included, since it carries each override's operator
//!   identity and note and the old service was never reachable from the
//!   network (127.0.0.1:5156, nginx proxied only the price route); every
//!   set/clear appends its audit row in the same statement; the writes sit
//!   behind `DATA_WRITES_ENABLED` (`marketplace::DataWrites`).
//!
//! Not carried: `PRICE_PG_COMPONENT_PSQL_CONNECTION_STRING` (the ONE pool;
//! the `mana_price` database is not read or migrated), the per-service
//! `/health` (the content server owns it at the root), the `daily_stats`
//! table (no reader and no writer in the old crate), the two audit indexes
//! no query used. `CATALYRST_PRICE_ADMIN_TOKEN` is `PRICE_ADMIN_TOKEN`.
//!
//! Mount: `.merge(price::router(state))` at the root (`/api/v3/simple/price`
//! and `/admin/api/price/*` collide with nothing) and `.nest("/price",
//! price::router(state))` for a per-vhost proxy.

// The crates this one builds on, under their module names (AGENTS.md).
#[cfg(test)]
pub(crate) use catalyst_db as db;
pub(crate) use catalyst_jobs as jobs;
pub(crate) use catalyst_shared as shared;
pub(crate) use dcl_auth as auth;
pub(crate) use dcl_http as http;

pub mod handlers;
pub mod poller;
pub mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use serde_json::json;
use sqlx::PgPool;

use crate::http::Outbound;
use crate::jobs::{Jobs, PeriodicCfg};
use crate::shared::writes::DataWrites;

pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 300;
pub const JOB_NAME: &str = "mana-price-poll";
/// MANA's CoinGecko coin id: what the poller asks for and the price route
/// answers under.
pub const MANA_ID: &str = "decentraland";

/// The raw values `main.rs` reads from `Config` (the old env names).
#[derive(Clone)]
pub struct RawSettings {
    /// `PRICE_POLL_ENABLED` (false): run the CoinGecko poller in this process.
    pub poll_enabled: bool,
    /// `COINGECKO_URL` (empty): the API base, no trailing slash; the poller needs one.
    pub coingecko_url: String,
    /// `PRICE_POLL_INTERVAL_SECS` (300).
    pub poll_interval_secs: u64,
    /// `PRICE_FALLBACK_SOURCES` (empty; the mirror posture names the three):
    /// `exchange=base` entries a pass takes the spot from, in order, when
    /// CoinGecko fails; empty = none.
    pub fallback_sources: String,
    /// `PRICE_ADMIN_TOKEN` (or `_FILE`): the override console's bearer; unset refuses it.
    pub admin_token: Option<String>,
    /// `DATA_WRITES_ENABLED`.
    pub writes_enabled: bool,
}

#[derive(Clone)]
pub struct Settings {
    pub poll_enabled: bool,
    /// Checked to be a public https endpoint that is not this node when the poller is on.
    pub coingecko_url: String,
    pub poll_interval: Duration,
    /// Each checked like `coingecko_url` when the poller is on.
    pub fallbacks: Vec<(poller::Exchange, String)>,
    pub admin_token: Option<String>,
    pub writes_enabled: bool,
}

/// No derived `Debug` on either: `admin_token` is the console bearer; both
/// print whether one is set.
impl std::fmt::Debug for RawSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawSettings")
            .field("poll_enabled", &self.poll_enabled)
            .field("coingecko_url", &self.coingecko_url)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("fallback_sources", &self.fallback_sources)
            .field("admin_token", &self.admin_token.is_some())
            .field("writes_enabled", &self.writes_enabled)
            .finish()
    }
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("poll_enabled", &self.poll_enabled)
            .field("coingecko_url", &self.coingecko_url)
            .field("poll_interval", &self.poll_interval)
            .field("fallbacks", &self.fallbacks)
            .field("admin_token", &self.admin_token.is_some())
            .field("writes_enabled", &self.writes_enabled)
            .finish()
    }
}

impl Settings {
    /// `self_hosts`: this node's own hosts (`content::own_hosts`), which the
    /// poller may never source from.
    pub fn resolve(raw: RawSettings, self_hosts: &[String]) -> anyhow::Result<Self> {
        let coingecko_url = raw.coingecko_url.trim().trim_end_matches('/').to_string();
        let poll_interval = Duration::from_secs(raw.poll_interval_secs.max(1));
        let fallbacks = raw
            .fallback_sources
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|entry| {
                let Some((name, base)) = entry.split_once('=') else {
                    anyhow::bail!(
                        "PRICE_FALLBACK_SOURCES: {entry:?} names no API base (exchange=https://...)"
                    );
                };
                let (name, base) = (name.trim(), base.trim().trim_end_matches('/'));
                let exchange = poller::Exchange::parse(name).with_context(|| {
                    format!(
                        "PRICE_FALLBACK_SOURCES: unknown exchange {name:?} (known: coinbase, binance, kraken)"
                    )
                })?;
                if base.is_empty() {
                    anyhow::bail!("PRICE_FALLBACK_SOURCES: {name} names an empty API base");
                }
                Ok((exchange, base.to_string()))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if raw.poll_enabled {
            if coingecko_url.is_empty() {
                anyhow::bail!(
                    "PRICE_POLL_ENABLED=true with no COINGECKO_URL: name the API base the poller reads"
                );
            }
            for (exchange, base) in &fallbacks {
                crate::shared::upstream::check_upstream(
                    &format!("PRICE_FALLBACK_SOURCES {}", exchange.name()),
                    base,
                    self_hosts,
                )?;
            }
            crate::shared::upstream::check_upstream("COINGECKO_URL", &coingecko_url, self_hosts)?;
            if !coingecko_url.starts_with("https://") {
                anyhow::bail!(
                    "COINGECKO_URL must be https://: the MANA spot decides checkout amounts"
                );
            }
            if poll_interval * 2 > store::SNAPSHOT_MAX_AGE {
                anyhow::bail!(
                    "PRICE_POLL_INTERVAL_SECS={} is more than half the {} s snapshot freshness budget: the price route and the credit-priced feeds would answer the unavailable envelope between two polls",
                    poll_interval.as_secs(),
                    store::SNAPSHOT_MAX_AGE.as_secs()
                );
            }
        }
        Ok(Self {
            poll_enabled: raw.poll_enabled,
            coingecko_url,
            poll_interval,
            fallbacks,
            admin_token: raw
                .admin_token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            writes_enabled: raw.writes_enabled,
        })
    }

    pub fn for_tests() -> Self {
        Self {
            poll_enabled: false,
            coingecko_url: "https://coingecko.example.org/api/v3".to_string(),
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            fallbacks: Vec::new(),
            admin_token: None,
            writes_enabled: false,
        }
    }
}

pub struct State {
    pub settings: Settings,
    pub prices: store::Prices,
    pub overrides: store::Overrides,
    pub http: Outbound,
}

pub type SharedState = Arc<State>;

impl State {
    pub fn new(pool: PgPool, http: Outbound, settings: Settings) -> Self {
        Self {
            prices: store::Prices::new(pool.clone(), settings.poll_enabled),
            overrides: store::Overrides::new(pool),
            http,
            settings,
        }
    }

    /// Attaches `mana-price-poll` when `PRICE_POLL_ENABLED`: one periodic job
    /// with a 15 s first-fire jitter and a 1 h error cap. Off, the route
    /// serves whatever row an external ingester keeps fresh, and logs it.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs) {
        if !self.settings.poll_enabled {
            tracing::warn!(
                "PRICE_POLL_ENABLED=false: not polling CoinGecko; /api/v3/simple/price serves the last \
                 economy.price_snapshots row (the unavailable envelope while there is none) and goes stale \
                 unless an external ingester writes the table"
            );
            return;
        }
        let state = self.clone();
        jobs.spawn_periodic_leased(
            JOB_NAME,
            PeriodicCfg::every(self.settings.poll_interval)
                .with_jitter(Duration::from_secs(15))
                .with_error_backoff(Duration::from_secs(3600)),
            move |_ctx| {
                let state = state.clone();
                async move {
                    let sources = poller::Sources {
                        coingecko: state.settings.coingecko_url.clone(),
                        fallbacks: state.settings.fallbacks.clone(),
                    };
                    poller::run_pass(&state.http, &sources, &state.prices).await
                }
            },
        );
    }
}

/// The data-plane unavailable envelope for this route's source (the
/// `marketplace::unavailable` shape; the source is this module's job, which
/// `marketplace::Source` does not name): `503 {"error", "message", "source":
/// "mana-price-poll", "configure": "PRICE_POLL_ENABLED"}` with `Retry-After`.
pub fn unavailable(message: &str) -> Response {
    let body = json!({
        "error": "Service Unavailable",
        "message": message,
        "source": JOB_NAME,
        "configure": "PRICE_POLL_ENABLED",
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::RETRY_AFTER, "60"),
        ],
        body.to_string(),
    )
        .into_response()
}

/// The price and override routes, wrapped in `DataWrites::guard` with the
/// state's `DATA_WRITES_ENABLED` (the override console's `PUT`/`DELETE`).
pub fn router(state: SharedState) -> Router {
    let writes_enabled = state.settings.writes_enabled;
    let router = Router::new()
        .route("/api/v3/simple/price", get(handlers::simple_price))
        .route("/admin/api/price/overrides", get(handlers::list_overrides))
        .route(
            "/admin/api/price/overrides/{token}/{vs}",
            put(handlers::set_override).delete(handlers::clear_override),
        )
        .layer(axum::middleware::from_fn(
            crate::shared::cors::cors_middleware,
        ))
        .with_state(state);
    DataWrites::guard(router, writes_enabled)
}
