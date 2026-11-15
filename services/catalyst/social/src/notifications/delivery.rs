//! The outbox worker for the out-of-process channels (`email` today):
//! DATABASE.md's "poll indexed, bounded due sets ... short atomic claim with
//! lease/recovery semantics; do network work outside the transaction;
//! preserve idempotency on redelivery", over `social.notification_deliveries`
//! The old crate had no out-of-process channel (rows were in-app only), so this
//! is new behaviour with an explicit degraded mode: without the SendGrid key,
//! sender or template, or without `SIGNING_KEY`, rows stay `pending`, unclaimed
//! and with their retry budget intact, until they are set. Pending rows idle past
//! the configured max age end `skipped` (`expired`); with expiry disabled a held
//! row still ends after [`DEFAULT_MAX_AGE`]. Opt-outs are checked again at claim time, so
//! an unsubscribe stops queued rows. Terminal rows are pruned after [`RETENTION`].

use std::sync::LazyLock;
use std::time::Duration;

use chrono::NaiveDateTime;
use serde_json::Value as Json;
use sqlx::PgPool;
use uuid::Uuid;

use super::email::{EmailSender, SendFailure};
use super::first_wear::creator_profile_name;
use super::store::{community_opt_out, details_flag, uuid_or_null};
use super::templates;
use crate::shared::now_utc;

pub const BATCH: i64 = 100;
pub const MAX_ATTEMPTS: i32 = 5;
pub const LEASE: Duration = Duration::from_secs(300);
const BACKOFF_BASE: Duration = Duration::from_secs(30);
const BACKOFF_MAX: Duration = Duration::from_secs(3600);
/// A row the channel could not sign is due again after this, attempt uncounted.
const HOLD: Duration = Duration::from_secs(300);
pub const RETENTION: Duration = Duration::from_secs(30 * 86_400);
pub const PRUNE_BATCH: i64 = 1000;
/// The `last_error` of a pending row idle past the max age.
pub const EXPIRED: &str = "expired";
/// `NOTIFICATION_DELIVERY_MAX_AGE_SECS`'s default: 7 days.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(7 * 86_400);
/// The smallest non-zero max age; shorter values would drop mail before a pass sends it.
pub const MIN_MAX_AGE: Duration = Duration::from_secs(3600);
/// The `last_error` of a row held for a missing `SIGNING_KEY`.
const UNSIGNED: &str = "SIGNING_KEY unset";
/// The `last_error` of a row held for a missing SendGrid key, sender or template.
const UNCONFIGURED: &str = "email delivery not configured";

/// `NOTIFICATION_DELIVERY_MAX_AGE_SECS` as the worker uses it: `0` disables
/// expiry (`None`), any other value is floored to [`MIN_MAX_AGE`].
pub fn max_age_from_secs(secs: u64) -> Option<Duration> {
    if secs == 0 {
        return None;
    }
    let age = Duration::from_secs(secs);
    if age < MIN_MAX_AGE {
        tracing::warn!(
            configured_secs = secs,
            floor_secs = MIN_MAX_AGE.as_secs(),
            "NOTIFICATION_DELIVERY_MAX_AGE_SECS below the floor: clamped up"
        );
        return Some(MIN_MAX_AGE);
    }
    Some(age)
}

/// A claimed `email` row with what the send needs, read in the claim.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub notification_id: Uuid,
    pub attempts: i32,
    /// The notification's insert time: the age reference of a held row.
    pub created_at: NaiveDateTime,
    pub address: String,
    pub kind: String,
    pub metadata: Json,
    pub email: Option<String>,
    /// Why the recipient no longer takes this email, read in the claim.
    pub opted_out: Option<String>,
}

/// What a channel does with a claimed row; the SendGrid sender is the
/// production one, tests plug a recorder in.
#[async_trait::async_trait]
pub trait EmailChannel: Send + Sync {
    async fn send(&self, to: &str, claimed: &Claimed) -> Result<(), SendFailure>;
    /// Whether unsubscribe links can be signed; `false` leaves rows unclaimed.
    fn can_sign(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl EmailChannel for EmailSender {
    async fn send(&self, to: &str, claimed: &Claimed) -> Result<(), SendFailure> {
        self.send_notification(
            to,
            &claimed.address,
            claimed.notification_id,
            &claimed.kind,
            &claimed.metadata,
        )
        .await
    }

    fn can_sign(&self) -> bool {
        self.has_signing_key()
    }
}

/// The wait after the `attempts`-th failure: 30 s, 60 s, 120 s ... capped at an hour.
pub fn backoff(attempts: i32) -> Duration {
    let factor = 1u32 << attempts.clamp(1, 16).saturating_sub(1) as u32;
    BACKOFF_BASE.saturating_mul(factor).min(BACKOFF_MAX)
}

/// Claims the due rows (`state = 'pending' AND next_attempt_at <= now`, the
/// `notification_deliveries_due_idx` order, `SKIP LOCKED`) and, in the same
/// statement, counts the attempt and moves `next_attempt_at` to `now + LEASE`
/// so a crash before the outcome lands makes the row due again later.
pub async fn claim(
    pool: &PgPool,
    now: NaiveDateTime,
    limit: i64,
) -> Result<Vec<Claimed>, sqlx::Error> {
    let lease_until = now + chrono::Duration::from_std(LEASE).expect("LEASE fits");
    type Row = (
        Uuid,
        i32,
        NaiveDateTime,
        String,
        String,
        Json,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(CLAIM_SQL.as_str()))
        .bind(now)
        .bind(limit)
        .bind(lease_until)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(notification_id, attempts, created_at, address, kind, metadata, email, opted_out)| {
                Claimed {
                    notification_id,
                    attempts,
                    created_at,
                    address,
                    kind,
                    metadata,
                    email,
                    opted_out,
                }
            },
        )
        .collect())
}

static CLAIM_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
        WITH due AS (
            SELECT d.notification_id
            FROM social.notification_deliveries d
            WHERE d.state = 'pending' AND d.channel = 'email' AND d.next_attempt_at <= $1
            ORDER BY d.next_attempt_at, d.notification_id
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        UPDATE social.notification_deliveries d
        SET attempts = d.attempts + 1, next_attempt_at = $3, updated_at = $1
        FROM due
        JOIN social.notifications n ON n.id = due.notification_id
        LEFT JOIN social.subscriptions s ON s.address = n.address
        WHERE d.notification_id = due.notification_id AND d.channel = 'email'
        RETURNING d.notification_id, d.attempts, n.created_at, n.address, n.type, n.metadata, s.email,
            CASE
                WHEN {} THEN 'ignore_all_email'
                WHEN NOT {} THEN 'opted out of this type'
                WHEN EXISTS ({}) THEN 'opted out of this community'
            END
        "#,
        details_flag("->'ignore_all_email'", false),
        details_flag("->'message_type'->n.type->'email'", true),
        community_opt_out("n.address", &uuid_or_null("n.metadata->>'communityId'")),
    )
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    Skipped(String),
    Failed(String),
    Retry {
        after: Duration,
        error: String,
    },
    /// Back to `pending` after [`HOLD`] without counting the claim's attempt.
    Held(&'static str),
}

/// The state transition for one claimed row's send result.
pub fn outcome_of(claimed: &Claimed, result: Result<(), SendFailure>) -> Outcome {
    match result {
        Ok(()) => Outcome::Sent,
        Err(SendFailure::NotConfigured) => Outcome::Held(UNCONFIGURED),
        Err(SendFailure::NoSigningKey) => Outcome::Held(UNSIGNED),
        Err(SendFailure::Permanent(e)) => Outcome::Failed(e),
        Err(SendFailure::Transient(e)) if claimed.attempts >= MAX_ATTEMPTS => {
            Outcome::Failed(format!("{e} (after {} attempts)", claimed.attempts))
        }
        Err(SendFailure::Transient(e)) => Outcome::Retry {
            after: backoff(claimed.attempts),
            error: e,
        },
    }
}

pub async fn record(
    pool: &PgPool,
    id: Uuid,
    now: NaiveDateTime,
    outcome: &Outcome,
) -> Result<(), sqlx::Error> {
    let after = |d: Duration| now + chrono::Duration::from_std(d).expect("backoff fits");
    let (state, next_attempt_at, last_error, uncount): (&str, NaiveDateTime, Option<&str>, i32) =
        match outcome {
            Outcome::Sent => ("sent", now, None, 0),
            Outcome::Skipped(reason) => ("skipped", now, Some(reason), 0),
            Outcome::Failed(error) => ("failed", now, Some(error), 0),
            Outcome::Retry { after: d, error } => ("pending", after(*d), Some(error), 0),
            Outcome::Held(reason) => ("pending", after(HOLD), Some(*reason), 1),
        };
    sqlx::query(
        r#"
        UPDATE social.notification_deliveries
        SET state = $2, next_attempt_at = $3, last_error = $4, updated_at = $5,
            attempts = GREATEST(attempts - $6, 0)
        WHERE notification_id = $1 AND channel = 'email'
        "#,
    )
    .bind(id)
    .bind(state)
    .bind(next_attempt_at)
    .bind(last_error)
    .bind(now)
    .bind(uncount)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassSummary {
    pub claimed: usize,
    pub sent: usize,
    pub skipped: usize,
    pub failed: usize,
    pub retried: usize,
    /// Due rows left unclaimed for missing SendGrid settings or `SIGNING_KEY` (at most [`BATCH`]).
    pub held: usize,
    /// Held rows ended `expired` (also counted in `skipped`).
    pub expired: usize,
}

/// The old processor's render-time names, never persisted: `userName` is the
/// recipient's profile name and a tip's `senderUsername` the sender's, else
/// `Unknown`.
async fn enrich_names(pool: &PgPool, row: &mut Claimed) -> anyhow::Result<()> {
    let name = |n: Option<String>| Json::String(n.unwrap_or_else(|| "Unknown".into()));
    let user = creator_profile_name(pool, &row.address.to_lowercase()).await?;
    let sender = match row.metadata.get("senderAddress").and_then(Json::as_str) {
        Some(s) if row.kind == "tip_received" => {
            Some(creator_profile_name(pool, &s.to_lowercase()).await?)
        }
        _ => None,
    };
    if let Some(m) = row.metadata.as_object_mut() {
        m.insert("userName".into(), name(user));
        if let Some(sender) = sender {
            m.insert("senderUsername".into(), name(sender));
        }
    }
    Ok(())
}

/// Deletes up to [`PRUNE_BATCH`] terminal rows last touched before
/// `now - RETENTION`, oldest first over `notification_deliveries_terminal_idx`.
pub async fn prune(pool: &PgPool, now: NaiveDateTime) -> Result<u64, sqlx::Error> {
    let cutoff = now - chrono::Duration::from_std(RETENTION).expect("RETENTION fits");
    let res = sqlx::query(
        r#"
        DELETE FROM social.notification_deliveries d
        USING (
            SELECT notification_id, channel FROM social.notification_deliveries
            WHERE state <> 'pending' AND updated_at < $1
            ORDER BY updated_at
            LIMIT $2
        ) old
        WHERE d.notification_id = old.notification_id AND d.channel = old.channel
          AND d.state <> 'pending'
        "#,
    )
    .bind(cutoff)
    .bind(PRUNE_BATCH)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Ends up to [`PRUNE_BATCH`] pending rows not due-touched since
/// `now - max_age` as `skipped` (`expired`), over `notification_deliveries_due_idx`:
/// a row left unclaimed for a missing key keeps its insert time as
/// `next_attempt_at`, and a retried row reaches a terminal state within hours.
/// With expiry disabled (`None`) only a hold is bounded: when nothing can be sent
/// (`deliverable` false), due rows whose notification is older than
/// [`DEFAULT_MAX_AGE`] end `expired`.
pub async fn expire(
    pool: &PgPool,
    now: NaiveDateTime,
    max_age: Option<Duration>,
    deliverable: bool,
) -> Result<u64, sqlx::Error> {
    let cutoff = |age: Duration| {
        chrono::Duration::from_std(age)
            .ok()
            .and_then(|age| now.checked_sub_signed(age))
            .unwrap_or(NaiveDateTime::MIN)
    };
    let (cutoff, sql) = match max_age {
        Some(age) => (
            cutoff(age),
            r#"
            UPDATE social.notification_deliveries d
            SET state = 'skipped', last_error = $3, updated_at = $4
            FROM (
                SELECT notification_id, channel FROM social.notification_deliveries
                WHERE state = 'pending' AND next_attempt_at < $1
                ORDER BY next_attempt_at, notification_id
                LIMIT $2
                FOR UPDATE SKIP LOCKED
            ) old
            WHERE d.notification_id = old.notification_id AND d.channel = old.channel
            "#,
        ),
        None if !deliverable => (
            cutoff(DEFAULT_MAX_AGE),
            r#"
            UPDATE social.notification_deliveries d
            SET state = 'skipped', last_error = $3, updated_at = $4
            FROM (
                SELECT pd.notification_id, pd.channel
                FROM social.notification_deliveries pd
                JOIN social.notifications n ON n.id = pd.notification_id
                WHERE pd.state = 'pending' AND pd.next_attempt_at <= $4 AND n.created_at < $1
                ORDER BY pd.next_attempt_at, pd.notification_id
                LIMIT $2
                FOR UPDATE OF pd SKIP LOCKED
            ) old
            WHERE d.notification_id = old.notification_id AND d.channel = old.channel
            "#,
        ),
        None => return Ok(0),
    };
    let res = sqlx::query(sql)
        .bind(cutoff)
        .bind(PRUNE_BATCH)
        .bind(EXPIRED)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// One pass: claim, send each outside any transaction, record; the caller logs `held`. A row whose
/// recipient lost their email between insert and send, or whose type has no
/// template (rows written before the allowlist), is `skipped` without a send.
/// A row held at send time past `max_age` (or [`DEFAULT_MAX_AGE`] with expiry
/// disabled) since its notification was created ends `expired`.
pub async fn run_pass(
    pool: &PgPool,
    channel: &dyn EmailChannel,
    configured: bool,
    max_age: Option<Duration>,
) -> anyhow::Result<PassSummary> {
    let now = now_utc();
    if !configured || !channel.can_sign() {
        let held: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM (
                SELECT 1 FROM social.notification_deliveries
                WHERE state = 'pending' AND channel = 'email' AND next_attempt_at <= $1
                LIMIT $2
            ) due
            "#,
        )
        .bind(now)
        .bind(BATCH)
        .fetch_one(pool)
        .await?;
        return Ok(PassSummary {
            held: held as usize,
            ..Default::default()
        });
    }
    let claimed = claim(pool, now, BATCH).await?;
    let mut summary = PassSummary {
        claimed: claimed.len(),
        ..Default::default()
    };
    for row in &claimed {
        let outcome = match &row.email {
            None => Outcome::Skipped("no confirmed email at send time".to_string()),
            _ if !templates::is_emailable(&row.kind) => {
                Outcome::Skipped("no email template".to_string())
            }
            _ if row.opted_out.is_some() => {
                Outcome::Skipped(row.opted_out.clone().unwrap_or_default())
            }
            Some(to) => {
                let mut row = row.clone();
                let result = match enrich_names(pool, &mut row).await {
                    Ok(()) => channel.send(to, &row).await,
                    Err(e) => Err(SendFailure::Transient(format!(
                        "profile lookup failed: {e}"
                    ))),
                };
                outcome_of(&row, result)
            }
        };
        // `EmailSender` never gets here: the gate above reads the same settings,
        // fixed at construction, and returns first.
        let outcome = match outcome {
            Outcome::Held(_) if held_too_long(row, now, max_age) => {
                summary.expired += 1;
                Outcome::Skipped(EXPIRED.to_string())
            }
            other => other,
        };
        match &outcome {
            Outcome::Sent => summary.sent += 1,
            Outcome::Skipped(_) => summary.skipped += 1,
            Outcome::Failed(_) => summary.failed += 1,
            Outcome::Retry { .. } => summary.retried += 1,
            Outcome::Held(_) => summary.held += 1,
        }
        record(pool, row.notification_id, now_utc(), &outcome).await?;
    }
    if summary.expired > 0 {
        tracing::warn!(
            expired = summary.expired,
            "notification emails held past the max age expired unsent"
        );
    }
    Ok(summary)
}

/// Bounds `Outcome::Held`, whose attempt is uncounted: the age runs from the
/// notification's insert, which a hold never moves.
fn held_too_long(row: &Claimed, now: NaiveDateTime, max_age: Option<Duration>) -> bool {
    let age = chrono::Duration::from_std(max_age.unwrap_or(DEFAULT_MAX_AGE))
        .unwrap_or(chrono::Duration::MAX);
    now.signed_duration_since(row.created_at) >= age
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_30s_to_the_hour_cap() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(3), Duration::from_secs(120));
        assert_eq!(backoff(7), Duration::from_secs(1920));
        assert_eq!(backoff(8), Duration::from_secs(3600));
        assert_eq!(backoff(40), Duration::from_secs(3600));
        assert_eq!(backoff(0), Duration::from_secs(30));
    }

    fn claimed(attempts: i32) -> Claimed {
        Claimed {
            notification_id: Uuid::nil(),
            attempts,
            created_at: now_utc(),
            address: "0xa".into(),
            kind: "bid_received".into(),
            metadata: serde_json::json!({}),
            email: Some("a@example.com".into()),
            opted_out: None,
        }
    }

    #[test]
    fn outcome_table() {
        assert_eq!(outcome_of(&claimed(1), Ok(())), Outcome::Sent);
        assert_eq!(
            outcome_of(&claimed(1), Err(SendFailure::NotConfigured)),
            Outcome::Held(UNCONFIGURED)
        );
        assert_eq!(
            outcome_of(&claimed(1), Err(SendFailure::Permanent("400".into()))),
            Outcome::Failed("400".into())
        );
        assert_eq!(
            outcome_of(&claimed(2), Err(SendFailure::Transient("503".into()))),
            Outcome::Retry {
                after: Duration::from_secs(60),
                error: "503".into()
            }
        );
        assert!(matches!(
            outcome_of(
                &claimed(MAX_ATTEMPTS),
                Err(SendFailure::Transient("503".into()))
            ),
            Outcome::Failed(_)
        ));
        assert_eq!(
            outcome_of(&claimed(MAX_ATTEMPTS + 3), Err(SendFailure::NoSigningKey)),
            Outcome::Held(UNSIGNED),
            "a missing signing key never ends the row"
        );
    }

    #[test]
    fn max_age_zero_disables_and_small_values_are_floored() {
        assert_eq!(max_age_from_secs(0), None);
        assert_eq!(max_age_from_secs(1), Some(MIN_MAX_AGE));
        assert_eq!(max_age_from_secs(600), Some(MIN_MAX_AGE));
        assert_eq!(max_age_from_secs(3600), Some(MIN_MAX_AGE));
        assert_eq!(max_age_from_secs(604_800), Some(DEFAULT_MAX_AGE));
    }
}

#[cfg(test)]
mod pg_tests {
    use super::*;
    use crate::notifications::store::test_support::*;
    use crate::notifications::store::{NewNotification, insert_notifications};
    use parking_lot::Mutex;

    struct Recorder {
        sent: Mutex<Vec<(String, Uuid)>>,
        metadata: Mutex<Vec<(Uuid, Json)>>,
        fail_with: Mutex<Option<SendFailure>>,
        unsigned: std::sync::atomic::AtomicBool,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                metadata: Mutex::new(Vec::new()),
                fail_with: Mutex::new(None),
                unsigned: Default::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl EmailChannel for Recorder {
        async fn send(&self, to: &str, claimed: &Claimed) -> Result<(), SendFailure> {
            if let Some(f) = self.fail_with.lock().clone() {
                return Err(f);
            }
            self.sent
                .lock()
                .push((to.to_string(), claimed.notification_id));
            self.metadata
                .lock()
                .push((claimed.notification_id, claimed.metadata.clone()));
            Ok(())
        }

        fn can_sign(&self) -> bool {
            !self.unsigned.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    async fn make_due(pool: &PgPool, id: Uuid) {
        sqlx::query("UPDATE social.notification_deliveries SET next_attempt_at = $2 WHERE notification_id = $1")
            .bind(id)
            .bind(now_utc() - chrono::Duration::seconds(1))
            .execute(pool)
            .await
            .unwrap();
    }

    async fn next_attempt(pool: &PgPool, id: Uuid) -> NaiveDateTime {
        sqlx::query_scalar(
            "SELECT next_attempt_at FROM social.notification_deliveries WHERE notification_id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn pending_for(
        pool: &PgPool,
        address: &crate::identity::Address,
    ) -> Vec<NewNotification> {
        notify(
            pool,
            address,
            "bid_received",
            serde_json::json!({"title": "t"}),
        )
        .await
    }

    async fn notify(
        pool: &PgPool,
        address: &crate::identity::Address,
        kind: &str,
        metadata: Json,
    ) -> Vec<NewNotification> {
        sqlx::query("INSERT INTO social.subscriptions (address, email) VALUES ($1, $2)")
            .bind(address)
            .bind(format!("{}@example.com", &address.as_str()[2..10]))
            .execute(pool)
            .await
            .unwrap();
        let batch = vec![NewNotification::now(address.clone(), kind, metadata)];
        assert_eq!(insert_notifications(pool, &batch).await.unwrap(), 1);
        batch
    }

    #[tokio::test]
    async fn a_pass_sends_pending_rows_once_and_records_the_outcome() {
        let Some(pool) = private_pool("dlv").await else {
            return;
        };
        let a = fresh_address();
        let batch = pending_for(&pool, &a).await;
        let id = batch[0].id;
        let recorder = Recorder::new();

        let summary = run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(
            summary,
            PassSummary {
                claimed: 1,
                sent: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            *recorder.sent.lock(),
            vec![(format!("{}@example.com", &a.as_str()[2..10]), id)]
        );
        assert_eq!(
            delivery_state(&pool, id).await.unwrap(),
            ("sent".into(), 1, None)
        );

        let summary = run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(
            summary,
            PassSummary::default(),
            "a sent row is never claimed again"
        );
        assert_eq!(recorder.sent.lock().len(), 1);
    }

    #[tokio::test]
    async fn unconfigured_email_holds_rows_and_a_missing_recipient_email_skips_without_a_send() {
        let Some(pool) = private_pool("dlv").await else {
            return;
        };
        let a = pending_for(&pool, &fresh_address()).await;
        let recorder = Recorder::new();
        let summary = run_pass(&pool, &recorder, false, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!((summary.claimed, summary.held), (0, 1), "{summary:?}");
        assert_eq!(
            delivery_state(&pool, a[0].id).await.unwrap(),
            ("pending".into(), 0, None),
            "held unclaimed, retry budget intact"
        );
        assert!(recorder.sent.lock().is_empty());
        assert_eq!(expire(&pool, now_utc(), None, false).await.unwrap(), 0);
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(delivery_state(&pool, a[0].id).await.unwrap().0, "sent");

        let b_addr = fresh_address();
        let b = pending_for(&pool, &b_addr).await;
        sqlx::query("UPDATE social.subscriptions SET email = NULL WHERE address = $1")
            .bind(&b_addr)
            .execute(&pool)
            .await
            .unwrap();
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(
            delivery_state(&pool, b[0].id).await.unwrap(),
            (
                "skipped".into(),
                1,
                Some("no confirmed email at send time".into())
            )
        );
        assert!(recorder.sent.lock().iter().all(|(_, n)| *n != b[0].id));
    }

    #[tokio::test]
    async fn transient_failures_back_off_then_fail_and_a_lapsed_lease_is_reclaimed() {
        let Some(pool) = private_pool("dlv").await else {
            return;
        };
        let batch = pending_for(&pool, &fresh_address()).await;
        let id = batch[0].id;
        let recorder = Recorder::new();
        *recorder.fail_with.lock() = Some(SendFailure::Transient("503".into()));

        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        let (state, attempts, err) = delivery_state(&pool, id).await.unwrap();
        assert_eq!(
            (state.as_str(), attempts, err.as_deref()),
            ("pending", 1, Some("503"))
        );
        let next: NaiveDateTime = sqlx::query_scalar(
            "SELECT next_attempt_at FROM social.notification_deliveries WHERE notification_id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            next > now_utc() + chrono::Duration::seconds(20),
            "backed off 30 s, not due now"
        );

        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(
            delivery_state(&pool, id).await.unwrap().1,
            1,
            "a row in backoff is not reclaimed"
        );

        // A claim whose outcome never landed (crash inside the lease): due again once the lease lapses.
        sqlx::query("UPDATE social.notification_deliveries SET next_attempt_at = $2 WHERE notification_id = $1")
            .bind(id)
            .bind(now_utc() - chrono::Duration::seconds(1))
            .execute(&pool)
            .await
            .unwrap();
        let claimed = claim(&pool, now_utc(), BATCH).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!((claimed[0].notification_id, claimed[0].attempts), (id, 2));
        let leased: NaiveDateTime = sqlx::query_scalar(
            "SELECT next_attempt_at FROM social.notification_deliveries WHERE notification_id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            leased > now_utc() + chrono::Duration::seconds(200),
            "the claim leases the row"
        );
        assert!(
            claim(&pool, now_utc(), BATCH).await.unwrap().is_empty(),
            "not claimable inside the lease"
        );

        for _ in 0..MAX_ATTEMPTS {
            sqlx::query("UPDATE social.notification_deliveries SET next_attempt_at = $2 WHERE notification_id = $1 AND state = 'pending'")
                .bind(id)
                .bind(now_utc() - chrono::Duration::seconds(1))
                .execute(&pool)
                .await
                .unwrap();
            run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
                .await
                .unwrap();
        }
        let (state, attempts, err) = delivery_state(&pool, id).await.unwrap();
        assert_eq!(state, "failed");
        assert!(attempts >= MAX_ATTEMPTS, "{attempts}");
        assert!(
            err.unwrap().contains("after"),
            "the terminal error names the attempt count"
        );
        assert!(recorder.sent.lock().is_empty());

        // A permanent refusal is terminal at once.
        let other = pending_for(&pool, &fresh_address()).await;
        *recorder.fail_with.lock() = Some(SendFailure::Permanent("400 bad template".into()));
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(
            delivery_state(&pool, other[0].id).await.unwrap(),
            ("failed".into(), 1, Some("400 bad template".into()))
        );
    }

    async fn deploy_profile(pool: &PgPool, address: &str, name: &str) {
        let entity = format!("bafy-{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain)
             VALUES ($1, 'v3', 'profile', $2, $3::json, $4, ARRAY[$1::text], $4, '[]'::json)",
        )
        .bind(address)
        .bind(&entity)
        .bind(serde_json::json!({"v": {"avatars": [{"name": name}]}}))
        .bind(now_utc())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'profile') ON CONFLICT (pointer) DO UPDATE SET entity_id = EXCLUDED.entity_id")
            .bind(address)
            .bind(&entity)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_type_without_a_template_gets_a_skipped_row_and_is_never_sent() {
        let Some(pool) = private_pool("dlv-tpl").await else {
            return;
        };
        let a = fresh_address();
        let rows = notify(&pool, &a, "land_transfer_received", serde_json::json!({})).await;
        assert_eq!(
            delivery_state(&pool, rows[0].id).await.unwrap(),
            ("skipped".into(), 0, Some("no email template".into()))
        );
        // A row written before the allowlist is skipped at send time.
        sqlx::query("UPDATE social.notification_deliveries SET state = 'pending', last_error = NULL WHERE notification_id = $1")
            .bind(rows[0].id)
            .execute(&pool)
            .await
            .unwrap();
        let recorder = Recorder::new();
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(
            delivery_state(&pool, rows[0].id).await.unwrap(),
            ("skipped".into(), 1, Some("no email template".into()))
        );
        assert!(recorder.sent.lock().iter().all(|(_, n)| *n != rows[0].id));
    }

    #[tokio::test]
    async fn names_are_enriched_at_send_time_and_never_persisted() {
        let Some(pool) = private_pool("dlv-names").await else {
            return;
        };
        let named = fresh_address();
        let sender = fresh_address();
        deploy_profile(&pool, named.as_str(), "Recipient").await;
        deploy_profile(&pool, sender.as_str(), "Tipper").await;
        let tip = notify(
            &pool,
            &named,
            "tip_received",
            serde_json::json!({"senderAddress": sender.as_str().to_uppercase().replacen("0X", "0x", 1)}),
        )
        .await;
        let anon = pending_for(&pool, &fresh_address()).await;
        let recorder = Recorder::new();
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        let seen: std::collections::HashMap<Uuid, Json> =
            recorder.metadata.lock().iter().cloned().collect();
        assert_eq!(seen[&tip[0].id]["userName"], "Recipient");
        assert_eq!(seen[&tip[0].id]["senderUsername"], "Tipper");
        assert_eq!(seen[&anon[0].id]["userName"], "Unknown");
        assert!(seen[&anon[0].id].get("senderUsername").is_none());
        let stored: Json =
            sqlx::query_scalar("SELECT metadata FROM social.notifications WHERE id = $1")
                .bind(tip[0].id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            stored.get("userName").is_none(),
            "enrichment is not persisted"
        );
    }

    #[tokio::test]
    async fn prune_deletes_only_terminal_rows_past_retention() {
        let Some(pool) = private_pool("dlv-prune").await else {
            return;
        };
        let old = now_utc() - chrono::Duration::days(31);
        let mut ids = Vec::new();
        for state in ["sent", "skipped", "failed", "pending"] {
            let rows = pending_for(&pool, &fresh_address()).await;
            sqlx::query("UPDATE social.notification_deliveries SET state = $2, updated_at = $3 WHERE notification_id = $1")
                .bind(rows[0].id)
                .bind(state)
                .bind(old)
                .execute(&pool)
                .await
                .unwrap();
            ids.push(rows[0].id);
        }
        let recent = pending_for(&pool, &fresh_address()).await;
        sqlx::query(
            "UPDATE social.notification_deliveries SET state = 'sent' WHERE notification_id = $1",
        )
        .bind(recent[0].id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(prune(&pool, now_utc()).await.unwrap(), 3);
        for id in &ids[..3] {
            assert_eq!(delivery_state(&pool, *id).await, None);
        }
        assert_eq!(delivery_state(&pool, ids[3]).await.unwrap().0, "pending");
        assert_eq!(delivery_state(&pool, recent[0].id).await.unwrap().0, "sent");
        assert_eq!(prune(&pool, now_utc()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_claim_rechecks_opt_outs_and_skips_rows_queued_before_them() {
        let Some(pool) = private_pool("dlv-optout").await else {
            return;
        };
        let all = fresh_address();
        let typed = fresh_address();
        let member = fresh_address();
        let kept = fresh_address();
        let community = Uuid::new_v4();
        let rows_all = pending_for(&pool, &all).await;
        let rows_typed = pending_for(&pool, &typed).await;
        let rows_member = notify(
            &pool,
            &member,
            "events_started",
            serde_json::json!({"communityId": community.to_string().to_uppercase()}),
        )
        .await;
        let rows_kept = pending_for(&pool, &kept).await;
        for id in [
            rows_all[0].id,
            rows_typed[0].id,
            rows_member[0].id,
            rows_kept[0].id,
        ] {
            assert_eq!(delivery_state(&pool, id).await.unwrap().0, "pending");
        }
        let store = crate::notifications::NotificationsStore::new(pool.clone());
        store.unsubscribe_email(&all, None).await.unwrap();
        store
            .unsubscribe_email(&typed, Some("bid_received"))
            .await
            .unwrap();
        sqlx::query("INSERT INTO social.subscription_opt_outs (address, scope, scope_id) VALUES ($1, 'community', $2)")
            .bind(&member)
            .bind(community.to_string())
            .execute(&pool)
            .await
            .unwrap();

        let recorder = Recorder::new();
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        for (id, reason) in [
            (rows_all[0].id, "ignore_all_email"),
            (rows_typed[0].id, "opted out of this type"),
            (rows_member[0].id, "opted out of this community"),
        ] {
            assert_eq!(
                delivery_state(&pool, id).await.unwrap(),
                ("skipped".into(), 1, Some(reason.into()))
            );
        }
        let sent: Vec<Uuid> = recorder.sent.lock().iter().map(|(_, id)| *id).collect();
        assert_eq!(
            sent,
            vec![rows_kept[0].id],
            "only the row still wanted is sent"
        );
    }

    #[tokio::test]
    async fn without_a_signing_key_rows_stay_unclaimed_and_keep_their_retry_budget() {
        let Some(pool) = private_pool("dlv-unsigned").await else {
            return;
        };
        let a = fresh_address();
        deploy_profile(&pool, a.as_str(), "Held").await;
        let rows = pending_for(&pool, &a).await;
        let id = rows[0].id;
        let recorder = Recorder::new();
        recorder
            .unsigned
            .store(true, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..MAX_ATTEMPTS + 2 {
            make_due(&pool, id).await;
            let summary = run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
                .await
                .unwrap();
            assert_eq!((summary.claimed, summary.held), (0, 1), "{summary:?}");
        }
        assert_eq!(
            delivery_state(&pool, id).await.unwrap(),
            ("pending".into(), 0, None),
            "holds neither claim nor count an attempt"
        );
        assert!(
            recorder.metadata.lock().is_empty(),
            "nothing enriched or sent"
        );

        // The key appears: the first transient failure retries with the first backoff.
        recorder
            .unsigned
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *recorder.fail_with.lock() = Some(SendFailure::Transient("503".into()));
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(
            delivery_state(&pool, id).await.unwrap(),
            ("pending".into(), 1, Some("503".into()))
        );
        assert!(next_attempt(&pool, id).await < now_utc() + chrono::Duration::seconds(60));

        *recorder.fail_with.lock() = None;
        make_due(&pool, id).await;
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(delivery_state(&pool, id).await.unwrap().0, "sent");
    }

    #[tokio::test]
    async fn a_send_refused_for_a_missing_key_is_held_without_counting_the_attempt() {
        let Some(pool) = private_pool("dlv-held").await else {
            return;
        };
        let id = pending_for(&pool, &fresh_address()).await[0].id;
        let recorder = Recorder::new();
        *recorder.fail_with.lock() = Some(SendFailure::NoSigningKey);
        for _ in 0..MAX_ATTEMPTS + 2 {
            make_due(&pool, id).await;
            assert_eq!(
                run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
                    .await
                    .unwrap()
                    .held,
                1
            );
        }
        assert_eq!(
            delivery_state(&pool, id).await.unwrap(),
            ("pending".into(), 0, Some("SIGNING_KEY unset".into()))
        );
        let next = next_attempt(&pool, id).await;
        assert!(next > now_utc() + chrono::Duration::seconds(200));
        assert!(next <= now_utc() + chrono::Duration::from_std(HOLD).unwrap());
    }

    #[tokio::test]
    async fn a_row_held_at_send_time_expires_once_its_notification_is_past_the_max_age() {
        let Some(pool) = private_pool("dlv-held-cap").await else {
            return;
        };
        let old = pending_for(&pool, &fresh_address()).await[0].id;
        let young = pending_for(&pool, &fresh_address()).await[0].id;
        let set_created = |id: Uuid, at: NaiveDateTime| {
            let pool = pool.clone();
            async move {
                sqlx::query("UPDATE social.notifications SET created_at = $2 WHERE id = $1")
                    .bind(id)
                    .bind(at)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        set_created(old, now_utc() - chrono::Duration::days(8)).await;
        let recorder = Recorder::new();
        *recorder.fail_with.lock() = Some(SendFailure::NoSigningKey);
        let summary = run_pass(&pool, &recorder, true, Some(MIN_MAX_AGE))
            .await
            .unwrap();
        assert_eq!((summary.held, summary.expired), (1, 1), "{summary:?}");
        assert_eq!(
            delivery_state(&pool, old).await.unwrap(),
            ("skipped".into(), 1, Some(EXPIRED.into()))
        );

        // A hold moves `next_attempt_at` but never the age reference.
        for _ in 0..3 {
            make_due(&pool, young).await;
            run_pass(&pool, &recorder, true, Some(MIN_MAX_AGE))
                .await
                .unwrap();
        }
        assert_eq!(delivery_state(&pool, young).await.unwrap().0, "pending");
        set_created(young, now_utc() - chrono::Duration::hours(2)).await;
        make_due(&pool, young).await;
        let summary = run_pass(&pool, &recorder, true, Some(MIN_MAX_AGE))
            .await
            .unwrap();
        assert_eq!((summary.held, summary.expired), (0, 1), "{summary:?}");
        assert_eq!(
            delivery_state(&pool, young).await.unwrap(),
            ("skipped".into(), 1, Some(EXPIRED.into()))
        );

        // With expiry disabled a hold is still bounded by the default age.
        let stale = pending_for(&pool, &fresh_address()).await[0].id;
        let fresh = pending_for(&pool, &fresh_address()).await[0].id;
        set_created(stale, now_utc() - chrono::Duration::days(8)).await;
        let summary = run_pass(&pool, &recorder, true, None).await.unwrap();
        assert_eq!((summary.held, summary.expired), (1, 1), "{summary:?}");
        assert_eq!(delivery_state(&pool, stale).await.unwrap().0, "skipped");
        assert_eq!(delivery_state(&pool, fresh).await.unwrap().0, "pending");
        assert!(recorder.sent.lock().is_empty());
    }

    #[tokio::test]
    async fn expire_with_the_max_age_disabled_ends_only_holds_past_the_default_age() {
        let Some(pool) = private_pool("dlv-expire-off").await else {
            return;
        };
        let backdate = |sql: &'static str, id: Uuid, days: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query(sql)
                    .bind(id)
                    .bind(now_utc() - chrono::Duration::days(days))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        let idle = pending_for(&pool, &fresh_address()).await[0].id;
        backdate(
            "UPDATE social.notification_deliveries SET next_attempt_at = $2 WHERE notification_id = $1",
            idle,
            400,
        )
        .await;
        let old = pending_for(&pool, &fresh_address()).await[0].id;
        backdate(
            "UPDATE social.notifications SET created_at = $2 WHERE id = $1",
            old,
            8,
        )
        .await;

        // With the key set nothing expires.
        assert_eq!(expire(&pool, now_utc(), None, true).await.unwrap(), 0);
        assert_eq!(delivery_state(&pool, idle).await.unwrap().0, "pending");
        assert_eq!(delivery_state(&pool, old).await.unwrap().0, "pending");

        // Without it a hold is bounded by the notification's age, not by idleness.
        assert_eq!(expire(&pool, now_utc(), None, false).await.unwrap(), 1);
        assert_eq!(
            delivery_state(&pool, old).await.unwrap(),
            ("skipped".into(), 0, Some(EXPIRED.into()))
        );
        assert_eq!(delivery_state(&pool, idle).await.unwrap().0, "pending");

        // The floor keeps a just-inserted row from expiring before a pass sends it.
        let fresh = pending_for(&pool, &fresh_address()).await[0].id;
        assert_eq!(
            expire(&pool, now_utc(), max_age_from_secs(1), true)
                .await
                .unwrap(),
            1
        );
        assert_eq!(delivery_state(&pool, fresh).await.unwrap().0, "pending");
    }

    #[tokio::test]
    async fn expire_ends_only_pending_rows_idle_past_the_max_age() {
        let Some(pool) = private_pool("dlv-expire").await else {
            return;
        };
        let old = now_utc() - chrono::Duration::days(8);
        let stale = pending_for(&pool, &fresh_address()).await[0].id;
        let stale_sent = pending_for(&pool, &fresh_address()).await[0].id;
        let fresh = pending_for(&pool, &fresh_address()).await[0].id;
        for (id, state) in [(stale, "pending"), (stale_sent, "sent")] {
            sqlx::query("UPDATE social.notification_deliveries SET state = $2, next_attempt_at = $3, updated_at = $3 WHERE notification_id = $1")
                .bind(id)
                .bind(state)
                .bind(old)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(
            expire(&pool, now_utc(), Some(DEFAULT_MAX_AGE), false)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            delivery_state(&pool, stale).await.unwrap(),
            ("skipped".into(), 0, Some(EXPIRED.into()))
        );
        assert_eq!(delivery_state(&pool, stale_sent).await.unwrap().0, "sent");
        assert_eq!(delivery_state(&pool, fresh).await.unwrap().0, "pending");
        assert_eq!(
            expire(&pool, now_utc(), Some(DEFAULT_MAX_AGE), false)
                .await
                .unwrap(),
            0
        );

        // An expired row is never sent once the key is set.
        let recorder = Recorder::new();
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert!(recorder.sent.lock().iter().all(|(_, n)| *n != stale));
    }

    #[tokio::test]
    async fn a_non_boolean_flag_is_treated_as_unset() {
        let Some(pool) = private_pool("dlv-flags").await else {
            return;
        };
        let a = fresh_address();
        sqlx::query("INSERT INTO social.subscriptions (address, email, details) VALUES ($1, 'f@example.com', $2)")
            .bind(&a)
            .bind(serde_json::json!({
                "ignore_all_email": "maybe",
                "ignore_all_in_app": "nope",
                "message_type": {"bid_received": {"email": "false", "in_app": 1}}
            }))
            .execute(&pool)
            .await
            .unwrap();
        let batch = vec![NewNotification::now(
            a.clone(),
            "bid_received",
            serde_json::json!({"title": "t"}),
        )];
        assert_eq!(insert_notifications(&pool, &batch).await.unwrap(), 1);
        let in_app: bool =
            sqlx::query_scalar("SELECT in_app FROM social.notifications WHERE id = $1")
                .bind(batch[0].id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(in_app, "malformed in-app flags default to shown");
        assert_eq!(
            delivery_state(&pool, batch[0].id).await.unwrap().0,
            "pending"
        );

        let recorder = Recorder::new();
        run_pass(&pool, &recorder, true, Some(DEFAULT_MAX_AGE))
            .await
            .unwrap();
        assert_eq!(delivery_state(&pool, batch[0].id).await.unwrap().0, "sent");
    }

    /// Prune and claim racing on the same table: prune takes only terminal
    /// rows, a claimed (pending) row survives however old, and neither blocks.
    #[tokio::test]
    async fn prune_alongside_a_claim_never_deletes_a_pending_row() {
        let Some(pool) = private_pool("dlv-race").await else {
            return;
        };
        let old = now_utc() - chrono::Duration::days(40);
        let mut terminal = Vec::new();
        let mut pending = Vec::new();
        for i in 0..20 {
            let rows = pending_for(&pool, &fresh_address()).await;
            let state = if i % 2 == 0 { "sent" } else { "pending" };
            sqlx::query("UPDATE social.notification_deliveries SET state = $2, updated_at = $3, next_attempt_at = $3 WHERE notification_id = $1")
                .bind(rows[0].id)
                .bind(state)
                .bind(old)
                .execute(&pool)
                .await
                .unwrap();
            if state == "sent" {
                terminal.push(rows[0].id);
            } else {
                pending.push(rows[0].id);
            }
        }
        let now = now_utc();
        let (claimed, pruned) = tokio::join!(claim(&pool, now, BATCH), prune(&pool, now));
        let claimed: std::collections::HashSet<Uuid> = claimed
            .unwrap()
            .into_iter()
            .map(|c| c.notification_id)
            .collect();
        pruned.unwrap();
        for id in &pending {
            assert!(claimed.contains(id), "every due pending row is claimed");
            let (state, attempts, _) = delivery_state(&pool, *id).await.expect("never pruned");
            assert_eq!((state.as_str(), attempts), ("pending", 1));
        }
        assert!(terminal.iter().all(|id| !claimed.contains(id)));
        prune(&pool, now).await.unwrap();
        for id in &terminal {
            assert_eq!(delivery_state(&pool, *id).await, None);
        }
        for id in &pending {
            assert!(delivery_state(&pool, *id).await.is_some());
        }
    }
}
