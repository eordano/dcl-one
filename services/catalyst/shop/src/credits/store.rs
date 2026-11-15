//! The credits component: the ONE pool, the active-packs memo and the
//! fulfilment kick, plus the enrolment/progress reads over `credits.wallets`
//! (the old `user_program` + `user_credits` pair, one row now) and the ONE
//! audit writer every operator mutation of this module records through
//! (`ops.admin_audit`, domain `credits`).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as JsonValue;
use sqlx::Row;
use sqlx::postgres::PgPool;
use tokio::sync::Notify;

use super::errors::ApiError;
use super::packs::PackRow;
use crate::shared::cache::TtlMap;

/// Active packs change through the admin API only; public reads share one row set for this long.
pub const PACKS_CACHE_TTL: Duration = Duration::from_secs(60);

/// The request clock every statement binds (UTC, `timestamp without time zone`).
pub fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

#[derive(Clone)]
pub struct Credits {
    pub pool: PgPool,
    pub(crate) packs: Arc<TtlMap<(), Vec<PackRow>>>,
    pub(crate) kick: Arc<Notify>,
    /// The chain of a configured CreditUsed index (`usage.rs`): the expiry
    /// sweep then waits for the index to read past a pledge's expiry.
    pub(crate) usage_chain: Option<i64>,
    /// The grant notification's link (`SHOP_BASE_URL`).
    pub(crate) shop_base_url: Arc<str>,
    /// The grant notification's icon base (`NOTIFICATIONS_CDN_URL`).
    pub(crate) notifications_cdn_url: Arc<str>,
}

/// A wallet's balances in integer US cents at the peg (`floor(credits * 10)`;
/// the numeric text is the ledger's, never an `f64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletRow {
    pub available_cents: i64,
    pub earned_cents: i64,
    /// The operator's block (`credits.wallets.is_blocked_for_claiming`) or a
    /// multi-accounting flag (`credits.wallet_flags`, notified on its channel
    /// or posted through `POST /admin/flags`). The flag blocks claiming only:
    /// this row and the captcha claim fold it in, the spend and the
    /// reservation read the operator's block alone, so paid credits are
    /// never held by a flag. The admin's unflag and the operator's unblock
    /// delete the wallet's flag.
    pub is_blocked_for_claiming: bool,
}

/// One `ops.admin_audit` row: `action` is the verb under the `credits`
/// domain (`grant`, `revoke`, `refund`, `user.block`, `pack.create`, ...),
/// `actor` the authenticated identity (`token:CATALYRST_CREDITS_ADMIN_TOKEN`,
/// `stripe`, `system`), `target` the wallet or the row acted on, `detail` the
/// request as received (never a secret).
#[derive(Debug, Clone, Copy)]
pub(crate) struct AuditEntry<'a> {
    pub action: &'a str,
    pub actor: &'a str,
    pub target: Option<&'a str>,
    pub detail: &'a JsonValue,
}

pub const AUDIT_DOMAIN: &str = "credits";

pub const SYSTEM_ACTOR: &str = "system";

impl Credits {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            packs: Arc::new(TtlMap::new(PACKS_CACHE_TTL)),
            kick: Arc::new(Notify::new()),
            usage_chain: None,
            shop_base_url: crate::subscriptions::DEFAULT_SHOP_BASE_URL.into(),
            notifications_cdn_url: catalyst_social::notifications::DEFAULT_CDN_URL.into(),
        }
    }

    pub fn with_shop_base_url(mut self, url: &str) -> Self {
        self.shop_base_url = url.trim_end_matches('/').into();
        self
    }

    pub fn with_notifications_cdn_url(mut self, url: &str) -> Self {
        self.notifications_cdn_url = url.into();
        self
    }

    /// Bounds the authorization expiry sweep by the CreditUsed index of `chain_id`.
    pub fn with_usage_index(mut self, chain_id: i64) -> Self {
        self.usage_chain = Some(chain_id);
        self
    }

    /// Wake the fulfilment worker ahead of its next tick.
    pub(crate) fn kick_fulfillment(&self) {
        self.kick.notify_one();
    }

    /// The audit row of an operator or system mutation, in the caller's
    /// transaction, through the ONE writer (`shared::audit::record`);
    /// returns its id for the ledger's `admin_audit_id`. The money paths
    /// that must link the ledger row to the audit row in one statement
    /// (`wallet.rs`, `admin_store.rs`) inline the same INSERT as a CTE.
    pub(crate) async fn audit<'e, E>(executor: E, entry: AuditEntry<'_>) -> Result<i64, ApiError>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let row = crate::shared::audit::Entry::new(entry.actor, AUDIT_DOMAIN, entry.action)
            .target(entry.target)
            .detail(entry.detail.clone());
        Ok(crate::shared::audit::record(executor, &row).await?)
    }

    pub async fn mark_started(&self, address: &str) -> Result<(), ApiError> {
        sqlx::query(
            "INSERT INTO credits.wallets (address, has_started_program) \
             VALUES ($1, TRUE) \
             ON CONFLICT (address) DO UPDATE SET has_started_program = TRUE, updated_at = timezone('UTC', now())",
        )
        .bind(address)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn has_started(&self, address: &str) -> Result<bool, ApiError> {
        let row = sqlx::query("SELECT has_started_program FROM credits.wallets WHERE address = $1")
            .bind(address)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|r| r.get::<bool, _>("has_started_program"))
            .unwrap_or(false))
    }

    pub async fn user_credits(&self, address: &str) -> Result<Option<WalletRow>, ApiError> {
        let row = sqlx::query(
            "SELECT floor(available * $2::numeric)::bigint AS available_cents, \
                    floor(earned_available * $2::numeric)::bigint AS earned_cents, \
                    (is_blocked_for_claiming OR EXISTS (SELECT 1 FROM credits.wallet_flags f WHERE f.address = $1)) AS is_blocked_for_claiming \
             FROM credits.wallets WHERE address = $1",
        )
        .bind(address)
        .bind(crate::types::UsdCents::CENTS_PER_CREDIT)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| WalletRow {
            available_cents: r.get("available_cents"),
            earned_cents: r.get("earned_cents"),
            is_blocked_for_claiming: r.get("is_blocked_for_claiming"),
        }))
    }

    /// Enrolment flag and balances in one statement; a wallet with no row
    /// reads as not started with no credits, unless a flag holds it: then
    /// zero credits, blocked, so the progress page agrees with the claim.
    pub async fn user_progress(
        &self,
        address: &str,
    ) -> Result<(bool, Option<WalletRow>), ApiError> {
        let row = sqlx::query(
            "SELECT COALESCE(w.has_started_program, false) AS started, \
                    w.address IS NOT NULL AS has_credits, \
                    COALESCE(floor(w.available * $2::numeric), 0)::bigint AS available_cents, \
                    COALESCE(floor(w.earned_available * $2::numeric), 0)::bigint AS earned_cents, \
                    (COALESCE(w.is_blocked_for_claiming, false) \
                     OR EXISTS (SELECT 1 FROM credits.wallet_flags f WHERE f.address = $1)) \
                        AS is_blocked_for_claiming \
             FROM (VALUES ($1::text)) AS requested(address) \
             LEFT JOIN credits.wallets w USING (address)",
        )
        .bind(address)
        .bind(crate::types::UsdCents::CENTS_PER_CREDIT)
        .fetch_one(&self.pool)
        .await?;
        let blocked: bool = row.get("is_blocked_for_claiming");
        let credits = (row.get::<bool, _>("has_credits") || blocked).then(|| WalletRow {
            available_cents: row.get("available_cents"),
            earned_cents: row.get("earned_cents"),
            is_blocked_for_claiming: blocked,
        });
        Ok((row.get("started"), credits))
    }
}
