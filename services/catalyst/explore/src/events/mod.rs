//! The events read port over `explore.event` (the mirrored catalogue) and the
//! local overlays (`explore.event_attendance_local`, `explore.moderators`).
//! Occurrences are recomputed from `raw->'recurrent_dates'` at read time, so a
//! recurring event's `next_start_at`/`live` never depend on how fresh the
//! mirror's snapshot columns are.

pub mod attendees;
mod audit;
pub mod categories;
pub mod foundation;
pub mod handlers;
pub(crate) mod hooks;
mod metadata;
pub mod mirror;
pub mod moderation;
pub mod notifications;
pub mod posters;
pub mod profiles;
pub(crate) mod recurrence;
pub mod schedules;
pub mod sitemap;
pub mod v1;
pub mod writes;

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;

use crate::sanitize::{
    sanitize_asset_url, sanitize_event_description, strip_all_markup, strip_all_markup_opt,
    validate_event_url,
};
use crate::shared::errors::ApiError;

/// The JSON form of every date the events routes answer: upstream serialised
/// node `Date`s (`Date#toJSON`: millisecond precision and `Z`,
/// `2026-01-01T00:00:00.000Z`). chrono's serializer drops a zero fraction
/// (`2026-01-01T00:00:00Z`), so the record structs serialise through here.
/// Fixtures `events.create.minimal_success`,
/// `events.create.recurrent_success_dates_capped`,
/// `events.patch.owner_basic_fields`.
pub(crate) use crate::json_date;
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "events/"))]
pub struct EventRecord {
    pub id: String,
    pub name: String,
    pub image: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    pub image_vertical: Option<Value>,
    pub description: Option<String>,
    /// Present (null included) only for the owner's view and the admin
    /// bearer's answers (`Event/model.ts:478-480`, `admin.ts:37-42,100-102`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "string | null"))]
    pub contact: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "string | null"))]
    pub details: Option<Option<String>>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub start_at: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub finish_at: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub next_start_at: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub next_finish_at: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub duration: Option<i64>,
    pub all_day: bool,
    pub x: i32,
    pub y: i32,
    pub server: Option<String>,
    pub url: Option<String>,
    pub user: Option<String>,
    pub user_name: Option<String>,
    pub estate_id: Option<String>,
    pub estate_name: Option<String>,
    pub scene_name: Option<String>,
    pub approved: bool,
    pub rejected: bool,
    pub highlighted: bool,
    pub trending: bool,
    pub world: bool,
    pub recurrent: bool,
    pub recurrent_frequency: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub recurrent_weekday_mask: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub recurrent_month_mask: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub recurrent_interval: i64,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub recurrent_setpos: Option<i64>,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub recurrent_monthday: Option<i64>,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub recurrent_count: Option<i64>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub recurrent_until: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "Array<string>"))]
    #[serde(serialize_with = "json_date::serialize_vec")]
    pub recurrent_dates: Vec<DateTime<Utc>>,
    pub categories: Vec<String>,
    pub schedules: Vec<String>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total_attendees: i64,
    pub latest_attendees: Vec<String>,
    pub coordinates: [i32; 2],
    pub position: [i32; 2],
    pub live: bool,
    pub attending: bool,
    pub place_id: Option<String>,
    pub community_id: Option<String>,
    pub featured_item: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub created_at: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub updated_at: Option<DateTime<Utc>>,
    pub approved_by: Option<String>,
    pub rejected_by: Option<String>,
    pub rejection_reason: Option<String>,
    pub deleted_by_user: bool,
    pub deleted_by_admin: bool,
    pub deleted_by: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub deleted_at: Option<DateTime<Utc>>,
    pub deleted_reason: Option<String>,
    /// dcl-one's own column; the `/api` answers drop it (upstream's record
    /// never had it), the `/v1` ones keep it. The contract types it
    /// `string | null` optional, as the web fixtures write it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub previous_place_id: Option<String>,
    /// Always an array when `with_connected_users` was asked
    /// (`getEventList.ts:317-323`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub connected_addresses: Option<Vec<String>>,
}

impl EventRecord {
    /// The record as upstream's `/api` routes answered it: no
    /// `previous_place_id`.
    pub fn api_shape(mut self) -> Self {
        self.previous_place_id = None;
        self
    }

    /// `contact`/`details` revealed, as `toPublic` does for the owner and for
    /// the admin bearer's `adminServiceProfile(event.user)`.
    fn reveal_private(&mut self, raw: &Value) {
        self.contact = Some(raw_str(raw, "contact"));
        self.details = Some(raw_str(raw, "details"));
    }
}

/// `MAX_EVENT_RECURRENT` (`Event/types.ts:98`): the next occurrences a
/// record carries; the stored series stays bounded by `recurrence`.
pub const MAX_EVENT_RECURRENT: usize = 10;

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "events/"))]
pub struct EventCategoryRecord {
    pub name: String,
    pub active: bool,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(serialize_with = "json_date::serialize")]
    pub created_at: DateTime<Utc>,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(serialize_with = "json_date::serialize")]
    pub updated_at: DateTime<Utc>,
    #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
    pub i18n: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "events/"))]
pub struct EventAttendeeRecord {
    pub event_id: String,
    pub user: String,
    pub user_name: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(serialize_with = "json_date::serialize")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "events/"))]
pub struct ScheduleRecord {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub image: Option<String>,
    pub theme: Option<String>,
    pub background: Vec<String>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub active_since: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub active_until: Option<DateTime<Utc>>,
    pub active: bool,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub created_at: Option<DateTime<Utc>>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(serialize_with = "json_date::serialize_opt")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// The next upcoming occurrence of an event at a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextEvent {
    pub id: String,
    pub name: String,
    pub next_start_at: String,
}

pub struct EventsComponent {
    pool: PgPool,
    rewrite_domain: Option<String>,
    events_asset_base_url: String,
    pub foundation: Arc<foundation::Foundation>,
}

#[derive(Debug, Clone, Default)]
pub struct EventListFilters {
    pub limit: i64,
    pub offset: i64,
    pub list: EventListType,
    pub order: SortOrder,
    pub highlighted: Option<bool>,
    pub creator: Option<String>,
    pub world: Option<bool>,
    pub recurrent: Option<bool>,
    pub world_names: Vec<String>,
    pub positions: Vec<(i32, i32)>,
    pub estate_id: Option<String>,
    pub community_id: Option<String>,
    pub schedule: Option<String>,
    /// Lists include at most 100 active/future dates, or 1000 when requested.
    pub with_recurrent_dates: bool,
    pub places_ids: Vec<String>,
    pub destination_ids: Vec<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub search: Option<String>,
    pub user: Option<String>,
    pub rejected: Option<bool>,
    pub approved: Option<bool>,
    pub deleted: Option<bool>,
    /// The admin bearer's list (`getEventListRoute`, `index.ts:40-47`):
    /// `approved`/`rejected`/`deleted` filters honoured, rejected rows
    /// included by default (`include_rejected`).
    pub admin: bool,
    /// `include_rejected` (`getEventList.ts:170`): only the admin bearer.
    pub include_rejected: bool,
    pub only_attendee: bool,
    pub owner: bool,
    pub include_own: bool,
    pub include_deleted: bool,
    pub include_unapproved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventListType {
    All,
    #[default]
    Active,
    Live,
    Upcoming,
    Relevance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: String,
    name: String,
    start_at: Option<NaiveDateTime>,
    finish_at: Option<NaiveDateTime>,
    duration_ms: Option<i64>,
    recurrent: bool,
    highlighted: bool,
    trending: bool,
    approved: bool,
    community_id: Option<String>,
    user_creator: Option<String>,
    coordinates_x: Option<i32>,
    coordinates_y: Option<i32>,
    description: Option<String>,
    raw: Value,
    local_attendees: i64,
    #[sqlx(default)]
    local_latest_attendees: Vec<String>,
    #[sqlx(default)]
    cancelled_attendees: Vec<String>,
    #[sqlx(default)]
    total_count: i64,
    #[sqlx(default)]
    viewer_attending: Option<bool>,
}

impl EventsComponent {
    /// `rewrite_domain`: the deployment's own domain, swapped into mirrored
    /// `events-assets-*.decentraland.org` poster URLs (`None` keeps them).
    pub fn new(
        pool: PgPool,
        rewrite_domain: Option<String>,
        events_asset_base_url: String,
    ) -> Self {
        Self {
            pool,
            rewrite_domain,
            events_asset_base_url,
            foundation: Arc::new(foundation::Foundation::default()),
        }
    }

    /// The record options of a read: the asset rewrite, the recurrence
    /// window and the foundation names, refreshed when stale.
    async fn view(&self, max_dates: usize) -> RecordView<'_> {
        self.foundation.refresh_if_stale(&self.pool).await;
        RecordView {
            rewrite_domain: self.rewrite_domain.as_deref(),
            events_asset_base_url: &self.events_asset_base_url,
            max_dates,
            foundation: self.foundation.snapshot(),
        }
    }

    fn destination_clause(ids: &[String], binds: &mut Vec<EventBind>) -> String {
        let ids: Vec<String> = ids.iter().map(|d| d.to_lowercase()).collect();
        let p = next_placeholder(binds, EventBind::TextArray(ids));
        format!("(place_id_lower = ANY({p}) OR (world IS TRUE AND server_lower = ANY({p})))")
    }

    fn build_where(f: &EventListFilters, binds: &mut Vec<EventBind>) -> String {
        let mut sql = String::from(" WHERE 1=1");
        let now = Utc::now();
        // The stored `live_until` (schema_functions.rs EVENT_LIVE_UNTIL) passes
        // `now` exactly when the effective finish, `EFF_NEXT_START_SQL` plus
        // the duration, passes it; a partial index serves the open events.
        let nf = "live_until";
        let ns = EFF_NEXT_START_SQL;

        let is_owner = f.owner;
        if is_owner {
            match f.user.as_deref() {
                Some(u) => {
                    let p = next_placeholder(binds, EventBind::Text(u.to_lowercase()));
                    sql.push_str(&format!(" AND lower(user_creator) = {p}"));
                }
                None => sql.push_str(" AND FALSE"),
            }
        }

        match f.list {
            EventListType::All | EventListType::Relevance => {}
            EventListType::Active => {
                let p = next_placeholder(binds, EventBind::Time(now));
                sql.push_str(&format!(" AND {nf} > {p}"));
            }
            EventListType::Live => {
                let p1 = next_placeholder(binds, EventBind::Time(now));
                let p2 = next_placeholder(binds, EventBind::Time(now));
                sql.push_str(&format!(" AND {nf} > {p1} AND {ns} < {p2}"));
            }
            EventListType::Upcoming => {
                let p1 = next_placeholder(binds, EventBind::Time(now));
                let p2 = next_placeholder(binds, EventBind::Time(now));
                sql.push_str(&format!(" AND {nf} > {p1} AND {ns} > {p2}"));
            }
        }

        if !is_owner && let Some(c) = &f.creator {
            let p = next_placeholder(binds, EventBind::Text(c.to_lowercase()));
            sql.push_str(&format!(" AND lower(user_creator) = {p}"));
        }
        if let Some(eid) = &f.estate_id {
            let p = next_placeholder(binds, EventBind::Text(eid.clone()));
            sql.push_str(&format!(" AND raw->>'estate_id' = {p}"));
        }
        if let Some(schedule) = &f.schedule {
            let p = next_placeholder(binds, EventBind::Text(schedule.clone()));
            sql.push_str(&format!(" AND raw->'schedules' ? {p}"));
        }
        if f.highlighted == Some(true) {
            sql.push_str(" AND highlighted IS TRUE");
        }
        match f.world {
            Some(true) => sql.push_str(" AND world IS TRUE"),
            Some(false) => sql.push_str(" AND world IS FALSE"),
            None => {}
        }
        match f.recurrent {
            Some(true) => sql.push_str(" AND recurrent IS TRUE"),
            Some(false) => sql.push_str(" AND recurrent IS FALSE"),
            None => {}
        }
        if !f.world_names.is_empty() {
            let p = next_placeholder(binds, EventBind::TextArray(f.world_names.clone()));
            sql.push_str(&format!(" AND raw->>'server' = ANY({p})"));
        }
        if !f.positions.is_empty() {
            let mut clauses: Vec<String> = Vec::new();
            for (x, y) in &f.positions {
                let px = next_placeholder(binds, EventBind::Int(*x));
                let py = next_placeholder(binds, EventBind::Int(*y));
                clauses.push(format!("(coordinates_x = {px} AND coordinates_y = {py})"));
            }
            sql.push_str(&format!(" AND ({})", clauses.join(" OR ")));
        }

        let mut by_destination: Vec<String> = Vec::new();
        if !f.places_ids.is_empty() {
            // `e.place_id = ANY($ids)` (`Event/model.ts:268-270`), the ids
            // verbatim; the folded column only narrows through its index.
            let lower = f.places_ids.iter().map(|d| d.to_lowercase()).collect();
            let pl = next_placeholder(binds, EventBind::TextArray(lower));
            let pe = next_placeholder(binds, EventBind::TextArray(f.places_ids.clone()));
            by_destination.push(format!(
                "(place_id_lower = ANY({pl}) AND raw->>'place_id' = ANY({pe}))"
            ));
        }
        if let Some(c) = &f.community_id {
            let pc = next_placeholder(binds, EventBind::Text(c.clone()));
            by_destination.push(format!("community_id = {pc}"));
        }
        if !by_destination.is_empty() {
            sql.push_str(&format!(" AND ({})", by_destination.join(" OR ")));
        }
        if !f.destination_ids.is_empty() {
            let clause = Self::destination_clause(&f.destination_ids, binds);
            sql.push_str(&format!(" AND {clause}"));
        }

        if let Some(from) = f.from {
            let p = next_placeholder(binds, EventBind::Time(from));
            sql.push_str(&format!(" AND {ns} >= {p}"));
        }
        if let Some(to) = f.to {
            let p = next_placeholder(binds, EventBind::Time(to));
            sql.push_str(&format!(" AND {ns} < {p}"));
        }

        if let Some(s) = &f.search {
            // A positive cover-density rank of an `&`-joined query is a match.
            let p = next_placeholder(binds, EventBind::Text(to_tsquery(s)));
            sql.push_str(&format!(" AND textsearch @@ to_tsquery('english', {p})"));
        }

        let own_viewer = f
            .user
            .as_deref()
            .filter(|_| !f.include_unapproved && f.include_own);
        if let (false, Some(u)) = (is_owner, own_viewer) {
            // model.ts:183-190: `rejected IS FALSE` applies to every list but
            // the owner's and the admin bearer's; :238-241 then widens
            // `approved` to the viewer's own rows. A signed viewer lists their
            // pending events, never their rejected ones (only `owner=true`
            // does).
            let p = next_placeholder(binds, EventBind::Text(u.to_lowercase()));
            sql.push_str(&format!(
                " AND rejected IS FALSE AND (approved IS TRUE OR lower(user_creator) = {p})"
            ));
        } else if !is_owner {
            // model.ts:184-191: `rejected` filter, else `include_rejected`
            // (the admin bearer) or `rejected IS FALSE` (everyone else,
            // permission wallets included).
            match (f.include_rejected, f.rejected) {
                (_, Some(true)) => sql.push_str(" AND rejected IS TRUE"),
                (_, Some(false)) | (false, None) => sql.push_str(" AND rejected IS FALSE"),
                (true, None) => {}
            }
            match (f.include_unapproved, f.approved) {
                (_, Some(true)) | (false, None) => sql.push_str(" AND approved IS TRUE"),
                (_, Some(false)) => sql.push_str(" AND approved IS FALSE"),
                (true, None) => {}
            }
        }

        if f.only_attendee
            && let Some(u) = &f.user
        {
            let p = next_placeholder(binds, EventBind::Text(u.to_lowercase()));
            sql.push_str(&format!(" AND {}", attendance_sql(&p)));
        }

        if let Some(own) = crate::copies::origin::event_rows_sql() {
            sql.push_str(" AND ");
            sql.push_str(own);
        }

        match (f.admin, f.deleted) {
            (true, Some(true)) => sql.push_str(DELETED_ONLY_SQL),
            (true, None) if f.include_deleted => {}
            _ => sql.push_str(NOT_DELETED_SQL),
        }
        sql
    }

    pub async fn query(
        &self,
        f: &EventListFilters,
        with_total: bool,
    ) -> Result<(Vec<EventRecord>, i64), ApiError> {
        let mut binds: Vec<EventBind> = Vec::new();
        let sql = build_list_sql(f, with_total, &mut binds);
        let mut q = sqlx::query_as::<_, EventRow>(sqlx::AssertSqlSafe(sql));
        for b in &binds {
            q = bind_one(q, b);
        }
        let rows = q.fetch_all(&self.pool).await?;
        let total = if with_total {
            match rows.first() {
                Some(r) => r.total_count,
                None if f.offset > 0 || f.limit <= 0 => self.count_only(f).await?,
                None => 0,
            }
        } else {
            0
        };
        let user = f.user.as_deref();
        let view = self
            .view(if f.with_recurrent_dates {
                FULL_RECURRENCE_WINDOW
            } else {
                MAX_EVENT_RECURRENT
            })
            .await;
        let records = rows
            .into_iter()
            .map(|r| event_row_to_record(r, user, &view))
            .collect();
        Ok((records, total))
    }

    async fn count_only(&self, f: &EventListFilters) -> Result<i64, ApiError> {
        let mut cbinds: Vec<EventBind> = Vec::new();
        let count_sql = count_only_sql(f, &mut cbinds);
        let mut cq = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql));
        for b in &cbinds {
            cq = bind_one_scalar(cq, b);
        }
        Ok(cq.fetch_one(&self.pool).await?)
    }

    pub async fn get(&self, event_id: &str) -> Result<Option<EventRecord>, ApiError> {
        let row = sqlx::query_as::<_, EventRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {EVENT_COLUMNS} FROM explore.event WHERE id = $1"
        )))
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;
        let view = self.view(MAX_EVENT_RECURRENT).await;
        Ok(row.map(|r| event_row_to_record(r, None, &view)))
    }

    /// The admin bearer's view (`admin.ts:37-42,100-102`, `index.ts:26-36`:
    /// `adminServiceProfile(event.user)`): `contact` and `details` present,
    /// `attending` false (no viewer).
    pub async fn get_for_admin(&self, event_id: &str) -> Result<Option<EventRecord>, ApiError> {
        let row = sqlx::query_as::<_, EventRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {EVENT_COLUMNS} FROM explore.event WHERE id = $1"
        )))
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;
        let view = self.view(MAX_EVENT_RECURRENT).await;
        Ok(row.map(|r| {
            let raw = r.raw.clone();
            let mut record = event_row_to_record(r, None, &view);
            record.reveal_private(&raw);
            record
        }))
    }

    /// `get` plus the viewer's attendance (local RSVP or upstream attendee) in one statement.
    pub async fn get_for_viewer(
        &self,
        event_id: &str,
        viewer: Option<&str>,
    ) -> Result<Option<EventRecord>, ApiError> {
        let Some(viewer) = viewer else {
            return self.get(event_id).await;
        };
        let row = sqlx::query_as::<_, EventRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {EVENT_COLUMNS}, {} AS viewer_attending FROM explore.event WHERE id = $1",
            attendance_sql("$2")
        )))
        .bind(event_id)
        .bind(viewer.to_lowercase())
        .fetch_optional(&self.pool)
        .await?;
        let view = self.view(MAX_EVENT_RECURRENT).await;
        Ok(row.map(|r| event_row_to_record(r, Some(viewer), &view)))
    }

    /// `GET /api/events/attending` (`Event/model.ts:461-480`,
    /// `getAttending`): every event that has not finished, not rejected and
    /// not deleted, pending ones of other creators included, each with the
    /// viewer's `attending` flag; no limit. Fixture
    /// `events.get_events_attending.signed`.
    pub async fn attending(&self, user: &str) -> Result<Vec<EventRecord>, ApiError> {
        let user_lc = user.to_lowercase();
        let rows = sqlx::query_as::<_, EventRow>(sqlx::AssertSqlSafe(attending_sql()))
            .bind(&user_lc)
            .fetch_all(&self.pool)
            .await?;
        let view = self.view(MAX_EVENT_RECURRENT).await;
        Ok(rows
            .into_iter()
            .map(|r| event_row_to_record(r, Some(&user_lc), &view))
            .collect())
    }

    pub async fn moderation_pending(&self, limit: i64) -> Result<Vec<EventRecord>, ApiError> {
        let limit = limit.clamp(0, 500);
        let columns = list_event_columns(false);
        let rows = sqlx::query_as::<_, EventRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {columns} FROM explore.event \
             WHERE approved IS NOT TRUE OR rejected IS TRUE \
             ORDER BY next_start_at DESC NULLS LAST, id ASC \
             LIMIT $1"
        )))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let view = self.view(MAX_EVENT_RECURRENT).await;
        Ok(rows
            .into_iter()
            .map(|r| event_row_to_record(r, None, &view))
            .collect())
    }

    /// The live event (if any) at each destination key: a place id, or a world
    /// name for a world event. `None` under a key means nothing is live there.
    pub async fn live_event_names(
        &self,
        keys: &[String],
    ) -> Result<HashMap<String, Option<String>>, ApiError> {
        let mut out: HashMap<String, Option<String>> =
            keys.iter().map(|k| (k.clone(), None)).collect();
        if keys.is_empty() {
            return Ok(out);
        }
        let rows = self.events_at(keys, EventListType::Live).await?;
        let asked = AskedKeys::new(keys);
        for ev in rows {
            let Some(key) = asked.resolve(&ev) else {
                continue;
            };
            if out.get(&key).is_some_and(Option::is_some) {
                continue;
            }
            out.insert(
                key,
                ev.name
                    .map(|n| strip_all_markup(&n))
                    .filter(|n| !n.is_empty()),
            );
        }
        Ok(out)
    }

    /// The first upcoming occurrence at each destination key that has one.
    pub async fn next_events(
        &self,
        keys: &[String],
    ) -> Result<HashMap<String, NextEvent>, ApiError> {
        let mut out: HashMap<String, NextEvent> = HashMap::new();
        if keys.is_empty() {
            return Ok(out);
        }
        let rows = self.events_at(keys, EventListType::Upcoming).await?;
        let asked = AskedKeys::new(keys);
        for ev in rows {
            let Some(key) = asked.resolve(&ev) else {
                continue;
            };
            let (Some(name), Some(next_start_at)) = (ev.name, ev.next_start_at) else {
                continue;
            };
            out.entry(key).or_insert(NextEvent {
                id: ev.id,
                name: strip_all_markup(&name),
                next_start_at: json_date::wire(&next_start_at.and_utc()),
            });
        }
        Ok(out)
    }

    /// Public (approved, not rejected, not deleted) events at the given
    /// destinations, earliest next occurrence first.
    async fn events_at(
        &self,
        keys: &[String],
        list: EventListType,
    ) -> Result<Vec<EventAt>, ApiError> {
        let f = EventListFilters {
            list,
            destination_ids: keys.to_vec(),
            ..Default::default()
        };
        let mut binds: Vec<EventBind> = Vec::new();
        let where_sql = Self::build_where(&f, &mut binds);
        let sql = format!(
            "SELECT id, name, raw->>'place_id' AS place_id, raw->>'server' AS server, world, \
                    {EFF_NEXT_START_SQL} AS next_start_at \
             FROM explore.event{where_sql} \
             ORDER BY {EFF_NEXT_START_SQL} ASC NULLS LAST, id ASC LIMIT 2500"
        );
        let mut q = sqlx::query_as::<_, EventAt>(sqlx::AssertSqlSafe(sql));
        for b in &binds {
            q = bind_one(q, b);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }
}

#[derive(sqlx::FromRow)]
struct EventAt {
    id: String,
    name: Option<String>,
    place_id: Option<String>,
    server: Option<String>,
    world: bool,
    /// The effective next start, a UTC instant stored without a zone like
    /// the columns it derives from.
    next_start_at: Option<chrono::NaiveDateTime>,
}

/// The keys a caller asked for, matched case-insensitively: a place by its
/// wire place_id, a world by its server name (only when the event is a world
/// event), and reported back under the spelling the caller used.
struct AskedKeys(HashMap<String, String>);

impl AskedKeys {
    fn new(asked: &[String]) -> Self {
        Self(
            asked
                .iter()
                .map(|key| (key.to_lowercase(), key.clone()))
                .collect(),
        )
    }

    fn resolve(&self, ev: &EventAt) -> Option<String> {
        let by_place = ev
            .place_id
            .as_deref()
            .and_then(|id| self.0.get(&id.to_lowercase()));
        let by_world = ev
            .server
            .as_deref()
            .filter(|_| ev.world)
            .and_then(|name| self.0.get(&name.to_lowercase()));
        by_place.or(by_world).cloned()
    }
}

// Keep malformed/missing previews safe in every read. The macro also permits
// composing the static column list without allocating a query at each call.
macro_rules! latest_attendees_array_sql {
    () => {
        "CASE WHEN jsonb_typeof(raw->'latest_attendees')='array' THEN raw->'latest_attendees' ELSE '[]' END"
    };
}
const LATEST_ATTENDEES_ARRAY_SQL: &str = latest_attendees_array_sql!();

/// Column list shared by every event read query.
/// The signed delta deduplicates only known preview members. An upstream count
/// can include wallets absent from its incomplete preview, so the merged count
/// is an estimate. Cancellation overrides membership and subtracts known members.
/// Materializing the preview once avoids expanding it for every local signer.
const EVENT_COLUMNS: &str = concat!(
    "id, name, start_at, finish_at, duration_ms, recurrent, highlighted, trending, \
     approved, community_id, user_creator, coordinates_x, coordinates_y, \
     description, raw, \
     (WITH upstream AS MATERIALIZED (SELECT ARRAY(SELECT lower(ua.address) FROM jsonb_array_elements_text(",
    latest_attendees_array_sql!(),
    ") ua(address) WHERE ua.address IS NOT NULL) AS addresses) \
      SELECT COALESCE(sum(CASE \
         WHEN la.action='going' AND NOT (la.signer=ANY(upstream.addresses)) THEN 1 \
         WHEN la.action='cancel' AND la.signer=ANY(upstream.addresses) THEN -1 \
         ELSE 0 END),0)::bigint FROM explore.event_attendance_local la CROSS JOIN upstream WHERE la.event_id=event.id) AS local_attendees, \
     ARRAY(SELECT la.signer FROM explore.event_attendance_local la WHERE la.event_id=event.id AND la.action='going' ORDER BY la.signed_at DESC,la.signer LIMIT 10) AS local_latest_attendees, \
     ARRAY(SELECT lower(ua.address) FROM jsonb_array_elements_text(",
    latest_attendees_array_sql!(),
    ") ua(address) \
           WHERE ua.address IS NOT NULL AND EXISTS (SELECT 1 FROM explore.event_attendance_local la WHERE la.event_id=event.id AND la.signer=lower(ua.address) AND la.action='cancel')) AS cancelled_attendees"
);

/// Explicit local state, including cancellation, overrides a mirrored preview.
fn attendance_sql(user: &str) -> String {
    format!(
        "COALESCE((SELECT la.action='going' FROM explore.event_attendance_local la WHERE la.event_id=event.id AND la.signer={user}), \
        EXISTS (SELECT 1 FROM jsonb_array_elements_text({LATEST_ATTENDEES_ARRAY_SQL}) ua(address) WHERE ua.address IS NOT NULL AND lower(ua.address)={user}))"
    )
}

// Keep a bounded active/future date window for existing list consumers.
// Effective next_start_at still uses the complete series, before projection.
fn list_event_columns(include_dates: bool) -> String {
    let date_limit = if include_dates { 1000 } else { 100 };
    // The stored instants are UTC timestamps; the wire form carries the zone.
    let dates = format!(
        "COALESCE((SELECT jsonb_agg(upcoming.starts_at AT TIME ZONE 'UTC' ORDER BY upcoming.starts_at) \
         FROM (SELECT d.starts_at FROM ({VALID_RECURRENCE_DATES_SQL}) d \
           WHERE d.starts_at + COALESCE(duration_ms,0) * interval '1 millisecond' > now() \
           ORDER BY d.starts_at LIMIT {date_limit}) upcoming),'[]'::jsonb)"
    );
    EVENT_COLUMNS.replace(", raw,", &format!(
        ", ((raw - 'recurrent_dates') || jsonb_build_object('next_start_at', ({EFF_NEXT_START_SQL})::timestamptz, 'recurrent_dates', {dates})) AS raw,"
    ))
}

/// `deleted` is `deleted_by_user = 'true' OR deleted_by_admin = 'true'` of the
/// raw document, stored (schemas/10_explore.sql).
const NOT_DELETED_SQL: &str = " AND deleted IS FALSE";

const DELETED_ONLY_SQL: &str = " AND deleted IS TRUE";

/// The effective next occurrence, upstream's `calculateNextRecurrentDates`
/// as its cron leaves the stored column: the stored `next_start_at` while its
/// occurrence has not finished (an occurrence the creator's edit dropped from
/// `recurrent_dates` keeps running), else the first recurrence that has not
/// finished, else the last recurrence, else the snapshot columns. The
/// recurrence instants are the stored `recurrence_dates` (UTC timestamps,
/// schema explore.recurrence_dates); they join `now()` under the pool's UTC
/// session.
macro_rules! valid_recurrence_dates_sql {
    () => {
        "SELECT d.starts_at FROM unnest(recurrence_dates) AS d(starts_at)"
    };
}
const VALID_RECURRENCE_DATES_SQL: &str = valid_recurrence_dates_sql!();

macro_rules! eff_next_start_sql {
    () => {
        concat!(
            "CASE WHEN next_start_at + COALESCE(duration_ms, 0) * interval '1 millisecond' > now() \
             THEN next_start_at ELSE COALESCE((SELECT min(d.starts_at) FROM (",
            valid_recurrence_dates_sql!(),
            ") d WHERE d.starts_at + COALESCE(duration_ms, 0) * interval '1 millisecond' > now()), \
             (SELECT max(d.starts_at) FROM (",
            valid_recurrence_dates_sql!(),
            ") d), next_start_at, start_at) END"
        )
    };
}

const EFF_NEXT_START_SQL: &str = eff_next_start_sql!();

fn next_start_order_by(order: SortOrder) -> String {
    let dir = match order {
        SortOrder::Asc => "ASC",
        SortOrder::Desc => "DESC",
    };
    format!(" ORDER BY {EFF_NEXT_START_SQL} {dir} NULLS LAST, id ASC")
}

/// `Event/model.ts:470-477`: `finish_at > now() AND rejected IS FALSE AND
/// deleted_by_user IS FALSE AND deleted_by_admin IS FALSE`, the attendance
/// a LEFT JOIN flag; dcl-one orders by the next occurrence for a stable page.
fn attending_sql() -> String {
    let attending = attendance_sql("$1");
    let columns = list_event_columns(false);
    format!(
        "SELECT {columns}, {attending} AS viewer_attending FROM explore.event \
         WHERE live_until > now() AND rejected IS FALSE AND deleted IS FALSE \
         ORDER BY {EFF_NEXT_START_SQL} ASC NULLS LAST, id ASC"
    )
}

/// `Event/model.ts:35-48` (`textsearch`): name 'A', user_name 'B',
/// estate_name 'B', description 'C'.
/// The stored `textsearch` of explore.event (schemas/10_explore.sql pins it).
#[cfg(test)]
const TEXTSEARCH_EXPR: &str = "(setweight(to_tsvector('english', coalesce(name,'')), 'A') || \
     setweight(to_tsvector('english', coalesce(raw->>'user_name','')), 'B') || \
     setweight(to_tsvector('english', coalesce(raw->>'estate_name','')), 'B') || \
     setweight(to_tsvector('english', coalesce(description,'')), 'C'))";

fn to_tsquery(input: &str) -> String {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .map(|t| format!("{t}:*"))
        .collect();
    terms.join(" & ")
}

#[derive(Debug)]
enum EventBind {
    Text(String),
    Int(i32),
    Int64(i64),
    Time(DateTime<Utc>),
    TextArray(Vec<String>),
}

fn next_placeholder(binds: &mut Vec<EventBind>, bind: EventBind) -> String {
    binds.push(bind);
    format!("${}", binds.len())
}

/// Assemble the paginated list query: a `page` CTE sorts, counts and cuts the keys, and the
/// columns are read for the page alone. When `with_total`, the pre-LIMIT row count rides along
/// as `count(*) OVER() AS total_count` on the keys -- same semantics as a separate `count(*)`
/// over the same WHERE, no extra bind parameters.
fn build_list_sql(f: &EventListFilters, with_total: bool, binds: &mut Vec<EventBind>) -> String {
    let where_sql = EventsComponent::build_where(f, binds);
    let (window, extra) = if with_total {
        (", count(*) OVER() AS total_count", ", page.total_count")
    } else {
        ("", "")
    };
    let viewer = match &f.user {
        Some(u) => {
            let p = next_placeholder(binds, EventBind::Text(u.to_lowercase()));
            format!(", {} AS viewer_attending", attendance_sql(&p))
        }
        None => String::new(),
    };
    let order_clause = if let Some(s) = &f.search {
        let dir = if matches!(f.order, SortOrder::Asc) {
            "ASC"
        } else {
            "DESC"
        };
        let p = next_placeholder(binds, EventBind::Text(to_tsquery(s)));
        format!(" ORDER BY ts_rank_cd(textsearch, to_tsquery('english', {p})) {dir}, id ASC")
    } else {
        next_start_order_by(f.order)
    };
    let columns = list_event_columns(f.with_recurrent_dates);
    let limit = f
        .limit
        .clamp(0, if f.with_recurrent_dates { 25 } else { 500 });
    let lim_p = next_placeholder(binds, EventBind::Int64(limit));
    let off_p = next_placeholder(binds, EventBind::Int64(f.offset.max(0)));
    // The page CTE sorts, counts and cuts the keys; the columns are read for the page alone.
    format!(
        "WITH page AS (SELECT id{window} FROM explore.event{where_sql}{order_clause} LIMIT {lim_p} OFFSET {off_p}) \
         SELECT {columns}{extra}{viewer} FROM explore.event JOIN page USING (id){order_clause}"
    )
}

/// The rare-path count, unchanged from the old two-query design: recovers the true total for an
/// empty page that ran past the offset.
fn count_only_sql(f: &EventListFilters, binds: &mut Vec<EventBind>) -> String {
    let where_sql = EventsComponent::build_where(f, binds);
    format!("SELECT count(*) FROM explore.event{where_sql}")
}

fn bind_one<'q, R>(
    q: sqlx::query::QueryAs<'q, sqlx::Postgres, R, sqlx::postgres::PgArguments>,
    b: &'q EventBind,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, R, sqlx::postgres::PgArguments> {
    match b {
        EventBind::Text(s) => q.bind(s),
        EventBind::Int(i) => q.bind(i),
        EventBind::Int64(i) => q.bind(i),
        EventBind::Time(t) => q.bind(t),
        EventBind::TextArray(v) => q.bind(v),
    }
}

fn bind_one_scalar<'q>(
    q: sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    b: &'q EventBind,
) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments> {
    match b {
        EventBind::Text(s) => q.bind(s),
        EventBind::Int(i) => q.bind(i),
        EventBind::Int64(i) => q.bind(i),
        EventBind::Time(t) => q.bind(t),
        EventBind::TextArray(v) => q.bind(v),
    }
}

fn rewrite_asset_host(url: &str, domain: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    let bucket = host.strip_suffix(".decentraland.org")?;
    bucket
        .starts_with("events-assets-")
        .then(|| format!("https://{bucket}.{domain}/{path}"))
}

fn rewrite_asset_url(url: &str, domain: Option<&str>) -> String {
    domain
        .and_then(|d| rewrite_asset_host(url, d))
        .unwrap_or_else(|| url.to_string())
}

/// `with_recurrent_dates` on `/v1/events`: the stored window instead of
/// upstream's ten.
pub const FULL_RECURRENCE_WINDOW: usize = 1000;

/// What a read projects a row with.
struct RecordView<'a> {
    rewrite_domain: Option<&'a str>,
    events_asset_base_url: &'a str,
    max_dates: usize,
    foundation: foundation::Names,
}

fn event_row_to_record(
    r: EventRow,
    verified_viewer: Option<&str>,
    view: &RecordView<'_>,
) -> EventRecord {
    let rewrite_domain = view.rewrite_domain;
    let events_asset_base_url = view.events_asset_base_url;
    let raw = &r.raw;
    let start_at = r.start_at.map(|n| n.and_utc());
    let finish_at = r.finish_at.map(|n| n.and_utc());
    let x = r.coordinates_x.unwrap_or(0);
    let y = r.coordinates_y.unwrap_or(0);
    let image = raw
        .get("image")
        .and_then(|v| v.as_str())
        .and_then(|s| sanitize_asset_url(s).ok())
        .map(|s| {
            rewrite_asset_url(
                &posters::absolute_default_image(&s, events_asset_base_url),
                rewrite_domain,
            )
        });
    let image_vertical = raw.get("image_vertical").cloned().and_then(|v| match v {
        Value::String(s) => sanitize_asset_url(&s).ok().map(|safe| {
            Value::String(rewrite_asset_url(
                &posters::absolute_default_image(&safe, events_asset_base_url),
                rewrite_domain,
            ))
        }),
        other => Some(other),
    });
    let server = raw_str(raw, "server");
    let url = raw
        .get("url")
        .and_then(|v| v.as_str())
        .and_then(|s| validate_event_url(s).ok());
    let user = raw_str(raw, "user").or_else(|| r.user_creator.clone());
    let owner_view = matches!((verified_viewer,user.as_deref()),(Some(viewer),Some(owner)) if viewer.eq_ignore_ascii_case(owner));
    let scene_name = strip_all_markup_opt(raw_str(raw, "scene_name"));
    let estate_name =
        strip_all_markup_opt(raw_str(raw, "estate_name")).or_else(|| scene_name.clone());
    let world = raw.get("world").and_then(|v| v.as_bool()).unwrap_or(false);
    let duration = r
        .duration_ms
        .or_else(|| raw.get("duration").and_then(|v| v.as_i64()));
    let recurrent_dates: Vec<DateTime<Utc>> = raw
        .get("recurrent_dates")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| parse_dt(Some(x))).collect())
        .unwrap_or_default();
    let occurrence_span = chrono::Duration::milliseconds(duration.unwrap_or(0));
    let now = Utc::now();
    let stored_next = parse_dt(raw.get("next_start_at"));
    let next_start_at = match stored_next {
        Some(ns) if ns + occurrence_span > now => Some(ns),
        _ => recurrent_dates
            .iter()
            .copied()
            .filter(|d| *d + occurrence_span > now)
            .min()
            .or_else(|| recurrent_dates.iter().copied().max())
            .or(stored_next)
            .or(start_at),
    };
    let next_finish_at = next_start_at.map(|d| d + occurrence_span);
    // `futureRecurrentDates` (`Event/utils.ts:191-233`): the next
    // `MAX_EVENT_RECURRENT` occurrences that have not ended, else the start
    // (`calculateRecurrentProperties`, `:436-438`).
    let projected_dates: Vec<DateTime<Utc>> = {
        let mut dates: Vec<DateTime<Utc>> = recurrent_dates
            .iter()
            .copied()
            .filter(|d| *d + occurrence_span > now)
            .take(view.max_dates)
            .collect();
        if dates.is_empty() {
            dates.extend(start_at);
        }
        // `EventModel.build` (`Event/model.ts:138-140`) puts the start first
        // when the stored series no longer begins with it.
        if let Some(start) = start_at.filter(|s| dates.first() != Some(s)) {
            dates.insert(0, start);
        }
        dates
    };
    // `calculateRecurrentProperties` (`Event/utils.ts:426-432`): a recurrent
    // event ends when the last of its next `MAX_EVENT_RECURRENT` occurrences
    // ends, re-rolled by upstream's cron as they pass; the node stores the
    // whole series, so the served value is projected the same way.
    let finish_at = if r.recurrent {
        recurrent_dates
            .iter()
            .copied()
            .filter(|d| *d + occurrence_span > now)
            .take(MAX_EVENT_RECURRENT)
            .last()
            .or(start_at)
            .map(|d| d + occurrence_span)
            .or(finish_at)
    } else {
        finish_at
    };
    let live = match (next_start_at, duration) {
        (Some(ns), Some(d)) => now >= ns && now < ns + chrono::Duration::milliseconds(d),
        _ => raw.get("live").and_then(|v| v.as_bool()).unwrap_or(false),
    };
    let attending = match verified_viewer {
        Some(u) => r.viewer_attending.unwrap_or_else(|| {
            raw.get("latest_attendees")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|x| {
                        x.as_str()
                            .map(|s| s.eq_ignore_ascii_case(u))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        }),
        None => false,
    };
    let str_list = |key: &str| -> Vec<String> {
        raw.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };

    EventRecord {
        id: r.id,
        name: strip_all_markup(&r.name),
        image,
        image_vertical,
        description: r
            .description
            .or_else(|| raw_str(raw, "description"))
            .map(|d| sanitize_event_description(&d)),
        contact: owner_view.then(|| raw_str(raw, "contact")),
        details: owner_view.then(|| raw_str(raw, "details")),
        start_at,
        finish_at,
        next_start_at,
        next_finish_at,
        duration,
        all_day: raw
            .get("all_day")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        x,
        y,
        server,
        url,
        user_name: strip_all_markup_opt(foundation::display_name(
            &view.foundation,
            user.as_deref(),
            raw_str(raw, "user_name"),
        )),
        user,
        estate_id: raw_str(raw, "estate_id"),
        estate_name,
        scene_name,
        approved: r.approved,
        rejected: raw
            .get("rejected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        highlighted: r.highlighted,
        trending: r.trending,
        world,
        recurrent: r.recurrent,
        recurrent_frequency: raw_str(raw, "recurrent_frequency"),
        recurrent_weekday_mask: raw
            .get("recurrent_weekday_mask")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        recurrent_month_mask: raw
            .get("recurrent_month_mask")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        // `calculateRecurrentProperties` defaults the interval to 1 (`utils.ts:389`).
        recurrent_interval: raw
            .get("recurrent_interval")
            .and_then(|v| v.as_i64())
            .unwrap_or(1),
        recurrent_setpos: raw.get("recurrent_setpos").and_then(|v| v.as_i64()),
        recurrent_monthday: raw.get("recurrent_monthday").and_then(|v| v.as_i64()),
        recurrent_count: raw.get("recurrent_count").and_then(|v| v.as_i64()),
        recurrent_until: parse_dt(raw.get("recurrent_until")),
        recurrent_dates: projected_dates,
        categories: str_list("categories"),
        schedules: str_list("schedules"),
        total_attendees: (raw
            .get("total_attendees")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0)
            + r.local_attendees)
            .max(0),
        latest_attendees: {
            let mut seen: std::collections::HashSet<String> = r
                .cancelled_attendees
                .into_iter()
                .map(|address| address.to_ascii_lowercase())
                .collect();
            r.local_latest_attendees
                .into_iter()
                .chain(str_list("latest_attendees"))
                .filter(|address| seen.insert(address.to_ascii_lowercase()))
                .take(10)
                .collect()
        },
        coordinates: [x, y],
        position: [x, y],
        live,
        attending,
        place_id: raw_str(raw, "place_id"),
        community_id: r.community_id,
        featured_item: raw_str(raw, "featured_item"),
        created_at: parse_dt(raw.get("created_at")),
        updated_at: parse_dt(raw.get("updated_at")),
        approved_by: raw_str(raw, "approved_by"),
        rejected_by: raw_str(raw, "rejected_by"),
        rejection_reason: raw_str(raw, "rejection_reason"),
        deleted_by_user: raw
            .get("deleted_by_user")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        deleted_by_admin: raw
            .get("deleted_by_admin")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        deleted_by: raw_str(raw, "deleted_by"),
        deleted_at: parse_dt(raw.get("deleted_at")),
        deleted_reason: raw_str(raw, "deleted_reason"),
        previous_place_id: raw_str(raw, "previous_place_id"),
        connected_addresses: None,
    }
}

fn raw_str(raw: &Value, key: &str) -> Option<String> {
    raw.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn parse_dt(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = v?.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_view(rewrite_domain: Option<&'static str>) -> RecordView<'static> {
        RecordView {
            rewrite_domain,
            events_asset_base_url: "https://events.decentraland.org",
            max_dates: MAX_EVENT_RECURRENT,
            foundation: Arc::new(std::collections::HashSet::new()),
        }
    }
    use serde_json::json;

    /// The feed's live and next lookups decode the effective next start, a
    /// `timestamp without time zone` like the columns it derives from.
    #[tokio::test]
    async fn live_and_next_events_at_a_place_decode_the_stored_instants() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let run = uuid::Uuid::new_v4().simple().to_string();
        let place = format!("place-{run}");
        let now = Utc::now();
        for (name, start) in [
            ("live", now - chrono::Duration::minutes(30)),
            ("next", now + chrono::Duration::hours(3)),
        ] {
            let stored = format!("<link=\"file:///x\">{name}</link><size=300>");
            sqlx::query(
                "INSERT INTO explore.event (id, name, start_at, next_start_at, duration_ms, approved, raw) \
                 VALUES ($1, $2, $3, $3, 7200000, true, $4)",
            )
            .bind(format!("ev-{run}-{name}"))
            .bind(&stored)
            .bind(start.naive_utc())
            .bind(json!({ "approved": true, "place_id": place.to_uppercase() }))
            .execute(&pool)
            .await
            .unwrap();
        }
        let component =
            EventsComponent::new(pool.clone(), None, "https://events.decentraland.org".into());
        let live = component
            .live_event_names(std::slice::from_ref(&place))
            .await
            .unwrap();
        assert_eq!(live.get(&place), Some(&Some("live".to_string())));
        let next = component
            .next_events(std::slice::from_ref(&place))
            .await
            .unwrap();
        assert_eq!(next.get(&place).map(|e| e.name.as_str()), Some("next"));
        sqlx::query("DELETE FROM explore.event WHERE id LIKE $1")
            .bind(format!("ev-{run}-%"))
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn event_connected_users_uses_shared_rooms_and_deduplicates_locations() {
        struct Participants(std::sync::Mutex<Vec<(bool, String)>>);
        impl crate::places::live::Participants for Participants {
            fn participants<'a>(
                &'a self,
                world: bool,
                key: &'a str,
            ) -> futures::future::BoxFuture<'a, Option<Vec<String>>> {
                Box::pin(async move {
                    self.0.lock().unwrap().push((world, key.into()));
                    Some(vec![if world { "0xworld" } else { "0xparcel" }.into()])
                })
            }
        }
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let live = std::sync::Arc::new(crate::places::live::Live::new(pool));
        let participants = std::sync::Arc::new(Participants(Default::default()));
        live.set_participants(participants.clone());
        let mut events = vec![
            event_row_to_record(
                row_with(json!({"x":10,"y":20}), None, None),
                None,
                &test_view(None),
            ),
            event_row_to_record(
                row_with(json!({"x":10,"y":20}), None, None),
                None,
                &test_view(None),
            ),
            event_row_to_record(
                row_with(json!({"world":true,"server":"MyWorld.DCL.eth"}), None, None),
                None,
                &test_view(None),
            ),
        ];
        super::handlers::fill_connected(&mut events, &live).await;
        assert_eq!(events[0].connected_addresses, Some(vec!["0xparcel".into()]));
        assert_eq!(events[1].connected_addresses, events[0].connected_addresses);
        assert_eq!(events[2].connected_addresses, Some(vec!["0xworld".into()]));
        let calls = participants.0.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&(true, "myworld.dcl.eth".into())));
    }

    #[tokio::test]
    async fn event_connected_users_preserves_unknown_and_successfully_empty_rooms() {
        struct Fixed(Option<Vec<String>>);
        impl crate::places::live::Participants for Fixed {
            fn participants<'a>(
                &'a self,
                _world: bool,
                _key: &'a str,
            ) -> futures::future::BoxFuture<'a, Option<Vec<String>>> {
                Box::pin(async move { self.0.clone() })
            }
        }
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let mut events = vec![event_row_to_record(
            row_with(json!({"x":10,"y":20}), None, None),
            None,
            &test_view(None),
        )];
        let unknown = std::sync::Arc::new(crate::places::live::Live::new(pool.clone()));
        unknown.set_participants(std::sync::Arc::new(Fixed(None)));
        super::handlers::fill_connected(&mut events, &unknown).await;
        assert_eq!(
            events[0].connected_addresses,
            Some(vec![]),
            "getEventList.ts:317-323: an unknown room is still an array"
        );
        assert_eq!(
            serde_json::to_value(&events[0])
                .unwrap()
                .get("connected_addresses"),
            Some(&json!([]))
        );
        let empty = std::sync::Arc::new(crate::places::live::Live::new(pool));
        empty.set_participants(std::sync::Arc::new(Fixed(Some(vec![]))));
        super::handlers::fill_connected(&mut events, &empty).await;
        assert_eq!(events[0].connected_addresses, Some(vec![]));
    }

    const SCHEMA: &str = include_str!(concat!(env!("DCL_ONE_ROOT"), "/schemas/10_explore.sql"));

    fn squeezed(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The event table stores the vector this module searched by expression.
    #[test]
    fn the_schema_stores_the_event_textsearch() {
        let column = squeezed(&format!(
            "textsearch tsvector NOT NULL GENERATED ALWAYS AS ({TEXTSEARCH_EXPR}) STORED"
        ));
        assert!(squeezed(SCHEMA).contains(&column), "{column}");
    }

    fn row_with(raw: Value, start_at: Option<DateTime<Utc>>, duration_ms: Option<i64>) -> EventRow {
        EventRow {
            id: "e1".into(),
            name: "Weekly Show".into(),
            start_at: start_at.map(|s| s.naive_utc()),
            finish_at: start_at.map(|s| {
                (s + chrono::Duration::milliseconds(duration_ms.unwrap_or(0))).naive_utc()
            }),
            duration_ms,
            recurrent: true,
            highlighted: false,
            trending: false,
            approved: true,
            community_id: None,
            user_creator: None,
            coordinates_x: Some(0),
            coordinates_y: Some(0),
            description: None,
            raw,
            local_attendees: 0,
            local_latest_attendees: Vec::new(),
            cancelled_attendees: Vec::new(),
            total_count: 0,
            viewer_attending: None,
        }
    }

    #[test]
    fn next_start_order_by_and_attending_sql_include_the_id_tiebreaker() {
        for order in [SortOrder::Asc, SortOrder::Desc] {
            let sql = next_start_order_by(order);
            assert!(sql.trim_end().ends_with(", id ASC"), "{sql}");
        }
        assert!(attending_sql().contains("NULLS LAST, id ASC"));
    }

    #[test]
    fn rewrite_asset_host_swaps_only_the_upstream_poster_bucket() {
        assert_eq!(
            rewrite_asset_host(
                "https://events-assets-099ac00.decentraland.org/poster/abc.webp",
                "interconnected.online"
            )
            .as_deref(),
            Some("https://events-assets-099ac00.interconnected.online/poster/abc.webp")
        );
        for untouched in [
            "https://peer.decentraland.org/content/contents/x",
            "https://example.com/poster/abc.webp",
            "http://events-assets-099ac00.decentraland.org/poster/abc.webp",
            "/poster/abc.webp",
        ] {
            assert_eq!(
                rewrite_asset_host(untouched, "interconnected.online"),
                None,
                "{untouched}"
            );
        }
        let raw = json!({
            "image": "https://events-assets-099ac00.decentraland.org/poster/d2dfb36b6a012314.jpg",
            "image_vertical": "https://events-assets-099ac00.decentraland.org/poster/v.webp",
        });
        let rec = event_row_to_record(
            row_with(raw.clone(), None, None),
            None,
            &test_view(Some("interconnected.online")),
        );
        assert_eq!(
            rec.image.as_deref(),
            Some("https://events-assets-099ac00.interconnected.online/poster/d2dfb36b6a012314.jpg")
        );
        assert_eq!(
            rec.image_vertical,
            Some(Value::String(
                "https://events-assets-099ac00.interconnected.online/poster/v.webp".into()
            ))
        );
        let rec = event_row_to_record(row_with(raw, None, None), None, &test_view(None));
        assert_eq!(
            rec.image.as_deref(),
            Some("https://events-assets-099ac00.decentraland.org/poster/d2dfb36b6a012314.jpg")
        );
    }

    #[test]
    fn event_record_surfaces_featured_item_from_raw() {
        let urn =
            "urn:decentraland:matic:collections-v2:0x1234567890abcdef1234567890abcdef12345678:1";
        let rec = event_row_to_record(
            row_with(json!({ "featured_item": urn }), None, None),
            None,
            &test_view(None),
        );
        assert_eq!(rec.featured_item.as_deref(), Some(urn));
        for absent in [json!({}), json!({ "featured_item": null })] {
            let rec = event_row_to_record(row_with(absent, None, None), None, &test_view(None));
            assert_eq!(rec.featured_item, None);
        }
    }

    #[test]
    fn build_where_excludes_soft_deleted_for_every_list_type_and_appends_it_last() {
        for list in [
            EventListType::All,
            EventListType::Active,
            EventListType::Live,
            EventListType::Upcoming,
        ] {
            let f = EventListFilters {
                list,
                ..Default::default()
            };
            let mut binds = Vec::new();
            let sql = EventsComponent::build_where(&f, &mut binds);
            assert!(sql.contains(" AND deleted IS FALSE"), "{list:?}: {sql}");
        }
        let f = EventListFilters {
            creator: Some("0xABC".into()),
            community_id: Some("c1".into()),
            highlighted: Some(true),
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&f, &mut binds);
        assert!(sql.ends_with(NOT_DELETED_SQL), "{sql}");
    }

    #[test]
    fn build_where_lets_only_an_admin_opt_into_deleted_rows() {
        let opted_in = EventListFilters {
            admin: true,
            include_deleted: true,
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&opted_in, &mut binds);
        assert!(
            !sql.contains(NOT_DELETED_SQL) && !sql.contains(DELETED_ONLY_SQL),
            "{sql}"
        );

        for guarded in [
            EventListFilters {
                include_deleted: true,
                ..Default::default()
            },
            EventListFilters {
                admin: true,
                include_deleted: true,
                deleted: Some(false),
                ..Default::default()
            },
            EventListFilters {
                admin: true,
                ..Default::default()
            },
        ] {
            let mut binds = Vec::new();
            let sql = EventsComponent::build_where(&guarded, &mut binds);
            assert!(sql.ends_with(NOT_DELETED_SQL), "{sql}");
        }
        let only_deleted = EventListFilters {
            admin: true,
            include_deleted: true,
            deleted: Some(true),
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&only_deleted, &mut binds);
        assert!(sql.ends_with(DELETED_ONLY_SQL), "{sql}");
    }

    /// The date filters read the stored occurrence columns, never the
    /// mirrored next_start_at alone.
    #[test]
    fn date_filters_use_recomputed_occurrences() {
        for (list, start) in [
            (EventListType::Active, false),
            (EventListType::Live, true),
            (EventListType::Upcoming, true),
        ] {
            let f = EventListFilters {
                list,
                ..Default::default()
            };
            let mut binds = Vec::new();
            let sql = EventsComponent::build_where(&f, &mut binds);
            assert!(sql.contains(" AND live_until > $1"), "{list:?}: {sql}");
            assert_eq!(
                sql.contains("unnest(recurrence_dates)"),
                start,
                "{list:?}: {sql}"
            );
        }
        let attending = attending_sql();
        assert!(
            attending
                .contains("WHERE live_until > now() AND rejected IS FALSE AND deleted IS FALSE")
        );
        assert!(attending.contains("unnest(recurrence_dates)"));
    }

    const PUBLIC_ONLY_CLAUSE: &str = " AND rejected IS FALSE AND approved IS TRUE";
    const APPROVED_OR_MINE_CLAUSE: &str =
        " AND rejected IS FALSE AND (approved IS TRUE OR lower(user_creator) = ";

    #[test]
    fn a_signed_viewer_sees_their_own_pending_events_and_an_admin_list_is_unfiltered() {
        let f = EventListFilters {
            user: Some("0xABC".into()),
            include_own: true,
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&f, &mut binds);
        assert!(sql.contains(APPROVED_OR_MINE_CLAUSE), "{sql}");
        assert!(!sql.contains(PUBLIC_ONLY_CLAUSE), "{sql}");
        assert!(
            matches!(binds.last(), Some(EventBind::Text(u)) if u == "0xabc"),
            "{binds:?}"
        );

        for f in [
            EventListFilters::default(),
            EventListFilters {
                user: Some("0xABC".into()),
                ..Default::default()
            },
            EventListFilters {
                include_own: true,
                ..Default::default()
            },
        ] {
            let mut binds = Vec::new();
            let sql = EventsComponent::build_where(&f, &mut binds);
            assert!(sql.contains(PUBLIC_ONLY_CLAUSE), "{sql}");
            assert!(!sql.contains("lower(user_creator)"), "{sql}");
        }
        let admin = EventListFilters {
            user: Some("0xABC".into()),
            include_own: true,
            admin: true,
            include_unapproved: true,
            ..Default::default()
        };
        let mut binds = Vec::new();
        assert!(!EventsComponent::build_where(&admin, &mut binds).contains("approved"));

        let moderator = EventListFilters {
            user: Some("0xABC".into()),
            include_own: true,
            include_unapproved: true,
            include_deleted: true,
            deleted: Some(true),
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&moderator, &mut binds);
        assert!(
            !sql.contains("approved") && !sql.contains("lower(user_creator)"),
            "{sql}"
        );
        assert!(
            sql.ends_with(NOT_DELETED_SQL) && !sql.contains(DELETED_ONLY_SQL),
            "{sql}"
        );
        let bearer_only = EventListFilters {
            admin: true,
            ..Default::default()
        };
        let mut binds = Vec::new();
        assert!(
            EventsComponent::build_where(&bearer_only, &mut binds).contains(PUBLIC_ONLY_CLAUSE)
        );
    }

    const DESTINATION_CLAUSE: &str =
        "(place_id_lower = ANY($1) OR (world IS TRUE AND server_lower = ANY($1)))";

    const PLACE_IDS_CLAUSE: &str = "(place_id_lower = ANY($1) AND raw->>'place_id' = ANY($2))";

    #[test]
    fn place_ids_match_verbatim_and_destinations_by_their_world_too() {
        let f = EventListFilters {
            list: EventListType::All,
            places_ids: vec!["P1".into(), "My-World.dcl.eth".into()],
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&f, &mut binds);
        assert!(sql.contains(&format!(" AND ({PLACE_IDS_CLAUSE})")), "{sql}");
        assert!(
            matches!(&binds[0], EventBind::TextArray(ids) if ids == &["p1", "my-world.dcl.eth"]),
            "{binds:?}"
        );
        assert!(
            matches!(&binds[1], EventBind::TextArray(ids) if ids == &["P1", "My-World.dcl.eth"]),
            "{binds:?}"
        );

        let with_community = EventListFilters {
            list: EventListType::All,
            places_ids: vec!["p1".into()],
            community_id: Some("c1".into()),
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&with_community, &mut binds);
        assert!(
            sql.contains(&format!(" AND ({PLACE_IDS_CLAUSE} OR community_id = $3)")),
            "{sql}"
        );

        let by_destination = EventListFilters {
            list: EventListType::All,
            destination_ids: vec!["my-world.dcl.eth".into()],
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&by_destination, &mut binds);
        assert!(sql.contains(&format!(" AND {DESTINATION_CLAUSE}")), "{sql}");
    }

    #[test]
    fn list_sql_folds_total_into_a_single_statement() {
        let f = EventListFilters {
            places_ids: vec!["p1".into(), "p2".into()],
            list: EventListType::Active,
            limit: 20,
            offset: 0,
            ..Default::default()
        };
        let mut binds_t: Vec<EventBind> = Vec::new();
        let sql_t = build_list_sql(&f, true, &mut binds_t);
        assert_eq!(
            sql_t.matches("count(*) OVER() AS total_count").count(),
            1,
            "{sql_t}"
        );
        assert!(!sql_t.contains(';'), "{sql_t}");
        let mut binds_f: Vec<EventBind> = Vec::new();
        let sql_f = build_list_sql(&f, false, &mut binds_f);
        assert!(!sql_f.contains("total_count"), "{sql_f}");
        assert_eq!(binds_t.len(), binds_f.len());
        assert_eq!(
            sql_t,
            sql_f
                .replacen(
                    "SELECT id FROM explore.event",
                    "SELECT id, count(*) OVER() AS total_count FROM explore.event",
                    1,
                )
                .replacen(
                    &format!(
                        "SELECT {} FROM explore.event JOIN page",
                        list_event_columns(false)
                    ),
                    &format!(
                        "SELECT {}, page.total_count FROM explore.event JOIN page",
                        list_event_columns(false)
                    ),
                    1,
                )
        );
        let mut wbinds: Vec<EventBind> = Vec::new();
        let where_out = EventsComponent::build_where(&f, &mut wbinds);
        let mut cbinds: Vec<EventBind> = Vec::new();
        assert_eq!(
            count_only_sql(&f, &mut cbinds),
            format!("SELECT count(*) FROM explore.event{where_out}")
        );
    }

    /// `Date#toJSON`: a whole-second date keeps its `.000` and `Z`
    /// (`events.create.minimal_success`: `recurrent_dates`, `finish_at`,
    /// `next_start_at`, `next_finish_at`; `events.patch.owner_basic_fields`:
    /// `start_at`), whatever form the stored row holds.
    #[test]
    fn record_dates_serialize_in_the_old_json_form() {
        let raw = json!({
            "recurrent_dates": ["2031-01-01T00:00:00+00:00"],
            "updated_at": "2026-01-01T00:00:00.5Z",
        });
        let rec = event_row_to_record(
            row_with(
                raw,
                Some("2031-01-01T00:00:00Z".parse().unwrap()),
                Some(3_600_000),
            ),
            None,
            &test_view(None),
        );
        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["start_at"], "2031-01-01T00:00:00.000Z");
        assert_eq!(json["finish_at"], "2031-01-01T01:00:00.000Z");
        assert_eq!(json["next_start_at"], "2031-01-01T00:00:00.000Z");
        assert_eq!(json["next_finish_at"], "2031-01-01T01:00:00.000Z");
        assert_eq!(json["recurrent_dates"], json!(["2031-01-01T00:00:00.000Z"]));
        assert_eq!(json["updated_at"], "2026-01-01T00:00:00.500Z");
        assert_eq!(json["created_at"], Value::Null);
        assert_eq!(
            serde_json::to_value(ScheduleRecord {
                id: "s".into(),
                name: "s".into(),
                description: None,
                image: None,
                theme: None,
                background: vec![],
                active_since: Some("2026-01-01T00:00:00Z".parse().unwrap()),
                active_until: None,
                active: true,
                created_at: None,
                updated_at: None,
            })
            .unwrap()["active_since"],
            "2026-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn record_recomputes_next_occurrence_from_recurrent_dates() {
        let now = Utc::now();
        let past = now - chrono::Duration::days(7);
        let future = now + chrono::Duration::days(1);
        let raw = json!({
            "next_start_at": past.to_rfc3339(),
            "next_finish_at": (past + chrono::Duration::hours(2)).to_rfc3339(),
            "recurrent_dates": [past.to_rfc3339(), future.to_rfc3339()],
        });
        let rec = event_row_to_record(
            row_with(
                raw,
                Some(past - chrono::Duration::days(30)),
                Some(7_200_000),
            ),
            None,
            &test_view(None),
        );
        assert_eq!(
            rec.next_start_at.map(|d| d.timestamp()),
            Some(future.timestamp())
        );
        assert_eq!(
            rec.next_finish_at.map(|d| d.timestamp()),
            Some((future + chrono::Duration::hours(2)).timestamp())
        );
        assert!(!rec.live);

        let started = now - chrono::Duration::minutes(30);
        let raw = json!({ "recurrent_dates": [started.to_rfc3339()] });
        let rec = event_row_to_record(
            row_with(raw, Some(started), Some(7_200_000)),
            None,
            &test_view(None),
        );
        assert_eq!(
            rec.next_start_at.map(|d| d.timestamp()),
            Some(started.timestamp())
        );
        assert!(rec.live);

        let raw = json!({
            "next_start_at": past.to_rfc3339(),
            "next_finish_at": (past + chrono::Duration::hours(2)).to_rfc3339(),
            "recurrent_dates": [past.to_rfc3339()],
        });
        let rec = event_row_to_record(
            row_with(raw, Some(past), Some(7_200_000)),
            None,
            &test_view(None),
        );
        assert_eq!(
            rec.next_start_at.map(|d| d.timestamp()),
            Some(past.timestamp())
        );
        assert!(!rec.live);

        let running = now - chrono::Duration::minutes(30);
        let raw = json!({
            "next_start_at": running.to_rfc3339(),
            "next_finish_at": (running + chrono::Duration::hours(2)).to_rfc3339(),
            "recurrent_dates": [future.to_rfc3339()],
        });
        let rec = event_row_to_record(
            row_with(raw, Some(past), Some(7_200_000)),
            None,
            &test_view(None),
        );
        assert_eq!(
            rec.next_start_at.map(|d| d.timestamp()),
            Some(running.timestamp()),
            "the stored occurrence keeps running although the recurrence list dropped it"
        );
        assert_eq!(
            rec.next_finish_at.map(|d| d.timestamp()),
            Some((running + chrono::Duration::hours(2)).timestamp())
        );
        assert!(rec.live);
    }

    #[test]
    fn owner_listing_scopes_to_the_user_and_drops_status_filters() {
        let f = EventListFilters {
            owner: true,
            user: Some("0xABC".into()),
            creator: Some("0xdef".into()),
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&f, &mut binds);
        assert_eq!(sql.matches("lower(user_creator)").count(), 1, "{sql}");
        assert!(sql.contains("lower(user_creator) = $1"), "{sql}");
        assert!(!sql.contains("approved IS TRUE"), "{sql}");
        assert!(!sql.contains("rejected IS FALSE"), "{sql}");
        assert!(matches!(binds.first(), Some(EventBind::Text(u)) if u.as_str() == "0xabc"));

        let f = EventListFilters {
            user: Some("0xabc".into()),
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = EventsComponent::build_where(&f, &mut binds);
        assert!(sql.contains("approved IS TRUE") && sql.contains("rejected IS FALSE"));

        let f = EventListFilters {
            owner: true,
            user: None,
            ..Default::default()
        };
        let mut binds = Vec::new();
        assert!(EventsComponent::build_where(&f, &mut binds).contains(" AND FALSE"));
    }

    #[test]
    fn lists_with_explicit_occurrence_payloads_have_a_smaller_page_limit() {
        let filters = EventListFilters {
            limit: 500,
            with_recurrent_dates: true,
            ..Default::default()
        };
        let mut binds = Vec::new();
        let sql = build_list_sql(&filters, false, &mut binds);
        assert!(sql.contains("LIMIT 1000"));
        assert!(matches!(
            binds.get(binds.len() - 2),
            Some(EventBind::Int64(25))
        ));
    }

    #[test]
    fn ordinary_lists_keep_requested_event_limits_with_a_hundred_date_window() {
        for limit in [100, 500] {
            let filters = EventListFilters {
                limit,
                ..Default::default()
            };
            let mut binds = Vec::new();
            let sql = build_list_sql(&filters, false, &mut binds);
            assert!(sql.contains("LIMIT 100)"));
            assert!(
                matches!(binds.get(binds.len()-2),Some(EventBind::Int64(value)) if *value==limit)
            );
        }
    }

    #[test]
    fn a_running_series_serves_its_start_before_the_next_occurrences() {
        let now = Utc::now();
        let base = DateTime::from_timestamp(now.timestamp(), 0).unwrap();
        let start = base - chrono::Duration::days(7);
        let next = [
            base + chrono::Duration::days(7),
            base + chrono::Duration::days(14),
        ];
        let series: Vec<String> = [start, next[0], next[1]]
            .iter()
            .map(|d| d.to_rfc3339())
            .collect();
        let mut row = row_with(
            json!({"recurrent": true, "recurrent_dates": series}),
            Some(start),
            Some(7_200_000),
        );
        row.recurrent = true;
        let rec = event_row_to_record(row, None, &test_view(None));
        // `EventModel.build` (`Event/model.ts:138-140`): the start, then the
        // occurrences that have not ended.
        assert_eq!(rec.recurrent_dates, vec![start, next[0], next[1]]);
    }

    #[test]
    fn record_sanitizes_descriptions_urls_and_images() {
        let mut row = row_with(json!({}), None, None);
        row.description = Some(
            "Join <link=\"file:///etc/passwd\">here</link> or <link=\"https://decentraland.org\">our site</link>".into(),
        );
        let rec = event_row_to_record(row, None, &test_view(None));
        assert_eq!(
            rec.description.as_deref(),
            Some("Join here or <link=\"https://decentraland.org\">our site</link>")
        );
        let metadata_ip = ["169", "254", "169", "254"].join(".");
        let raw = json!({
            "description": format!("<a href=\"smb://attacker/share\">x</a> <link=\"http://{metadata_ip}/\">y</link>")
        });
        let rec = event_row_to_record(row_with(raw, None, None), None, &test_view(None));
        assert_eq!(rec.description.as_deref(), Some("x y"));

        let raw = json!({
            "url": "javascript:alert(1)",
            "image": "http://127.0.0.1/poster.png",
            "image_vertical": "javascript:alert(1)",
        });
        let rec = event_row_to_record(row_with(raw, None, None), None, &test_view(None));
        assert_eq!((rec.url, rec.image, rec.image_vertical), (None, None, None));

        let raw = json!({
            "url": "https://decentraland.org/events",
            "image": "https://decentraland.org/poster.png",
            "image_vertical": "/poster/bafabc123",
        });
        let rec = event_row_to_record(row_with(raw, None, None), None, &test_view(None));
        assert_eq!(rec.url.as_deref(), Some("https://decentraland.org/events"));
        assert_eq!(
            rec.image.as_deref(),
            Some("https://decentraland.org/poster.png")
        );
        assert_eq!(
            rec.image_vertical,
            Some(Value::String("/poster/bafabc123".into()))
        );
    }

    #[test]
    fn record_names_carry_no_rich_text_tags() {
        let raw = json!({
            "user_name": "<size=200>Host</size>",
            "scene_name": "<link=\"file:///x\">Scene</link>",
            "estate_name": "<#ff0000>Estate",
        });
        let mut row = row_with(raw, None, None);
        row.name = "Party <link=\"smb://h/s\">here</link>".into();
        let rec = event_row_to_record(row, None, &test_view(None));
        assert_eq!(rec.name, "Party here");
        assert_eq!(rec.scene_name.as_deref(), Some("Scene"));
        assert_eq!(rec.estate_name.as_deref(), Some("Estate"));
        assert_eq!(rec.user_name.as_deref(), Some("Host"));

        let mut row = row_with(json!({ "scene_name": "Plain Scene" }), None, None);
        row.name = "Plain <3 party".into();
        let rec = event_row_to_record(row, None, &test_view(None));
        assert_eq!(rec.name, "Plain <3 party");
        assert_eq!(rec.estate_name.as_deref(), Some("Plain Scene"));
    }

    fn at(
        id: &str,
        place_id: Option<&str>,
        server: Option<&str>,
        world: bool,
        name: Option<&str>,
    ) -> EventAt {
        EventAt {
            id: id.into(),
            name: name.map(String::from),
            place_id: place_id.map(String::from),
            server: server.map(String::from),
            world,
            next_start_at: Some(Utc::now().naive_utc()),
        }
    }

    #[test]
    fn a_world_event_resolves_by_its_world_and_a_scene_event_never_by_its_realm() {
        let asked = AskedKeys::new(&["My-World.dcl.eth".to_string(), "Place-1".to_string()]);
        for place_id in [Some("upstream-uuid"), None] {
            let ev = at(
                "e",
                place_id,
                Some("my-world.dcl.eth"),
                true,
                Some("World Party"),
            );
            assert_eq!(asked.resolve(&ev).as_deref(), Some("My-World.dcl.eth"));
        }
        let ev = at(
            "e",
            Some("place-1"),
            Some("peer.decentraland.org"),
            false,
            Some("Movie Night"),
        );
        assert_eq!(asked.resolve(&ev).as_deref(), Some("Place-1"));
        let ev = at(
            "e",
            None,
            Some("peer.decentraland.org"),
            false,
            Some("Movie Night"),
        );
        assert_eq!(asked.resolve(&ev), None);
        let ev = at("e", Some("place-9"), None, false, Some("Elsewhere"));
        assert_eq!(asked.resolve(&ev), None);
    }
}
