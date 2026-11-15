//! The SQL of the notifications routes and producers over `social.*`.
//! `occurred_at` is `timestamp(3)` (the wire's epoch-ms `timestamp`/`from`,
//! converted here); `read`/`read_at` are the in-app delivery state; every
//! insert writes its `email` delivery row in the same statement
//! (`pending` or `skipped` with the reason), which the outbox worker in
//! `delivery.rs` drains. The time is always a bind parameter.

use crate::shared::now_utc;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value as Json;
use sqlx::PgPool;
use uuid::Uuid;

use super::email::{EmailSource, make_code};
use super::errors::ApiError;
use super::seen::SeenDebounce;
use super::templates;
use super::types::{NotificationItem, Subscription, normalize_details};
use crate::identity::Address;

pub fn ms_to_naive(ms: i64) -> Option<NaiveDateTime> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|t| t.naive_utc())
}

pub fn naive_to_ms(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp_millis()
}

/// The list statement for one flag combination. The `from` bound and the
/// unread filter are spelled only when set, never folded as `($2 IS NULL OR
/// occurred_at >= $2)` / `($3 = FALSE OR read = FALSE)`: once PostgreSQL
/// switched the prepared statement to a generic plan those folds left the
/// index condition and the 30-second bell poll with nothing new read the
/// wallet's whole history (METRICS.md, "Stage 5 review: the notifications
/// poll through the plan cache"). With the flags in the text, the generic
/// plan keeps `occurred_at >= $2` in the index condition and the unread poll
/// matches the partial `notifications_unread_idx`. Binds: `$1` address,
/// then `from` when present, then the limit, then the broadcast horizon when set
/// (oldest listed broadcast `occurred_at`), then the touch time when
/// `touch` (the reader_seen upsert rides along as a data-modifying CTE;
/// GREATEST keeps concurrent replicas from moving the mark backwards).
fn list_sql(with_from: bool, only_unread: bool, touch: bool, with_horizon: bool) -> String {
    let mut next = 2;
    let mut bind = || {
        let n = next;
        next += 1;
        format!("${n}")
    };
    let from = if with_from { Some(bind()) } else { None };
    let from_cond = |col: &str| match &from {
        Some(b) => format!(" AND {col} >= {b}"),
        None => String::new(),
    };
    let (own_from, bc_from) = (from_cond("occurred_at"), from_cond("b.occurred_at"));
    let unread_cond = if only_unread { " AND NOT read" } else { "" };
    let bc_unread = if only_unread {
        " AND r.broadcast_id IS NULL"
    } else {
        ""
    };
    let limit = bind();
    let horizon = if with_horizon {
        format!(" AND b.occurred_at >= {}", bind())
    } else {
        String::new()
    };
    let touch_cte = if touch {
        format!(
            "WITH seen AS (
                INSERT INTO social.notification_reader_seen (address, last_fetch_at)
                VALUES ($1, {}::timestamp)
                ON CONFLICT (address) DO UPDATE
                  SET last_fetch_at = GREATEST(social.notification_reader_seen.last_fetch_at,
                                               EXCLUDED.last_fetch_at)
            ) ",
            bind()
        )
    } else {
        String::new()
    };
    // Each branch keeps its own index plan; broadcasts carry a null address.
    format!(
        "{touch_cte}SELECT id, type, address,
                   (EXTRACT(EPOCH FROM occurred_at) * 1000)::bigint AS timestamp,
                   read,
                   to_char(created_at, 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at,
                   to_char(COALESCE(read_at, created_at), 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS updated_at,
                   metadata
            FROM (
              (SELECT id, type, address, occurred_at, read, read_at, created_at, metadata
               FROM social.notifications
               WHERE address = $1{own_from}{unread_cond}
                 AND in_app IS TRUE
               ORDER BY occurred_at DESC, id DESC
               LIMIT {limit})
              UNION ALL
              (SELECT b.id, b.type, NULL::text, b.occurred_at, r.read_at IS NOT NULL,
                      r.read_at, b.created_at, b.metadata
               FROM social.broadcast_notifications b
               LEFT JOIN social.broadcast_reads r ON r.broadcast_id = b.id AND r.address = $1
               WHERE TRUE{horizon}{bc_from}{bc_unread}
               ORDER BY b.occurred_at DESC, b.id DESC
               LIMIT {limit})
            ) n
            ORDER BY occurred_at DESC, id DESC
            LIMIT {limit}"
    )
}

/// The subquery, for `EXISTS`, of an opt-out row `o` of `address` from the
/// community whose id is the uuid expression `community`. Stored ids are
/// compared as uuids: `create_opt_out` keeps the canonical spelling, so a
/// producer's or a legacy row's other spelling of the same id still matches;
/// a stored id that is not a uuid matches no community.
pub(super) fn community_opt_out(address: &str, community: &str) -> String {
    format!(
        "SELECT 1 FROM social.subscription_opt_outs o \
         WHERE o.address = {address} AND {}",
        community_opt_out_row(community)
    )
}

/// Whether the opt-out row `o` is a community opt-out from the community whose
/// id is the uuid expression `community`, compared as uuids.
fn community_opt_out_row(community: &str) -> String {
    format!(
        "o.scope = 'community' AND {} = {community}",
        uuid_or_null("o.scope_id")
    )
}

/// The uuid the text expression `text` spells, else NULL.
pub(super) fn uuid_or_null(text: &str) -> String {
    format!("CASE WHEN pg_input_is_valid({text}, 'uuid') THEN ({text})::uuid END")
}

/// The `details` flag at `path` (a `->` chain on `s.details`) when it is a JSON
/// boolean, else `default`: a malformed flag never fails the statement.
pub(crate) fn details_flag(path: &str, default: bool) -> String {
    format!(
        "COALESCE(CASE WHEN jsonb_typeof(s.details{path}) = 'boolean' THEN (s.details{path})::boolean END, {default})"
    )
}

/// The `email` delivery row of every notification a CTE named `ins`
/// (columns `id, address, type`) inserted: `pending` when the type has a
/// template and the recipient can receive it, else `skipped` with the
/// reason (pruned with the other terminal rows). A type absent from
/// `message_type` emails, as the old `autoMigrate` defaults.
pub(super) static DELIVERY_ROWS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
    INSERT INTO social.notification_deliveries (notification_id, channel, state, last_error)
    SELECT n.id, 'email',
           CASE WHEN d.reason IS NULL THEN 'pending' ELSE 'skipped' END,
           d.reason
    FROM ins n
    LEFT JOIN social.subscriptions s ON s.address = n.address
    CROSS JOIN LATERAL (
        SELECT CASE
            WHEN n.type NOT IN ({}) THEN 'no email template'
            WHEN s.email IS NULL THEN 'no confirmed email'
            WHEN {} THEN 'ignore_all_email'
            WHEN NOT {} THEN 'opted out of this type'
        END AS reason
    ) d
    ON CONFLICT (notification_id, channel) DO NOTHING
"#,
        templates::EMAILABLE_SQL_LIST,
        details_flag("->'ignore_all_email'", false),
        details_flag("->'message_type'->n.type->'email'", true),
    )
});

/// Follows an `ups` CTE (`id, address, type, inserted`): delivery rows for the
/// new rows only, then the count of new rows.
static UPSERT_TAIL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "ins AS (SELECT id, address, type FROM ups WHERE inserted),
        delivered AS ({})
        SELECT count(*) FROM ins",
        *DELIVERY_ROWS_SQL
    )
});

/// A repeated broadcast key refreshes the wallet's row like upstream's upsert.
const BROADCAST_REFRESH: &str =
    "ON CONFLICT (address, broadcast_key) WHERE broadcast_key IS NOT NULL DO UPDATE
        SET metadata = EXCLUDED.metadata, occurred_at = EXCLUDED.occurred_at
        WHERE t.type = EXCLUDED.type
    RETURNING id, address, type, (xmax = 0) AS inserted";

type ListRow = (
    Uuid,
    String,
    Option<String>,
    i64,
    bool,
    String,
    String,
    Json,
);

/// One notification a producer wants delivered. `id` is the producer's
/// idempotency key (a v5 uuid over the event for replayable sources).
#[derive(Debug, Clone)]
pub struct NewNotification {
    pub id: Uuid,
    pub address: Address,
    pub kind: String,
    pub metadata: Json,
    pub occurred_at: NaiveDateTime,
}

impl NewNotification {
    pub fn now(address: Address, kind: &str, metadata: Json) -> Self {
        Self {
            id: Uuid::new_v4(),
            address,
            kind: kind.to_string(),
            metadata,
            occurred_at: now_utc(),
        }
    }
}

/// Inserts the batch with its delivery rows in one statement; returns how
/// many were new. A repeated id refreshes metadata and time and keeps the
/// read state; only a new row gets a delivery row, so a repeat never emails.
pub async fn insert_notifications<'e, E>(
    executor: E,
    batch: &[NewNotification],
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    insert_batch(executor, batch, false).await
}

/// Event delivery opts into in-app preferences without silently changing the
/// established behavior of other producers. Hidden rows are already read so
/// they never enter the bell's partial unread index; email remains independent.
pub(super) async fn insert_event_notifications<'e, E>(
    executor: E,
    batch: &[NewNotification],
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    insert_batch(executor, batch, true).await
}

/// One notification deduplicated per `(address, broadcast_key)` (the
/// source's event key) instead of its id, with its delivery row. A repeat of
/// the same type refreshes metadata and time like [`insert_notifications`].
pub async fn insert_keyed_notification<'e, E>(
    executor: E,
    broadcast_key: &str,
    notification: &NewNotification,
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let sql = format!(
        r#"
        WITH ups AS (
            INSERT INTO social.notifications AS t (id, address, type, metadata, broadcast_key, occurred_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (address, broadcast_key) WHERE broadcast_key IS NOT NULL DO UPDATE
                SET metadata = EXCLUDED.metadata, occurred_at = EXCLUDED.occurred_at
                WHERE t.type = EXCLUDED.type
            RETURNING id, address, type, (xmax = 0) AS inserted
        ),
        {tail}
        "#,
        tail = *UPSERT_TAIL
    );
    let inserted: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(notification.id)
        .bind(&notification.address)
        .bind(&notification.kind)
        .bind(&notification.metadata)
        .bind(broadcast_key)
        .bind(notification.occurred_at)
        .fetch_one(executor)
        .await?;
    Ok(inserted as u64)
}

async fn insert_batch<'e, E>(
    executor: E,
    batch: &[NewNotification],
    event_preferences: bool,
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if batch.is_empty() {
        return Ok(0);
    }
    // DO UPDATE may touch a row once per statement: the last of a repeated id
    // wins; id order keeps overlapping batches locking rows in one order.
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<&NewNotification> =
        batch.iter().rev().filter(|n| seen.insert(n.id)).collect();
    unique.sort_by_key(|n| n.id);
    let batch = unique;
    let ids: Vec<Uuid> = batch.iter().map(|n| n.id).collect();
    let addresses: Vec<&str> = batch.iter().map(|n| n.address.as_str()).collect();
    let kinds: Vec<&str> = batch.iter().map(|n| n.kind.as_str()).collect();
    let metadata: Vec<Json> = batch.iter().map(|n| n.metadata.clone()).collect();
    let occurred: Vec<NaiveDateTime> = batch.iter().map(|n| n.occurred_at).collect();
    let sql = format!(
        r#"
        WITH ups AS (
            INSERT INTO social.notifications AS t (id, address, type, metadata, occurred_at, in_app, read, read_at)
            SELECT n.*, v.visible, NOT v.visible,
                   CASE WHEN NOT v.visible THEN timezone('UTC', now()) END
            FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::jsonb[], $5::timestamp[])
                AS n(id,address,type,metadata,occurred_at)
            LEFT JOIN social.subscriptions s ON s.address = n.address
            CROSS JOIN LATERAL (
                SELECT NOT $6::boolean OR (
                    NOT {ignore_all}
                    AND {in_app}
                ) AS visible
            ) v
            ORDER BY n.id
            ON CONFLICT (id) DO UPDATE
                SET metadata = EXCLUDED.metadata, occurred_at = EXCLUDED.occurred_at
                WHERE t.address = EXCLUDED.address AND t.type = EXCLUDED.type
            RETURNING id, address, type, (xmax = 0) AS inserted
        ),
        {tail}
        "#,
        tail = *UPSERT_TAIL,
        ignore_all = details_flag("->'ignore_all_in_app'", false),
        in_app = details_flag("->'message_type'->n.type->'in_app'", true),
    );
    let inserted: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(&ids)
        .bind(&addresses)
        .bind(&kinds)
        .bind(&metadata)
        .bind(&occurred)
        .bind(event_preferences)
        .fetch_one(executor)
        .await?;
    Ok(inserted as u64)
}

#[derive(Debug)]
pub enum SetEmailOutcome {
    NoEmailSent,
    SendConfirmation { source: EmailSource, code: String },
}

#[derive(Clone)]
pub struct NotificationsStore {
    pool: PgPool,
    seen: Arc<SeenDebounce>,
    /// Broadcasts older are neither listed nor kept; `None` keeps them forever, as upstream.
    broadcast_horizon: Option<Duration>,
}

impl NotificationsStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            seen: Arc::new(SeenDebounce::default()),
            broadcast_horizon: None,
        }
    }

    pub fn with_broadcast_horizon(mut self, horizon: Option<Duration>) -> Self {
        self.broadcast_horizon = horizon;
        self
    }

    fn broadcast_cutoff(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        self.broadcast_horizon.map(|horizon| {
            chrono::Duration::from_std(horizon)
                .ok()
                .and_then(|h| now.checked_sub_signed(h))
                .unwrap_or(NaiveDateTime::MIN)
        })
    }

    /// Deletes up to `limit` broadcasts past the horizon (their reads cascade).
    pub async fn prune_broadcasts(
        &self,
        now: NaiveDateTime,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        let Some(cutoff) = self.broadcast_cutoff(now) else {
            return Ok(0);
        };
        let res = sqlx::query(
            r#"
            DELETE FROM social.broadcast_notifications
            WHERE id IN (
                SELECT id FROM social.broadcast_notifications
                WHERE occurred_at < $1
                ORDER BY occurred_at
                LIMIT $2
            )
            "#,
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Lists and, at most once per address per `SEEN_WINDOW`, records the fetch in
    /// `notification_reader_seen` inside the same statement. The touch stays
    /// non-fatal: if the combined form fails the plain list runs once.
    pub async fn list_and_touch(
        &self,
        address: &Address,
        limit: i64,
        from_ms: Option<i64>,
        only_unread: bool,
    ) -> Result<Vec<NotificationItem>, ApiError> {
        if self.seen.claim(address.as_str(), Instant::now()) {
            match self
                .fetch_list(address, limit, from_ms, only_unread, Some(now_utc()))
                .await
            {
                Ok(items) => return Ok(items),
                Err(err) => {
                    self.seen.release(address.as_str());
                    tracing::warn!(error = %err, "reader_seen touch failed; listing without it");
                }
            }
        }
        self.list(address, limit, from_ms, only_unread).await
    }

    pub async fn list(
        &self,
        address: &Address,
        limit: i64,
        from_ms: Option<i64>,
        only_unread: bool,
    ) -> Result<Vec<NotificationItem>, ApiError> {
        self.fetch_list(address, limit, from_ms, only_unread, None)
            .await
            .map_err(Into::into)
    }

    async fn fetch_list(
        &self,
        address: &Address,
        limit: i64,
        from_ms: Option<i64>,
        only_unread: bool,
        touch_at: Option<NaiveDateTime>,
    ) -> Result<Vec<NotificationItem>, sqlx::Error> {
        // Past chrono's range only `>= MAX` is faithful: no row, not every row.
        let from = from_ms.map(|ms| ms_to_naive(ms).unwrap_or(NaiveDateTime::MAX));
        let cutoff = self.broadcast_cutoff(now_utc());
        let sql = list_sql(
            from.is_some(),
            only_unread,
            touch_at.is_some(),
            cutoff.is_some(),
        );
        let mut query = sqlx::query_as::<_, ListRow>(sqlx::AssertSqlSafe(sql)).bind(address);
        if let Some(from) = from {
            query = query.bind(from);
        }
        query = query.bind(limit);
        if let Some(cutoff) = cutoff {
            query = query.bind(cutoff);
        }
        if let Some(at) = touch_at {
            query = query.bind(at);
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, kind, address, timestamp, read, created_at, updated_at, metadata)| {
                    NotificationItem {
                        id,
                        kind,
                        address,
                        timestamp,
                        read,
                        created_at,
                        updated_at,
                        metadata,
                    }
                },
            )
            .collect())
    }

    /// One row per recipient (every subscription, or the explicit list),
    /// one per `(address, broadcast_key)`: a repeat refreshes metadata and time,
    /// keeps the read state and never emails again; returns the new rows.
    pub async fn broadcast(
        &self,
        broadcast_key: &str,
        kind: &str,
        metadata: &Json,
        addresses: Option<&[Address]>,
    ) -> Result<u64, ApiError> {
        let now = now_utc();
        let Some(list) = addresses else {
            let sql = format!(
                r#"
                WITH ups AS (
                    INSERT INTO social.notifications AS t (id, address, type, metadata, broadcast_key, occurred_at)
                    SELECT gen_random_uuid(), address, $1, $2, $3, $4 FROM social.subscriptions
                    ORDER BY address
                    {BROADCAST_REFRESH}
                ),
                {tail}
                "#,
                tail = *UPSERT_TAIL
            );
            let inserted: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(kind)
                .bind(metadata)
                .bind(broadcast_key)
                .bind(now)
                .fetch_one(&self.pool)
                .await?;
            return Ok(inserted as u64);
        };
        if list.is_empty() {
            return Ok(0);
        }
        // One row per address (DO UPDATE touches a row once), in lock order.
        let targets: Vec<&str> = list
            .iter()
            .map(Address::as_str)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let ids: Vec<Uuid> = (0..targets.len()).map(|_| Uuid::new_v4()).collect();
        let sql = format!(
            r#"
            WITH ups AS (
                INSERT INTO social.notifications AS t (id, address, type, metadata, broadcast_key, occurred_at)
                SELECT u.id, u.address, $3, $4, $5, $6 FROM UNNEST($1::uuid[], $2::text[]) AS u(id, address)
                ORDER BY u.address
                {BROADCAST_REFRESH}
            ),
            {tail}
            "#,
            tail = *UPSERT_TAIL
        );
        let inserted: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(&ids)
            .bind(&targets)
            .bind(kind)
            .bind(metadata)
            .bind(broadcast_key)
            .bind(now)
            .fetch_one(&self.pool)
            .await?;
        Ok(inserted as u64)
    }

    pub async fn mark_read(&self, address: &Address, ids: &[Uuid]) -> Result<u64, ApiError> {
        // A broadcast id records this wallet's read; the count covers both kinds.
        let updated: i64 = sqlx::query_scalar(
            r#"
            WITH own AS (
                UPDATE social.notifications
                SET read = TRUE, read_at = $3
                WHERE address = $1 AND id = ANY($2) AND read = FALSE
                RETURNING 1
            ), bc AS (
                INSERT INTO social.broadcast_reads (broadcast_id, address, read_at)
                SELECT id, $1, $3 FROM social.broadcast_notifications WHERE id = ANY($2)
                ON CONFLICT (broadcast_id, address) DO NOTHING
                RETURNING 1
            )
            SELECT (SELECT count(*) FROM own) + (SELECT count(*) FROM bc)
            "#,
        )
        .bind(address)
        .bind(ids)
        .bind(now_utc())
        .fetch_one(&self.pool)
        .await?;
        Ok(updated as u64)
    }

    /// The subscription row (the defaults without one) and, read on its own,
    /// the pending email, as upstream's two lookups. A row set-email wrote only to
    /// track its workflow (empty details, no email) counts as none: upstream had no row.
    pub async fn get_subscription(&self, address: &Address) -> Result<Subscription, ApiError> {
        let (has_row, email, details, unconfirmed_email) =
            sqlx::query_as::<_, (bool, Option<String>, Option<Json>, Option<String>)>(
                r#"
                SELECT COALESCE(s.email IS NOT NULL OR s.details <> '{}'::jsonb, FALSE),
                       s.email, s.details,
                       (SELECT u.email FROM social.unconfirmed_emails u WHERE u.address = $1)
                FROM (SELECT 1) one
                LEFT JOIN social.subscriptions s ON s.address = $1
                "#,
            )
            .bind(address)
            .fetch_one(&self.pool)
            .await?;
        Ok(Subscription {
            address: address.as_str().to_string(),
            email: has_row.then_some(email),
            unconfirmed_email,
            details: normalize_details(&details.unwrap_or_else(|| serde_json::json!({}))),
        })
    }

    /// Writes the flags; `message_type` entries the body omits keep their
    /// stored value, so an older client's partial list changes only what it sends.
    pub async fn put_subscription_details(
        &self,
        address: &Address,
        details: &Json,
    ) -> Result<(), ApiError> {
        sqlx::query(
            r#"
            INSERT INTO social.subscriptions (address, details, updated_at)
            VALUES ($1, $2, $3)
            ON CONFLICT (address) DO UPDATE
              SET details = EXCLUDED.details || jsonb_build_object('message_type',
                    COALESCE(social.subscriptions.details->'message_type', '{}'::jsonb)
                      || (EXCLUDED.details->'message_type')),
                  updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(address)
        .bind(details)
        .bind(now_utc())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// An unsubscribe link's opt-out in one upsert: `ignore_all_email` for
    /// `None`, else `message_type.<kind>.email = false`; every other
    /// preference is left as a concurrent write stored it.
    pub async fn unsubscribe_email(
        &self,
        address: &Address,
        kind: Option<&str>,
    ) -> Result<(), ApiError> {
        let mut fresh = normalize_details(&serde_json::json!({}));
        match kind {
            None => fresh["ignore_all_email"] = Json::Bool(true),
            Some(kind) => fresh["message_type"][kind]["email"] = Json::Bool(false),
        }
        sqlx::query(
            r#"
            INSERT INTO social.subscriptions AS s (address, details, updated_at)
            VALUES ($1, $2, $3)
            ON CONFLICT (address) DO UPDATE
              SET details = CASE WHEN $4::text IS NULL
                    THEN jsonb_set(s.details, '{ignore_all_email}', 'true'::jsonb)
                    ELSE s.details || jsonb_build_object('message_type',
                      CASE WHEN jsonb_typeof(s.details->'message_type') = 'object'
                           THEN s.details->'message_type' ELSE '{}'::jsonb END
                      || jsonb_build_object($4::text,
                        CASE WHEN jsonb_typeof(s.details->'message_type'->$4::text) = 'object'
                             THEN s.details->'message_type'->$4::text ELSE '{}'::jsonb END
                        || '{"email": false}'::jsonb))
                  END,
                  updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(address)
        .bind(&fresh)
        .bind(now_utc())
        .bind(kind)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_email(
        &self,
        address: &Address,
        email: &str,
        is_credits_workflow: bool,
        blacklisted: bool,
    ) -> Result<SetEmailOutcome, ApiError> {
        let now = now_utc();

        if email.is_empty() {
            sqlx::query(
                r#"
                WITH cleared AS (
                    DELETE FROM social.unconfirmed_emails WHERE address = $1
                )
                INSERT INTO social.subscriptions (address, email, details, updated_at)
                VALUES ($1, NULL, jsonb_build_object('ignore_all_email', true), $2)
                ON CONFLICT (address) DO UPDATE
                  SET email = NULL,
                      details = jsonb_set(
                          COALESCE(social.subscriptions.details, '{}'::jsonb),
                          '{ignore_all_email}', 'true'::jsonb, true),
                      updated_at = EXCLUDED.updated_at
                "#,
            )
            .bind(address)
            .bind(now)
            .execute(&self.pool)
            .await?;
            return Ok(SetEmailOutcome::NoEmailSent);
        }

        let email_lc = email.to_lowercase();
        let (current_email, taken_by_other): (Option<String>, bool) = sqlx::query_as(
            r#"
            SELECT (SELECT email FROM social.subscriptions WHERE address = $2),
                   EXISTS (SELECT 1 FROM social.subscriptions
                           WHERE lower(email) = $1 AND address <> $2)
            "#,
        )
        .bind(&email_lc)
        .bind(address)
        .fetch_one(&self.pool)
        .await?;

        if taken_by_other {
            return Err(ApiError::bad_request(
                "Email already registered to another account. Please use a different email to proceed",
            ));
        }
        if current_email.as_deref() == Some(email) {
            sqlx::query("DELETE FROM social.unconfirmed_emails WHERE address = $1")
                .bind(address)
                .execute(&self.pool)
                .await?;
            return Ok(SetEmailOutcome::NoEmailSent);
        }
        if blacklisted {
            return Err(ApiError::bad_request(super::email::INVALID_EMAIL));
        }

        let source = EmailSource::from_credits_workflow(is_credits_workflow);
        let code = make_code();
        sqlx::query(
            r#"
            WITH pending AS (
                INSERT INTO social.unconfirmed_emails (address, email, code, source, created_at)
                VALUES ($1, $2, $3, $4, $6)
                ON CONFLICT (address) DO UPDATE
                  SET email = EXCLUDED.email,
                      code = EXCLUDED.code,
                      source = EXCLUDED.source,
                      created_at = EXCLUDED.created_at
            )
            INSERT INTO social.subscriptions (address, is_credits_workflow, details, updated_at)
            VALUES ($1, $5, '{}'::jsonb, $6)
            ON CONFLICT (address) DO UPDATE
              SET is_credits_workflow = EXCLUDED.is_credits_workflow,
                  updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(address)
        .bind(email)
        .bind(&code)
        .bind(source.as_str())
        .bind(is_credits_workflow)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(SetEmailOutcome::SendConfirmation { source, code })
    }

    /// The pending `(email, code)` of `address`.
    pub async fn find_unconfirmed_email(
        &self,
        address: &Address,
    ) -> Result<Option<(String, String)>, ApiError> {
        Ok(
            sqlx::query_as("SELECT email, code FROM social.unconfirmed_emails WHERE address = $1")
                .bind(address)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Promotes the pending email when the code matches, in one statement;
    /// a wrong code consumes nothing.
    pub async fn confirm_email(
        &self,
        address: &Address,
        code: &str,
    ) -> Result<Option<EmailSource>, ApiError> {
        let row: Option<(String,)> = sqlx::query_as(
            r#"
            WITH claimed AS (
                DELETE FROM social.unconfirmed_emails
                WHERE address = $1 AND code = $2
                RETURNING email, source
            ), promoted AS (
                INSERT INTO social.subscriptions (address, email, details, updated_at)
                SELECT $1, email, '{}'::jsonb, $3 FROM claimed
                ON CONFLICT (address) DO UPDATE
                  SET email = EXCLUDED.email,
                      updated_at = EXCLUDED.updated_at
            )
            SELECT source FROM claimed
            "#,
        )
        .bind(address)
        .bind(code)
        .bind(now_utc())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(source,)| EmailSource::parse(&source)))
    }

    pub async fn is_opted_out(
        &self,
        address: &Address,
        scope: &str,
        scope_id: &str,
    ) -> Result<bool, ApiError> {
        let exists = sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(format!(
            r#"
            SELECT EXISTS (
              SELECT 1 FROM social.subscription_opt_outs o
              WHERE o.address = $1 AND o.scope = $2
                AND (o.scope_id = $3 OR ({matches}))
            )
            "#,
            matches = community_opt_out_row("$4::uuid"),
        )))
        .bind(address)
        .bind(scope)
        .bind(scope_id)
        .bind(Uuid::parse_str(scope_id).ok())
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    pub async fn create_opt_out(
        &self,
        address: &Address,
        scope: &str,
        scope_id: &str,
    ) -> Result<(), ApiError> {
        // Keep opaque legacy IDs compatible; UUID community IDs have one spelling.
        let canonical = (scope == "community")
            .then(|| Uuid::parse_str(scope_id).ok())
            .flatten()
            .map(|id| id.to_string());
        let scope_id = canonical.as_deref().unwrap_or(scope_id);
        sqlx::query(
            r#"
            INSERT INTO social.subscription_opt_outs (address, scope, scope_id, created_at)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (address, scope, scope_id) DO NOTHING
            "#,
        )
        .bind(address)
        .bind(scope)
        .bind(scope_id)
        .bind(now_utc())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_opt_out(
        &self,
        address: &Address,
        scope: &str,
        scope_id: &str,
    ) -> Result<bool, ApiError> {
        let res = sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"
            DELETE FROM social.subscription_opt_outs o
            WHERE o.address = $1 AND o.scope = $2
              AND (o.scope_id = $3 OR ({matches}))
            "#,
            matches = community_opt_out_row("$4::uuid"),
        )))
        .bind(address)
        .bind(scope)
        .bind(scope_id)
        .bind(Uuid::parse_str(scope_id).ok())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod sql_shape_tests {
    use super::list_sql;

    /// The flags are in the statement text, the values are binds: no
    /// `IS NULL OR` fold the generic plan would have to filter after the
    /// index (the review's 3,166-buffer poll).
    #[test]
    fn the_list_statement_spells_only_the_set_flags_and_numbers_its_binds() {
        let plain = list_sql(false, false, false, false);
        assert!(
            plain.contains("WHERE address = $1\n") && plain.contains("LIMIT $2"),
            "{plain}"
        );
        assert!(!plain.contains("IS NULL OR") && !plain.contains("NOT read"));
        assert!(plain.contains("WHERE TRUE\n"), "no horizon: {plain}");
        let from = list_sql(true, false, false, false);
        assert!(
            from.contains("WHERE address = $1 AND occurred_at >= $2") && from.contains("LIMIT $3"),
            "{from}"
        );
        let unread = list_sql(false, true, false, false);
        assert!(
            unread.contains("WHERE address = $1 AND NOT read") && unread.contains("LIMIT $2"),
            "{unread}"
        );
        let both = list_sql(true, true, true, true);
        assert!(both.starts_with("WITH seen AS"), "{both}");
        assert!(both.contains("VALUES ($1, $5::timestamp)"), "{both}");
        assert!(
            both.contains("WHERE TRUE AND b.occurred_at >= $4 AND b.occurred_at >= $2"),
            "{both}"
        );
        assert!(
            both.contains("WHERE address = $1 AND occurred_at >= $2 AND NOT read")
                && both.contains("LIMIT $3"),
            "{both}"
        );
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A fresh lowercase wallet address per call: the lib tests share one
    /// scratch database per process, so every row is scoped by it.
    pub fn fresh_address() -> Address {
        let hex = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        Address::parse(&format!("0x{}", &hex[..40])).unwrap()
    }

    pub async fn store() -> Option<NotificationsStore> {
        let pool = crate::db::scratch::pool(4).await?;
        Some(NotificationsStore::new(pool))
    }

    /// A database of this test's own (`catalyst_test_lib_<pid>_<tag>_<rand>`,
    /// swept with the process's scratch database by the next run) for the
    /// tests whose subject is a whole-table pass: the outbox claim takes
    /// every due row it can see, so two such tests on one database race.
    pub async fn private_pool(tag: &str) -> Option<PgPool> {
        use crate::db::scratch::{OwnedDatabase, Schema};
        Some(OwnedDatabase::new(tag, Schema::Full, 4).await?.pool)
    }

    pub async fn seed_notification(
        pool: &PgPool,
        address: &Address,
        occurred_ms: i64,
        read: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO social.notifications (id, address, type, occurred_at, read, read_at) VALUES ($1, $2, 'bid_received', $3, $4, CASE WHEN $4 THEN $3 END)",
        )
        .bind(id)
        .bind(address)
        .bind(ms_to_naive(occurred_ms).unwrap())
        .bind(read)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    pub async fn last_fetch_at_ms(pool: &PgPool, address: &Address) -> Option<i64> {
        sqlx::query_scalar::<_, NaiveDateTime>(
            "SELECT last_fetch_at FROM social.notification_reader_seen WHERE address = $1",
        )
        .bind(address)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(naive_to_ms)
    }

    pub async fn delivery_state(pool: &PgPool, id: Uuid) -> Option<(String, i32, Option<String>)> {
        sqlx::query_as("SELECT state, attempts, last_error FROM social.notification_deliveries WHERE notification_id = $1 AND channel = 'email'")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }
}

#[cfg(test)]
mod pg_tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn list_and_touch_records_the_fetch_once_per_window() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let (a, b) = (fresh_address(), fresh_address());
        let a_old = seed_notification(&pool, &a, 100_000, true).await;
        let a_new = seed_notification(&pool, &a, 200_000, false).await;
        seed_notification(&pool, &b, 300_000, false).await;

        let before = Utc::now().timestamp_millis();
        let items = c.list_and_touch(&a, 50, None, false).await.unwrap();
        assert_eq!(
            items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![a_new, a_old]
        );
        assert_eq!(items[0].timestamp, 200_000);
        assert_eq!(items[0].address.as_deref(), Some(a.as_str()));
        assert!(
            items[0].created_at.ends_with('Z') && items[0].created_at.len() == 24,
            "{}",
            items[0].created_at
        );
        assert!(items[1].read);
        assert_eq!(
            items[1].updated_at, "1970-01-01T00:01:40.000Z",
            "updated_at is read_at when read"
        );
        let first = last_fetch_at_ms(&pool, &a).await.expect("touched");
        assert!(first >= before - 1, "{first} vs {before}");
        assert_eq!(last_fetch_at_ms(&pool, &b).await, None);

        sqlx::query("UPDATE social.notification_reader_seen SET last_fetch_at = to_timestamp(0.001) WHERE address = $1")
            .bind(&a)
            .execute(&pool)
            .await
            .unwrap();
        let items = c.list_and_touch(&a, 50, Some(150_000), true).await.unwrap();
        assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![a_new]);
        assert_eq!(
            last_fetch_at_ms(&pool, &a).await,
            Some(1),
            "second poll inside the window must not touch"
        );

        let plain = c.list(&a, 50, Some(150_000), true).await.unwrap();
        let touch_at = ms_to_naive(before).unwrap();
        let combined = c
            .fetch_list(&a, 50, Some(150_000), true, Some(touch_at))
            .await
            .unwrap();
        let shape = |v: &Vec<NotificationItem>| {
            v.iter()
                .map(|i| {
                    (
                        i.id,
                        i.timestamp,
                        i.read,
                        i.created_at.clone(),
                        i.updated_at.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&plain), shape(&combined));
        assert_eq!(last_fetch_at_ms(&pool, &a).await, Some(before));

        let future = before + 10_000_000;
        sqlx::query(
            "UPDATE social.notification_reader_seen SET last_fetch_at = $2 WHERE address = $1",
        )
        .bind(&a)
        .bind(ms_to_naive(future).unwrap())
        .execute(&pool)
        .await
        .unwrap();
        NotificationsStore::new(pool.clone())
            .list_and_touch(&a, 50, None, false)
            .await
            .unwrap();
        assert_eq!(
            last_fetch_at_ms(&pool, &a).await,
            Some(future),
            "GREATEST never moves the mark back"
        );
    }

    #[tokio::test]
    async fn mark_read_is_scoped_to_the_signer_and_idempotent() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let (a, b) = (fresh_address(), fresh_address());
        let mine = seed_notification(&pool, &a, 1_000, false).await;
        let theirs = seed_notification(&pool, &b, 1_000, false).await;
        assert_eq!(c.mark_read(&a, &[mine, theirs]).await.unwrap(), 1);
        assert_eq!(c.mark_read(&a, &[mine, theirs]).await.unwrap(), 0);
        let listed = c.list(&b, 10, None, true).await.unwrap();
        assert_eq!(listed.len(), 1, "the other wallet's row stays unread");
        let (read, read_at): (bool, Option<NaiveDateTime>) =
            sqlx::query_as("SELECT read, read_at FROM social.notifications WHERE id = $1")
                .bind(mine)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(read && read_at.is_some());
    }

    #[tokio::test]
    async fn confirm_email_promotes_and_clears_in_one_statement() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let a = fresh_address();
        sqlx::query("INSERT INTO social.unconfirmed_emails (address, email, code, source) VALUES ($1, 'Me@Example.com', 'right-code', 'credits')")
            .bind(&a)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(c.confirm_email(&a, "wrong-code").await.unwrap(), None);
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.unconfirmed_emails WHERE address = $1")
                .bind(&a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 1, "a wrong code must not consume the pending row");
        let sub = c.get_subscription(&a).await.unwrap();
        assert_eq!(sub.email, None, "no row: email absent");
        assert_eq!(
            sub.unconfirmed_email.as_deref(),
            Some("Me@Example.com"),
            "read without a subscription row"
        );

        assert_eq!(
            c.confirm_email(&a, "right-code").await.unwrap(),
            Some(EmailSource::Credits)
        );
        let sub = c.get_subscription(&a).await.unwrap();
        assert_eq!(sub.email, Some(Some("Me@Example.com".into())));
        assert_eq!(sub.unconfirmed_email, None);
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.unconfirmed_emails WHERE address = $1")
                .bind(&a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 0);
        assert_eq!(c.confirm_email(&a, "right-code").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_email_flow_keeps_its_outcomes() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let (a, b) = (fresh_address(), fresh_address());
        let unique = format!("Foo-{}@Example.com", Uuid::new_v4().simple());

        let SetEmailOutcome::SendConfirmation { source, code } =
            c.set_email(&a, &unique, true, false).await.unwrap()
        else {
            panic!("a new email must send a confirmation")
        };
        assert_eq!(source, EmailSource::Credits);
        let (pending_email, pending_code, pending_source): (String, String, String) =
            sqlx::query_as(
                "SELECT email, code, source FROM social.unconfirmed_emails WHERE address = $1",
            )
            .bind(&a)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            (
                pending_email.as_str(),
                pending_code.as_str(),
                pending_source.as_str()
            ),
            (unique.as_str(), code.as_str(), "credits")
        );
        let sub = c.get_subscription(&a).await.unwrap();
        assert_eq!(
            (sub.email, sub.unconfirmed_email.as_deref()),
            (None, Some(unique.as_str())),
            "set-email's own row is not a subscription: email absent"
        );
        let credits: bool = sqlx::query_scalar(
            "SELECT is_credits_workflow FROM social.subscriptions WHERE address = $1",
        )
        .bind(&a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(credits);

        assert_eq!(
            c.confirm_email(&a, &code).await.unwrap(),
            Some(EmailSource::Credits)
        );
        assert!(matches!(
            c.set_email(&a, &unique, false, true).await.unwrap(),
            SetEmailOutcome::NoEmailSent
        ));
        assert!(
            c.set_email(&a, &unique.to_lowercase(), false, true)
                .await
                .is_err(),
            "another case is not the same email: it reaches the blacklist check"
        );
        assert!(matches!(
            c.set_email(&a, &unique.to_lowercase(), false, false)
                .await
                .unwrap(),
            SetEmailOutcome::SendConfirmation { .. }
        ));
        let err = c
            .set_email(&b, &unique.to_uppercase(), false, false)
            .await
            .expect_err("taken by another address");
        assert_eq!(err.status(), 400);
        assert_eq!(
            c.get_subscription(&b).await.unwrap().email,
            None,
            "a refused set-email writes nothing"
        );

        assert!(matches!(
            c.set_email(&a, "", false, false).await.unwrap(),
            SetEmailOutcome::NoEmailSent
        ));
        let sub = c.get_subscription(&a).await.unwrap();
        assert_eq!(sub.email, Some(None));
        assert_eq!(sub.details["ignore_all_email"], serde_json::json!(true));
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.unconfirmed_emails WHERE address = $1")
                .bind(&a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 0);
    }

    #[tokio::test]
    async fn broadcast_is_one_row_per_recipient_refreshed_on_repeat_with_delivery_rows() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let (a, b) = (fresh_address(), fresh_address());
        let key_all = format!("bc-all-{}", Uuid::new_v4());
        for addr in [&a, &b] {
            sqlx::query("INSERT INTO social.subscriptions (address) VALUES ($1)")
                .bind(addr)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE social.subscriptions SET email = 'a@example.com' WHERE address = $1")
            .bind(&a)
            .execute(&pool)
            .await
            .unwrap();
        let meta = serde_json::json!({"title": "hi"});
        let inserted = c
            .broadcast(&key_all, "bid_received", &meta, None)
            .await
            .unwrap();
        assert!(inserted >= 2, "every subscription gets one row: {inserted}");
        type Row = (String, String, Json, Option<String>, String, Option<String>);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT n.address, n.type, n.metadata, n.broadcast_key, d.state, d.last_error
             FROM social.notifications n JOIN social.notification_deliveries d ON d.notification_id = n.id
             WHERE n.broadcast_key = $1 AND n.address = ANY($2) ORDER BY n.address",
        )
        .bind(&key_all)
        .bind(vec![a.as_str(), b.as_str()])
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        let by_addr: std::collections::HashMap<&str, &Row> =
            rows.iter().map(|r| (r.0.as_str(), r)).collect();
        assert!(rows.iter().all(|r| r.1 == "bid_received"
            && r.2 == meta
            && r.3.as_deref() == Some(key_all.as_str())));
        assert_eq!(
            (
                by_addr[a.as_str()].4.as_str(),
                by_addr[a.as_str()].5.as_deref()
            ),
            ("pending", None),
            "a confirmed email is deliverable"
        );
        assert_eq!(
            (
                by_addr[b.as_str()].4.as_str(),
                by_addr[b.as_str()].5.as_deref()
            ),
            ("skipped", Some("no confirmed email"))
        );

        // The all-subscribers re-run is asserted on this test's own recipients:
        // the lib tests share one scratch database, so a sibling test can add
        // a subscription between the two calls and the re-run legitimately
        // inserts one row for that new subscriber.
        let refreshed = serde_json::json!({"title": "again"});
        c.broadcast(&key_all, "bid_received", &refreshed, None)
            .await
            .unwrap();
        let metas: Vec<Json> = sqlx::query_scalar(
            "SELECT metadata FROM social.notifications WHERE broadcast_key = $1 AND address = ANY($2)",
        )
        .bind(&key_all)
        .bind(vec![a.as_str(), b.as_str()])
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            metas,
            vec![refreshed.clone(), refreshed],
            "a re-run refreshes the payload"
        );
        let again: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.notifications n JOIN social.notification_deliveries d ON d.notification_id = n.id
             WHERE n.broadcast_key = $1 AND n.address = ANY($2)",
        )
        .bind(&key_all)
        .bind(vec![a.as_str(), b.as_str()])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            again, 2,
            "a re-run broadcast inserts nothing for a recipient it already reached"
        );

        let key_some = format!("bc-some-{}", Uuid::new_v4());
        assert_eq!(
            c.broadcast(
                &key_some,
                "bid_received",
                &meta,
                Some(&[a.clone(), a.clone()])
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            c.broadcast(
                &key_some,
                "bid_received",
                &meta,
                Some(std::slice::from_ref(&a))
            )
            .await
            .unwrap(),
            0
        );
        let listed = c.list(&a, 50, None, false).await.unwrap();
        assert_eq!(listed.len(), 2);

        // A repeat refreshes the payload, keeps the read state, adds no delivery row.
        let first: Uuid = sqlx::query_scalar(
            "SELECT id FROM social.notifications WHERE broadcast_key = $1 AND address = $2",
        )
        .bind(&key_some)
        .bind(&a)
        .fetch_one(&pool)
        .await
        .unwrap();
        c.mark_read(&a, &[first]).await.unwrap();
        let newer = serde_json::json!({"title": "updated"});
        c.broadcast(
            &key_some,
            "bid_received",
            &newer,
            Some(std::slice::from_ref(&a)),
        )
        .await
        .unwrap();
        let row: (Uuid, Json, bool, i64) = sqlx::query_as(
            "SELECT n.id, n.metadata, n.read, (SELECT count(*) FROM social.notification_deliveries d WHERE d.notification_id = n.id)
             FROM social.notifications n WHERE n.broadcast_key = $1 AND n.address = $2",
        )
        .bind(&key_some)
        .bind(&a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, (first, newer, true, 1));
    }

    #[tokio::test]
    async fn event_in_app_preferences_leave_email_and_existing_producers_independent() {
        let Some(c) = store().await else { return };
        let address = fresh_address();
        sqlx::query("INSERT INTO social.subscriptions(address,email,details) VALUES($1,'recipient@example.org',$2)")
            .bind(&address).bind(serde_json::json!({"message_type":{"events_started":{"in_app":false,"email":true},"events_starts_soon":{"in_app":true,"email":false}}}))
            .execute(c.pool()).await.unwrap();
        let hidden = NewNotification::now(address.clone(), "events_started", serde_json::json!({}));
        let visible =
            NewNotification::now(address.clone(), "events_starts_soon", serde_json::json!({}));
        insert_event_notifications(c.pool(), &[hidden.clone(), visible.clone()])
            .await
            .unwrap();
        let hidden_flags: (bool, bool, bool) = sqlx::query_as(
            "SELECT in_app,read,read_at IS NOT NULL FROM social.notifications WHERE id=$1",
        )
        .bind(hidden.id)
        .fetch_one(c.pool())
        .await
        .unwrap();
        assert_eq!(hidden_flags, (false, true, true));
        assert_eq!(
            delivery_state(c.pool(), hidden.id).await.unwrap().0,
            "pending"
        );
        assert_eq!(
            delivery_state(c.pool(), visible.id).await.unwrap().0,
            "skipped"
        );
        let unread = c.list(&address, 100, None, true).await.unwrap();
        assert!(unread.iter().any(|notice| notice.id == visible.id));
        assert!(unread.iter().all(|notice| notice.id != hidden.id));
        sqlx::query("UPDATE social.subscriptions SET details='{\"ignore_all_in_app\":true}' WHERE address=$1").bind(&address).execute(c.pool()).await.unwrap();
        let legacy =
            NewNotification::now(address.clone(), "friend_first_wear", serde_json::json!({}));
        insert_notifications(c.pool(), std::slice::from_ref(&legacy))
            .await
            .unwrap();
        let legacy_flags: (bool, bool) =
            sqlx::query_as("SELECT in_app,read FROM social.notifications WHERE id=$1")
                .bind(legacy.id)
                .fetch_one(c.pool())
                .await
                .unwrap();
        assert_eq!(
            legacy_flags,
            (true, false),
            "existing generic producers retain their prior visibility contract"
        );
    }

    #[tokio::test]
    async fn producer_inserts_honour_the_subscription_flags() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let (opted_out, ignoring, fine, nobody) = (
            fresh_address(),
            fresh_address(),
            fresh_address(),
            fresh_address(),
        );
        sqlx::query("INSERT INTO social.subscriptions (address, email, details) VALUES ($1, 'o@example.com', '{\"message_type\":{\"bid_received\":{\"email\":false,\"in_app\":true}}}'::jsonb)")
            .bind(&opted_out)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO social.subscriptions (address, email, details) VALUES ($1, 'i@example.com', '{\"ignore_all_email\":true}'::jsonb)")
            .bind(&ignoring)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO social.subscriptions (address, email) VALUES ($1, 'f@example.com')",
        )
        .bind(&fine)
        .execute(&pool)
        .await
        .unwrap();
        let batch: Vec<NewNotification> = [&opted_out, &ignoring, &fine, &nobody]
            .into_iter()
            .map(|a| NewNotification::now(a.clone(), "bid_received", serde_json::json!({})))
            .collect();
        assert_eq!(insert_notifications(&pool, &batch).await.unwrap(), 4);
        assert_eq!(
            insert_notifications(&pool, &batch).await.unwrap(),
            0,
            "the id is the idempotency key"
        );
        let state = |i: usize| delivery_state(&pool, batch[i].id);
        assert_eq!(
            state(0).await.unwrap(),
            ("skipped".into(), 0, Some("opted out of this type".into()))
        );
        assert_eq!(
            state(1).await.unwrap(),
            ("skipped".into(), 0, Some("ignore_all_email".into()))
        );
        assert_eq!(state(2).await.unwrap(), ("pending".into(), 0, None));
        assert_eq!(
            state(3).await.unwrap(),
            ("skipped".into(), 0, Some("no confirmed email".into()))
        );
    }

    #[tokio::test]
    async fn a_repeated_id_refreshes_metadata_and_time_keeps_read_and_never_re_emails() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let a = fresh_address();
        sqlx::query(
            "INSERT INTO social.subscriptions (address, email) VALUES ($1, 'r@example.com')",
        )
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
        let mut first =
            NewNotification::now(a.clone(), "bid_received", serde_json::json!({ "v": 1 }));
        first.occurred_at = ms_to_naive(1_700_000_000_000).unwrap();
        assert_eq!(
            insert_notifications(&pool, std::slice::from_ref(&first))
                .await
                .unwrap(),
            1
        );
        c.mark_read(&a, &[first.id]).await.unwrap();
        let read_at = || {
            sqlx::query_scalar::<_, Option<NaiveDateTime>>(
                "SELECT read_at FROM social.notifications WHERE id = $1",
            )
            .bind(first.id)
            .fetch_one(&pool)
        };
        let read_before = read_at().await.unwrap();
        assert!(read_before.is_some());
        sqlx::query("DELETE FROM social.notification_deliveries WHERE notification_id = $1")
            .bind(first.id)
            .execute(&pool)
            .await
            .unwrap();
        let mut again = first.clone();
        again.metadata = serde_json::json!({ "v": 2 });
        again.occurred_at = ms_to_naive(1_700_000_500_000).unwrap();
        let mut stale = again.clone();
        stale.metadata = serde_json::json!({ "v": 0 });
        // A repeated id inside one batch keeps the last.
        assert_eq!(
            insert_notifications(&pool, &[stale, again]).await.unwrap(),
            0
        );
        let row: (serde_json::Value, i64, bool) = sqlx::query_as(
            "SELECT metadata, (extract(epoch FROM occurred_at) * 1000)::bigint, read FROM social.notifications WHERE id = $1",
        )
        .bind(first.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (serde_json::json!({ "v": 2 }), 1_700_000_500_000, true)
        );
        assert_eq!(read_at().await.unwrap(), read_before);
        assert_eq!(delivery_state(&pool, first.id).await, None);
        let mut other_type = first.clone();
        other_type.kind = "bid_accepted".into();
        other_type.metadata = serde_json::json!({ "v": 3 });
        insert_notifications(&pool, &[other_type]).await.unwrap();
        let kept: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM social.notifications WHERE id = $1")
                .bind(first.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            kept,
            serde_json::json!({ "v": 2 }),
            "an id never changes type"
        );
    }

    #[tokio::test]
    async fn a_repeated_key_refreshes_the_keyed_row_without_a_second_delivery() {
        let Some(c) = store().await else { return };
        let pool = c.pool().clone();
        let a = fresh_address();
        let key = format!("k-{}", Uuid::new_v4());
        let first = NewNotification::now(a.clone(), "bid_received", serde_json::json!({ "v": 1 }));
        assert_eq!(
            insert_keyed_notification(&pool, &key, &first)
                .await
                .unwrap(),
            1
        );
        let mut again =
            NewNotification::now(a.clone(), "bid_received", serde_json::json!({ "v": 2 }));
        again.occurred_at = first.occurred_at + chrono::Duration::seconds(5);
        assert_eq!(
            insert_keyed_notification(&pool, &key, &again)
                .await
                .unwrap(),
            0
        );
        let rows: Vec<(Uuid, serde_json::Value)> =
            sqlx::query_as("SELECT id, metadata FROM social.notifications WHERE address = $1")
                .bind(&a)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![(first.id, serde_json::json!({ "v": 2 }))]);
        let deliveries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.notification_deliveries WHERE notification_id = $1",
        )
        .bind(first.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(deliveries, 1);
    }

    #[tokio::test]
    async fn opt_outs_round_trip() {
        let Some(c) = store().await else { return };
        let a = fresh_address();
        let community = Uuid::new_v4().to_string();
        assert!(!c.is_opted_out(&a, "community", &community).await.unwrap());
        c.create_opt_out(&a, "community", &community).await.unwrap();
        c.create_opt_out(&a, "community", &community).await.unwrap();
        assert!(c.is_opted_out(&a, "community", &community).await.unwrap());
        assert!(c.delete_opt_out(&a, "community", &community).await.unwrap());
        assert!(!c.delete_opt_out(&a, "community", &community).await.unwrap());
        assert!(!c.is_opted_out(&a, "community", &community).await.unwrap());
    }
}
