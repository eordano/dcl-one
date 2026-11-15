//! Notifications: the signed-fetch reader/marker surface of
//! notifications.decentraland.org (`GET /notifications`, `PUT
//! /notifications/read`, the subscription, email confirmation and opt-out
//! routes), the bearer-gated broadcast, the producers' ingest (`ingest`), the `email` outbox worker
//! (`delivery`), the `friend_first_wear` producer (`first_wear`), the
//! `land_transfer_received` producer over the land_authz index
//! (`land_transfers`) and the community events' rows (`community`), all
//! over the ONE pool and the ONE outbound client.
//! Tables: `social.notifications`, `notification_deliveries`,
//! `subscriptions`, `unconfirmed_emails`, `subscription_opt_outs`,
//! `notification_reader_seen`, `broadcast_notifications`, `broadcast_reads`, `worn_history`, `first_wear_baseline`,
//! `first_wear_cursor`, `land_transfer_cursor`.
//!
//! Mounted by `serve` as `.nest("/notifications", router(state))` (the old
//! gateway stripped that prefix in front of the bundle); a per-vhost proxy
//! forwards `notifications.<domain>/<path>` to `/notifications/<path>` with
//! `x-original-path` so the signature still verifies.

pub mod admin;
pub mod common_email;
pub mod community;
pub mod delivery;
pub mod email;
pub mod errors;
mod event_ended;
pub mod events;
pub mod first_wear;
pub mod handlers;
pub mod ingest;
pub mod land_transfers;
pub(crate) mod moderation;
pub mod seen;
pub mod signing;
pub mod store;
pub mod templates;
pub mod types;
pub mod unsubscribe;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post, put};
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::chain_index::ChainIndex;
use crate::http::Outbound;
use crate::jobs::{Jobs, PeriodicCfg};

pub use email::EmailConfig;
pub use errors::ApiError;
pub use first_wear::FirstWearSettings;
pub use store::{NewNotification, NotificationsStore, insert_notifications};

pub const ADMIN_TOKEN_ENV: &str = "CATALYRST_NOTIFICATIONS_ADMIN_TOKEN";

/// `NOTIFICATIONS_CDN_URL`'s default: upstream's `CDN_URL`.
pub const DEFAULT_CDN_URL: &str = "https://assets-cdn.decentraland.org";

/// `path` under the notification icons' CDN `base` (upstream `${CDN_URL}<path>`).
pub fn cdn_asset(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim().trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// The settings `serve` resolves from `Config`; env names are the old
/// binary's where they map 1:1 (`main.rs` of catalyrst-notifications).
#[derive(Clone, Debug)]
pub struct Settings {
    /// `CATALYRST_NOTIFICATIONS_ADMIN_TOKEN`: bearer of `POST /notifications/broadcast`; unset refuses it.
    pub admin_token: Option<String>,
    /// `SENDGRID_*`, `ACCOUNT_BASE_URL`, `TURNSTILE_SECRET_KEY`, `EMAIL_DOMAIN_BLACKLIST`, the `dapps` flags.
    pub email: EmailConfig,
    /// `SIGNING_KEY`: HMAC key of the e-mail unsubscribe links; unset refuses them.
    pub signing_key: Option<String>,
    /// Public notifications origin the unsubscribe links are signed under.
    pub service_base_url: String,
    /// `NOTIFICATION_SERVICE_TOKEN`: bearer of `POST /notifications/email`; unset refuses it.
    pub notification_service_token: Option<String>,
    /// `NOTIFICATIONS_GOVERNANCE_TOKEN`: bearer of `POST /notifications/notifications` for `governance_*` types; unset disables it.
    pub governance_token: Option<String>,
    /// `NOTIFICATIONS_REWARDS_TOKEN`: the same for `reward_*` types.
    pub rewards_token: Option<String>,
    /// `FIRST_WEAR_ENABLED`: run the friend_first_wear producer (the old
    /// binary ran it when its three extra database DSNs were set).
    pub first_wear_enabled: bool,
    /// `SHOP_ITEM_BASE_URL`, `FIRST_WEAR_IMAGE_BASE`.
    pub first_wear: FirstWearSettings,
    /// `NOTIFICATION_DELIVERY_INTERVAL_SECS` (default 15; floored to 1).
    pub delivery_interval: Duration,
    /// `NOTIFICATIONS_BROADCAST_HORIZON_DAYS`: broadcasts older are hidden and pruned; `None` (0, the default) keeps them.
    pub broadcast_horizon: Option<Duration>,
    /// `NOTIFICATION_DELIVERY_MAX_AGE_SECS` (default 7 days, `None` = never, but
    /// a hold for a missing key still ends): see [`delivery::expire`].
    pub delivery_max_age: Option<Duration>,
    /// Deployment origin used by event notice links (`/whats-on/{id}`).
    pub events_base_url: String,
    /// Backend event asset origin and mount, distinct from notice links.
    pub events_asset_base_url: String,
    /// `CURRENT_VERSION` and the node's `COMMIT_HASH`, for `GET /status`.
    pub current_version: Option<String>,
    pub commit_hash: Option<String>,
    /// Community thumbnail CDN (`communities::Settings::cdn_url`).
    pub community_cdn_url: String,
    /// `JUMP_IN_SITE_URL`: the jump-in site the reminder, start and approval
    /// notices link to (the old events service's `Notifications/index.ts:18`).
    pub jump_in_site_url: String,
    /// `EVENTS_UPCOMING_SUMMARIES_ENABLED` with explore's Slack moderation outbox
    /// active (`OPS_WRITES_ENABLED` and `SLACK_WEBHOOK`): the reminder job then
    /// queues the old `:runner:` upcoming-event notice through it, one per
    /// occurrence; off, it queues none.
    pub event_slack_notices: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            admin_token: None,
            email: EmailConfig {
                account_base_url: "https://account.decentraland.org".to_string(),
                ..Default::default()
            },
            signing_key: None,
            service_base_url: "https://notifications.decentraland.org".into(),
            notification_service_token: None,
            governance_token: None,
            rewards_token: None,
            first_wear_enabled: false,
            first_wear: FirstWearSettings::default(),
            delivery_interval: Duration::from_secs(15),
            broadcast_horizon: None,
            delivery_max_age: Some(delivery::DEFAULT_MAX_AGE),
            events_base_url: "https://decentraland.org".into(),
            events_asset_base_url: "http://localhost/events".into(),
            current_version: None,
            commit_hash: None,
            community_cdn_url: "https://decentraland.org".into(),
            jump_in_site_url: "https://decentraland.org/jump".into(),
            event_slack_notices: false,
        }
    }
}

pub struct State {
    pub store: NotificationsStore,
    pub email: email::EmailSender,
    pub settings: Settings,
    pub http: Outbound,
    /// Contract-wallet (EIP-1654) signature check for signed-fetch chains;
    /// `None` refuses such chains (the old binary had no validator either).
    pub eip1654: Option<Arc<dyn Eip1654Validator>>,
    /// The collections subgraph reader the first-wear producer resolves
    /// items through; `None` means no first wear is ever notifiable.
    pub market: Option<Arc<ChainIndex>>,
    /// The data-warehouse events client (`context: notification_server`).
    analytics: OnceLock<crate::analytics::Analytics>,
}

pub type SharedState = Arc<State>;

impl State {
    pub fn new(
        pool: PgPool,
        settings: Settings,
        http: Outbound,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
        market: Option<Arc<ChainIndex>>,
    ) -> Self {
        Self {
            store: NotificationsStore::new(pool).with_broadcast_horizon(settings.broadcast_horizon),
            email: email::EmailSender::new(settings.email.clone(), http.clone())
                .with_unsubscribe(settings.signing_key.as_deref(), &settings.service_base_url),
            settings,
            http,
            eip1654,
            market,
            analytics: OnceLock::new(),
        }
    }

    /// Emits the old inbox's data-warehouse events through `analytics`.
    pub fn set_analytics(&self, analytics: crate::analytics::Analytics) {
        let _ = self
            .analytics
            .set(analytics.with_context("notification_server"));
    }

    /// Fire-and-forget; a node without analytics drops the event.
    pub fn track(&self, event: &str, body: serde_json::Value) {
        if let Some(analytics) = self.analytics.get() {
            analytics.fire_event(event, body);
        }
    }

    /// The outbox worker and broadcast retention always; the first-wear producer when enabled.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs) {
        let state = self.clone();
        let max_age = self.settings.delivery_max_age;
        let configured = self.email.notifications_configured();
        let can_sign = self.email.has_signing_key();
        if self.email.turnstile_secret_without_flag() {
            tracing::warn!(
                "TURNSTILE_SECRET_KEY is set but the dapps turnstile-verification flag is off or not loaded: confirm-email runs no challenge"
            );
        }
        let last_held = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if !configured {
            tracing::warn!(
                "SENDGRID_API_KEY, SENDGRID_FROM_EMAIL or SENDGRID_NOTIFICATION_TEMPLATE_ID unset, or SENDGRID_API_URL empty: notification emails stay pending until they are set"
            );
        } else if !self.email.has_signing_key() {
            tracing::error!(
                "SIGNING_KEY unset: notification emails stay pending (no unsubscribe link can be signed) until it is set"
            );
        }
        jobs.spawn_periodic_leased(
            "notification-deliveries",
            PeriodicCfg::every(self.settings.delivery_interval.max(Duration::from_secs(1)))
                .with_jitter(Duration::from_secs(5))
                .with_error_backoff(Duration::from_secs(300)),
            move |ctx| {
                let (state, last_held) = (state.clone(), last_held.clone());
                async move {
                    let pool = state.store.pool();
                    let expired = delivery::expire(
                        pool,
                        crate::shared::now_utc(),
                        max_age,
                        configured && can_sign,
                    )
                    .await?;
                    if expired > 0 {
                        tracing::warn!(expired, "stale notification deliveries expired unsent");
                    }
                    let summary =
                        delivery::run_pass(pool, &state.email, configured, max_age).await?;
                    let previous =
                        last_held.swap(summary.held, std::sync::atomic::Ordering::Relaxed);
                    if summary.held != previous && summary.held > 0 {
                        tracing::warn!(
                            held = summary.held,
                            sendgrid_configured = configured,
                            signing_key = can_sign,
                            "notification emails stay pending until email delivery is configured"
                        );
                    }
                    if summary.claimed > 0 {
                        tracing::info!(?summary, "notification deliveries pass");
                        ctx.progress(format!("{summary:?}"));
                    }
                    let pruned = delivery::prune(pool, crate::shared::now_utc()).await?;
                    if pruned > 0 {
                        tracing::info!(pruned, "notification deliveries pruned");
                    }
                    Ok(())
                }
            },
        );
        if !self.email.spawn_disposable_refresh(jobs) {
            tracing::info!(
                "DISPOSABLE_EMAIL_DOMAINS_URL empty: no disposable-domain list is fetched"
            );
        }
        let store = self.store.clone();
        if self.settings.broadcast_horizon.is_some() {
            jobs.spawn_periodic_leased(
                "notification-broadcast-retention",
                PeriodicCfg::every(Duration::from_secs(3600)).with_jitter(Duration::from_secs(60)),
                move |_ctx| {
                    let store = store.clone();
                    async move {
                        let pruned = store
                            .prune_broadcasts(crate::shared::now_utc(), 100)
                            .await?;
                        if pruned > 0 {
                            tracing::info!(pruned, "expired broadcasts pruned");
                        }
                        Ok(())
                    }
                },
            );
        }
        land_transfers::spawn(self.store.pool().clone(), jobs);
        events::spawn(
            self.store.pool().clone(),
            events::Links {
                base: self.settings.events_base_url.clone(),
                asset_base: self.settings.events_asset_base_url.clone(),
                jump_in_site_url: self.settings.jump_in_site_url.clone(),
                community_cdn: self.settings.community_cdn_url.clone(),
                slack_notices: self.settings.event_slack_notices,
            },
            jobs,
        );
        if self.settings.first_wear_enabled {
            first_wear::spawn(self, jobs);
            tracing::info!(
                market = self.market.is_some(),
                "friend_first_wear producer up"
            );
        } else {
            tracing::info!("FIRST_WEAR_ENABLED=false: no friend_first_wear producer");
        }
    }
}

impl admin::AdminSecretSource for SharedState {
    const ENVIRONMENT_VARIABLE: &'static str = ADMIN_TOKEN_ENV;
    const UNSET_MESSAGE: &'static str =
        "admin broadcast disabled: CATALYRST_NOTIFICATIONS_ADMIN_TOKEN is not set";
    const MISMATCH_MESSAGE: &'static str = "You are not authorized to access this resource";
    fn admin_token(&self) -> Option<&str> {
        self.settings.admin_token.as_deref()
    }
}

/// The routes, unprefixed: nest under `/notifications`. Bodies are capped
/// at 256 KiB as before (the producer ingest reads up to
/// [`ingest::MAX_BODY_BYTES`] itself, after its bearer); CORS is permissive as the old bundle's was; bodies
/// and refusals keep upstream's JSON wire ([`errors::json_wire`]).
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/status", get(handlers::get_status))
        .route(
            "/notifications",
            get(handlers::get_notifications).post(ingest::post_notifications),
        )
        .route("/notifications/read", put(handlers::put_read))
        .route(
            "/subscription",
            get(handlers::get_subscription).put(handlers::put_subscription),
        )
        .route("/set-email", put(handlers::put_set_email))
        .route("/confirm-email", put(handlers::put_confirm_email))
        .route("/subscription/opt-outs", post(handlers::post_opt_out))
        .route(
            "/subscription/opt-outs/{scope}/{scopeId}",
            get(handlers::get_opt_out).delete(handlers::delete_opt_out),
        )
        .route("/notifications/broadcast", post(handlers::post_broadcast))
        .route("/notifications/email", post(common_email::post_email))
        .route("/unsubscribe/{address}", get(unsubscribe::unsubscribe_all))
        .route(
            "/unsubscribe/{address}/{notificationType}",
            get(unsubscribe::unsubscribe_one),
        )
        .fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "Not found") })
        .layer(axum::middleware::from_fn(errors::json_wire))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

#[cfg(test)]
mod tests;
