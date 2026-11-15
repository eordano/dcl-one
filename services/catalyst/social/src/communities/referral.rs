//! Referral progress (upstream social-service-ea `/v1/referral-progress`).
//! The invited user is always the signed-fetch signer: POST records the
//! attribution (first referrer wins), PATCH marks the signer as signed up,
//! GET reports the signer's own stats as a referrer. Login events grant
//! progress after the configured distinct UTC login days. `POST
//! /v1/referral-email` stores the contact email of a referrer at the IRL-swag
//! tier, checked as upstream's `setReferralEmail` checks it.

use crate::http::Outbound;
use axum::Json;
use axum::extract::{ConnectInfo, Extension, OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;

use crate::Address;

use super::http::CommError;
pub use super::referral_slack::Settings as SlackSettings;
use super::{SharedState, signed_path};

/// These referral handlers catch unexpected dependency errors themselves and
/// return `{message}`. Keep this conversion local rather than changing other
/// community routes' generic error envelope.
fn database_error(error: sqlx::Error) -> CommError {
    if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("22P02") {
        return CommError::bad_request(super::http::INVALID_IDENTIFIER);
    }
    tracing::warn!(%error, "referral database operation failed");
    CommError::MessageOnly(
        StatusCode::INTERNAL_SERVER_ERROR,
        super::http::database_message(&error),
    )
}

// Native ingress exposes contention as retryable service unavailability.
fn login_database_error(error: sqlx::Error) -> CommError {
    if matches!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55P03" | "57014")
    ) {
        return CommError::MessageOnly(
            StatusCode::SERVICE_UNAVAILABLE,
            "Login progress is temporarily busy".into(),
        );
    }
    database_error(error)
}

const LOGIN_MAX_AGE_MS: i64 = 86_400_000;
// Login timestamps may run up to five minutes ahead.
const LOGIN_FUTURE_SKEW_MS: i64 = 300_000;

/// node-pg sends a JavaScript numeric parameter as Number.toString(). In
/// particular fractional MAX values reach bigint parsing and are refused; they
/// must not silently round, and Infinity must not become an unbounded quota.
fn source_number_text(number: f64) -> String {
    if number == 0.0 {
        return "0".into();
    }
    if number.is_infinite() {
        return if number.is_sign_negative() {
            "-Infinity"
        } else {
            "Infinity"
        }
        .into();
    }
    if number.abs() >= 1e21 || number.abs() < 1e-6 {
        let text = format!("{number:e}");
        let (mantissa, exponent) = text.split_once('e').expect("scientific number");
        let exponent: i32 = exponent.parse().expect("float exponent");
        format!("{mantissa}e{exponent:+}")
    } else {
        number.to_string()
    }
}

/// Source referral settings; external operations use the process outbound client.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Only direct socket peers in these networks may supply X-Real-IP.
    /// Such proxies must overwrite that header from their validated client IP.
    pub trusted_proxy_cidrs: Vec<ipnet::IpNet>,
    pub max_ip_matches: f64,
    pub min_login_days: f64,
    pub suspicious_window_ms: f64,
    pub slack: SlackSettings,
    pub profile_url: String,
    /// `NOTIFICATIONS_CDN_URL`: base of the referral notice icon.
    pub cdn_url: String,
    pub reward_server_url: Option<String>,
    pub reward_keys: std::collections::HashMap<i64, String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            trusted_proxy_cidrs: Vec::new(),
            max_ip_matches: 2.0,
            min_login_days: 3.0,
            suspicious_window_ms: 300_000.0,
            slack: Default::default(),
            profile_url: "https://decentraland.org".into(),
            cdn_url: crate::notifications::DEFAULT_CDN_URL.into(),
            reward_server_url: None,
            reward_keys: Default::default(),
        }
    }
}
impl Settings {
    /// Every reward tier has a fixture campaign key, as the source requires.
    pub fn for_tests() -> Self {
        Self {
            reward_keys: REWARD_TIERS
                .iter()
                .map(|tier| (*tier, format!("fixture-campaign-{tier}")))
                .collect(),
            ..Self::default()
        }
    }
}

pub struct Referral {
    pool: sqlx::PgPool,
    http: Outbound,
    settings: Settings,
    slack: std::sync::Arc<super::referral_slack::Adapter>,
    mailer: OnceLock<crate::notifications::email::EmailSender>,
    /// The node's explorer denylist (mirror plus local overlay), held in memory
    /// by `explore`. Unset, no wallet is denied.
    denylist: OnceLock<std::sync::Arc<crate::explore::api::denylist::Snapshot>>,
}
impl Referral {
    pub fn new(pool: sqlx::PgPool, http: Outbound, settings: Settings) -> Self {
        let slack = super::referral_slack::Adapter::new(http.clone(), settings.slack.clone());
        Self {
            pool,
            http,
            settings,
            slack: std::sync::Arc::new(slack),
            mailer: OnceLock::new(),
            denylist: OnceLock::new(),
        }
    }
    #[cfg(test)]
    pub(crate) fn set_test_slack_endpoint(&mut self, endpoint: String) {
        std::sync::Arc::get_mut(&mut self.slack)
            .unwrap()
            .set_test_endpoint(endpoint);
    }
    pub fn set_mailer(&self, mailer: crate::notifications::email::EmailSender) {
        let _ = self.mailer.set(mailer);
    }

    pub fn set_denylist(&self, denylist: std::sync::Arc<crate::explore::api::denylist::Snapshot>) {
        let _ = self.denylist.set(denylist);
    }

    pub(crate) fn denied(&self, wallet: &str) -> bool {
        self.denylist
            .get()
            .is_some_and(|list| list.contains(wallet))
    }
    async fn assert_not_banned(&self, referrer: &str, ip: Option<&str>) -> Result<(), CommError> {
        let context = match ip {
            Some(ip) => format!("{referrer}, {ip}"),
            None => referrer.into(),
        };
        if self.denied(referrer) {
            return Err(CommError::bad_request(format!(
                "Referrer is on the deny list {context}"
            )));
        }
        let original: Option<String> = sqlx::query_scalar("SELECT referrer FROM social.referral_progress WHERE invited_user=$1 ORDER BY created_at DESC LIMIT 1")
            .bind(referrer).fetch_optional(&self.pool).await.map_err(database_error)?;
        if original.is_some_and(|original| self.denied(&original)) {
            return Err(CommError::bad_request(format!(
                "Referrer is part of a banned referral chain {context}"
            )));
        }
        Ok(())
    }
    pub async fn create(
        &self,
        referrer: &Address,
        invited: &Address,
        ip: &str,
    ) -> Result<(), CommError> {
        if referrer == invited {
            return Err(CommError::bad_request(format!(
                "User cannot refer themselves: {invited}"
            )));
        }
        if let Some(existing) = sqlx::query_scalar::<_, String>(
            "SELECT referrer FROM social.referral_progress WHERE invited_user=$1",
        )
        .bind(invited)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        {
            resolve_existing(&existing, referrer, invited)?;
            return Ok(());
        }
        let ip = ip
            .parse::<std::net::IpAddr>()
            .map_err(|_| CommError::bad_request("Unable to determine client IP address"))?
            .to_canonical();
        // Historical source rows may retain other IPv6 spellings, including
        // IPv4-mapped IPv6. Count those facts without rewriting stored history.
        let alternate_ip = match ip {
            std::net::IpAddr::V4(ip) => format!("::ffff:{ip}"),
            std::net::IpAddr::V6(ip) => ip.to_string(),
        };
        let ip = ip.to_string();
        let ip = ip.as_str();
        self.assert_not_banned(referrer.as_str(), Some(ip)).await?;
        let mut tx = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        // Separate statements take a fresh READ COMMITTED snapshot after the
        // lock is acquired, even with a REPEATABLE READ role default.
        // Count and insert must share that transaction.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("social-referral-ip:{referrer}:{ip}"))
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        let status: Option<String> = sqlx::query_scalar("WITH other_users_invited AS (SELECT count(*) AS count FROM social.referral_progress WHERE referrer=$1 AND CASE WHEN pg_input_is_valid(invited_user_ip,'inet') THEN invited_user_ip::inet IN ($3::text::inet,$5::text::inet) ELSE false END) INSERT INTO social.referral_progress(referrer,invited_user,invited_user_ip,status) SELECT $1,$2,$3,CASE WHEN count < $4::text::bigint THEN 'pending' ELSE 'rejected_ip_match' END FROM other_users_invited ON CONFLICT(invited_user) DO NOTHING RETURNING status")
            .bind(referrer).bind(invited).bind(ip).bind(source_number_text(self.settings.max_ip_matches)).bind(alternate_ip).fetch_optional(&mut *tx).await.map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        let status = match status {
            Some(status) => status,
            None => {
                let existing: String = sqlx::query_scalar(
                    "SELECT referrer FROM social.referral_progress WHERE invited_user=$1",
                )
                .bind(invited)
                .fetch_one(&self.pool)
                .await
                .map_err(database_error)?;
                resolve_existing(&existing, referrer, invited)?;
                return Ok(());
            }
        };
        // Source queries recent rows after its committed INSERT, even if the
        // Slack sender is unavailable. Query errors therefore retain the insert
        // and reach the handler; notification errors are awaited and swallowed.
        let recent: Vec<chrono::NaiveDateTime> = sqlx::query_scalar(
            "SELECT created_at FROM social.referral_progress WHERE referrer=$1 ORDER BY created_at DESC LIMIT 2"
        ).bind(referrer).fetch_all(&self.pool).await.map_err(database_error)?;
        if let [newest, previous, ..] = recent.as_slice() {
            let newest = newest.and_utc().timestamp_millis();
            let previous = previous.and_utc().timestamp_millis();
            if ((newest - previous) as f64) < self.settings.suspicious_window_ms {
                self.slack
                    .send(self.slack.timing(referrer.as_str(), newest, previous))
                    .await;
            }
        }
        if status == "rejected_ip_match" {
            self.slack
                .send(self.slack.ip(
                    referrer.as_str(),
                    invited.as_str(),
                    ip,
                    self.settings.max_ip_matches,
                ))
                .await;
            return Err(CommError::bad_request(format!(
                "Invited user has already reached the maximum number of {} referrals from the same IP: {ip}",
                self.settings.max_ip_matches
            )));
        }
        Ok(())
    }

    pub async fn signed_up(&self, invited: &Address) -> Result<(), CommError> {
        let progress: Option<(String,Option<String>,String)> = sqlx::query_as("SELECT referrer,invited_user_ip,status FROM social.referral_progress WHERE invited_user=$1").bind(invited).fetch_optional(&self.pool).await.map_err(database_error)?;
        let Some((referrer, ip, current)) = progress else {
            return Err(CommError::not_found(format!(
                "Referral progress not found for user: {invited}"
            )));
        };
        self.assert_not_banned(&referrer, ip.as_deref()).await?;
        if current != "pending" {
            return Err(CommError::bad_request(format!(
                "Invalid referral status: {current}. Expected: pending"
            )));
        }
        // Source updateProgress validates the fetched status, then performs an
        // unconditional signup update. Only the grant transition has a guard.
        sqlx::query("UPDATE social.referral_progress SET status='signed_up',signed_up_at=timezone('UTC',now()),updated_at=timezone('UTC',now()) WHERE invited_user=$1").bind(invited).execute(&self.pool).await.map_err(database_error)?;
        Ok(())
    }
    /// Records an authenticated login using only local SQL. Reward delivery is
    /// resumed by the existing leased referral worker.
    pub async fn record_authenticated_login(
        &self,
        invited: &str,
        login_id: uuid::Uuid,
        occurred_at: i64,
    ) -> Result<(), CommError> {
        self.record_login_at(invited, None, Some((login_id, occurred_at)))
            .await
    }

    #[cfg(test)]
    pub(super) async fn finalize_referral(&self, invited: &str) -> Result<(), CommError> {
        self.finalize_at(invited, None).await
    }

    #[cfg(test)]
    async fn finalize_at(
        &self,
        invited: &str,
        today: Option<chrono::NaiveDate>,
    ) -> Result<(), CommError> {
        self.record_login_at(invited, today, None).await?;
        let pending: Option<(String, i32)> = sqlx::query_as("SELECT referrer,reward_tier FROM social.referral_progress WHERE invited_user=lower($1) AND reward_tier IS NOT NULL")
            .bind(invited).fetch_optional(&self.pool).await.map_err(database_error)?;
        if let Some((referrer, tier)) = pending {
            self.reward(&referrer, invited, tier.into()).await?;
        }
        Ok(())
    }

    async fn record_login_at(
        &self,
        invited: &str,
        today: Option<chrono::NaiveDate>,
        event: Option<(uuid::Uuid, i64)>,
    ) -> Result<(), CommError> {
        if crate::shared::writes::refusal(true).is_some() {
            return Err(CommError::MessageOnly(
                StatusCode::SERVICE_UNAVAILABLE,
                "Social writes are held".into(),
            ));
        }
        if !crate::auth::is_eth_address(invited) {
            return Err(CommError::bad_request("Invalid invitedUser address"));
        }
        let invited = Address::parse(invited)
            .map_err(|_| CommError::bad_request("Invalid invitedUser address"))?;
        let progress: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT referrer,invited_user_ip FROM social.referral_progress WHERE invited_user=$1",
        )
        .bind(&invited)
        .fetch_optional(&self.pool)
        .await
        .map_err(login_database_error)?;
        let Some((referrer, ip)) = progress else {
            return Ok(());
        };
        if let Err(error) = self.assert_not_banned(&referrer, ip.as_deref()).await {
            if event.is_some() && matches!(error, CommError::BadRequest(_)) {
                // Native callers must not learn the referrer's stored IP or ban status.
                return Ok(());
            }
            return Err(error);
        }
        let mut tx = self.pool.begin().await.map_err(login_database_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(login_database_error)?;
        if event.is_some() {
            sqlx::raw_sql("SET LOCAL lock_timeout='1s'; SET LOCAL statement_timeout='2s'")
                .execute(&mut *tx)
                .await
                .map_err(login_database_error)?;
        }
        let (status,granted,mut days): (String,bool,Vec<chrono::NaiveDate>) = sqlx::query_as("SELECT status,tier_granted,login_days FROM social.referral_progress WHERE invited_user=$1 FOR UPDATE")
            .bind(&invited).fetch_one(&mut *tx).await.map_err(login_database_error)?;
        if granted || status == "tier_granted" || status == "rejected_ip_match" {
            tx.commit().await.map_err(login_database_error)?;
            return Ok(());
        }
        if let Some((login_id, occurred_at)) = event {
            let now: chrono::NaiveDateTime =
                sqlx::query_scalar("SELECT timezone('UTC',clock_timestamp())")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(login_database_error)?;
            let now_ms = now.and_utc().timestamp_millis();
            if occurred_at < now_ms - LOGIN_MAX_AGE_MS
                || occurred_at > now_ms + LOGIN_FUTURE_SKEW_MS
            {
                return Err(CommError::bad_request(
                    "Login occurrence is outside the accepted time window",
                ));
            }
            let duplicate: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.referral_login_receipts WHERE invited_user=$1 AND login_id=$2)")
                .bind(&invited).bind(login_id).fetch_one(&mut *tx).await.map_err(login_database_error)?;
            if duplicate {
                return Ok(());
            }
            // Legacy receipts retain their old expiry after migration; their
            // backfilled server quota window must also finish before deletion.
            sqlx::query("DELETE FROM social.referral_login_receipts WHERE invited_user=$1 AND expires_at<$2 AND received_at <= $2::timestamp - interval '24 hours'")
                .bind(&invited).bind(now).execute(&mut *tx).await.map_err(login_database_error)?;
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM social.referral_login_receipts WHERE invited_user=$1 AND received_at > $2::timestamp - interval '24 hours'",
            )
            .bind(&invited)
            .bind(now)
            .fetch_one(&mut *tx)
            .await
            .map_err(login_database_error)?;
            if count >= 120 {
                return Err(CommError::MessageOnly(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Login event limit reached".into(),
                ));
            }
            let expires_at =
                chrono::DateTime::from_timestamp_millis(now_ms.max(occurred_at) + LOGIN_MAX_AGE_MS)
                    .ok_or_else(|| CommError::bad_request("Invalid login occurrence"))?
                    .naive_utc();
            sqlx::query("INSERT INTO social.referral_login_receipts(invited_user,login_id,expires_at,received_at) VALUES($1,$2,$3,$4)")
                .bind(&invited).bind(login_id).bind(expires_at).bind(now).execute(&mut *tx).await.map_err(login_database_error)?;
        }
        if (days.len() as f64) < self.settings.min_login_days {
            let today = match today {
                Some(day) => day,
                None => sqlx::query_scalar("SELECT timezone('UTC',clock_timestamp())::date")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(login_database_error)?,
            };
            if !days.contains(&today) {
                days.push(today);
            }
            sqlx::query("UPDATE social.referral_progress SET login_days=$2 WHERE invited_user=$1")
                .bind(&invited)
                .bind(&days)
                .execute(&mut *tx)
                .await
                .map_err(login_database_error)?;
            tx.commit().await.map_err(login_database_error)?;
            return Ok(());
        }
        // Every finalizer locks its own invitee before the referrer tier key.
        // Attribution's IP-quota key and reward's per-invitee key never acquire
        // this tier key, so they cannot reverse this lock order.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("social-referral-tier:{referrer}"))
            .execute(&mut *tx)
            .await
            .map_err(login_database_error)?;
        sqlx::query("UPDATE social.referral_progress SET status='tier_granted',tier_granted=true,tier_granted_at=timezone('UTC',now()),updated_at=timezone('UTC',now()),login_days='{}' WHERE invited_user=$1 AND NOT tier_granted")
            .bind(&invited).execute(&mut *tx).await.map_err(login_database_error)?;
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.referral_progress WHERE referrer=$1 AND tier_granted",
        )
        .bind(&referrer)
        .fetch_one(&mut *tx)
        .await
        .map_err(login_database_error)?;
        let tier = REWARD_TIERS.contains(&total);
        if tier {
            sqlx::query("UPDATE social.referral_progress SET reward_tier=$2 WHERE invited_user=$1")
                .bind(&invited)
                .bind(total as i32)
                .execute(&mut *tx)
                .await
                .map_err(login_database_error)?;
        }
        let notification = self.notification(&referrer, invited.as_str(), total, None)?;
        crate::notifications::insert_notifications(&mut *tx, &[notification])
            .await
            .map_err(login_database_error)?;
        tx.commit().await.map_err(login_database_error)?;
        if total == TIERS_IRL_SWAG {
            self.slack.send_milestone(&referrer);
        }
        Ok(())
    }
    /// Resume a bounded due set without requiring another login. Requests with
    /// no confirmed response stay quarantined for provider reconciliation.
    pub async fn drain_pending_rewards(&self) -> Result<(), CommError> {
        if crate::shared::writes::refusal(true).is_some() {
            return Ok(());
        }
        if let Err(error) = sqlx::query("DELETE FROM social.referral_login_receipts WHERE (invited_user,login_id) IN (SELECT invited_user,login_id FROM social.referral_login_receipts WHERE expires_at < timezone('UTC',clock_timestamp()) AND received_at <= timezone('UTC',clock_timestamp()) - interval '24 hours' ORDER BY expires_at,invited_user,login_id LIMIT 1000 FOR UPDATE SKIP LOCKED)")
            .execute(&self.pool).await {
            tracing::warn!(%error, "login receipt cleanup failed; continuing reward delivery");
        }
        let rows: Vec<(String, String, i32)> = sqlx::query_as(
            "SELECT referrer,invited_user,reward_tier FROM social.referral_progress \
             WHERE reward_tier IS NOT NULL \
               AND (reward_requested_at IS NULL OR reward_response IS NOT NULL) \
               AND (reward_retry_at IS NULL OR reward_retry_at <= timezone('UTC',now())) \
             ORDER BY reward_retry_at NULLS FIRST,invited_user LIMIT 64",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        for (referrer, invited, tier) in rows {
            // The restore hold is live: a long batch must stop before its next
            // grant if the stamps expire or are withdrawn during the pass.
            if crate::shared::writes::refusal(true).is_some() {
                break;
            }
            // The request path checks this too; a later ban also stops retries.
            if let Err(error) = self.assert_not_banned(&referrer, None).await {
                tracing::warn!(?error, "pending referral reward is blocked");
                sqlx::query("UPDATE social.referral_progress SET reward_retry_at=timezone('UTC',now())+interval '1 minute' WHERE invited_user=$1")
                    .bind(&invited).execute(&self.pool).await.map_err(database_error)?;
                continue;
            }
            if let Err(error) = self.reward(&referrer, &invited, tier.into()).await {
                tracing::warn!(?error, "pending referral reward failed");
            }
        }
        Ok(())
    }

    /// Commit a claim before the non-idempotent provider POST and retain its
    /// response before attempting any downstream effect. A lost response is
    /// ambiguous and must never automatically reissue a reward.
    async fn reward(&self, referrer: &str, invited: &str, total: i64) -> Result<(), CommError> {
        let mut connection = self.pool.acquire().await.map_err(database_error)?;
        // Closing this infrequent worker connection also releases a lock when
        // cancellation interrupts acquisition (SQLx try_acquire is not cancel safe).
        connection.close_on_drop();
        let lock = sqlx::postgres::PgAdvisoryLock::new(format!("social-referral-reward:{invited}"));
        let Some(mut guard) = lock
            .try_acquire(connection)
            .await
            .map_err(database_error)?
            .left()
        else {
            return Ok(());
        };
        let state: Option<(Option<chrono::NaiveDateTime>, Option<Value>, Option<Value>)> =
            sqlx::query_as(
                "UPDATE social.referral_progress \
             SET reward_retry_at=timezone('UTC',now())+interval '1 minute' \
             WHERE invited_user=$1 AND reward_tier=$2 \
             RETURNING reward_requested_at,reward_response,reward_paid",
            )
            .bind(invited)
            .bind(total as i32)
            .fetch_optional(&mut *guard)
            .await
            .map_err(database_error)?;
        let Some((requested, stored, paid)) = state else {
            return Ok(());
        };
        let reward = if let Some(reward) = stored {
            reward
        } else {
            if requested.is_some() {
                // The original failed attempt reports the error. A later login
                // cannot resolve this receipt, so leave it for reconciliation
                // without treating every login as another provider failure.
                return Ok(());
            }
            // Configuration failures happen before claiming and remain retryable.
            let url = self.settings.reward_server_url.as_deref().ok_or_else(|| {
                CommError::MessageOnly(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Reward server is not configured".into(),
                )
            })?;
            let url = url::Url::parse(url)
                .and_then(|base| base.join("/api/rewards"))
                .map_err(|_| CommError::Internal)?;
            let key = self
                .settings
                .reward_keys
                .get(&total)
                .filter(|key| !key.is_empty())
                .ok_or_else(|| {
                    CommError::MessageOnly(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Reward campaign key is not configured for tier {total}"),
                    )
                })?;
            let claimed = sqlx::query("UPDATE social.referral_progress SET reward_requested_at=timezone('UTC',now()) WHERE invited_user=$1 AND reward_tier=$2 AND reward_requested_at IS NULL")
                .bind(invited).bind(total as i32).execute(&mut *guard).await.map_err(database_error)?.rows_affected();
            if claimed == 0 {
                return Ok(());
            }
            let response = self
                .http
                .post(url)
                // Serve's shared client has only an inactivity limit; this
                // deadline also bounds a provider that keeps trickling body bytes.
                .timeout(std::time::Duration::from_secs(30))
                .json(&json!({"campaign_key":key,"beneficiary":referrer}))
                .send()
                .await
                .map_err(|_| CommError::Internal)?
                .error_for_status()
                .map_err(|_| CommError::Internal)?;
            let value: Value = response.json().await.map_err(|_| CommError::Internal)?;
            let reward = value["data"]
                .as_array()
                .and_then(|values| values.first())
                .filter(|reward| reward["image"].as_str().is_some())
                .ok_or(CommError::Internal)?
                .clone();
            sqlx::query(
                "UPDATE social.referral_progress SET reward_response=$2, reward_paid=$3 WHERE invited_user=$1",
            )
            .bind(invited)
            .bind(&reward)
            .bind(json!({"tier": total, "reward": reward, "notified": false}))
            .execute(&mut *guard)
            .await
            .map_err(database_error)?;
            reward
        };
        // The provider receipt above is durable before these local effects.
        // Their common transaction either completes the reward or leaves its
        // confirmed receipt available to the worker without another POST.
        let image = reward["image"].as_str().ok_or(CommError::Internal)?;
        let mut tx = sqlx::Connection::begin(&mut *guard)
            .await
            .map_err(database_error)?;
        sqlx::query("INSERT INTO social.referral_reward_images(referrer,reward_image_url,tier) SELECT $1,$2,$3 WHERE NOT EXISTS (SELECT 1 FROM social.referral_reward_images WHERE referrer=$1 AND tier=$3)")
            .bind(referrer).bind(image).bind(total as i32).execute(&mut *tx).await.map_err(database_error)?;
        if !paid.as_ref().is_some_and(|paid| paid["notified"] == true) {
            let notification = self.notification(referrer, invited, total, Some(&reward))?;
            crate::notifications::insert_notifications(&mut *tx, &[notification])
                .await
                .map_err(database_error)?;
        }
        sqlx::query("UPDATE social.referral_progress SET reward_tier=NULL,reward_paid=NULL,reward_retry_at=NULL WHERE invited_user=$1 AND reward_tier=$2")
            .bind(invited).bind(total as i32).execute(&mut *tx).await.map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        Ok(())
    }
    #[cfg(test)]
    async fn accepted(&self, referrer: &str) -> Result<i64, CommError> {
        sqlx::query_scalar(
            "SELECT count(*) FROM social.referral_progress WHERE referrer=$1 AND tier_granted",
        )
        .bind(referrer)
        .fetch_one(&self.pool)
        .await
        .map_err(database_error)
    }
    fn notification(
        &self,
        referrer: &str,
        invited: &str,
        total: i64,
        reward: Option<&Value>,
    ) -> Result<crate::notifications::NewNotification, CommError> {
        let tier = REWARD_TIERS
            .iter()
            .position(|tier| total <= *tier)
            .map_or(0, |index| index + 1);
        let (kind, title, description, image, rarity) = match reward {
            Some(reward) => (
                "referral_new_tier_reached",
                "Referral Reward Unlocked!",
                "Check the 'Referral Rewards' tab in your web profile to see your prize!",
                reward["image"].as_str().unwrap_or_default().to_string(),
                reward.get("rarity").cloned().unwrap_or(Value::Null),
            ),
            None => (
                "referral_invited_users_accepted",
                "Referral Completed!",
                "Your friend jumped into Decentraland, so you're closer to unlocking your next reward!",
                crate::notifications::cdn_asset(
                    &self.settings.cdn_url,
                    "referral/referral-invited-user-accepted-icon.png",
                ),
                Value::Null,
            ),
        };
        let metadata = json!({"address":referrer,"title":title,"description":description,"tier":tier,"url":format!("{}/accounts/{referrer}/referral",self.settings.profile_url),"image":image,"invitedUserAddress":invited,"invitedUsers":total,"rarity":rarity});
        let address = Address::parse(referrer).map_err(|_| CommError::Internal)?;
        Ok(crate::notifications::NewNotification::now(
            address, kind, metadata,
        ))
    }
}

// The socket identifies the trust boundary. Direct clients cannot choose their
// quota bucket with forwarding headers. A configured backend proxy overwrites
// X-Real-IP; CDN and forwarding-chain headers never authorize that value here.
use crate::shared::client_ip::peer_client_ip as client_ip;

#[derive(Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct ReferralRewardImage {
    pub tier: i32,
    pub url: String,
}

#[derive(Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct ReferralProgressStats {
    #[serde(rename = "invitedUsersAccepted")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub invited_users_accepted: i64,
    #[serde(rename = "invitedUsersAcceptedViewed")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub invited_users_accepted_viewed: i64,
    #[serde(rename = "rewardImages")]
    pub reward_images: Vec<ReferralRewardImage>,
}

#[derive(Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct CreateReferralBody {
    pub referrer: String,
}

/// GET reports the stats and marks them viewed; the mark is a social write,
/// skipped while the social switch is off or held (the stats still answer).
pub async fn get_referral_progress(
    State(state): State<SharedState>,
    headers: HeaderMap,
    switch: Option<Extension<crate::comms::writes::Switch>>,
    original: OriginalUri,
) -> Result<Json<ReferralProgressStats>, CommError> {
    let referrer = state
        .require_signer(
            &headers,
            "get",
            &signed_path(&original, "/v1/referral-progress"),
        )
        .await?;
    let accepted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM social.referral_progress WHERE referrer = $1 AND tier_granted",
    )
    .bind(&referrer)
    .fetch_one(&state.pool)
    .await
    .map_err(database_error)?;
    let viewed: i64 = sqlx::query_scalar(
        "SELECT invites_accepted_viewed FROM social.referral_progress_viewed WHERE referrer = $1",
    )
    .bind(&referrer)
    .fetch_optional(&state.pool)
    .await
    .map_err(database_error)?
    .unwrap_or(0);
    let reward_images = sqlx::query_as::<_, (i32, String)>(
        "SELECT tier,reward_image_url FROM social.referral_reward_images WHERE referrer=$1",
    )
    .bind(&referrer)
    .fetch_all(&state.pool)
    .await
    .map_err(database_error)?
    .into_iter()
    .map(|(tier, url)| ReferralRewardImage { tier, url })
    .collect();
    if crate::comms::writes::side_refusal(switch).is_none() {
        sqlx::query(
            "INSERT INTO social.referral_progress_viewed (referrer, invites_accepted_viewed) \
             VALUES ($1, $2) \
             ON CONFLICT (referrer) DO UPDATE \
             SET invites_accepted_viewed = EXCLUDED.invites_accepted_viewed, updated_at = timezone('UTC', now())",
        )
        .bind(&referrer)
        .bind(accepted)
        .execute(&state.pool)
        .await.map_err(database_error)?;
    }
    Ok(Json(ReferralProgressStats {
        invited_users_accepted: accepted,
        invited_users_accepted_viewed: viewed,
        reward_images,
    }))
}

/// A same-referrer duplicate is an idempotent no-op (204) so client retries
/// converge; a different referrer is a conflict, attribution is first-wins.
fn resolve_existing(
    existing: &str,
    referrer: &Address,
    invited: &Address,
) -> Result<StatusCode, CommError> {
    if existing == referrer.as_str() {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(CommError::bad_request(format!(
            "Referral progress already exists for the invited user: {invited}"
        )))
    }
}

pub async fn create_referral(
    State(state): State<SharedState>,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    headers: HeaderMap,
    original: OriginalUri,
    body: super::http::DeferredBody,
) -> Result<StatusCode, CommError> {
    let invited = state
        .require_signer(
            &headers,
            "post",
            &signed_path(&original, "/v1/referral-progress"),
        )
        .await?;
    let body = body.schema().await?;
    let body = crate::rpc::http::body(
        &body,
        &[(
            "referrer",
            crate::rpc::http::Kind::TextRange {
                min: 1,
                max: usize::MAX,
                non_whitespace: false,
            },
        )],
        &["referrer"],
        0,
    )?;
    if state.feature_enabled(super::features::REFERRAL_REGISTRATION_DISABLED) {
        return Err(CommError::Body(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"Service Unavailable","message":"Referral registration is temporarily disabled"}),
        ));
    }
    let ip = client_ip(
        &headers,
        peer.map(|Extension(ConnectInfo(peer))| peer),
        &state.referral.settings.trusted_proxy_cidrs,
    )
    .ok_or_else(|| CommError::bad_request("Unable to determine client IP address"))?;
    let raw_referrer = body["referrer"].as_str().unwrap();
    if !crate::auth::is_eth_address(raw_referrer) {
        return Err(CommError::bad_request("Invalid referrer address"));
    }
    let referrer = Address::parse(raw_referrer)
        .map_err(|_| CommError::bad_request("Invalid referrer address"))?;
    state.referral.create(&referrer, &invited, &ip).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Explorer reports both fresh and restored authenticated login occurrences.
/// Identity comes only from the verified chain; event fields are signed metadata.
pub async fn client_login(
    State(state): State<SharedState>,
    headers: HeaderMap,
    original: OriginalUri,
) -> Result<StatusCode, CommError> {
    let invited = state
        .require_signer(
            &headers,
            "post",
            &signed_path(&original, "/v1/client-login"),
        )
        .await?;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LoginMetadata {
        login_id: uuid::Uuid,
        occurred_at: i64,
    }
    let metadata: LoginMetadata = headers
        .get(crate::shared::signed_fetch::AUTH_METADATA_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| serde_json::from_str(value).ok())
        .ok_or_else(|| {
            CommError::bad_request("Signed loginId and occurredAt metadata are required")
        })?;
    // Check even wallets with no referral, before the domain's no-op path.
    let now = chrono::Utc::now().timestamp_millis();
    if metadata.occurred_at < now - LOGIN_MAX_AGE_MS
        || metadata.occurred_at > now + LOGIN_FUTURE_SKEW_MS
    {
        return Err(CommError::bad_request(
            "Login occurrence is outside the accepted time window",
        ));
    }
    state
        .referral
        .record_authenticated_login(invited.as_str(), metadata.login_id, metadata.occurred_at)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn update_referral_signed_up(
    State(state): State<SharedState>,
    headers: HeaderMap,
    original: OriginalUri,
) -> Result<StatusCode, CommError> {
    let invited = state
        .require_signer(
            &headers,
            "patch",
            &signed_path(&original, "/v1/referral-progress"),
        )
        .await?;
    state.referral.signed_up(&invited).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Upstream's `TIERS`: the accepted invites that unlock a reward.
const REWARD_TIERS: [i64; 8] = [5, 10, 20, 25, 30, 50, 60, 75];
/// Upstream's `TIERS_IRL_SWAG`: the accepted invites that unlock the email.
const TIERS_IRL_SWAG: i64 = 100;

/// `setReferralEmail`'s refusal of characters usable in markup or script
/// (`logic/referral/referral.ts` at `b6b2cef`), case-insensitive.
fn has_dangerous_chars(email: &str) -> bool {
    email.chars().any(|c| "<>\"'`&;(){}[]\\".contains(c))
        || [
            "script",
            "javascript",
            "vbscript",
            "onload",
            "onerror",
            "onclick",
        ]
        .iter()
        .any(|word| email.to_lowercase().contains(word))
}

/// @dcl/schemas Email.validate is a separate pattern from AJV's email
/// format: ASCII word/hyphen/dot local, word/hyphen labels, final label>=2.
fn is_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let labels = domain.split('.').collect::<Vec<_>>();
    !local.is_empty()
        && local.bytes().all(|c| word(c) || c == b'-' || c == b'.')
        && labels.len() >= 2
        && labels.last().unwrap().len() >= 2
        && labels
            .into_iter()
            .all(|part| !part.is_empty() && part.bytes().all(|c| word(c) || c == b'-'))
}

/// The checks of upstream `setReferralEmail` that need no stored row, in its
/// order; the email lower-cased and trimmed.
fn validated_email(raw: Option<&str>) -> Result<String, CommError> {
    let email = raw
        .map(|value| value.trim_matches(crate::rpc::http::js_whitespace))
        .unwrap_or_default();
    if email.is_empty() {
        return Err(CommError::bad_request("Email is required"));
    }
    let email = email.to_lowercase();
    if email.encode_utf16().count() > 254 {
        return Err(CommError::bad_request("Email is too long"));
    }
    if has_dangerous_chars(&email) {
        return Err(CommError::bad_request("Email contains invalid characters"));
    }
    if !is_email(&email) {
        return Err(CommError::bad_request("Invalid email format"));
    }
    Ok(email)
}

/// `POST /v1/referral-email`: a referrer with 100 accepted invites leaves a
/// contact email, at most once per 24 hours. Upstream also mails it to its
/// marketing team; the existing local mail sender performs that best-effort
/// effect after the contact row commits.
pub async fn add_referral_email(
    State(state): State<SharedState>,
    headers: HeaderMap,
    original: OriginalUri,
    body: super::http::DeferredBody,
) -> Result<StatusCode, CommError> {
    let referrer = state
        .require_signer(
            &headers,
            "post",
            &signed_path(&original, "/v1/referral-email"),
        )
        .await?;
    let body = body.schema().await?;
    let parsed = crate::rpc::http::body(
        &body,
        &[("email", crate::rpc::http::Kind::Email)],
        &["email"],
        0,
    )?;
    if state.referral.denied(referrer.as_str()) {
        return Err(CommError::bad_request(format!(
            "Referrer is on the deny list {referrer}"
        )));
    }
    let accepted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM social.referral_progress WHERE referrer = $1 AND tier_granted",
    )
    .bind(&referrer)
    .fetch_one(&state.pool)
    .await
    .map_err(database_error)?;
    if accepted < TIERS_IRL_SWAG {
        return Err(CommError::bad_request(format!(
            "You must have at least {TIERS_IRL_SWAG} accepted invites to set an email"
        )));
    }
    let email = validated_email(parsed.get("email").and_then(|v| v.as_str()))?;
    let inserted = sqlx::query(
        "INSERT INTO social.referral_emails (referrer, email) \
         SELECT $1, $2 WHERE NOT EXISTS ( \
           SELECT 1 FROM social.referral_emails \
           WHERE referrer = $1 AND updated_at > timezone('UTC', now()) - interval '24 hours')",
    )
    .bind(&referrer)
    .bind(&email)
    .execute(&state.pool)
    .await
    .map_err(database_error)?
    .rows_affected();
    if inserted == 0 {
        return Err(CommError::bad_request(format!(
            "Email can only be updated once per day. Last update was less than 24 hours ago for user: {referrer}"
        )));
    }
    if let Some(mailer) = state.referral.mailer.get() {
        let content = format!(
            "A user has unlocked the IRL Swag Referral Tier and provided the following email for contact: {email}"
        );
        if let Err(error) = mailer
            .send_html(
                "marketing@decentraland.org",
                "[Action Needed] IRL Swag Referral Tier Unlocked",
                &content,
                &content,
                &json!({}),
            )
            .await
        {
            tracing::warn!(
                ?error,
                "Referral marketing email failed after contact persisted"
            );
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                )
            })
            .collect()
    }
    fn peer(ip: &str) -> Option<std::net::SocketAddr> {
        Some(std::net::SocketAddr::new(ip.parse().unwrap(), 12345))
    }
    #[test]
    fn client_ip_uses_the_untrusted_socket_peer_despite_all_forged_headers() {
        for spoofed in ["192.0.2.1", "192.0.2.2", "invalid"] {
            let headers = headers(&[
                ("cf-connecting-ip", spoofed),
                ("x-forwarded-for", spoofed),
                ("x-real-ip", spoofed),
            ]);
            assert_eq!(
                client_ip(&headers, peer("198.51.100.9"), &[]),
                Some("198.51.100.9".into())
            );
            assert_eq!(client_ip(&headers, None, &[]), None);
        }
        assert_eq!(
            client_ip(&HeaderMap::new(), peer("::ffff:198.51.100.9"), &[]),
            Some("198.51.100.9".into())
        );
        assert_eq!(
            client_ip(&HeaderMap::new(), peer("2001:0DB8:0:0:0:0:0:0001"), &[]),
            Some("2001:db8::1".into())
        );
    }
    #[test]
    fn only_an_explicit_trusted_proxy_may_supply_a_single_valid_real_ip() {
        let proxies = [
            "192.0.2.0/24".parse().unwrap(),
            "2001:db8:1::/48".parse().unwrap(),
        ];
        let headers = headers(&[
            ("cf-connecting-ip", "198.51.100.77"),
            ("x-forwarded-for", "198.51.100.1, 198.51.100.2"),
            ("x-real-ip", "2001:0DB8:0:0:0:0:0:0002"),
        ]);
        for peer in [
            peer("192.0.2.9"),
            peer("::ffff:192.0.2.9"),
            peer("2001:db8:1::9"),
        ] {
            assert_eq!(
                client_ip(&headers, peer, &proxies),
                Some("2001:db8::2".into())
            );
        }
        assert_eq!(
            client_ip(&headers, peer("198.51.100.9"), &proxies),
            Some("198.51.100.9".into())
        );
        assert_eq!(client_ip(&headers, None, &proxies), None);
        for raw in [
            "invalid",
            " 192.0.2.1",
            "192.168.001.1",
            "[2001:db8::1]",
            "192.0.2.1:123",
            "192.0.2.1,192.0.2.2",
        ] {
            assert_eq!(
                client_ip(
                    &self::headers(&[("x-real-ip", raw), ("cf-connecting-ip", "198.51.100.9")]),
                    peer("192.0.2.9"),
                    &proxies
                ),
                None,
                "{raw}"
            );
        }
        assert_eq!(
            client_ip(&HeaderMap::new(), peer("192.0.2.9"), &proxies),
            None
        );
        let mut duplicate = self::headers(&[("x-real-ip", "198.51.100.1")]);
        duplicate.append("x-real-ip", "198.51.100.2".parse().unwrap());
        assert_eq!(client_ip(&duplicate, peer("192.0.2.9"), &proxies), None);
    }

    #[tokio::test]
    async fn referral_schema_answers_match_executed_upstream_ajv_middleware() {
        use axum::response::IntoResponse;
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/social/referral-cases.json"
        )))
        .unwrap();
        for case in fixture["schemas"].as_array().unwrap() {
            let (name, kind) = if case["schema"] == "CreateReferralSchema" {
                (
                    "referrer",
                    crate::rpc::http::Kind::TextRange {
                        min: 1,
                        max: usize::MAX,
                        non_whitespace: false,
                    },
                )
            } else {
                ("email", crate::rpc::http::Kind::Email)
            };
            let response = crate::rpc::http::body(
                case["raw"].as_str().unwrap().as_bytes(),
                &[(name, kind)],
                &[name],
                0,
            );
            match response {
                Ok(_) => assert_eq!(case["response"]["status"], 204),
                Err(error) => {
                    let response = error.into_response();
                    assert_eq!(
                        response.status().as_u16(),
                        case["response"]["status"].as_u64().unwrap() as u16
                    );
                    let bytes = axum::body::to_bytes(response.into_body(), 16384)
                        .await
                        .unwrap();
                    let body: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(body, case["response"]["body"], "{}", case["raw"]);
                }
            }
        }
    }

    /// Replaces this test's wallets in the node's local denylist overlay.
    async fn set_denied(
        pool: &sqlx::PgPool,
        denylist: &crate::explore::api::denylist::Snapshot,
        denied: &mut Vec<String>,
        wallets: Vec<String>,
    ) {
        sqlx::query("DELETE FROM explore_api.denylist_users_local WHERE wallet = ANY($1)")
            .bind(&*denied)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO explore_api.denylist_users_local (wallet, present) SELECT lower(w), TRUE FROM unnest($1::text[]) AS w")
            .bind(&wallets)
            .execute(pool)
            .await
            .unwrap();
        *denied = wallets;
        denylist.reload(pool).await.unwrap();
    }

    #[tokio::test]
    async fn referral_sql_lifecycle_matches_executed_upstream_component_and_db_cases() {
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/social/referral-cases.json"
        )))
        .unwrap();
        let app=axum::Router::new()
            .route("/api/rewards",axum::routing::post(||async{Json(json!({"data":[{"image":"https://reward.example.org/tier.png","rarity":"rare"}]}))}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let settings = Settings {
            profile_url: "https://profile.example.org".into(),
            reward_server_url: Some(base),
            ..Settings::for_tests()
        };
        let referral = Referral::new(pool.clone(), Outbound::for_tests(), settings);
        // The node's in-memory explorer denylist, fed from its local overlay.
        let denylist = std::sync::Arc::new(crate::explore::api::denylist::Snapshot::default());
        referral.set_denylist(denylist.clone());
        let mut denied: Vec<String> = Vec::new();
        for case in fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| !case["id"].as_str().unwrap().contains("email"))
        {
            // Map stable fixture wallets into per-test random wallets, while
            // preserving all source identity relationships across steps.
            let mut addresses = std::collections::HashMap::<String, String>::new();
            let mapped = |raw: &str, addresses: &mut std::collections::HashMap<String, String>| {
                addresses
                    .entry(raw.into())
                    .or_insert_with(|| format!("0x{}00000000", uuid::Uuid::new_v4().simple()))
                    .clone()
            };
            let referrer = mapped(fixture["referrer"].as_str().unwrap(), &mut addresses);
            set_denied(&pool, &denylist, &mut denied, Vec::new()).await;
            for attempt in case["attempts"].as_array().unwrap() {
                let step = &attempt["step"];
                if let Some(deny) = step["deny"].as_array() {
                    let wallets = deny
                        .iter()
                        .map(|raw| mapped(raw.as_str().unwrap(), &mut addresses))
                        .collect();
                    set_denied(&pool, &denylist, &mut denied, wallets).await;
                }
                let invited = step["invited"]
                    .as_str()
                    .map(|raw| mapped(raw, &mut addresses));
                let actor = step["referrer"]
                    .as_str()
                    .map(|raw| mapped(raw, &mut addresses))
                    .unwrap_or(referrer.clone());
                let now =
                    chrono::DateTime::from_timestamp_millis(attempt["now_ms"].as_i64().unwrap())
                        .unwrap();
                let result = match step["action"].as_str().unwrap() {
                    "create" => {
                        referral
                            .create(
                                &Address::parse(&actor).unwrap(),
                                &Address::parse(invited.as_deref().unwrap()).unwrap(),
                                step["ip"].as_str().unwrap_or("192.0.2.1"),
                            )
                            .await
                    }
                    "signup" => {
                        referral
                            .signed_up(&Address::parse(invited.as_deref().unwrap()).unwrap())
                            .await
                    }
                    "login" => {
                        referral
                            .finalize_at(invited.as_deref().unwrap(), Some(now.date_naive()))
                            .await
                    }
                    "seed-granted" => {
                        for index in 1..=step["count"].as_i64().unwrap() {
                            let raw = format!("0x{:040x}", index + 10000);
                            let invited = mapped(&raw, &mut addresses);
                            sqlx::query("INSERT INTO social.referral_progress(invited_user,referrer,status,tier_granted) VALUES($1,$2,'tier_granted',true)").bind(invited).bind(&referrer).execute(&pool).await.unwrap();
                        }
                        Ok(())
                    }
                    "stats" => {
                        assert_eq!(
                            referral.accepted(&actor).await.unwrap(),
                            attempt["result"]["invitedUsersAccepted"].as_i64().unwrap()
                        );
                        Ok(())
                    }
                    other => panic!("Unhandled source step {other}"),
                };
                if let Some(expected) = attempt["result"]["message"].as_str() {
                    let actual = match result.unwrap_err() {
                        CommError::BadRequest(message) | CommError::NotFound(message) => message,
                        other => panic!("{other:?}"),
                    };
                    let mut expected = expected.to_string();
                    for (raw, mapped) in &addresses {
                        expected = expected.replace(raw, mapped);
                    }
                    assert_eq!(actual, expected, "{}", case["id"]);
                } else {
                    result.unwrap();
                }
                for expected in attempt["rows"].as_array().unwrap() {
                    let invited =
                        mapped(expected["invited_user"].as_str().unwrap(), &mut addresses);
                    let row:(String,String,Option<String>,bool,bool,bool,Vec<chrono::NaiveDate>)=sqlx::query_as("SELECT referrer,status,invited_user_ip,tier_granted,signed_up_at IS NOT NULL,tier_granted_at IS NOT NULL,login_days FROM social.referral_progress WHERE invited_user=$1").bind(&invited).fetch_one(&pool).await.unwrap();
                    assert_eq!(
                        (row.0, row.1, row.2, row.3, row.4, row.5),
                        (
                            mapped(expected["referrer"].as_str().unwrap(), &mut addresses),
                            expected["status"].as_str().unwrap().into(),
                            expected["invited_user_ip"].as_str().map(str::to_string),
                            expected["tier_granted"].as_bool().unwrap(),
                            !expected["signed_up_at"].is_null(),
                            !expected["tier_granted_at"].is_null()
                        ),
                        "{} step {}",
                        case["id"],
                        step
                    );
                    let key = format!(
                        "referral:invited-user:{}",
                        expected["invited_user"].as_str().unwrap()
                    );
                    if let Some(days) = attempt["cache"][&key].as_array() {
                        assert_eq!(
                            row.6.iter().map(ToString::to_string).collect::<Vec<_>>(),
                            days.iter()
                                .map(|day| day.as_str().unwrap().to_string())
                                .collect::<Vec<_>>()
                        );
                    }
                }
                let images:Vec<(i32,String)>=sqlx::query_as("SELECT tier,reward_image_url FROM social.referral_reward_images WHERE referrer=$1 ORDER BY tier").bind(&referrer).fetch_all(&pool).await.unwrap();
                let expected = attempt["images"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|image| {
                        (
                            image["tier"].as_i64().unwrap() as i32,
                            image["reward_image_url"].as_str().unwrap().to_string(),
                        )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(images, expected);
                for effect in attempt["effects"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|effect| effect["kind"] == "sns")
                {
                    let mut metadata = effect["event"]["metadata"].clone();
                    for key in ["address", "invitedUserAddress"] {
                        let raw = metadata[key].as_str().unwrap();
                        metadata[key] = json!(mapped(raw, &mut addresses));
                    }
                    let url = metadata["url"]
                        .as_str()
                        .unwrap()
                        .replace(fixture["referrer"].as_str().unwrap(), &referrer);
                    metadata["url"] = json!(url);
                    let kind = effect["event"]["subType"]
                        .as_str()
                        .unwrap()
                        .replace('-', "_");
                    let found:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.notifications WHERE address=$1 AND type=$2 AND metadata=$3)").bind(&referrer).bind(kind).bind(metadata).fetch_one(&pool).await.unwrap();
                    assert!(found, "{} notification metadata", case["id"]);
                }
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn referral_receipt_migration_matches_bootstrap_columns() {
        let Some(pool) = crate::db::scratch::pool(3).await else {
            return;
        };
        let schema = format!("referral_receipt_{}", uuid::Uuid::new_v4().simple());
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE SCHEMA {schema}; CREATE TABLE {schema}.referral_progress (invited_user text PRIMARY KEY, reward_tier integer, reward_paid jsonb)"
        ))).execute(&pool).await.unwrap();
        let step = include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/schemas/migrations/0024_social_referral_reward_receipt.sql"
        ))
        .replace("social.", &format!("{schema}."));
        // The additive step owns the column changes. New indexes are created
        // by the migrator's object pass, covered by the full upgrade test below.
        for _ in 0..2 {
            sqlx::raw_sql(sqlx::AssertSqlSafe(step.clone()))
                .execute(&pool)
                .await
                .unwrap();
        }
        let columns = |table: String| {
            let pool = &pool;
            async move {
                sqlx::query_as::<_, (String, String, bool)>(
                    "SELECT attname::text,format_type(atttypid,atttypmod),attnotnull FROM pg_attribute WHERE attrelid=to_regclass($1) AND attname IN ('reward_requested_at','reward_response','reward_retry_at','reward_paid') AND NOT attisdropped ORDER BY attname"
                ).bind(table).fetch_all(pool).await.unwrap()
            }
        };
        let fresh = columns("social.referral_progress".into()).await;
        assert_eq!(fresh.len(), 4);
        assert_eq!(columns(format!("{schema}.referral_progress")).await, fresh);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn referral_migration_preserves_populated_ids_and_is_repeatable() {
        let Some(pool) = crate::db::scratch::pool(3).await else {
            return;
        };
        let schema = format!("referral_upgrade_{}", uuid::Uuid::new_v4().simple());
        let older = format!(
            "CREATE SCHEMA {schema}; CREATE TABLE {schema}.referral_progress (id uuid NOT NULL DEFAULT gen_random_uuid(),invited_user text PRIMARY KEY,referrer text NOT NULL,status text NOT NULL,reward_tier integer,created_at timestamp NOT NULL DEFAULT timezone('UTC',now()),updated_at timestamp NOT NULL DEFAULT timezone('UTC',now())); INSERT INTO {schema}.referral_progress(invited_user,referrer,status,reward_tier) VALUES('invited','referrer','tier_granted',5),('pending','referrer','pending',NULL)"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(older))
            .execute(&pool)
            .await
            .unwrap();
        let paid_receipts = [
            (
                "paid",
                json!({"tier": 10, "reward": {"image": "https://reward.example.org/paid.png"}, "notified": true}),
            ),
            (
                "missing_image",
                json!({"tier": 10, "reward": {}, "notified": true}),
            ),
            (
                "invalid_image",
                json!({"tier": 10, "reward": {"image": 42}, "notified": false}),
            ),
        ];
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {schema}.referral_progress ADD COLUMN reward_paid jsonb"
        )))
        .execute(&pool)
        .await
        .unwrap();
        for (name, paid) in &paid_receipts {
            sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO {schema}.referral_progress(invited_user,referrer,status,reward_paid) VALUES($1,'referrer','tier_granted',$2)")))
                .bind(name).bind(paid).execute(&pool).await.unwrap();
        }
        let ids: Vec<uuid::Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM {schema}.referral_progress ORDER BY invited_user"
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        let source = include_str!(concat!(env!("DCL_ONE_ROOT"), "/schemas/40_social.sql"));
        let canonical = source
            .split("-- ── referral ─")
            .nth(1)
            .unwrap()
            .split("-- ── media:")
            .next()
            .unwrap();
        let canonical = format!(
            "CREATE SCHEMA {schema};\nCREATE TABLE social.referral_progress {}",
            canonical
                .split("CREATE TABLE social.referral_progress")
                .nth(1)
                .unwrap()
        )
        .replace("social.", &format!("{schema}."));
        let step_name = format!("0021_referral_test_{}", uuid::Uuid::new_v4().simple());
        let steps = format!(
            "-- ════ step {step_name} ════\n{}\n-- ════ step {step_name}_01_reward ════\n{}\n-- ════ step {step_name}_02_receipt ════\n{}",
            include_str!(concat!(
                env!("DCL_ONE_ROOT"),
                "/schemas/migrations/0021_social_referral_lifecycle.sql"
            )),
            include_str!(concat!(
                env!("DCL_ONE_ROOT"),
                "/schemas/migrations/0023_social_referral_pending_reward.sql"
            )),
            include_str!(concat!(
                env!("DCL_ONE_ROOT"),
                "/schemas/migrations/0024_social_referral_reward_receipt.sql"
            ))
        )
        .replace("social.", &format!("{schema}."));
        let done = crate::db::schema_migrate::migrate(&pool, &canonical, &steps)
            .await
            .unwrap();
        assert_eq!(
            done.steps,
            vec![
                step_name.clone(),
                format!("{step_name}_01_reward"),
                format!("{step_name}_02_receipt")
            ]
        );
        assert!(
            !done.created.is_empty(),
            "new reward table and indexes come from canonical bootstrap pass"
        );
        assert!(
            crate::db::schema_migrate::migrate(&pool, &canonical, &steps)
                .await
                .unwrap()
                .is_empty()
        );
        // New-object reconciliation must produce the same due-work index
        // as a fresh bootstrap after the additive column steps have run.
        let index = |name: String| {
            let pool = &pool;
            async move {
                sqlx::query_scalar::<_, String>("SELECT pg_get_indexdef(to_regclass($1))")
                    .bind(name)
                    .fetch_one(pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            index(format!("{schema}.referral_progress_reward_due_idx"))
                .await
                .replace(&format!("{schema}."), "social."),
            index("social.referral_progress_reward_due_idx".into()).await,
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM meta.schema_steps WHERE step LIKE $1 || '%' AND NOT created_fresh",
        )
        .bind(&step_name)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 3);
        let actual: Vec<uuid::Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM {schema}.referral_progress ORDER BY invited_user"
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(actual, ids);
        let granted: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {schema}.referral_progress WHERE tier_granted"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(granted, 4);
        let receipt: (Option<i32>, bool, bool) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT reward_tier,reward_requested_at IS NOT NULL,reward_response IS NULL FROM {schema}.referral_progress WHERE invited_user='invited'"
        ))).fetch_one(&pool).await.unwrap();
        assert_eq!(
            receipt,
            (Some(5), true, true),
            "old pending markers have an unknown provider outcome"
        );
        for (name, paid) in paid_receipts {
            let imported: (Option<i32>, Option<Value>, Option<Value>, bool) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT reward_tier,reward_response,reward_paid,reward_requested_at IS NOT NULL FROM {schema}.referral_progress WHERE invited_user=$1"
            ))).bind(name).fetch_one(&pool).await.unwrap();
            assert_eq!(
                imported,
                (Some(10), Some(paid["reward"].clone()), Some(paid), true),
                "{name}: known payment and notification state survives incomplete image metadata"
            );
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM meta.schema_steps WHERE step LIKE $1 || '%'")
            .bind(step_name)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn referral_numeric_configuration_matches_executed_source_pg_and_login_days() {
        use axum::response::IntoResponse;
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let fixture: Value = serde_json::from_str(include_str!(concat!(env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/social/source-contracts/referral-numbers.json"))).unwrap();
        let referrer = Address::parse(fixture["referrer"].as_str().unwrap()).unwrap();
        let invited = fixture["invited"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| Address::parse(value.as_str().unwrap()).unwrap())
            .collect::<Vec<_>>();
        for case in fixture["cases"].as_array().unwrap() {
            if case["startup_error"].is_string() {
                continue;
            } // Actual Clap fields reject these in config tests.
            sqlx::query("DELETE FROM social.referral_progress WHERE referrer=$1")
                .bind(&referrer)
                .execute(&pool)
                .await
                .unwrap();
            let number: f64 = case["number"].as_str().unwrap().parse().unwrap();
            assert_eq!(source_number_text(number), case["number"].as_str().unwrap());
            let mut settings = Settings::for_tests();
            if case["kind"] == "max" {
                settings.max_ip_matches = number
            } else {
                settings.min_login_days = number
            }
            let referral = Referral::new(pool.clone(), Outbound::for_tests(), settings);
            if case["kind"] == "max" {
                for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                    let response = match referral
                        .create(&referrer, &invited[index], "192.0.2.1")
                        .await
                    {
                        Ok(()) => StatusCode::NO_CONTENT.into_response(),
                        Err(error) => error.into_response(),
                    };
                    assert_eq!(
                        response.status().as_u16(),
                        step["response"]["status"].as_u64().unwrap() as u16,
                        "{}",
                        case["raw"]
                    );
                    let bytes = axum::body::to_bytes(response.into_body(), 8192)
                        .await
                        .unwrap();
                    if let Some(body) = step["response"].get("body") {
                        assert_eq!(
                            serde_json::from_slice::<Value>(&bytes).unwrap(),
                            *body,
                            "{}",
                            case["raw"]
                        )
                    } else {
                        assert!(bytes.is_empty())
                    }
                    let rows: Vec<String> = sqlx::query_scalar("SELECT status FROM social.referral_progress WHERE referrer=$1 ORDER BY invited_user").bind(&referrer).fetch_all(&pool).await.unwrap();
                    assert_eq!(serde_json::to_value(rows).unwrap(), step["rows"]);
                }
            } else {
                referral
                    .create(&referrer, &invited[0], "192.0.2.1")
                    .await
                    .unwrap();
                for step in case["steps"].as_array().unwrap() {
                    let day = step["day"]
                        .as_str()
                        .unwrap()
                        .parse::<chrono::NaiveDate>()
                        .unwrap();
                    referral
                        .finalize_at(invited[0].as_str(), Some(day))
                        .await
                        .unwrap();
                    let (status,granted,days):(String,bool,Vec<chrono::NaiveDate>) = sqlx::query_as("SELECT status,tier_granted,login_days FROM social.referral_progress WHERE invited_user=$1").bind(&invited[0]).fetch_one(&pool).await.unwrap();
                    assert_eq!(
                        serde_json::json!({"status":status,"tier_granted":granted}),
                        step["row"],
                        "{}",
                        case["raw"]
                    );
                    assert_eq!(
                        serde_json::to_value(
                            days.iter().map(ToString::to_string).collect::<Vec<_>>()
                        )
                        .unwrap(),
                        step["days"],
                        "{}",
                        case["raw"]
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn concurrent_login_finalization_grants_and_notifies_once() {
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let settings = Settings {
            min_login_days: 0.0,
            ..Settings::for_tests()
        };
        let referral =
            std::sync::Arc::new(Referral::new(pool.clone(), Outbound::for_tests(), settings));
        let address =
            || Address::parse(&format!("0x{}00000000", uuid::Uuid::new_v4().simple())).unwrap();
        let (referrer, invited) = (address(), address());
        referral
            .create(&referrer, &invited, "192.0.2.1")
            .await
            .unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..12 {
            let referral = referral.clone();
            let invited = invited.clone();
            tasks.spawn(async move { referral.finalize_referral(invited.as_str()).await });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE address=$1 AND type='referral_invited_users_accepted'").bind(referrer).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn the_accepted_notice_icon_follows_the_configured_cdn() {
        let pool = sqlx::PgPool::connect_lazy("postgres://nobody@127.0.0.1:1/nowhere").unwrap();
        let settings = Settings {
            cdn_url: "https://cdn.example.org/".into(),
            ..Settings::for_tests()
        };
        let referral = Referral::new(pool, Outbound::for_tests(), settings);
        let referrer = "0x00000000000000000000000000000000000000a1";
        let invited = "0x00000000000000000000000000000000000000b1";
        let notice = referral.notification(referrer, invited, 1, None).unwrap();
        assert_eq!(
            notice.metadata["image"],
            "https://cdn.example.org/referral/referral-invited-user-accepted-icon.png"
        );
    }

    #[test]
    fn a_same_referrer_repeat_is_idempotent_and_a_different_one_conflicts() {
        let referrer = Address::parse("0x00000000000000000000000000000000000000a1").unwrap();
        let other = Address::parse("0x00000000000000000000000000000000000000a2").unwrap();
        let invited = Address::parse("0x00000000000000000000000000000000000000b1").unwrap();
        assert_eq!(
            resolve_existing(referrer.as_str(), &referrer, &invited).unwrap(),
            StatusCode::NO_CONTENT
        );
        let err = resolve_existing(other.as_str(), &referrer, &invited).unwrap_err();
        assert!(matches!(err, CommError::BadRequest(m) if m.contains("already exists")));
    }

    #[test]
    fn referral_emails_are_checked_in_upstreams_order() {
        let message = |raw: Option<&str>| match validated_email(raw) {
            Ok(email) => email,
            Err(CommError::BadRequest(m)) => m,
            Err(other) => panic!("{other:?}"),
        };
        assert_eq!(message(None), "Email is required");
        assert_eq!(message(Some("   ")), "Email is required");
        assert_eq!(
            message(Some(&format!("{}@example.com", "a".repeat(250)))),
            "Email is too long"
        );
        assert_eq!(
            message(Some("<b>@example.com")),
            "Email contains invalid characters"
        );
        assert_eq!(
            message(Some("javascript@example.com")),
            "Email contains invalid characters"
        );
        assert_eq!(message(Some("not-an-email")), "Invalid email format");
        assert_eq!(message(Some("a@-bad.com")), "a@-bad.com");
        assert_eq!(message(Some("a+b@example.com")), "Invalid email format");
        assert_eq!(
            message(Some(" Jane.Doe@Example.com ")),
            "jane.doe@example.com"
        );
    }
}

#[cfg(test)]
#[path = "referral_review_tests.rs"]
mod review_tests;
