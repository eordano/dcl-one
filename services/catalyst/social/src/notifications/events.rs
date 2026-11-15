//! Event notices over local RSVP consent and managed, local community
//! associations. A rolling one-hour catch-up uses retained occurrence intents
//! as durable scan progress. Explore captures moderation changes
//! atomically with the event. Dispatch pages commit their recipient cursor
//! with notification/email-outbox inserts, so crashes and retries are safe.

use std::sync::LazyLock;
use std::time::Duration;

use chrono::{Duration as Span, NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::store::{NewNotification, community_opt_out, insert_event_notifications};
use crate::identity::Address;

const PAGE: i64 = 100;
const CATCH_UP_SECONDS: i64 = 3600;
const PASS_BUDGET: Duration = Duration::from_secs(5);
const SCAN_BUDGET: Duration = Duration::from_secs(3);
const CLEANUP_BUDGET: Duration = Duration::from_millis(500);
const RETENTION_DAYS: i64 = 30;

#[derive(sqlx::FromRow)]
struct Occurrence {
    event_id: String,
    event: Value,
    local_event: bool,
    starts_at: NaiveDateTime,
    due_at: NaiveDateTime,
}

#[derive(sqlx::FromRow)]
struct Intent {
    id: Uuid,
    event_id: String,
    kind: String,
    event: Value,
    local_event: bool,
    occurrence_start: Option<NaiveDateTime>,
    occurred_at: NaiveDateTime,
    recipient_after: String,
}

#[derive(sqlx::FromRow)]
struct Recipient {
    address: String,
    community: bool,
    community_name: Option<String>,
    community_thumbnail: bool,
}

/// The sites the notices point at and the Slack switch.
#[derive(Clone, Debug)]
pub struct Links {
    /// The events site (`/whats-on/{id}`): the link of the notices the old
    /// service sent without one (rejected, deleted, community created).
    pub base: String,
    /// Backend asset origin and mount for relative default posters.
    pub asset_base: String,
    /// `JUMP_IN_SITE_URL`: `buildEventLink` (`Notifications/index.ts:30-39`)
    /// for the reminder, start and approval notices.
    pub jump_in_site_url: String,
    /// Community thumbnail CDN (`communities::thumbnail_url`).
    pub community_cdn: String,
    /// Explore's Slack moderation outbox is active: the reminder job queues
    /// the old `:runner:` notice there (`Event/cron.ts:76`).
    pub slack_notices: bool,
}

fn occurrence_id(event_id: &str, starts_at: NaiveDateTime, kind: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("event-occurrence:{event_id}:{starts_at}:{kind}").as_bytes(),
    )
}

// finish_at is the final series finish, unlike next_finish_at which can remain
// stale until the next catalogue refresh. Bound live candidates before expanding
// recurrence, then keyset-order the unqueued occurrences. The unique queue key
// makes concurrent scans safe and avoids losing late approvals behind a cursor.
static SCAN_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "
    SELECT DISTINCT e.id AS event_id,e.raw - 'recurrent_dates' AS event,e.origin='local' AS local_event,
           d.starts_at,d.starts_at - $2::int4 * interval '1 second' AS due_at
    FROM explore.event e CROSS JOIN LATERAL (
        SELECT CASE WHEN pg_input_is_valid(value, 'timestamp with time zone')
               THEN value::timestamptz AT TIME ZONE 'UTC' END AS starts_at
        FROM jsonb_array_elements_text(CASE WHEN jsonb_typeof(e.raw->'recurrent_dates')='array'
             AND e.raw->'recurrent_dates' <> '[]'::jsonb THEN e.raw->'recurrent_dates'
             ELSE jsonb_build_array(e.raw->'start_at') END)
    ) d
    WHERE e.approved AND e.finish_at > $3
      AND e.start_at <= $3 + $2::int4 * interval '1 second'
      AND COALESCE(e.raw->>'rejected','false') <> 'true'
      AND COALESCE(e.raw->>'deleted_by_user','false') <> 'true'
      AND COALESCE(e.raw->>'deleted_by_admin','false') <> 'true'
      AND (EXISTS (SELECT 1 FROM explore.event_attendance_local a
                   WHERE a.event_id=e.id AND a.action='going')
           OR ($1='events_started' AND e.origin='local' AND EXISTS (
               SELECT 1 FROM social.communities c JOIN social.community_members m ON m.community_id=c.id
               WHERE c.id=CASE WHEN pg_input_is_valid(e.community_id,'uuid') THEN e.community_id::uuid END
                 AND c.origin='local' AND c.active AND NOT c.suspended
                 AND NOT EXISTS ({opt_out}))))
      AND d.starts_at - $2::int4 * interval '1 second' >= $4
      AND d.starts_at - $2::int4 * interval '1 second' <= $3
      AND CASE WHEN $1='events_starts_soon' THEN d.starts_at > $3
               ELSE d.starts_at + e.duration_ms * interval '1 millisecond' > $3 END
      AND NOT EXISTS (SELECT 1 FROM social.event_notification_intents q
                      WHERE q.event_id=e.id AND q.occurrence_start=d.starts_at AND q.kind=$1)
    ORDER BY due_at,event_id LIMIT $5",
        opt_out = community_opt_out("m.member_address", "c.id")
    )
});

async fn scan_kind(
    pool: &PgPool,
    kind: &str,
    ahead: i32,
    now: NaiveDateTime,
) -> anyhow::Result<u64> {
    let mut tx = timed_transaction(pool).await?;
    sqlx::query("SET LOCAL statement_timeout = '2s'")
        .execute(&mut *tx)
        .await?;
    let rows: Vec<Occurrence> = sqlx::query_as(sqlx::AssertSqlSafe(SCAN_SQL.as_str()))
        .bind(kind)
        .bind(ahead)
        .bind(now)
        .bind(now - Span::seconds(CATCH_UP_SECONDS))
        .bind(PAGE)
        .fetch_all(&mut *tx)
        .await?;
    let ids: Vec<Uuid> = rows
        .iter()
        .map(|row| occurrence_id(&row.event_id, row.starts_at, kind))
        .collect();
    let event_ids: Vec<&str> = rows.iter().map(|row| row.event_id.as_str()).collect();
    let events: Vec<Value> = rows
        .iter()
        .map(|row| crate::explore::events::notifications::snapshot(&row.event))
        .collect();
    let local: Vec<bool> = rows.iter().map(|row| row.local_event).collect();
    let starts: Vec<NaiveDateTime> = rows.iter().map(|row| row.starts_at).collect();
    let due: Vec<NaiveDateTime> = rows.iter().map(|row| row.due_at).collect();
    sqlx::query(
        "INSERT INTO social.event_notification_intents (id,event_id,event,local_event,occurrence_start,occurred_at,kind) \
         SELECT n.*,$7 FROM UNNEST($1::uuid[],$2::text[],$3::jsonb[],$4::boolean[],$5::timestamp[],$6::timestamp[]) n \
         ON CONFLICT DO NOTHING",
    ).bind(ids).bind(event_ids).bind(events).bind(local).bind(starts).bind(due).bind(kind).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows.len() as u64)
}

/// `Date.prototype.toISOString()`: UTC, millisecond precision, `Z`.
fn js_iso(t: NaiveDateTime) -> String {
    t.and_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn text<'a>(event: &'a Value, field: &str) -> &'a str {
    event.get(field).and_then(Value::as_str).unwrap_or_default()
}

/// The page of an intent's recipients after `$6`: the event's author for a
/// moderation notice, its RSVPs for a reminder (on a started community event,
/// not those who opted out of the community) and the community's members for
/// a community event, unless opted out.
static RECIPIENTS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "WITH candidates AS ( \
            SELECT $3::text AS address,false AS community,NULL::text AS community_name,false AS community_thumbnail \
            WHERE $2 IN ('event_approved','event_rejected','event_deleted') AND $3 > $6 \
            UNION ALL \
            (SELECT a.signer,false,NULL,false FROM explore.event_attendance_local a JOIN explore.event e ON e.id=a.event_id \
            WHERE a.event_id=$1 AND a.signer > $6 AND a.action='going' AND $2 IN ('events_starts_soon','events_started') \
              AND e.approved AND COALESCE(e.raw->>'rejected','false') <> 'true' \
              AND COALESCE(e.raw->>'deleted_by_user','false') <> 'true' AND COALESCE(e.raw->>'deleted_by_admin','false') <> 'true' \
              AND NOT ($2='events_started' AND $4 AND EXISTS (SELECT 1 FROM social.community_members m \
                  JOIN social.communities c ON c.id=m.community_id \
                  WHERE m.community_id=$5::uuid AND m.member_address=a.signer \
                    AND c.origin='local' AND c.active AND NOT c.suspended \
                    AND CASE WHEN pg_input_is_valid(e.community_id,'uuid') THEN e.community_id::uuid END=$5::uuid \
                    AND EXISTS ({opt_out}))) \
            ORDER BY a.signer LIMIT $7) \
            UNION ALL \
            (SELECT m.member_address,true,c.name,EXISTS (SELECT 1 FROM social.community_thumbnails t WHERE t.community_id=c.id) \
            FROM social.community_members m \
            JOIN social.communities c ON c.id=m.community_id \
            JOIN explore.event e ON e.id=$1 AND CASE WHEN pg_input_is_valid(e.community_id,'uuid') THEN e.community_id::uuid END=$5::uuid AND e.origin='local' \
            WHERE $4 AND $2 IN ('event_created','events_started') AND c.id=$5::uuid AND m.member_address > $6 \
              AND c.origin='local' AND c.active AND NOT c.suspended AND e.approved \
              AND COALESCE(e.raw->>'rejected','false') <> 'true' \
              AND COALESCE(e.raw->>'deleted_by_user','false') <> 'true' AND COALESCE(e.raw->>'deleted_by_admin','false') <> 'true' \
              AND NOT EXISTS ({opt_out}) \
            ORDER BY m.member_address LIMIT $7) \
        ) SELECT address,bool_or(community) AS community,max(community_name) AS community_name, \
                 bool_or(community_thumbnail) AS community_thumbnail \
          FROM candidates WHERE address > $6 GROUP BY address ORDER BY address LIMIT $7",
        opt_out = community_opt_out("m.member_address", "$5::uuid")
    )
});

async fn recipients(
    tx: &mut Transaction<'_, Postgres>,
    intent: &Intent,
) -> anyhow::Result<Vec<Recipient>> {
    let community_id = Uuid::parse_str(text(&intent.event, "community_id")).ok();
    // Upstream scoped events_started to the community for every recipient: a
    // member who opted out of the community gets no notice, even with an RSVP.
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(RECIPIENTS_SQL.as_str()))
        .bind(&intent.event_id)
        .bind(&intent.kind)
        .bind(text(&intent.event, "user"))
        .bind(intent.local_event)
        .bind(community_id)
        .bind(&intent.recipient_after)
        .bind(PAGE)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows)
}

fn metadata(intent: &Intent, recipient: &Recipient, links: &Links) -> Value {
    let base = links.base.trim_end_matches('/');
    let name = crate::explore::sanitize::strip_all_markup(text(&intent.event, "name"));
    let community_name = recipient
        .community_name
        .as_deref()
        .map(crate::explore::sanitize::strip_all_markup)
        .filter(|n| !n.is_empty());
    // A name that was only markup is left out of the sentence.
    let named = if name.is_empty() {
        String::new()
    } else {
        format!(" {name}")
    };
    let description =
        crate::explore::sanitize::sanitize_event_description(text(&intent.event, "description"));
    // `Notifications/index.ts:48,85,205`: the jump-in link; the other kinds
    // had no link upstream and keep this node's event page.
    let link = match intent.kind.as_str() {
        "events_starts_soon" | "events_started" | "event_approved" => {
            crate::explore::events::notifications::event_link(
                &links.jump_in_site_url,
                &intent.event_id,
                &intent.event,
            )
        }
        _ => format!("{base}/whats-on/{}", urlencoding::encode(&intent.event_id)),
    };
    let (title, description) = match intent.kind.as_str() {
        // `Notifications/index.ts:56-57`
        "events_starts_soon" => (
            "Event starts in an hour".to_string(),
            format!("The event{named} starts in an hour."),
        ),
        "events_started" if recipient.community => (
            "Community Event starting".into(),
            format!(
                "A {} event is about to start.",
                community_name.as_deref().unwrap_or("community")
            ),
        ),
        "events_started" => (
            "Event started".into(),
            format!("The event{named} has begun!"),
        ),
        "event_created" => (
            "Community Event Added".into(),
            format!(
                "The {} Community has added a new event.",
                community_name.as_deref().unwrap_or("community")
            ),
        ),
        _ if name.is_empty() => ("Event".to_string(), description),
        _ => (name.to_string(), description),
    };
    let mut metadata = json!({"eventId":intent.event_id,"title":title,"description":description,"name":name,"image":catalyst_explore::events::posters::absolute_default_image(text(&intent.event,"image"),&links.asset_base),"link":link,"occurredAt":intent.occurred_at.and_utc().to_rfc3339()});
    if matches!(
        intent.kind.as_str(),
        "events_starts_soon" | "events_started" | "event_created"
    ) {
        // `Notifications/index.ts:63,105,138`: the recipient of a per-attendee notice.
        metadata["attendee"] = json!(recipient.address);
    }
    if let Some(start) = intent.occurrence_start {
        let duration = intent
            .event
            .get("duration")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0);
        metadata["startsAt"] = json!(js_iso(start));
        metadata["endsAt"] = json!(
            Span::try_milliseconds(duration)
                .and_then(|duration| start.checked_add_signed(duration))
                .map(js_iso)
        );
    }
    if recipient.community {
        let community = text(&intent.event, "community_id");
        metadata["communityId"] = json!(community);
        metadata["communityName"] = json!(community_name);
        if recipient.community_thumbnail {
            // The stored thumbnail lives under the canonical UUID.
            let id = Uuid::parse_str(community).map_or(community.to_string(), |id| id.to_string());
            metadata["communityThumbnail"] =
                json!(crate::communities::thumbnail_url(&links.community_cdn, &id));
        }
    }
    if matches!(
        intent.kind.as_str(),
        "event_approved" | "event_rejected" | "event_deleted"
    ) {
        metadata["myHangouts"] = json!(format!("{base}/whats-on?tab=my"));
    }
    if intent.kind == "event_rejected" {
        metadata["reason"] = json!(text(&intent.event, "rejection_reason"));
    }
    if intent.kind == "event_deleted" {
        // Upstream sent `reason` only when the moderator gave one.
        let reason = text(&intent.event, "deleted_reason");
        if !reason.is_empty() {
            metadata["reason"] = json!(reason);
        }
    }
    metadata
}

/// PostgreSQL cancels blocked or slow statements even if the Rust future is
/// dropped. SET LOCAL is transaction-scoped and never leaks into pooled users.
pub(super) async fn timed_transaction(
    pool: &PgPool,
) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '500ms'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout = '100ms'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

fn timed_out(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<sqlx::Error>(), Some(sqlx::Error::Database(db))
        if matches!(db.code().as_deref(), Some("57014" | "55P03")))
}

async fn dispatch_page(
    pool: &PgPool,
    links: &Links,
    now: NaiveDateTime,
) -> anyhow::Result<Option<u64>> {
    // Commit fair rotation separately from delivery. If a later page times out
    // or rolls back, another intent gets the next turn instead of starvation.
    let mut claim = timed_transaction(pool).await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "UPDATE social.event_notification_intents SET dispatch_at=timezone('UTC', clock_timestamp())
         WHERE id=(SELECT id FROM social.event_notification_intents WHERE processed_at IS NULL
                   ORDER BY dispatch_at,id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id",
    ).fetch_optional(&mut *claim).await?;
    claim.commit().await?;
    let Some(id) = id else { return Ok(None) };
    let mut tx = timed_transaction(pool).await?;
    let intent: Option<Intent> = sqlx::query_as(
        "SELECT id,event_id,kind,event,local_event,occurrence_start,occurred_at,recipient_after
         FROM social.event_notification_intents WHERE id=$1 AND processed_at IS NULL FOR UPDATE SKIP LOCKED",
    ).bind(id).fetch_optional(&mut *tx).await?;
    let Some(mut intent) = intent else {
        tx.commit().await?;
        return Ok(Some(0));
    };
    // A queued occurrence may have been moved or removed since its snapshot.
    // Revalidate the current event under a shared row lock before delivering;
    // a reschedule's old intent is consumed, while its new occurrence is scanned.
    let mut obsolete = false;
    if let Some(start) = intent.occurrence_start {
        let current: Option<(Value, bool)> = sqlx::query_as(
            "SELECT e.raw,e.origin='local' FROM explore.event e
             WHERE e.id=$1 AND e.approved
               AND COALESCE(e.raw->>'rejected','false') <> 'true'
               AND COALESCE(e.raw->>'deleted_by_user','false') <> 'true'
               AND COALESCE(e.raw->>'deleted_by_admin','false') <> 'true'
               AND EXISTS (SELECT 1 FROM jsonb_array_elements_text(
                   CASE WHEN jsonb_typeof(e.raw->'recurrent_dates')='array'
                        AND e.raw->'recurrent_dates' <> '[]'::jsonb THEN e.raw->'recurrent_dates'
                        ELSE jsonb_build_array(e.raw->'start_at') END) d(value)
                   WHERE CASE WHEN pg_input_is_valid(value,'timestamp with time zone')
                         THEN value::timestamptz AT TIME ZONE 'UTC' END=$2)
             FOR SHARE",
        )
        .bind(&intent.event_id)
        .bind(start)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((event, local)) = current {
            intent.event = event;
            intent.local_event = local;
        } else {
            obsolete = true;
        }
    }
    let delivered_at: NaiveDateTime =
        sqlx::query_scalar("SELECT timezone('UTC', clock_timestamp())")
            .fetch_one(&mut *tx)
            .await?;
    // Recheck expiry after lock waits and scans, not only at pass start. The
    // supplied horizon can be ahead of the wall clock for deterministic replay.
    let checked_at = now.max(delivered_at);
    let suppression = suppression_reason(&intent, checked_at, obsolete);
    let recipients = if suppression.is_some() {
        Vec::new()
    } else {
        recipients(&mut tx, &intent).await?
    };
    let batch: Vec<NewNotification> = recipients
        .iter()
        .filter_map(|recipient| {
            let address: Address = recipient.address.parse().ok()?;
            Some(NewNotification {
                id: Uuid::new_v5(&intent.id, address.as_str().as_bytes()),
                address,
                kind: intent.kind.clone(),
                metadata: metadata(&intent, recipient, links),
                occurred_at: delivered_at,
            })
        })
        .collect();
    let inserted = insert_event_notifications(&mut *tx, &batch).await?;
    let finished = recipients.len() < PAGE as usize;
    if finished
        && suppression.is_none()
        && links.slack_notices
        && intent.kind == "events_starts_soon"
    {
        // `Event/cron.ts:66-76`: the Slack notice counts the occurrence's
        // attendees (`attendees.length`), queued with the last reminder page
        // so a crash before this commit repeats neither.
        let attendees: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.event_attendance_local WHERE event_id=$1 AND action='going'",
        )
        .bind(&intent.event_id)
        .fetch_one(&mut *tx)
        .await?;
        crate::explore::events::notifications::record_upcoming_notice(
            &mut tx,
            &intent.event_id,
            &intent.event,
            attendees,
        )
        .await?;
    }
    sqlx::query("UPDATE social.event_notification_intents SET recipient_after=$2,processed_at=CASE WHEN $3 THEN $4 ELSE NULL END,dispatch_at=timezone('UTC', clock_timestamp()) WHERE id=$1")
        .bind(intent.id).bind(recipients.last().map(|row| row.address.as_str()).unwrap_or(&intent.recipient_after))
        .bind(finished).bind(checked_at).execute(&mut *tx).await?;
    tx.commit().await?;
    if let Some(reason) = suppression {
        tracing::warn!(intent_id=%intent.id,event_id=%intent.event_id,kind=%intent.kind,reason,suppressed_intents=1,"event notification intent suppressed");
    }
    Ok(Some(inserted))
}

fn suppression_reason(intent: &Intent, now: NaiveDateTime, obsolete: bool) -> Option<&'static str> {
    if obsolete {
        Some("obsolete_occurrence")
    } else if stale_occurrence(intent, now) {
        Some(if intent.kind == "events_started" {
            "event_ended"
        } else {
            "reminder_expired"
        })
    } else {
        None
    }
}

fn stale_occurrence(intent: &Intent, now: NaiveDateTime) -> bool {
    let Some(start) = intent.occurrence_start else {
        return false;
    };
    if intent.kind == "events_starts_soon" {
        return start <= now;
    }
    let end = intent
        .event
        .get("duration")
        .and_then(Value::as_i64)
        .and_then(Span::try_milliseconds)
        .and_then(|duration| start.checked_add_signed(duration));
    intent.kind == "events_started" && end.is_none_or(|end| end <= now)
}

async fn cleanup(pool: &PgPool, now: NaiveDateTime) -> anyhow::Result<()> {
    let mut tx = timed_transaction(pool).await?;
    // Pending work is never purged. Completed keys outlive the one-hour scan
    // window by 30 days, so cleanup cannot make an eligible occurrence new again.
    sqlx::query(
        "DELETE FROM social.event_notification_intents WHERE id IN (
        SELECT id FROM social.event_notification_intents
        WHERE processed_at < $1 AND occurred_at < $1
        ORDER BY processed_at,id LIMIT 1000 FOR UPDATE SKIP LOCKED)",
    )
    .bind(now - Span::days(RETENTION_DAYS))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn scan_with_budget(
    pool: &PgPool,
    kind: &str,
    ahead: i32,
    now: NaiveDateTime,
) -> anyhow::Result<()> {
    let scan = async {
        while scan_kind(pool, kind, ahead, now).await? == PAGE as u64 {}
        Ok::<_, anyhow::Error>(())
    };
    within_scan_budget(kind, scan).await
}

async fn within_scan_budget(
    kind: &str,
    scan: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    match tokio::time::timeout(SCAN_BUDGET, scan).await {
        Ok(Err(error)) if timed_out(&error) => {
            tracing::error!(%error,kind,operation="event_occurrence_scan",reason="database_timeout","event occurrence scan deferred; retaining normal delivery cadence");
            Ok(())
        }
        Ok(result) => {
            result.map_err(|error| error.context(format!("{kind} occurrence scan failed")))
        }
        Err(_) => {
            tracing::error!(
                kind,
                operation = "event_occurrence_scan",
                reason = "budget_exhausted",
                "event occurrence scan exceeded budget; retaining normal delivery cadence"
            );
            Ok(())
        }
    }
}

/// Cleanup always receives a turn before scanning/dispatch. Independent scan
/// budgets prevent one occurrence kind starving the other. Committed rotation
/// survives a timed-out page. Non-timeout scan failures surface after dispatch;
/// expected scan deferrals and maintenance failures are error-logged without
/// delaying the normal delivery cadence.
pub async fn run_once(pool: &PgPool, links: &Links, now: NaiveDateTime) -> anyhow::Result<u64> {
    match tokio::time::timeout(CLEANUP_BUDGET, cleanup(pool, now)).await {
        Ok(Err(error)) => {
            tracing::error!(%error, "event intent cleanup deferred")
        }
        Ok(Ok(())) => {}
        Err(_) => tracing::error!("event intent cleanup exceeded budget"),
    }
    let (reminders, starts, ended) = tokio::join!(
        scan_with_budget(pool, "events_starts_soon", 3600, now),
        scan_with_budget(pool, "events_started", 0, now),
        within_scan_budget("events_ended", async {
            super::event_ended::run_once(pool, now).await.map(|_| ())
        }),
    );
    let deadline = tokio::time::Instant::now() + PASS_BUDGET;
    let mut emitted = 0;
    loop {
        match tokio::time::timeout_at(deadline, dispatch_page(pool, links, now)).await {
            Ok(Err(error)) if timed_out(&error) => {
                tracing::warn!(%error, "event notification page deferred")
            }
            Ok(result) => match result? {
                Some(inserted) => emitted += inserted,
                None => break,
            },
            Err(_) => break,
        }
    }
    reminders?;
    starts?;
    ended?;
    Ok(emitted)
}

pub fn spawn(pool: PgPool, links: Links, jobs: &crate::jobs::Jobs) {
    jobs.spawn_periodic_leased(
        "event-notifications",
        crate::jobs::PeriodicCfg::every(Duration::from_secs(60))
            .with_error_backoff(Duration::from_secs(300)),
        move |ctx| {
            let (pool, links) = (pool.clone(), links.clone());
            async move {
                let emitted = run_once(&pool, &links, Utc::now().naive_utc()).await?;
                if emitted > 0 {
                    ctx.progress(format!("{emitted} event notices emitted"));
                }
                Ok(())
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const CDN: &str = "https://cdn.example.org";

    #[test]
    fn times_read_as_javascript_iso_strings() {
        let t = chrono::DateTime::from_timestamp_millis(4_084_016_400_123)
            .unwrap()
            .naive_utc();
        assert_eq!(js_iso(t), "2099-06-01T17:00:00.123Z");
        let micros = t + Span::microseconds(456);
        assert_eq!(js_iso(micros), "2099-06-01T17:00:00.123Z");
    }

    fn origins(link: &str, assets: &str) -> Links {
        Links {
            base: link.into(),
            asset_base: assets.into(),
            jump_in_site_url: "https://decentraland.org/jump".into(),
            community_cdn: CDN.into(),
            slack_notices: false,
        }
    }

    fn test_links(base: &str) -> Links {
        origins(base, "https://events.decentraland.org")
    }

    #[tokio::test(start_paused = true)]
    async fn client_scan_budget_defers_without_backoff() {
        let started = tokio::time::Instant::now();
        within_scan_budget("events_started", std::future::pending())
            .await
            .unwrap();
        assert_eq!(started.elapsed(), SCAN_BUDGET);
    }
    use crate::explore::events::moderation::{self, AdminPatch, CreateEvent};

    const HOST: &str = "0x1111111111111111111111111111111111111111";
    const RSVP: &str = "0x2222222222222222222222222222222222222222";
    const DUAL: &str = "0x3333333333333333333333333333333333333333";
    const EMAIL: &str = "0x4444444444444444444444444444444444444444";
    const OPTED: &str = "0x5555555555555555555555555555555555555555";
    const PREVIEW: &str = "0x6666666666666666666666666666666666666666";

    async fn count(pool: &PgPool, event: &str, kind: &str, address: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE metadata->>'eventId'=$1 AND type=$2 AND address=$3")
            .bind(event).bind(kind).bind(address).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn event_notices_recover_without_duplicates_respect_consent_and_separate_channels() {
        let _guard = super::super::event_ended::TEST_LOCK.lock().await;
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let community = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO social.communities (id,name,owner_address) VALUES ($1,'Calendar Club',$2)",
        )
        .bind(community)
        .bind(HOST)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO social.community_thumbnails (community_id,thumbnail_hash,updated_at) VALUES ($1,'hash',timezone('UTC', now()))")
            .bind(community).execute(&pool).await.unwrap();
        for member in [HOST, DUAL, EMAIL, OPTED] {
            sqlx::query("INSERT INTO social.community_members (community_id,member_address,role) VALUES ($1,$2,'member')")
                .bind(community).bind(member).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO social.subscriptions (address,email,details) VALUES ($1,'email@example.com','{\"ignore_all_in_app\":true}'::jsonb),($2,'dual@example.com','{\"ignore_all_email\":true}'::jsonb) ON CONFLICT (address) DO UPDATE SET email=EXCLUDED.email,details=EXCLUDED.details")
            .bind(EMAIL).bind(DUAL).execute(&pool).await.unwrap();
        for member in [DUAL, OPTED] {
            sqlx::query("INSERT INTO social.subscription_opt_outs (address,scope,scope_id) VALUES ($1,'community',$2)")
                .bind(member).bind(community.to_string()).execute(&pool).await.unwrap();
        }
        let body: CreateEvent = serde_json::from_value(json!({"name":"Calendar event","start_at":"2099-06-01T18:00:00Z","duration":3600000,"x":0,"y":0,"community_id":community.to_string(),"recurrent":true,"recurrent_frequency":"DAILY","recurrent_count":3})).unwrap();
        let event = moderation::create_as(
            &pool,
            &body,
            HOST,
            HOST,
            &catalyst_explore::Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.event_notification_intents WHERE event_id=$1",
        )
        .bind(&event)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            pending, 0,
            "pending creation does not announce an unapproved event"
        );
        for _ in 0..2 {
            moderation::moderate(
                &pool,
                &event,
                &AdminPatch {
                    approved: Some(true),
                    ..Default::default()
                },
                "moderator",
                &catalyst_explore::Settings::for_tests(),
            )
            .await
            .unwrap();
        }
        for member in [RSVP, DUAL] {
            crate::explore::events::attendees::rsvp_going(&pool, &event, member, None)
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE explore.event SET raw=jsonb_set(raw,'{latest_attendees}',$2) WHERE id=$1",
        )
        .bind(&event)
        .bind(json!([PREVIEW]))
        .execute(&pool)
        .await
        .unwrap();
        let base = "https://example.org";
        let t0 = chrono::DateTime::parse_from_rfc3339("2099-06-01T16:00:00Z")
            .unwrap()
            .naive_utc();
        let historical = Uuid::new_v4().to_string();
        crate::explore::events::mirror::upsert_event(&pool,&json!({
            "id":historical,"name":"Historical","start_at":"2099-05-31T18:00:00Z","finish_at":"2099-05-31T19:00:00Z",
            "duration":3600000,"approved":true,"recurrent_dates":["2099-05-31T18:00:00Z"]
        })).await.unwrap();
        crate::explore::events::attendees::rsvp_going(&pool, &historical, RSVP, None)
            .await
            .unwrap();
        run_once(&pool, &origins(base, "https://events.decentraland.org"), t0)
            .await
            .unwrap();
        assert_eq!(
            count(&pool, &historical, "events_started", RSVP).await,
            0,
            "first boot never replays historical occurrences"
        );
        assert_eq!(count(&pool, &event, "event_approved", HOST).await, 1);
        assert_eq!(count(&pool, &event, "event_created", HOST).await, 1);
        assert_eq!(count(&pool, &event, "event_created", OPTED).await, 0);
        assert_eq!(count(&pool, &event, "event_created", DUAL).await, 0);
        let (visible,email_state): (bool,String) = sqlx::query_as("SELECT n.in_app,d.state FROM social.notifications n JOIN social.notification_deliveries d ON d.notification_id=n.id WHERE n.metadata->>'eventId'=$1 AND n.type='event_created' AND n.address=$2")
            .bind(&event).bind(EMAIL).fetch_one(&pool).await.unwrap();
        assert!(!visible);
        assert_eq!(
            email_state, "skipped",
            "event_created has no email template upstream"
        );
        let store = super::super::NotificationsStore::new(pool.clone());
        assert!(
            store
                .list(&EMAIL.parse().unwrap(), 100, None, false)
                .await
                .unwrap()
                .iter()
                .all(|notice| notice.metadata["eventId"] != event)
        );

        let soon = t0 + Span::hours(1) + Span::seconds(1);
        let links = origins(base, "https://events.decentraland.org");
        let (first, second) =
            tokio::join!(run_once(&pool, &links, soon), run_once(&pool, &links, soon));
        first.unwrap();
        second.unwrap();
        assert_eq!(count(&pool, &event, "events_starts_soon", RSVP).await, 1);
        assert_eq!(count(&pool, &event, "events_starts_soon", DUAL).await, 1);
        assert_eq!(
            count(&pool, &event, "events_starts_soon", EMAIL).await,
            0,
            "community membership alone does not subscribe to reminders"
        );
        assert_eq!(
            count(&pool, &event, "events_starts_soon", PREVIEW).await,
            0,
            "upstream preview is not local consent"
        );
        let started = t0 + Span::hours(2) + Span::seconds(1);
        run_once(
            &pool,
            &origins(base, "https://events.decentraland.org"),
            started,
        )
        .await
        .unwrap();
        run_once(
            &pool,
            &origins(base, "https://events.decentraland.org"),
            started,
        )
        .await
        .unwrap();
        assert_eq!(
            count(&pool, &event, "events_started", DUAL).await,
            0,
            "a community opt-out drops the start notice even with an RSVP"
        );
        assert_eq!(count(&pool, &event, "events_started", OPTED).await, 0);
        let hosted: Value = sqlx::query_scalar("SELECT metadata FROM social.notifications WHERE metadata->>'eventId'=$1 AND type='events_started' AND address=$2")
            .bind(&event).bind(HOST).fetch_one(&pool).await.unwrap();
        assert_eq!(
            hosted["communityThumbnail"],
            format!("{CDN}/social/communities/{community}/raw-thumbnail.png")
        );
        let (state,visible): (String,bool) = sqlx::query_as("SELECT d.state,n.in_app FROM social.notifications n JOIN social.notification_deliveries d ON d.notification_id=n.id WHERE n.metadata->>'eventId'=$1 AND n.type='events_starts_soon' AND n.address=$2")
            .bind(&event).bind(DUAL).fetch_one(&pool).await.unwrap();
        assert_eq!(state, "skipped");
        assert!(visible, "email opt-out preserves in-app delivery");
        let metadata: Value = sqlx::query_scalar("SELECT metadata FROM social.notifications WHERE metadata->>'eventId'=$1 AND type='events_started' AND address=$2")
            .bind(&event).bind(RSVP).fetch_one(&pool).await.unwrap();
        assert!(
            metadata.get("communityId").is_none(),
            "an RSVP from a non-member keeps the generic notice"
        );
        assert_eq!(
            metadata["endsAt"], "2099-06-01T19:00:00.000Z",
            "occurrence duration, not series finish"
        );
        assert_eq!(
            metadata["link"],
            format!("https://decentraland.org/jump/events?id={event}&position=0%2C0"),
            "`Notifications/index.ts:85`: the jump-in link"
        );
        run_once(
            &pool,
            &origins(base, "https://events.decentraland.org"),
            soon + Span::days(1),
        )
        .await
        .unwrap();
        assert_eq!(
            count(&pool, &event, "events_starts_soon", RSVP).await,
            2,
            "each occurrence has a distinct idempotency key"
        );

        moderation::moderate(
            &pool,
            &event,
            &AdminPatch {
                rejected: Some(true),
                reason: Some("Reason".into()),
                ..Default::default()
            },
            "moderator",
            &catalyst_explore::Settings::for_tests(),
        )
        .await
        .unwrap();
        run_once(
            &pool,
            &origins(base, "https://events.decentraland.org"),
            soon + Span::days(1),
        )
        .await
        .unwrap();
        assert_eq!(count(&pool, &event, "event_rejected", HOST).await, 1);

        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.event_notification_intents WHERE event_id=$1",
        )
        .bind(&event)
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        crate::explore::events::notifications::record_transition(
            &mut tx,
            &event,
            None,
            &json!({"approved":true,"user":HOST}),
            true,
            &catalyst_explore::Settings::for_tests(),
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        let after: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.event_notification_intents WHERE event_id=$1",
        )
        .bind(&event)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            queued, after,
            "rolled-back event transitions leave no delivery intent"
        );

        // A high-degree audience advances in bounded pages. A restart or
        // concurrent worker resumes after the committed recipient key.
        let body: CreateEvent = serde_json::from_value(json!({"name":"Paged","start_at":"2100-01-01T12:00:00Z","duration":3600000,"x":0,"y":0})).unwrap();
        let paged = moderation::create_as(
            &pool,
            &body,
            HOST,
            HOST,
            &catalyst_explore::Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE explore.event SET approved=true,raw=raw || '{\"approved\":true}'::jsonb WHERE id=$1")
            .bind(&paged).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO explore.event_attendance_local (event_id,signer,action) SELECT $1,'0x'||lpad(to_hex(n),40,'0'),'going' FROM generate_series(1000,1104) n")
            .bind(&paged).execute(&pool).await.unwrap();
        let intent_id = Uuid::new_v4();
        let paged_start = chrono::DateTime::parse_from_rfc3339("2100-01-01T12:00:00Z")
            .unwrap()
            .naive_utc();
        sqlx::query("INSERT INTO social.event_notification_intents (id,event_id,kind,event,local_event,occurrence_start,occurred_at) SELECT $1,id,'events_started',raw,true,$3,$3 FROM explore.event WHERE id=$2")
            .bind(intent_id).bind(&paged).bind(paged_start).execute(&pool).await.unwrap();
        dispatch_page(
            &pool,
            &origins(base, "https://events.decentraland.org"),
            paged_start,
        )
        .await
        .unwrap();
        let pending: bool = sqlx::query_scalar("SELECT processed_at IS NULL AND recipient_after <> '' FROM social.event_notification_intents WHERE id=$1")
            .bind(intent_id).fetch_one(&pool).await.unwrap();
        assert!(pending, "a full page persists its resume key");
        let links = origins(base, "https://events.decentraland.org");
        let (left, right) = tokio::join!(
            dispatch_page(&pool, &links, paged_start),
            dispatch_page(&pool, &links, paged_start)
        );
        left.unwrap();
        right.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE metadata->>'eventId'=$1 AND type='events_started'")
            .bind(&paged).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 105);
        let finished: bool = sqlx::query_scalar(
            "SELECT processed_at IS NOT NULL FROM social.event_notification_intents WHERE id=$1",
        )
        .bind(intent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(finished);
        catch_up_pages_live_audiences_and_retention(&pool).await;
        bounded_work_preserves_fairness_cleanup_and_polling(&pool).await;
        drain_waits_for_an_intent_claimed_by_another_transaction(&pool).await;
        blocked_metrics_preserve_notification_delivery(&pool).await;
    }

    async fn blocked_metrics_preserve_notification_delivery(pool: &PgPool) {
        let id = Uuid::new_v4();
        let event_id = format!("metric-lock-{id}");
        let now = Utc::now().naive_utc();
        let address = format!("0x{}00000000", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at) VALUES($1,$2,'event_approved',$3,false,$4)")
            .bind(id).bind(&event_id).bind(json!({"user":address})).bind(now).execute(pool).await.unwrap();
        let mut held = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE social.event_ended_activation IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *held)
            .await
            .unwrap();
        let result = run_once(
            pool,
            &origins("https://example.org", "https://assets.example.org/events"),
            now,
        )
        .await;
        held.rollback().await.unwrap();
        result.expect("metric lock deferral preserves normal notification cadence");
        assert_eq!(count(pool, &event_id, "event_approved", &address).await, 1);
    }

    async fn drain_waits_for_an_intent_claimed_by_another_transaction(pool: &PgPool) {
        let id = Uuid::new_v4();
        let event_id = format!("claimed-{id}");
        let now = Utc::now().naive_utc();
        let address = format!("0x{}00000000", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at) VALUES($1,$2,'event_approved',$3,false,$4)")
            .bind(id).bind(&event_id).bind(json!({"user":address})).bind(now).execute(pool).await.unwrap();
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM social.event_notification_intents WHERE id=$1 FOR UPDATE")
            .bind(id)
            .execute(&mut *held)
            .await
            .unwrap();
        let claimable: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM social.event_notification_intents WHERE id=$1 FOR UPDATE SKIP LOCKED",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap();
        assert!(
            claimable.is_none(),
            "the regression starts with a held queue claim"
        );
        run_once(pool, &test_links("https://example.org"), now)
            .await
            .unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT processed_at IS NULL FROM social.event_notification_intents WHERE id=$1"
            )
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
        );
        held.rollback().await.unwrap();
        drain_until_complete(pool, now, &event_id, 1).await;
        assert_eq!(count(pool, &event_id, "event_approved", &address).await, 1);
    }

    // The worker deliberately returns when its wall-clock budget expires.
    // Assert eventual completion and durable progress within an elapsed bound.
    // SKIP LOCKED may finish a pass immediately while a cancelled SQL request
    // still holds a claim until its server timeout/rollback completes.
    async fn drain_until_complete(
        pool: &PgPool,
        now: NaiveDateTime,
        event_pattern: &str,
        expected_intents: usize,
    ) {
        let mut previous = std::collections::HashMap::<Uuid, (String, bool)>::new();
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(30);
        let mut passes = 0;
        loop {
            let progress: Vec<(Uuid, String, bool)> = sqlx::query_as(
                "SELECT id,recipient_after,processed_at IS NOT NULL
                 FROM social.event_notification_intents WHERE event_id LIKE $1",
            )
            .bind(event_pattern)
            .fetch_all(pool)
            .await
            .unwrap();
            let mut made_progress = false;
            for (id, cursor, completed) in &progress {
                made_progress |= previous.get(id) != Some(&(cursor.clone(), *completed));
                if let Some((old_cursor, old_completed)) = previous.get(id) {
                    assert!(
                        cursor >= old_cursor,
                        "a committed recipient cursor must not regress"
                    );
                    assert!(
                        !old_completed || *completed,
                        "completed intents must stay completed"
                    );
                }
                previous.insert(*id, (cursor.clone(), *completed));
            }
            if progress.len() == expected_intents && progress.iter().all(|(_, _, done)| *done) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            if !made_progress {
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + Duration::from_millis(10)).min(deadline),
                )
                .await;
            }
            passes += 1;
            match tokio::time::timeout_at(
                deadline,
                run_once(
                    pool,
                    &origins("https://example.org", "https://events.decentraland.org"),
                    now,
                ),
            )
            .await
            {
                Ok(result) => {
                    result.unwrap();
                }
                Err(_) => break,
            }
        }
        let matching: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(q) - 'event' FROM social.event_notification_intents q WHERE event_id LIKE $1 ORDER BY kind,occurrence_start,id")
            .bind(event_pattern).fetch_all(pool).await.unwrap();
        let pending: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(q) - 'event' FROM social.event_notification_intents q WHERE processed_at IS NULL ORDER BY dispatch_at,id LIMIT 20")
            .fetch_all(pool).await.unwrap();
        let (database_clock, timezone, isolation): (NaiveDateTime, String, String) = sqlx::query_as(
            "SELECT timezone('UTC', clock_timestamp()), current_setting('TimeZone'), current_setting('transaction_isolation')",
        ).fetch_one(pool).await.unwrap();
        let actual_intents = matching.len();
        let elapsed = started.elapsed();
        panic!(
            "event notification queue did not complete within 30s: elapsed={elapsed:?}, passes={passes}, pattern={event_pattern}, expected={expected_intents}, actual={actual_intents}, horizon={now}, database_clock={database_clock}, timezone={timezone}, isolation={isolation}, matching={matching:?}, pending_global={pending:?}"
        );
    }

    async fn catch_up_pages_live_audiences_and_retention(pool: &PgPool) {
        let now = chrono::DateTime::parse_from_rfc3339("2101-06-01T18:00:00Z")
            .unwrap()
            .naive_utc();
        let prefix = Uuid::new_v4().to_string();
        let address = format!("0x{}", Uuid::new_v4().simple())
            .chars()
            .chain("00000000".chars())
            .collect::<String>();
        // 125 simultaneous starts with local consent, 1,000 live catalogue
        // rows without local consent, and 1,000 expired rows with consent.
        sqlx::query("INSERT INTO explore.event (id,name,approved,start_at,finish_at,next_finish_at,duration_ms,raw)
            SELECT $1 || ':' || n,'Catchup',true,$2,$2 + interval '1 hour',$2 + interval '1 hour',3600000,
                jsonb_build_object('name','Catchup','approved',true,'start_at',$2::timestamp,'duration',3600000,'user',$3)
            FROM generate_series(1,1125) n")
            .bind(&prefix).bind(now-Span::minutes(10)).bind(&address).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO explore.event (id,name,approved,start_at,finish_at,next_finish_at,duration_ms,raw)
            SELECT $1 || ':' || n,'Expired',true,$2,$2 + interval '1 hour',$2 + interval '1 hour',3600000,
                jsonb_build_object('name','Expired','approved',true,'start_at',$2::timestamp,'duration',3600000,'user',$3)
            FROM generate_series(1126,2125) n")
            .bind(&prefix).bind(now-Span::hours(3)).bind(&address).execute(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO explore.event_attendance_local(event_id,signer,action)
            SELECT $1 || ':' || n,$2,'going' FROM generate_series(1,2125) n WHERE n<=125 OR n>1125",
        )
        .bind(&prefix)
        .bind(&address)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE explore.event")
            .execute(pool)
            .await
            .unwrap();
        let plan: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
            SCAN_SQL.as_str()
        )))
        .bind("events_started")
        .bind(0_i32)
        .bind(now)
        .bind(now - Span::hours(1))
        .bind(PAGE)
        .fetch_one(pool)
        .await
        .unwrap();
        eprintln!(
            "event scan smoke: 2125 rows, 125 eligible; execution_ms={}, plan={}",
            plan[0]["Execution Time"], plan[0]["Plan"]
        );
        let before = std::time::Instant::now();
        drain_until_complete(pool, now, &format!("{prefix}:%"), 125).await;
        let notices: i64=sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE metadata->>'eventId' LIKE $1 AND type='events_started'")
            .bind(format!("{prefix}:%")).fetch_one(pool).await.unwrap();
        assert_eq!(
            notices, 125,
            "all due-time ties across scan pages drain exactly once"
        );
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.event_notification_intents WHERE event_id LIKE $1",
        )
        .bind(format!("{prefix}:%"))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            queued, 125,
            "expired and no-audience catalogue rows never enter the queue"
        );
        eprintln!(
            "event queue smoke: drained 125 recipients in {:?}",
            before.elapsed()
        );

        // A large community resumes after a partially committed page and an
        // additional short-budget pass. Runner speed must not determine correctness.
        let large_community = Uuid::new_v4();
        let large_event = format!("{prefix}:large");
        sqlx::query("INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Large',$2)")
            .bind(large_community)
            .bind(&address)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO social.community_members(community_id,member_address,role)
            SELECT $1,'0x'||lpad(to_hex(n),40,'0'),'member' FROM generate_series(20000,30004) n",
        )
        .bind(large_community)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO explore.event(id,name,approved,origin,community_id,start_at,finish_at,duration_ms,raw)
            VALUES($1,'Large',true,'local',$2,$3,$3+interval '1 hour',3600000,
            jsonb_build_object('name','Large','approved',true,'community_id',$2::text,'start_at',$3::timestamp,'duration',3600000,'user',$4::text))")
            .bind(&large_event).bind(large_community.to_string()).bind(now+Span::hours(2)).bind(&address).execute(pool).await.unwrap();
        let large_intent = Uuid::new_v4();
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at)
            SELECT $1,id,'event_created',raw,true,$3 FROM explore.event WHERE id=$2")
            .bind(large_intent).bind(&large_event).bind(now).execute(pool).await.unwrap();
        let large_before = std::time::Instant::now();
        dispatch_page(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap();
        let (cursor, completed): (String, bool) = sqlx::query_as(
            "SELECT recipient_after,processed_at IS NOT NULL FROM social.event_notification_intents WHERE id=$1",
        ).bind(large_intent).fetch_one(pool).await.unwrap();
        assert!(
            !cursor.is_empty() && !completed,
            "one page must persist partial progress"
        );
        if let Ok(result) = tokio::time::timeout(
            Duration::from_millis(1),
            run_once(
                pool,
                &origins("https://example.org", "https://events.decentraland.org"),
                now,
            ),
        )
        .await
        {
            result.unwrap();
        }
        drain_until_complete(pool, now, &large_event, 1).await;
        let large_count:i64=sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE metadata->>'eventId'=$1 AND type='event_created'")
            .bind(&large_event).fetch_one(pool).await.unwrap();
        assert_eq!(large_count, 10005);
        let distinct_recipients: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT address) FROM social.notifications WHERE metadata->>'eventId'=$1 AND type='event_created'",
        ).bind(&large_event).fetch_one(pool).await.unwrap();
        assert_eq!(
            distinct_recipients, large_count,
            "resuming never duplicates recipient delivery"
        );
        eprintln!(
            "event queue smoke: drained 10005-member community in {:?}",
            large_before.elapsed()
        );

        // An approval or new mirror arriving after the scan has passed its due
        // time still gets a live started notice, and a late reminder is caught.
        for (suffix, start) in [
            ("late-start", now - Span::minutes(5)),
            ("late-reminder", now + Span::minutes(20)),
        ] {
            let id = format!("{prefix}:{suffix}");
            let body = json!({"id":id,"name":"Late","approved":false,"start_at":start.and_utc().to_rfc3339(),"finish_at":(start+Span::hours(1)).and_utc().to_rfc3339(),"duration":3600000});
            crate::explore::events::mirror::upsert_event(pool, &body)
                .await
                .unwrap();
            crate::explore::events::attendees::rsvp_going(pool, &id, &address, None)
                .await
                .unwrap();
            run_once(
                pool,
                &origins("https://example.org", "https://events.decentraland.org"),
                now,
            )
            .await
            .unwrap();
            sqlx::query("UPDATE explore.event SET approved=true,raw=raw || '{\"approved\":true}'::jsonb WHERE id=$1").bind(&id).execute(pool).await.unwrap();
            run_once(
                pool,
                &origins("https://example.org", "https://events.decentraland.org"),
                now,
            )
            .await
            .unwrap();
            let kind = if suffix == "late-start" {
                "events_started"
            } else {
                "events_starts_soon"
            };
            assert_eq!(count(pool, &id, kind, &address).await, 1);
        }
        // Moving an already-scanned future event earlier creates a fresh due
        // occurrence, without depending on a monotonically advancing cursor.
        let moved = format!("{prefix}:moved");
        let start = now + Span::hours(3);
        let mut body = json!({"id":moved,"name":"Moved","approved":true,"start_at":start.and_utc().to_rfc3339(),"finish_at":(start+Span::hours(1)).and_utc().to_rfc3339(),"duration":3600000});
        crate::explore::events::mirror::upsert_event(pool, &body)
            .await
            .unwrap();
        crate::explore::events::attendees::rsvp_going(pool, &moved, &address, None)
            .await
            .unwrap();
        run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap();
        let old_intent = Uuid::new_v4();
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurrence_start,occurred_at) VALUES($1,$2,'events_starts_soon',$3,false,$4,$5)")
            .bind(old_intent).bind(&moved).bind(&body).bind(start).bind(now).execute(pool).await.unwrap();
        body["start_at"] = json!((now + Span::minutes(30)).and_utc().to_rfc3339());
        body["finish_at"] = json!((now + Span::minutes(90)).and_utc().to_rfc3339());
        crate::explore::events::mirror::upsert_event(pool, &body)
            .await
            .unwrap();
        run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap();
        assert_eq!(count(pool, &moved, "events_starts_soon", &address).await, 1);

        // A queued started notice can become stale during an outage, and is
        // consumed without producing either an in-app row or email delivery.
        let stale_id = Uuid::new_v4();
        let stale_event = format!("{prefix}:1126");
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurrence_start,occurred_at)
            SELECT $1,id,'events_started',raw,false,$3,$3 FROM explore.event WHERE id=$2")
            .bind(stale_id).bind(&stale_event).bind(now-Span::hours(3)).execute(pool).await.unwrap();
        run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap();
        assert_eq!(
            count(pool, &stale_event, "events_started", &address).await,
            0
        );
        let finished: bool = sqlx::query_scalar(
            "SELECT processed_at IS NOT NULL FROM social.event_notification_intents WHERE id=$1",
        )
        .bind(stale_id)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(finished);
        let recent = Uuid::new_v4();
        let old = Uuid::new_v4();
        let pending = Uuid::new_v4();
        for (id, processed, occurred) in [
            (recent, Some(now), now - Span::days(40)),
            (old, Some(now - Span::days(40)), now - Span::days(40)),
            (pending, None, now - Span::days(40)),
        ] {
            sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at,processed_at) VALUES($1,'retention','event_approved','{}',false,$2,$3)")
                .bind(id).bind(occurred).bind(processed).execute(pool).await.unwrap();
        }
        cleanup(pool, now).await.unwrap();
        let retained: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM social.event_notification_intents WHERE id=ANY($1)")
                .bind(vec![recent, old, pending])
                .fetch_all(pool)
                .await
                .unwrap();
        assert!(retained.contains(&recent) && retained.contains(&pending));
        assert!(!retained.contains(&old));
        run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now + Span::days(40),
        )
        .await
        .unwrap();
        assert_eq!(
            count(pool, &stale_event, "events_started", &address).await,
            0,
            "retention never makes old occurrences eligible again"
        );
    }

    async fn bounded_work_preserves_fairness_cleanup_and_polling(pool: &PgPool) {
        let now = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
            .unwrap()
            .naive_utc();
        let address = format!("0x{}00000000", Uuid::new_v4().simple());
        let prefix = Uuid::new_v4().simple().to_string();
        async fn add_event(pool: &PgPool, id: &str, address: &str, start: NaiveDateTime) {
            let raw = json!({"id":id,"name":"Fair event","approved":true,"start_at":start.and_utc().to_rfc3339(),"finish_at":(start+Span::hours(1)).and_utc().to_rfc3339(),"duration":3600000,"user":address});
            crate::explore::events::mirror::upsert_event(pool, &raw)
                .await
                .unwrap();
            crate::explore::events::attendees::rsvp_going(pool, id, address, None)
                .await
                .unwrap();
        }
        let slow = format!("{prefix}-slow");
        let fast = format!("{prefix}-fast");
        let start = now - Span::minutes(10);
        add_event(pool, &slow, &address, start).await;
        let slow_intent = occurrence_id(&slow, start, "events_started");
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurrence_start,occurred_at,dispatch_at)
            SELECT $1,id,'events_started',raw,false,$3,$3,TIMESTAMP '1970-01-01' FROM explore.event WHERE id=$2")
            .bind(slow_intent).bind(&slow).bind(start).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at,dispatch_at)
            VALUES($1,$2,'event_approved',$3,false,$4,TIMESTAMP '1970-01-02')")
            .bind(Uuid::new_v4()).bind(&fast).bind(json!({"name":"Fast","user":address})).bind(now).execute(pool).await.unwrap();
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM explore.event WHERE id=$1 FOR UPDATE")
            .bind(&slow)
            .execute(&mut *held)
            .await
            .unwrap();
        let error = dispatch_page(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap_err();
        assert!(
            timed_out(&error),
            "PostgreSQL must cancel the blocked page: {error}"
        );
        let rotated:bool=sqlx::query_scalar("SELECT dispatch_at > TIMESTAMP '1970-01-02' AND recipient_after='' AND processed_at IS NULL FROM social.event_notification_intents WHERE id=$1")
            .bind(slow_intent).fetch_one(pool).await.unwrap();
        assert!(
            rotated,
            "claim rotation commits even when the delivery transaction rolls back"
        );
        dispatch_page(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap();
        assert_eq!(
            count(pool, &fast, "event_approved", &address).await,
            1,
            "a blocked page cannot monopolize the queue"
        );
        let expired = Uuid::new_v4();
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at,processed_at)
            VALUES($1,'expired','event_approved','{}',false,$2,$2)")
            .bind(expired).bind(now-Span::days(40)).execute(pool).await.unwrap();
        run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap();
        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM social.event_notification_intents WHERE id=$1)",
        )
        .bind(expired)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(
            !retained,
            "cleanup runs even while a blocked pending intent exhausts the dispatch budget"
        );
        held.rollback().await.unwrap();
        let expired_now = now + Span::hours(2);
        let intent: Intent = sqlx::query_as("SELECT id,event_id,kind,event,local_event,occurrence_start,occurred_at,recipient_after FROM social.event_notification_intents WHERE id=$1")
            .bind(slow_intent).fetch_one(pool).await.unwrap();
        assert_eq!(
            suppression_reason(&intent, expired_now, false),
            Some("event_ended")
        );
        drain_until_complete(pool, expired_now, &slow, 1).await;
        assert_eq!(
            count(pool, &slow, "events_started", &address).await,
            0,
            "contention followed by expiry records suppression rather than delivering stale work"
        );

        let baseline: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(pool)
            .await
            .unwrap();
        let mut tx = timed_transaction(pool).await.unwrap();
        let error = sqlx::query("SELECT pg_sleep(2)")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .as_database_error()
                .and_then(|db| db.code())
                .as_deref(),
            Some("57014")
        );
        tx.rollback().await.unwrap();
        let restored: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            restored, baseline,
            "server timeouts do not leak into pooled callers"
        );

        // Delay only reminder inserts on this fixture. The independent start
        // scanner still commits while PostgreSQL cancels the slow reminder SQL.
        let reminder = format!("{prefix}-reminder");
        let started = format!("{prefix}-started");
        add_event(pool, &reminder, &address, now + Span::minutes(10)).await;
        add_event(pool, &started, &address, now - Span::minutes(10)).await;
        let function = format!("delay_reminder_{prefix}");
        let trigger = format!("delay_reminder_trigger_{prefix}");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION social.{function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.event_id='{reminder}' AND NEW.kind='events_starts_soon' THEN PERFORM pg_sleep(4); END IF; RETURN NEW; END $$")))
            .execute(pool).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER {trigger} BEFORE INSERT ON social.event_notification_intents FOR EACH ROW EXECUTE FUNCTION social.{function}()")))
            .execute(pool).await.unwrap();
        let result = run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await;
        // Unlike deadline deferrals, an ordinary scan database failure remains
        // a failed job, but queued host notices still receive their turn.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE OR REPLACE FUNCTION social.{function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.event_id='{reminder}' AND NEW.kind='events_starts_soon' THEN RAISE EXCEPTION 'test scan failure'; END IF; RETURN NEW; END $$"))).execute(pool).await.unwrap();
        let queued_event = format!("{prefix}-scan-error-delivery");
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at) VALUES($1,$2,'event_approved',$3,false,$4)")
            .bind(Uuid::new_v4()).bind(&queued_event).bind(json!({"user":address})).bind(now).execute(pool).await.unwrap();
        let failed_scan = run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TRIGGER {trigger} ON social.event_notification_intents"
        )))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP FUNCTION social.{function}()"
        )))
        .execute(pool)
        .await
        .unwrap();
        result.expect("scan timeout must keep the normal delivery cadence");
        assert!(
            failed_scan
                .unwrap_err()
                .to_string()
                .contains("events_starts_soon occurrence scan failed")
        );
        assert_eq!(
            count(pool, &queued_event, "event_approved", &address).await,
            1
        );
        assert_eq!(count(pool, &started, "events_started", &address).await, 1);
        assert_eq!(
            count(pool, &reminder, "events_starts_soon", &address).await,
            0
        );
        let from = (now - Span::minutes(5)).and_utc().timestamp_millis();
        run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await
        .unwrap();
        let store = super::super::NotificationsStore::new(pool.clone());
        let fresh = store
            .list(&address.parse().unwrap(), 100, Some(from), false)
            .await
            .unwrap();
        let reminder_notice = fresh
            .iter()
            .find(|item| item.metadata["eventId"] == reminder)
            .expect("incremental polling must see a newly delivered catch-up reminder");
        assert_eq!(reminder_notice.metadata["title"], "Event starts in an hour");
        assert_eq!(
            reminder_notice.metadata["occurredAt"],
            (now - Span::minutes(50)).and_utc().to_rfc3339()
        );

        // Community opt-outs use one canonical UUID representation even if a
        // legacy event stores its association with uppercase hex digits.
        let community = Uuid::new_v4();
        let community_event = format!("{prefix}-community");
        sqlx::query(
            "INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Canonical',$2)",
        )
        .bind(community)
        .bind(&address)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) VALUES($1,$2,'member')").bind(community).bind(&address).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO social.subscription_opt_outs(address,scope,scope_id) VALUES($1,'community',$2)").bind(&address).bind(community.to_string().to_uppercase()).execute(pool).await.unwrap();
        let upper = community.to_string().to_uppercase();
        sqlx::query("INSERT INTO explore.event(id,name,approved,origin,community_id,start_at,finish_at,duration_ms,raw)
            VALUES($1,'Canonical',true,'local',$2,$3,$3+interval '1 hour',3600000,
            jsonb_build_object('name','Canonical','approved',true,'community_id',$2::text,'start_at',$3::timestamp,'duration',3600000))")
            .bind(&community_event).bind(&upper).bind(now-Span::minutes(5)).execute(pool).await.unwrap();
        scan_kind(pool, "events_started", 0, now).await.unwrap();
        let queued: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM social.event_notification_intents WHERE event_id=$1)",
        )
        .bind(&community_event)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(
            !queued,
            "canonical opt-out prevents noncanonical event IDs creating empty work"
        );
        // An already queued notice must also honor a legacy uppercase opt-out.
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at) SELECT $1,id,'event_created',raw,true,$3 FROM explore.event WHERE id=$2")
            .bind(Uuid::new_v4()).bind(&community_event).bind(now).execute(pool).await.unwrap();
        drain_until_complete(pool, now, &community_event, 1).await;
        assert_eq!(
            count(pool, &community_event, "event_created", &address).await,
            0
        );
        let store = super::super::NotificationsStore::new(pool.clone());
        let parsed_address = address.parse().unwrap();
        assert!(
            store
                .is_opted_out(&parsed_address, "community", &community.to_string())
                .await
                .unwrap()
        );
        assert!(
            store
                .delete_opt_out(&parsed_address, "community", &community.to_string())
                .await
                .unwrap()
        );
        store
            .create_opt_out(
                &parsed_address,
                "community",
                &community.to_string().to_uppercase(),
            )
            .await
            .unwrap();
        let stored: String = sqlx::query_scalar("SELECT scope_id FROM social.subscription_opt_outs WHERE address=$1 AND scope='community'")
            .bind(&address).fetch_one(pool).await.unwrap();
        assert_eq!(stored, community.to_string());
        assert!(
            store
                .delete_opt_out(
                    &parsed_address,
                    "community",
                    &community.to_string().to_uppercase()
                )
                .await
                .unwrap()
        );
        // Ordinary maintenance failures must not block already queued delivery.
        let cleanup_event = format!("{prefix}-cleanup-error");
        let delivery_event = format!("{prefix}-cleanup-delivery");
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at,processed_at) VALUES($1,$2,'event_approved','{}',false,$3,$3)")
            .bind(Uuid::new_v4()).bind(&cleanup_event).bind(now-Span::days(40)).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurred_at) VALUES($1,$2,'event_approved',$3,false,$4)")
            .bind(Uuid::new_v4()).bind(&delivery_event).bind(json!({"name":"Cleanup delivery","user":address})).bind(now).execute(pool).await.unwrap();
        let function = format!("fail_cleanup_{prefix}");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION social.{function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.event_id='{cleanup_event}' THEN RAISE EXCEPTION 'test cleanup failure'; END IF; RETURN OLD; END $$"))).execute(pool).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER {function} BEFORE DELETE ON social.event_notification_intents FOR EACH ROW EXECUTE FUNCTION social.{function}()"))).execute(pool).await.unwrap();
        let result = run_once(
            pool,
            &origins("https://example.org", "https://events.decentraland.org"),
            now,
        )
        .await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TRIGGER {function} ON social.event_notification_intents"
        )))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP FUNCTION social.{function}()"
        )))
        .execute(pool)
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(
            count(pool, &delivery_event, "event_approved", &address).await,
            1
        );
        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM social.event_notification_intents WHERE event_id=$1)",
        )
        .bind(&cleanup_event)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(
            retained,
            "failed cleanup rolls back without preventing delivery"
        );
        // A pass-start horizon must not permit a notice that expires during work.
        let expired_reminder = format!("{prefix}-expired-during-pass");
        let event_start = now - Span::minutes(1);
        add_event(pool, &expired_reminder, &address, event_start).await;
        sqlx::query("INSERT INTO social.event_notification_intents(id,event_id,kind,event,local_event,occurrence_start,occurred_at) SELECT $1,id,'events_starts_soon',raw,false,$3,$3-interval '1 hour' FROM explore.event WHERE id=$2")
            .bind(Uuid::new_v4()).bind(&expired_reminder).bind(event_start).execute(pool).await.unwrap();
        drain_until_complete(pool, now - Span::minutes(2), &expired_reminder, 1).await;
        assert_eq!(
            count(pool, &expired_reminder, "events_starts_soon", &address).await,
            0
        );
        // A real PostgreSQL lock timeout takes the same observable deferral
        // path as statement cancellation, without backing off delivery.
        let lock_event = format!("{prefix}-scan-lock");
        add_event(pool, &lock_event, &address, now + Span::minutes(10)).await;
        let mut held = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE social.event_notification_intents IN SHARE MODE")
            .execute(&mut *held)
            .await
            .unwrap();
        let error = scan_kind(pool, "events_starts_soon", 3600, now)
            .await
            .unwrap_err();
        let database_error = error
            .downcast_ref::<sqlx::Error>()
            .unwrap()
            .as_database_error()
            .unwrap();
        assert_eq!(database_error.code().as_deref(), Some("55P03"));
        scan_with_budget(pool, "events_starts_soon", 3600, now)
            .await
            .expect("lock timeout preserves normal cadence");
        held.rollback().await.unwrap();
        drain_until_complete(pool, now, &lock_event, 1).await;
        assert_eq!(
            count(pool, &lock_event, "events_starts_soon", &address).await,
            1
        );
        // This fixture's wall-clock events (a local community one among them)
        // would otherwise count as endings in the metric tests of this process.
        sqlx::query("DELETE FROM explore.event WHERE id LIKE $1")
            .bind(format!("{prefix}-%"))
            .execute(pool)
            .await
            .unwrap();
    }

    fn intent(kind: &str, event: Value) -> Intent {
        Intent {
            id: Uuid::new_v4(),
            event_id: "legacy-event".into(),
            kind: kind.into(),
            event,
            local_event: true,
            occurrence_start: None,
            occurred_at: Utc::now().naive_utc(),
            recipient_after: String::new(),
        }
    }

    fn recipient(community: bool, community_thumbnail: bool) -> Recipient {
        Recipient {
            address: "0x1111111111111111111111111111111111111111".into(),
            community,
            community_name: community.then(|| "Club".to_string()),
            community_thumbnail,
        }
    }

    #[test]
    fn queued_relative_defaults_use_the_asset_origin_and_keep_the_notice_link_origin() {
        let payload = metadata(
            &intent(
                "event_approved",
                json!({"name":"Legacy","image":catalyst_explore::events::posters::DEFAULT_EVENT_IMAGE}),
            ),
            &recipient(false, false),
            &origins(
                "https://web.example.org",
                "https://assets.example.org/events/",
            ),
        );
        assert_eq!(
            payload["image"],
            "https://assets.example.org/events/assets/event-default.webp"
        );
        assert_eq!(
            payload["link"], "https://decentraland.org/jump/events?id=legacy-event&position=0%2C0",
            "an approval links the jump-in site (`Notifications/index.ts:205`)"
        );
        assert!(payload.get("attendee").is_none());
        assert_eq!(
            metadata(
                &intent("event_rejected", json!({"name":"Legacy"})),
                &recipient(false, false),
                &origins(
                    "https://web.example.org",
                    "https://assets.example.org/events/"
                ),
            )["link"],
            "https://web.example.org/whats-on/legacy-event",
            "notices upstream sent without a link keep this node's event page"
        );
    }

    #[test]
    fn host_and_community_notices_carry_the_upstream_parser_fields() {
        let links = origins(
            "https://web.example.org/",
            "https://assets.example.org/events",
        );
        let community = "0F8E1A3C-4B5D-4E6F-8A9B-0C1D2E3F4A5B";
        let event = json!({"name":"Party","description":"Bring friends","community_id":community,"rejection_reason":"Spam"});
        for kind in ["event_approved", "event_rejected", "event_deleted"] {
            let payload = metadata(
                &intent(kind, event.clone()),
                &recipient(false, false),
                &links,
            );
            assert_eq!(
                payload["myHangouts"], "https://web.example.org/whats-on?tab=my",
                "{kind}"
            );
            assert_eq!(payload["title"], "Party", "{kind}");
            assert_eq!(payload["description"], "Bring friends", "{kind}");
        }
        let rejected = metadata(
            &intent("event_rejected", event.clone()),
            &recipient(false, false),
            &links,
        );
        assert_eq!(rejected["reason"], "Spam");
        let deleted = metadata(
            &intent("event_deleted", event.clone()),
            &recipient(false, false),
            &links,
        );
        assert!(
            deleted.get("reason").is_none(),
            "absent delete reason is omitted"
        );
        let mut with_reason = event.clone();
        with_reason["deleted_reason"] = json!("Duplicate");
        let deleted = metadata(
            &intent("event_deleted", with_reason),
            &recipient(false, false),
            &links,
        );
        assert_eq!(deleted["reason"], "Duplicate");

        let thumbnail = format!(
            "{CDN}/social/communities/{}/raw-thumbnail.png",
            community.to_lowercase()
        );
        for kind in ["event_created", "events_started"] {
            let payload = metadata(&intent(kind, event.clone()), &recipient(true, true), &links);
            assert_eq!(payload["communityThumbnail"], thumbnail, "{kind}");
            assert!(payload.get("myHangouts").is_none(), "{kind}");
            let bare = metadata(
                &intent(kind, event.clone()),
                &recipient(true, false),
                &links,
            );
            assert!(
                bare.get("communityThumbnail").is_none(),
                "{kind} without thumbnail"
            );
        }
        let generic = metadata(
            &intent("events_started", event),
            &recipient(false, true),
            &links,
        );
        assert!(
            generic.get("communityThumbnail").is_none(),
            "an RSVP-only notice is not community-scoped"
        );
    }

    #[test]
    fn notice_text_carries_the_event_name_without_rich_text_tags() {
        let links = test_links("https://example.org");
        let event =
            json!({"name":"Party <link=\"file:///x\">here</link><size=300>","description":"d"});
        let reminder = metadata(
            &intent("events_starts_soon", event.clone()),
            &recipient(false, false),
            &links,
        );
        assert_eq!(
            reminder["description"],
            "The event Party here starts in an hour."
        );
        assert_eq!(reminder["name"], "Party here");
        let approved = metadata(
            &intent("event_approved", event),
            &recipient(false, false),
            &links,
        );
        assert_eq!(approved["title"], "Party here");
        let plain = metadata(
            &intent("event_approved", json!({"name":"Launch"})),
            &recipient(false, false),
            &links,
        );
        assert_eq!(plain["title"], "Launch");
        let tags_only = json!({"name":"<size=300><b>","description":"d"});
        let reminder = metadata(
            &intent("events_starts_soon", tags_only.clone()),
            &recipient(false, false),
            &links,
        );
        assert_eq!(reminder["description"], "The event starts in an hour.");
        let approved = metadata(
            &intent("event_approved", tags_only.clone()),
            &recipient(false, false),
            &links,
        );
        assert_eq!(approved["title"], "Event");
        let mut member = recipient(true, false);
        member.community_name = Some("<b></b>".into());
        let started = metadata(&intent("events_started", tags_only), &member, &links);
        assert_eq!(
            started["description"],
            "A community event is about to start."
        );
    }

    /// `Notifications/index.ts:44-68,73-110`: the reminder and start titles,
    /// descriptions, jump-in links (`position` and `realm`) and `attendee`.
    #[test]
    fn reminder_and_start_notices_follow_upstream_titles_and_links() {
        let start = chrono::DateTime::parse_from_rfc3339("2099-06-01T18:00:00Z")
            .unwrap()
            .naive_utc();
        let links = test_links("https://example.org");
        let intent = Intent {
            id: Uuid::new_v4(),
            event_id: "ev-1".into(),
            kind: "events_starts_soon".into(),
            event: json!({"name":"Launch","x":10,"y":-20,"server":"world.dcl.eth","duration":3600000}),
            local_event: true,
            occurrence_start: Some(start),
            occurred_at: start - Span::hours(1),
            recipient_after: String::new(),
        };
        let attendee = Recipient {
            address: "0x1111111111111111111111111111111111111111".into(),
            community: false,
            community_name: None,
            community_thumbnail: false,
        };
        let reminder = metadata(&intent, &attendee, &links);
        assert_eq!(reminder["title"], "Event starts in an hour");
        assert_eq!(
            reminder["description"],
            "The event Launch starts in an hour."
        );
        assert_eq!(
            reminder["link"],
            "https://decentraland.org/jump/events?id=ev-1&position=10%2C-20&realm=world.dcl.eth"
        );
        assert_eq!(reminder["attendee"], attendee.address);
        assert_eq!(reminder["startsAt"], "2099-06-01T18:00:00.000Z");
        assert_eq!(reminder["endsAt"], "2099-06-01T19:00:00.000Z");
        let started = Intent {
            kind: "events_started".into(),
            ..intent
        };
        let plain = metadata(&started, &attendee, &links);
        assert_eq!(plain["title"], "Event started");
        assert_eq!(plain["description"], "The event Launch has begun!");
        assert_eq!(
            plain["link"],
            "https://decentraland.org/jump/events?id=ev-1&position=10%2C-20&realm=world.dcl.eth"
        );
        let member = Recipient {
            address: "0x2222222222222222222222222222222222222222".into(),
            community: true,
            community_name: Some("Builders".into()),
            community_thumbnail: false,
        };
        let community = metadata(&started, &member, &links);
        assert_eq!(community["title"], "Community Event starting");
        assert_eq!(
            community["description"],
            "A Builders event is about to start."
        );
        assert_eq!(community["communityName"], "Builders");
        assert_eq!(community["attendee"], member.address);
    }

    /// `Event/cron.ts:51-83`: an occurrence starting within the next hour is
    /// reminded once, a later one is not yet, a started one never; the
    /// reminder's Slack notice (`:76`) is queued with the attendee count only
    /// while the channel is on.
    #[tokio::test]
    async fn reminder_window_and_upcoming_slack_notice() {
        let _guard = super::super::event_ended::TEST_LOCK.lock().await;
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let now = chrono::DateTime::parse_from_rfc3339("2102-03-01T12:00:00Z")
            .unwrap()
            .naive_utc();
        let prefix = Uuid::new_v4().to_string();
        let address = format!("0x{}00000000", Uuid::new_v4().simple());
        let other = format!("0x{}00000000", Uuid::new_v4().simple());
        for (suffix, offset) in [
            ("in-59", Span::minutes(59)),
            ("in-61", Span::minutes(61)),
            ("started", -Span::minutes(1)),
        ] {
            let id = format!("{prefix}:{suffix}");
            let start = now + offset;
            crate::explore::events::mirror::upsert_event(&pool, &json!({
                "id":id,"name":"Window","approved":true,"x":5,"y":6,"user":address,
                "start_at":start.and_utc().to_rfc3339(),"finish_at":(start+Span::hours(1)).and_utc().to_rfc3339(),"duration":3600000
            })).await.unwrap();
            for signer in [&address, &other] {
                crate::explore::events::attendees::rsvp_going(&pool, &id, signer, None)
                    .await
                    .unwrap();
            }
        }
        let mut links = test_links("https://example.org");
        links.slack_notices = true;
        let reminders = |suffix: &str| {
            let (pool, id) = (pool.clone(), format!("{prefix}:{suffix}"));
            async move {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM social.notifications WHERE metadata->>'eventId'=$1 AND type='events_starts_soon'")
                    .bind(id).fetch_one(&pool).await.unwrap()
            }
        };
        let slack = |suffix: &str| {
            let (pool, id) = (pool.clone(), format!("{prefix}:{suffix}"));
            async move {
                sqlx::query_as::<_, (String, Value)>("SELECT kind,payload FROM explore.moderation_hooks WHERE channel='events' AND subject=$1 ORDER BY created_at")
                    .bind(id).fetch_all(&pool).await.unwrap()
            }
        };
        run_once(&pool, &links, now).await.unwrap();
        run_once(&pool, &links, now).await.unwrap();
        assert_eq!(reminders("in-59").await, 2, "both attendees, once");
        assert_eq!(reminders("in-61").await, 0, "not within the hour yet");
        assert_eq!(
            reminders("started").await,
            0,
            "a started event is no reminder"
        );
        let queued = slack("in-59").await;
        assert_eq!(queued.len(), 1, "one upcoming notice per occurrence");
        assert_eq!(queued[0].0, "upcoming");
        assert_eq!(queued[0].1["notifications"], 2);
        assert_eq!(queued[0].1["name"], "Window");
        assert!(slack("started").await.is_empty());
        let link: String = sqlx::query_scalar("SELECT metadata->>'link' FROM social.notifications WHERE metadata->>'eventId'=$1 AND type='events_starts_soon' LIMIT 1")
            .bind(format!("{prefix}:in-59")).fetch_one(&pool).await.unwrap();
        assert_eq!(
            link,
            format!(
                "https://decentraland.org/jump/events?id={}&position=5%2C6",
                urlencoding::encode(&format!("{prefix}:in-59"))
            )
        );
        // Two minutes later the later occurrence crosses the hour line, while
        // the Slack channel is off: the reminder goes out, no notice is queued.
        links.slack_notices = false;
        run_once(&pool, &links, now + Span::minutes(2))
            .await
            .unwrap();
        assert_eq!(reminders("in-61").await, 2);
        assert!(slack("in-61").await.is_empty());
        for suffix in ["in-59", "in-61", "started"] {
            sqlx::query(
                "DELETE FROM explore.moderation_hooks WHERE channel='events' AND subject=$1",
            )
            .bind(format!("{prefix}:{suffix}"))
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("DELETE FROM explore.event WHERE id LIKE $1")
            .bind(format!("{prefix}:%"))
            .execute(&pool)
            .await
            .unwrap();
    }
}
