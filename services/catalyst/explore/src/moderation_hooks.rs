//! The moderation outbox: the Slack notices the events writes
//! (`events::hooks`) and the places moderation (`places::moderation`)
//! capture in their own transaction, one table (`explore.moderation_hooks`)
//! whose `channel` names the webhook each row goes to, and one leased
//! worker (`moderation-hook`, every minute) that posts outside the
//! transaction: at least once (Slack has no idempotency contract), six
//! attempts with exponential backoff from thirty seconds, thirty days of
//! retention for completed and exhausted rows, deliverable retries never
//! removed. An events row stores the public labels and renders its message
//! at delivery; a places row stores the rendered message.
//!
//! The outbox is live from its first capture (`moderation_hook_activation`,
//! the database clock, written in the author's transaction): rows older than
//! that instant are never delivered, so an unconfigured node's history is
//! not replayed once a webhook is set.
//!
//! The table replaced `explore.event_moderation_hooks`; a node upgraded from
//! that layout keeps its pending rows there until a worker pass adopts them
//! ([`adopt_legacy_rows`], every pass while the table exists, so rows an
//! older replica writes during a rolling deploy move too), and a later
//! release drops the table.

use std::time::Duration;

use chrono::{NaiveDateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::events::hooks::Links;

const RETENTION_DAYS: i32 = 30;
pub(crate) const CLEANUP_LIMIT: i64 = 1000;

/// The webhook a notice goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// `SLACK_WEBHOOK`: the events moderation notices.
    Events,
    /// `PLACES_MODERATION_WEBHOOK`: the places moderation notices.
    Places,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Events => "events",
            Channel::Places => "places",
        }
    }
}

/// The configured webhooks by channel; a channel without one keeps its rows
/// queued (retention applies to completed and exhausted rows only).
#[derive(Clone, Default)]
pub struct Endpoints {
    pub events: Option<String>,
    pub places: Option<String>,
}

impl Endpoints {
    fn configured(&self) -> Vec<(Channel, &str)> {
        [
            (Channel::Events, self.events.as_deref()),
            (Channel::Places, self.places.as_deref()),
        ]
        .into_iter()
        .filter_map(|(channel, endpoint)| {
            endpoint
                .map(str::trim)
                .filter(|endpoint| !endpoint.is_empty())
                .map(|endpoint| (channel, endpoint))
        })
        .collect()
    }
}

/// The capture instant of a notice, floored at the activation cutoff: the
/// database clock for capture and activation alike, in the author's
/// transaction, so the first worker tick cannot discard queued work.
pub(crate) async fn capture_at(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<NaiveDateTime, sqlx::Error> {
    sqlx::query("INSERT INTO explore.moderation_hook_activation(activated_at) VALUES(clock_timestamp() AT TIME ZONE 'UTC') ON CONFLICT DO NOTHING")
        .execute(&mut **tx).await?;
    // A concurrent author may have won activation while this INSERT waited.
    sqlx::query_scalar("SELECT GREATEST(clock_timestamp() AT TIME ZONE 'UTC',activated_at) FROM explore.moderation_hook_activation WHERE singleton")
        .fetch_one(&mut **tx).await
}

/// Queues one notice in the author's transaction, due at once.
pub(crate) async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    channel: Channel,
    subject: &str,
    kind: &str,
    payload: &Value,
    creator_address: Option<&str>,
) -> Result<(), sqlx::Error> {
    let at = capture_at(tx).await?;
    sqlx::query(
        "INSERT INTO explore.moderation_hooks (id,channel,subject,kind,payload,creator_address,created_at,next_attempt_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$7)",
    )
    .bind(Uuid::new_v4())
    .bind(channel.as_str())
    .bind(subject)
    .bind(kind)
    .bind(payload)
    .bind(creator_address)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Drops expired terminal rows and history outside the activation boundary,
/// a bounded batch under short timeouts; deliverable retries are never
/// removed, delivery enabled or not.
pub(crate) async fn cleanup(pool: &PgPool, now: NaiveDateTime) -> anyhow::Result<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '500ms'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout = '100ms'")
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query(
        "DELETE FROM explore.moderation_hooks h WHERE h.id IN ( \
            SELECT id FROM explore.moderation_hooks \
            WHERE created_at < $1 - $2::int4 * interval '1 day' \
              AND (completed_at < $1 - $2::int4 * interval '1 day' \
                OR (attempts >= 6 AND next_attempt_at < $1 - $2::int4 * interval '1 day') \
                OR created_at < COALESCE((SELECT activated_at FROM explore.moderation_hook_activation WHERE singleton),$1)) \
            ORDER BY created_at,id LIMIT $3 FOR UPDATE SKIP LOCKED)",
    )
    .bind(now)
    .bind(RETENTION_DAYS)
    .bind(CLEANUP_LIMIT)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(removed)
}

/// One claimed row.
#[derive(sqlx::FromRow)]
struct Claimed {
    id: Uuid,
    channel: String,
    subject: String,
    kind: String,
    payload: Value,
    attempts: i32,
}

/// One worker pass: bounded cleanup, then up to ten due rows of the
/// configured channels, claimed one at a time (a short lease, `SKIP
/// LOCKED`), operator actions first, and posted outside the claim
/// transaction with a five-second budget; a failure backs off exponentially
/// and a sixth failure stays exhausted, distinct from completion. Returns
/// how many were delivered.
pub(crate) async fn run_once(
    pool: &PgPool,
    client: &crate::http::Outbound,
    endpoints: &Endpoints,
    links: &Links,
    now: NaiveDateTime,
) -> anyhow::Result<u64> {
    match tokio::time::timeout(Duration::from_secs(1), cleanup(pool, now)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(%error, "moderation hook cleanup deferred"),
        Err(_) => tracing::warn!("moderation hook cleanup exceeded budget"),
    }
    let configured = endpoints.configured();
    if configured.is_empty() {
        return Ok(0);
    }
    if !sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM explore.moderation_hook_activation)",
    )
    .fetch_one(pool)
    .await?
    {
        return Ok(0);
    }
    let channels: Vec<&str> = configured
        .iter()
        .map(|(channel, _)| channel.as_str())
        .collect();
    let mut sent = 0;
    for _ in 0..10 {
        let claim = Uuid::new_v4();
        let mut tx = pool.begin().await?;
        sqlx::query("SET LOCAL statement_timeout='2s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL lock_timeout='100ms'")
            .execute(&mut *tx)
            .await?;
        let row: Option<Claimed> = sqlx::query_as(
            "WITH due AS (SELECT id FROM explore.moderation_hooks \
             WHERE completed_at IS NULL AND attempts<6 AND next_attempt_at<=$1 AND channel=ANY($3) \
               AND created_at >= (SELECT activated_at FROM explore.moderation_hook_activation WHERE singleton) \
             ORDER BY CASE WHEN kind IN ('approved','rejected','deleted','disabled','rating_upgraded') THEN 0 WHEN kind='submitted' THEN 1 ELSE 2 END,next_attempt_at,id LIMIT 1 FOR UPDATE SKIP LOCKED) \
             UPDATE explore.moderation_hooks h SET claim=$2,attempts=attempts+1,next_attempt_at=$1+interval '1 minute' \
             FROM due WHERE h.id=due.id RETURNING h.id,h.channel,h.subject,h.kind,h.payload,h.attempts",
        )
        .bind(now)
        .bind(claim)
        .bind(&channels)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(Claimed {
            id,
            channel,
            subject,
            kind,
            payload,
            attempts,
        }) = row
        else {
            break;
        };
        let Some((_, endpoint)) = configured
            .iter()
            .find(|(configured, _)| configured.as_str() == channel)
        else {
            tracing::warn!(%id, channel, "moderation hook row names an unknown channel");
            continue;
        };
        let body = match channel.as_str() {
            "events" => crate::events::hooks::render(&subject, &kind, &payload, links),
            _ => payload,
        };
        let success = client
            .post(*endpoint)
            .timeout(Duration::from_secs(5))
            .json(&body)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        sqlx::query("UPDATE explore.moderation_hooks SET completed_at=CASE WHEN $3 THEN $4 ELSE NULL END,next_attempt_at=$4 + $5::int4*interval '1 second',claim=NULL WHERE id=$1 AND claim=$2")
            .bind(id).bind(claim).bind(success).bind(now).bind(30_i32 * (1 << attempts.min(6))).execute(pool).await?;
        if success {
            sent += 1
        } else {
            tracing::warn!(%id, channel, attempts, "moderation hook delivery failed");
        }
    }
    Ok(sent)
}

/// Moves the pending rows of the layout this outbox replaced
/// (`explore.event_moderation_hooks`, dropped by a later release) into it,
/// with their activation instant, so an upgrade loses no notice; the cutoff
/// moves back to cover the adopted rows even when a capture on some replica
/// set it first. A node without that table, or with nothing pending there,
/// moves nothing. Only due rows move: a row an older worker claimed keeps
/// its retry minute, so a notice in flight is not posted twice.
pub async fn adopt_legacy_rows(pool: &PgPool) -> anyhow::Result<u64> {
    let mut tx = pool.begin().await?;
    let legacy: bool =
        sqlx::query_scalar("SELECT to_regclass('explore.event_moderation_hooks') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    if !legacy {
        return Ok(0);
    }
    let activation: bool = sqlx::query_scalar(
        "SELECT to_regclass('explore.event_moderation_hook_activation') IS NOT NULL",
    )
    .fetch_one(&mut *tx)
    .await?;
    if activation {
        sqlx::query("INSERT INTO explore.moderation_hook_activation (activated_at) SELECT activated_at FROM explore.event_moderation_hook_activation WHERE singleton \
                     ON CONFLICT (singleton) DO UPDATE SET activated_at = LEAST(explore.moderation_hook_activation.activated_at, EXCLUDED.activated_at)")
            .execute(&mut *tx).await?;
    }
    let moved = sqlx::query(
        "WITH pending AS ( \
            DELETE FROM explore.event_moderation_hooks h WHERE h.id IN ( \
                SELECT id FROM explore.event_moderation_hooks WHERE completed_at IS NULL AND attempts < 6 AND next_attempt_at <= (now() AT TIME ZONE 'UTC') FOR UPDATE SKIP LOCKED) \
            RETURNING h.id, COALESCE(h.subject, h.event_id) AS subject, h.kind, h.event, h.creator_address, h.edit_bucket, h.created_at, h.next_attempt_at, h.attempts) \
         INSERT INTO explore.moderation_hooks (id, channel, subject, kind, payload, creator_address, edit_bucket, created_at, next_attempt_at, attempts) \
         SELECT id, 'events', subject, kind, event, creator_address, edit_bucket, created_at, next_attempt_at, attempts FROM pending WHERE subject IS NOT NULL \
         ON CONFLICT DO NOTHING",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved > 0 {
        // Adopted rows stay deliverable: an activation no later than them.
        sqlx::query("INSERT INTO explore.moderation_hook_activation (activated_at) SELECT min(created_at) FROM explore.moderation_hooks HAVING min(created_at) IS NOT NULL \
                     ON CONFLICT (singleton) DO UPDATE SET activated_at = LEAST(explore.moderation_hook_activation.activated_at, EXCLUDED.activated_at)")
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(moved)
}

/// One worker pass: the earlier layout's pending rows are adopted while that
/// table exists (one `to_regclass` check), then the due rows are delivered.
pub(crate) async fn pass(
    pool: &PgPool,
    client: &crate::http::Outbound,
    endpoints: &Endpoints,
    links: &Links,
    now: NaiveDateTime,
) -> anyhow::Result<u64> {
    let moved = adopt_legacy_rows(pool).await?;
    if moved > 0 {
        tracing::info!(
            moved,
            "moderation outbox adopted the earlier layout's pending rows"
        );
    }
    run_once(pool, client, endpoints, links, now).await
}

/// The leased worker, every minute. Without a configured endpoint it still
/// adopts and cleans up.
pub(crate) fn spawn(
    pool: PgPool,
    client: crate::http::Outbound,
    endpoints: Endpoints,
    links: Links,
    jobs: &crate::jobs::Jobs,
) {
    jobs.spawn_periodic_leased(
        "moderation-hook",
        crate::jobs::PeriodicCfg::every(Duration::from_secs(60)),
        move |_| {
            let (pool, client, endpoints, links) = (
                pool.clone(),
                client.clone(),
                endpoints.clone(),
                links.clone(),
            );
            async move {
                pass(&pool, &client, &endpoints, &links, Utc::now().naive_utc()).await?;
                Ok(())
            }
        },
    );
}

/// Held by every test that drains the shared outbox or needs it to hold only
/// its own rows. A test that deletes or rewrites the activation singleton runs
/// on `scratch::isolated_pool` instead: a capture on the shared database reads
/// the singleton in a statement after its insert, so a concurrent deletion
/// there fails an unrelated capture.
#[cfg(test)]
pub(crate) static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An unactivated node with the layout step 0024 leaves behind, its
    /// outbox activated two days ago: the wallet of a permission notice
    /// queued before that step sits in `event_id`, one after it in `subject`.
    async fn create_legacy_layout(pool: &PgPool) {
        sqlx::query("DELETE FROM explore.moderation_hook_activation")
            .execute(pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE explore.event_moderation_hooks (id uuid PRIMARY KEY, event_id text, subject text, kind text NOT NULL, event jsonb NOT NULL, \
                creator_address text, edit_bucket timestamp, created_at timestamp NOT NULL, next_attempt_at timestamp NOT NULL, \
                attempts integer NOT NULL DEFAULT 0, claim uuid, completed_at timestamp); \
             CREATE TABLE explore.event_moderation_hook_activation (singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton), activated_at timestamp NOT NULL); \
             INSERT INTO explore.event_moderation_hook_activation (activated_at) VALUES (timezone('UTC', now()) - interval '2 days');",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_earlier_layouts_pending_rows_are_adopted_on_every_pass_with_their_activation() {
        let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
            return;
        };
        let prefix = Uuid::new_v4().to_string();
        assert_eq!(
            adopt_legacy_rows(&pool).await.unwrap(),
            0,
            "a fresh layout has nothing to adopt"
        );
        create_legacy_layout(&pool).await;
        for (suffix, event_id, subject, kind, attempts, completed) in [
            ("submitted", Some("ev"), None, "submitted", 0, false),
            (
                "legacy-wallet",
                Some("0xwallet"),
                None,
                "profile_permissions",
                2,
                false,
            ),
            (
                "wallet",
                None,
                Some("0xwallet2"),
                "profile_permissions",
                0,
                false,
            ),
            ("done", Some("ev-done"), None, "approved", 1, true),
            ("exhausted", Some("ev-gone"), None, "edited", 6, false),
        ] {
            sqlx::query("INSERT INTO explore.event_moderation_hooks (id,event_id,subject,kind,event,creator_address,created_at,next_attempt_at,attempts,completed_at) \
                VALUES ($1,$2,$3,$4,$5,'creator',timezone('UTC', now()) - interval '1 day',timezone('UTC', now()),$6,CASE WHEN $7 THEN timezone('UTC', now()) END)")
                .bind(Uuid::new_v4())
                .bind(event_id.map(|e| format!("{prefix}-{e}")))
                .bind(subject.map(|s| format!("{prefix}-{s}")))
                .bind(kind)
                .bind(json!({"name": suffix}))
                .bind(attempts)
                .bind(completed)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(adopt_legacy_rows(&pool).await.unwrap(), 3);
        let adopted: Vec<(String, String, String, i32)> = sqlx::query_as(
            "SELECT channel, subject, kind, attempts FROM explore.moderation_hooks WHERE subject LIKE $1 ORDER BY subject",
        )
        .bind(format!("{prefix}-%"))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            adopted,
            vec![
                (
                    "events".into(),
                    format!("{prefix}-0xwallet"),
                    "profile_permissions".into(),
                    2
                ),
                (
                    "events".into(),
                    format!("{prefix}-0xwallet2"),
                    "profile_permissions".into(),
                    0
                ),
                (
                    "events".into(),
                    format!("{prefix}-ev"),
                    "submitted".into(),
                    0
                ),
            ]
        );
        let left: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM explore.event_moderation_hooks ORDER BY kind")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            left,
            vec!["approved", "edited"],
            "completed and exhausted rows stay as the old table's history"
        );
        let eligible: bool = sqlx::query_scalar(
            "SELECT bool_and(h.created_at >= a.activated_at) FROM explore.moderation_hooks h CROSS JOIN explore.moderation_hook_activation a WHERE h.subject LIKE $1",
        )
        .bind(format!("{prefix}-%"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            eligible,
            "the adopted activation keeps the rows deliverable"
        );
        assert_eq!(adopt_legacy_rows(&pool).await.unwrap(), 0);
        // A replica still on the earlier layout queues a notice after the
        // first adoption (a rolling deploy): the next worker pass moves it.
        sqlx::query("INSERT INTO explore.event_moderation_hooks (id,event_id,kind,event,created_at,next_attempt_at) \
            VALUES ($1,$2,'approved','{}',timezone('UTC', now()),timezone('UTC', now()))")
            .bind(Uuid::new_v4())
            .bind(format!("{prefix}-late"))
            .execute(&pool)
            .await
            .unwrap();
        let links = Links {
            base: "https://example.org".into(),
            jump_in_site_url: "https://decentraland.org/jump".into(),
            asset_base: "https://example.org/events".into(),
        };
        pass(
            &pool,
            &crate::http::Outbound::for_tests(),
            &Endpoints::default(),
            &links,
            Utc::now().naive_utc(),
        )
        .await
        .unwrap();
        let (adopted_late, left_late): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM explore.moderation_hooks WHERE subject=$1), \
                    (SELECT count(*) FROM explore.event_moderation_hooks WHERE event_id=$1)",
        )
        .bind(format!("{prefix}-late"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((adopted_late, left_late), (1, 0), "every pass adopts");
    }

    #[tokio::test]
    async fn adoption_moves_the_cutoff_back_over_a_capture_that_came_first() {
        let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
            return;
        };
        let prefix = Uuid::new_v4().to_string();
        create_legacy_layout(&pool).await;
        sqlx::query("INSERT INTO explore.event_moderation_hooks (id,event_id,kind,event,created_at,next_attempt_at) \
            VALUES ($1,$2,'submitted','{}',timezone('UTC', now()) - interval '1 day',timezone('UTC', now()))")
            .bind(Uuid::new_v4())
            .bind(format!("{prefix}-ev"))
            .execute(&pool)
            .await
            .unwrap();
        // A moderation write on any replica activates the outbox at the
        // present instant before the worker's first pass adopts the queue.
        let mut tx = pool.begin().await.unwrap();
        capture_at(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(adopt_legacy_rows(&pool).await.unwrap(), 1);
        let (eligible, earlier): (bool, bool) = sqlx::query_as(
            "SELECT bool_and(h.created_at >= a.activated_at), bool_and(a.activated_at = l.activated_at) \
             FROM explore.moderation_hooks h CROSS JOIN explore.moderation_hook_activation a \
             CROSS JOIN explore.event_moderation_hook_activation l WHERE h.subject LIKE $1",
        )
        .bind(format!("{prefix}-%"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(eligible, "the adopted row sits above the cutoff");
        assert!(earlier, "the cutoff is the earlier layout's activation");
    }
}
