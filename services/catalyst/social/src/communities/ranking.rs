//! Ranking contracts from social-service-ea b6b2cef, logic/community/ranking.ts,
//! communities-db.ts. Local lifecycle events use durable receipts. Mirrored
//! rankings are never recalculated here.
use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use chrono::{NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

#[derive(Debug, Default, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub events_count: i64,
    pub photos_count: i64,
    pub streams_count: i64,
    pub places_count: i64,
    pub posts_count: i64,
    pub new_members_count: i64,
    pub events_total_attendees: i64,
    pub streams_total_participants: i64,
    pub has_thumbnail: i64,
    pub has_description: i64,
    pub age_in_days: i32,
}

pub fn score(metrics: &Metrics) -> f64 {
    let values = [
        (metrics.has_thumbnail, 1.0_f64, false, 0.15),
        (metrics.has_description, 1.0, false, 0.1),
        (metrics.new_members_count, 50.0, true, 0.2),
        (metrics.events_count, 50.0, true, 0.15),
        (metrics.events_total_attendees, 1000.0, true, 0.1),
        (metrics.posts_count, 100.0, true, 0.1),
        (metrics.places_count, 10.0, false, 0.08),
        (metrics.streams_count, 20.0, true, 0.07),
        (metrics.streams_total_participants, 500.0, true, 0.04),
        (metrics.photos_count, 100.0, true, 0.01),
    ];
    let base: f64 = values
        .into_iter()
        .map(|(value, max, logarithmic, weight)| {
            let value = value.max(0) as f64;
            let normalized = if logarithmic {
                (1.0 + value).ln() / (1.0 + max).ln()
            } else {
                value / max
            };
            normalized.min(1.0) * weight
        })
        .sum();
    let boost = if metrics.age_in_days >= 30 {
        1.0
    } else {
        let by_age = if metrics.age_in_days <= 7 {
            1.15
        } else {
            1.05 + f64::from(30 - metrics.age_in_days) / 23.0 * (1.15 - 1.05)
        };
        (by_age - base * 0.3).max(1.0)
    };
    (base * boost).clamp(0.0, 1.0)
}

fn contribution(value: Option<f64>, maximum: i64) -> i64 {
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.trunc().min(maximum as f64) as i64)
        .unwrap_or(0)
}

const UPDATE: &str = "UPDATE social.communities SET
    ranking_events_count=LEAST(ranking_events_count::bigint+$2,2147483647)::int4,
    ranking_photos_count=LEAST(ranking_photos_count::bigint+$3,2147483647)::int4,
    ranking_streams_count=LEAST(ranking_streams_count::bigint+$4,2147483647)::int4,
    ranking_events_total_attendees=LEAST(ranking_events_total_attendees::bigint+$5,2147483647)::int4,
    ranking_streams_total_participants=LEAST(ranking_streams_total_participants::bigint+$6,2147483647)::int4
    WHERE id=$1";

async fn add(conn: &mut PgConnection, community: Uuid, delta: [i64; 5]) -> sqlx::Result<()> {
    let changed = sqlx::query(UPDATE)
        .bind(community)
        .bind(delta[0])
        .bind(delta[1])
        .bind(delta[2])
        .bind(delta[3])
        .bind(delta[4])
        .execute(conn)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

async fn local_delta(
    conn: &mut PgConnection,
    community: Uuid,
    kind: &str,
    key: &str,
    occurred_at: NaiveDateTime,
    delta: [i64; 5],
) -> sqlx::Result<()> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO social.community_ranking_events(kind,event_key,community_id,occurred_at)
         SELECT $1,$2,id,$4 FROM social.communities WHERE id=$3 AND origin='local'
         ON CONFLICT DO NOTHING RETURNING community_id",
    )
    .bind(kind)
    .bind(key)
    .bind(community)
    .bind(occurred_at)
    .fetch_optional(&mut *conn)
    .await?;
    if inserted.is_some() {
        add(conn, community, delta).await?;
    }
    Ok(())
}

/// Runs in the photo finalizer's transaction. All communities attached to the
/// place receive one contribution, including inactive communities as upstream.
pub async fn record_photo(
    conn: &mut PgConnection,
    key: &str,
    place: &str,
    occurred_at: NaiveDateTime,
) -> sqlx::Result<()> {
    sqlx::query("WITH receipts AS (
        INSERT INTO social.community_ranking_events(kind,event_key,community_id,occurred_at)
        SELECT 'photo',$1,p.community_id,$3 FROM social.community_places p
        JOIN social.communities c ON c.id=p.community_id AND c.origin='local' WHERE p.place_id=$2
        ON CONFLICT DO NOTHING RETURNING community_id)
        UPDATE social.communities c SET ranking_photos_count=LEAST(c.ranking_photos_count::bigint+1,2147483647)::int4
        FROM receipts r WHERE c.id=r.community_id")
        .bind(key).bind(place).bind(occurred_at).execute(conn).await?;
    Ok(())
}

pub(crate) async fn record_stream(
    conn: &mut PgConnection,
    community: Uuid,
    key: &str,
    event: &Value,
    occurred_at: NaiveDateTime,
) -> sqlx::Result<()> {
    let participants = event["metadata"]["totalParticipants"]
        .as_f64()
        .unwrap_or(0.0);
    if participants > 1.0 {
        local_delta(
            conn,
            community,
            "stream",
            key,
            occurred_at,
            [0, 0, 1, 0, contribution(Some(participants), 500)],
        )
        .await?;
    }
    Ok(())
}

const METRICS_SQL: &str = "SELECT c.ranking_events_count::bigint AS events_count,
    c.ranking_photos_count::bigint AS photos_count,c.ranking_streams_count::bigint AS streams_count,
    (SELECT count(*) FROM social.community_places p WHERE p.community_id=c.id) AS places_count,
    (SELECT count(*) FROM social.community_posts p WHERE p.community_id=c.id) AS posts_count,
    (SELECT count(*) FROM social.community_members m WHERE m.community_id=c.id AND m.joined_at >= $2 - interval '7 days') AS new_members_count,
    c.ranking_events_total_attendees::bigint AS events_total_attendees,
    c.ranking_streams_total_participants::bigint AS streams_total_participants,
    (CASE WHEN EXISTS (SELECT 1 FROM social.community_thumbnails t WHERE t.community_id=c.id) THEN 1 ELSE 0 END)::int8 AS has_thumbnail,
    (CASE WHEN trim(c.description)<>'' THEN 1 ELSE 0 END)::int8 AS has_description,($2::date-c.created_at::date) AS age_in_days
    FROM social.communities c WHERE c.id=ANY($1)";

#[derive(sqlx::FromRow)]
struct RankedMetrics {
    id: Uuid,
    #[sqlx(flatten)]
    metrics: Metrics,
}

pub async fn recalculate(pool: &PgPool, now: NaiveDateTime) -> anyhow::Result<u64> {
    let mut after = Uuid::nil();
    let mut total = 0;
    loop {
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM social.communities WHERE origin='local' AND active AND id>$1 ORDER BY id LIMIT 100")
            .bind(after).fetch_all(pool).await?;
        if ids.is_empty() {
            return Ok(total);
        }
        recalculate_ids(pool, &ids, now).await?;
        total += ids.len() as u64;
        after = *ids.last().unwrap();
    }
}

pub(crate) async fn recalculate_ids(
    pool: &PgPool,
    ids: &[Uuid],
    now: NaiveDateTime,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    let query = METRICS_SQL.replacen("SELECT ", "SELECT c.id,", 1);
    let rows: Vec<RankedMetrics> = sqlx::query_as(sqlx::AssertSqlSafe(query))
        .bind(ids)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
    let updates: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let scores: Vec<f32> = rows.iter().map(|row| score(&row.metrics) as f32).collect();
    sqlx::query("UPDATE social.communities c SET ranking_score=v.score,updated_at=$3
        FROM UNNEST($1::uuid[],$2::real[]) v(id,score) WHERE c.id=v.id AND c.origin='local' AND c.active")
        .bind(updates).bind(scores).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// The events producer counts event-attendee rows (events 6a5f33a1,
/// Event/model.ts#getEventsEndingInRangeWithAttendeeCount). Local going rows are
/// authoritative here; a mirrored attendee preview never enters this count.
/// A catalogue cycle revisits edits and newly inserted backdated rows. The
/// recurrence window rotates only after every source-event page has had a turn.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EndedScan {
    after_event: String,
    occurrence_offset: i32,
    maximum_occurrences: i32,
}

const ENDED_EVENT_PAGE_SQL: &str = "WITH page AS MATERIALIZED (
    SELECT id FROM explore.event WHERE id>$1 ORDER BY id LIMIT 100)
    SELECT e.id,CASE WHEN e.origin='local' AND jsonb_typeof(e.raw->'recurrent_dates')='array'
        THEN GREATEST(jsonb_array_length(e.raw->'recurrent_dates'),1) ELSE 1 END
    FROM page JOIN explore.event e ON e.id=page.id ORDER BY e.id";

const ENDED_OCCURRENCES_SQL: &str = "WITH schedules AS MATERIALIZED (
    SELECT c.id AS community_id,e.id AS event_id,e.duration_ms,c.ranking_tracking_since,
        CASE WHEN jsonb_typeof(e.raw->'recurrent_dates')='array'
        AND e.raw->'recurrent_dates'<>'[]'::jsonb THEN e.raw->'recurrent_dates'
        ELSE jsonb_build_array(e.raw->'start_at') END AS dates
    FROM explore.event e JOIN social.communities c ON c.id=CASE WHEN pg_input_is_valid(e.community_id,'uuid') THEN e.community_id::uuid END
    WHERE e.id=ANY($2) AND e.origin='local' AND c.origin='local' AND e.approved AND e.duration_ms>=0
    AND e.finish_at>=c.ranking_tracking_since AND e.start_at<=$1
    AND COALESCE(e.raw->>'rejected','false')<>'true'
    AND COALESCE(e.raw->>'deleted_by_user','false')<>'true' AND COALESCE(e.raw->>'deleted_by_admin','false')<>'true'),
    occurrences AS MATERIALIZED (
        SELECT s.community_id,s.event_id,s.duration_ms,s.ranking_tracking_since,o.ordinal,
            CASE WHEN pg_input_is_valid(s.dates->>o.ordinal,'timestamp with time zone')
                THEN (s.dates->>o.ordinal)::timestamptz AT TIME ZONE 'UTC' END AS starts_at
        FROM schedules s CROSS JOIN LATERAL generate_series($3::int,LEAST(jsonb_array_length(s.dates)-1,$3::bigint+99)::int) o(ordinal)),
    candidates AS MATERIALIZED (
    SELECT community_id,event_id,event_id||':'||starts_at::text AS event_key,
        starts_at+duration_ms*interval '1 millisecond' AS ends_at
    FROM occurrences o
    WHERE starts_at+duration_ms*interval '1 millisecond'>=ranking_tracking_since
    AND starts_at+duration_ms*interval '1 millisecond'<$1
    AND NOT EXISTS(SELECT 1 FROM social.community_ranking_events r WHERE r.kind='event'
        AND r.event_key=o.event_id||':'||o.starts_at::text AND r.community_id=o.community_id)
    ORDER BY event_id,ordinal LIMIT $4)
    SELECT community_id,event_key,ends_at,
        (SELECT count(*) FROM explore.event_attendance_local a WHERE a.event_id=candidates.event_id AND a.action='going')
    FROM candidates";

#[cfg(test)]
tokio::task_local! {
    static DISABLE_ENDED_STATEMENT_TIMEOUT: ();
}

async fn scan_ended_page(
    pool: &PgPool,
    now: NaiveDateTime,
    cursor: &mut EndedScan,
    limit: i64,
) -> anyhow::Result<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    #[cfg(test)]
    if DISABLE_ENDED_STATEMENT_TIMEOUT.try_with(|()| ()).is_ok() {
        sqlx::query("SET LOCAL statement_timeout=0")
            .execute(&mut *tx)
            .await?;
    }
    // Bound source keys before reading schedules, probing receipts or counting
    // attendance. Completed history cannot grow the work of this page.
    let page: Vec<(String, i32)> = sqlx::query_as(ENDED_EVENT_PAGE_SQL)
        .bind(&cursor.after_event)
        .fetch_all(&mut *tx)
        .await?;
    let ids: Vec<&str> = page.iter().map(|(id, _)| id.as_str()).collect();
    let rows: Vec<(Uuid, String, NaiveDateTime, i64)> = if page
        .iter()
        .all(|(_, length)| *length <= cursor.occurrence_offset)
    {
        Vec::new()
    } else {
        sqlx::query_as(ENDED_OCCURRENCES_SQL)
            .bind(now)
            .bind(&ids)
            .bind(cursor.occurrence_offset)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?
    };
    for (community, key, end, attendees) in &rows {
        local_delta(
            &mut tx,
            *community,
            "event",
            key,
            *end,
            [1, 0, 0, (*attendees).min(1000), 0],
        )
        .await?;
    }
    tx.commit().await?;
    // A failed/cancelled transaction leaves the cursor unchanged; committed
    // receipts make replay after a process or lease restart harmless.
    if let Some((last, _)) = page.last() {
        cursor.after_event.clone_from(last);
        cursor.maximum_occurrences = page
            .iter()
            .map(|(_, length)| *length)
            .max()
            .unwrap_or(0)
            .max(cursor.maximum_occurrences);
    } else {
        cursor.after_event.clear();
        let next_window = cursor.occurrence_offset.saturating_add(100);
        cursor.occurrence_offset = if next_window < cursor.maximum_occurrences {
            next_window
        } else {
            0
        };
        // Carry the maximum across ordinal windows; otherwise an empty page
        // could prematurely reset before the longest recurrence is visited.
        if cursor.occurrence_offset == 0 {
            cursor.maximum_occurrences = 0;
        }
    }
    Ok(rows.len() as u64)
}

/// Share one time and receipt budget across pages. A full ordinary 100k-event
/// catalogue fits the page cap; a slow/long schedule resumes next minute.
pub async fn scan_ended(
    pool: &PgPool,
    now: NaiveDateTime,
    cursor: &mut EndedScan,
) -> anyhow::Result<u64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut collected = 0;
    for _ in 0..1024 {
        match tokio::time::timeout_at(
            deadline,
            scan_ended_page(pool, now, cursor, 100 - collected as i64),
        )
        .await
        {
            Ok(Ok(count)) => collected += count,
            Ok(Err(error))
                if error.downcast_ref::<sqlx::Error>().is_some_and(|error| {
                    error.as_database_error().is_some_and(|error| {
                        error.code().as_deref() == Some("57014")
                            && error.message() == "canceling statement due to statement timeout"
                    })
                }) =>
            {
                break;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => break,
        }
        if collected == 100 || (cursor.after_event.is_empty() && cursor.occurrence_offset == 0) {
            break;
        }
    }
    Ok(collected)
}

pub fn spawn_jobs(state: &Arc<super::State>, jobs: &crate::jobs::Jobs) {
    let pool = state.pool.clone();
    jobs.spawn_leased("community-ranking-events", move |ctx| {
        let pool = pool.clone();
        async move {
            let mut cursor = EndedScan::default();
            loop {
                let result = tokio::select! {
                    _=ctx.cancel.cancelled()=>return Ok(()),
                    result=scan_ended(&pool, Utc::now().naive_utc(), &mut cursor)=>result,
                };
                match result {
                    Ok(count) => ctx.progress(format!("{count} local event occurrences ended")),
                    Err(error) => {
                        ctx.error(error.to_string());
                        tracing::error!(%error, "community ranking event scan failed");
                    }
                }
                ctx.pass_done();
                tokio::select! {
                    _=ctx.cancel.cancelled()=>return Ok(()),
                    _=tokio::time::sleep(Duration::from_secs(60))=>{}
                }
            }
        }
    });
    let pool = state.pool.clone();
    // Daily, first pass on every lease acquisition; a failed pass waits the
    // same day as a successful one.
    jobs.spawn_periodic_leased(
        "community-ranking",
        crate::jobs::PeriodicCfg::every(Duration::from_secs(86_400)),
        move |_ctx| {
            let pool = pool.clone();
            async move {
                let count = recalculate(&pool, Utc::now().naive_utc())
                    .await
                    .context("community ranking calculation failed")?;
                tracing::info!(count, "community scores calculated");
                Ok(())
            }
        },
    );
}

#[cfg(test)]
pub(crate) static ENDED_SCAN_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
#[path = "ranking_scan_tests.rs"]
mod scan_tests;
