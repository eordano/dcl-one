//! The events read routes (`/api/events...`, `/api/schedules...`).
//!
//! `/api/events`, `/api/events/search` and `/api/events/:event_id` validate
//! their query and params against the upstream ajv schemas (`validate`,
//! gatsby flavour: `createValidator`) after the admin bearer middleware and
//! before any lookup, in upstream's order (`Event/routes/index.ts:69-96`).
//! `/v1/events` is dcl-one's own and keeps the lenient parse with its extra
//! `recurrent`, `with_recurrent_dates` and `allow_deleted` params.

use crate::shared::js::{number_from_str, number_to_string};
use axum::Json;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{
    EventAttendeeRecord, EventCategoryRecord, EventListFilters, EventListType, EventRecord,
    ScheduleRecord, SortOrder,
};
use crate::SharedState;
use crate::shared::errors::{ApiData, ApiError};
pub(crate) use crate::shared::parcel::is_inside_world_limits;
use crate::validate::{self, Flavour};

/// `adminBearer.ts:19-23`: a `Bearer` header while the token is unset.
pub const ADMIN_TOKEN_UNCONFIGURED: &str =
    "EVENTS_ADMIN_AUTH_TOKEN is not configured; events admin endpoints are disabled";

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "events/"))]
pub struct EventListWithTotal {
    pub events: Vec<EventRecord>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: i64,
}

/// A places_ids/community_id query answers `{events, total}`; the plain list
/// answers a bare array. The Unity client parses each shape by query.
#[derive(Debug, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "events/"))]
pub enum EventListData {
    WithTotal(EventListWithTotal),
    Events(Vec<EventRecord>),
}

#[derive(Debug, Deserialize, Default)]
pub struct EventListQuery {
    pub limit: Option<String>,
    pub offset: Option<String>,
    pub list: Option<String>,
    pub order: Option<String>,
    pub highlighted: Option<String>,
    pub creator: Option<String>,
    pub world: Option<String>,
    pub recurrent: Option<String>,
    pub world_names: Option<Vec<String>>,
    pub position: Option<String>,
    pub positions: Option<Vec<String>>,
    pub estate_id: Option<String>,
    pub community_id: Option<String>,
    pub places_ids: Option<Vec<String>>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub search: Option<String>,
    pub only_attendee: Option<String>,
    pub schedule: Option<String>,
    pub owner: Option<String>,
    pub with_connected_users: Option<String>,
    pub with_recurrent_dates: Option<String>,
    pub approved: Option<String>,
    pub rejected: Option<String>,
    pub deleted: Option<String>,
    pub allow_deleted: Option<String>,
}

impl EventListQuery {
    fn from_pairs(pairs: &[(String, String)]) -> Self {
        let mut q = EventListQuery::default();
        let (mut positions, mut world_names, mut places_ids) = (Vec::new(), Vec::new(), Vec::new());
        for (k, v) in pairs {
            match k.as_str() {
                "limit" => q.limit = Some(v.clone()),
                "offset" => q.offset = Some(v.clone()),
                "list" => q.list = Some(v.clone()),
                "order" => q.order = Some(v.clone()),
                "highlighted" => q.highlighted = Some(v.clone()),
                "creator" => q.creator = Some(v.clone()),
                "world" => q.world = Some(v.clone()),
                "recurrent" => q.recurrent = Some(v.clone()),
                "position" => q.position = Some(v.clone()),
                "estate_id" => q.estate_id = Some(v.clone()),
                "community_id" => q.community_id = Some(v.clone()),
                "from" => q.from = Some(v.clone()),
                "to" => q.to = Some(v.clone()),
                "search" => q.search = Some(v.clone()),
                "only_attendee" => q.only_attendee = Some(v.clone()),
                "schedule" => q.schedule = Some(v.clone()),
                "owner" => q.owner = Some(v.clone()),
                "with_connected_users" => q.with_connected_users = Some(v.clone()),
                "with_recurrent_dates" => q.with_recurrent_dates = Some(v.clone()),
                "approved" => q.approved = Some(v.clone()),
                "rejected" => q.rejected = Some(v.clone()),
                "deleted" => q.deleted = Some(v.clone()),
                "allow_deleted" => q.allow_deleted = Some(v.clone()),
                "positions" | "positions[]" => positions.push(v.clone()),
                "world_names" | "world_names[]" => world_names.push(v.clone()),
                "places_ids" | "places_ids[]" => places_ids.push(v.clone()),
                _ => {}
            }
        }
        if !positions.is_empty() {
            q.positions = Some(positions);
        }
        if !world_names.is_empty() {
            q.world_names = Some(world_names);
        }
        if !places_ids.is_empty() {
            q.places_ids = Some(places_ids);
        }
        q
    }

    /// The query once `validate::EVENT_LIST_QUERY` accepted it: every key a
    /// string, the three lists arrays (`places_ids` always, from the body).
    fn from_express(query: &Map<String, Value>) -> Self {
        let s = |key: &str| query.get(key).and_then(Value::as_str).map(str::to_string);
        let list = |key: &str| {
            query.get(key).and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        };
        EventListQuery {
            limit: s("limit"),
            offset: s("offset"),
            list: s("list"),
            order: s("order"),
            highlighted: s("highlighted"),
            creator: s("creator"),
            world: s("world"),
            recurrent: None,
            world_names: list("world_names"),
            position: s("position"),
            positions: list("positions"),
            estate_id: s("estate_id"),
            community_id: s("community_id"),
            places_ids: list("places_ids").filter(|ids| !ids.is_empty()),
            from: s("from"),
            to: s("to"),
            search: s("search"),
            only_attendee: s("only_attendee"),
            schedule: s("schedule"),
            owner: s("owner"),
            with_connected_users: s("with_connected_users"),
            with_recurrent_dates: None,
            approved: s("approved"),
            rejected: s("rejected"),
            deleted: s("deleted"),
            allow_deleted: None,
        }
    }
}

/// `getEventList.ts:139-142`: `req.query` (as express parses it) with
/// `places_ids` replaced by the body's `placeIds` (`[]` on GET), validated
/// against `getEventListQuery` (`Event/schemas.ts:23-171`). Captures
/// `events.get_events.limit_not_uint`, `.from_not_datetime`,
/// `.positions_item_pattern`, `.unknown_param`.
fn validate_event_list_query(
    pairs: &[(String, String)],
    places_ids: Value,
) -> Result<EventListQuery, ApiError> {
    let mut query = validate::express_query(pairs);
    query.insert("places_ids".to_string(), places_ids);
    validate::validate(
        &validate::EVENT_LIST_QUERY,
        &Value::Object(query.clone()),
        Flavour::Gatsby,
    )?;
    Ok(EventListQuery::from_express(&query))
}

/// `getEventParamsSchema` on `:event_id` (`Event/schemas.ts:11-21`,
/// `getEvent.ts:35`): a non-UUID is a 400. Capture `events.get_event.not_uuid`.
fn validate_event_id(event_id: &str) -> Result<(), ApiError> {
    validate::validate(
        &validate::EVENT_PARAMS,
        &json!({ "event_id": event_id }),
        Flavour::Gatsby,
    )
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_position(s: &str) -> Option<(i32, i32)> {
    let (x, y) = crate::auth::parse_pointer(s)?;
    Some((i32::try_from(x).ok()?, i32::try_from(y).ok()?))
}

fn parse_filters(
    q: &EventListQuery,
    body_place_ids: Vec<String>,
    body_community_id: Option<String>,
    user: Option<String>,
    admin: bool,
    include_unapproved: bool,
) -> Result<Option<EventListFilters>, ApiError> {
    let recurrent = q
        .recurrent
        .as_deref()
        .map(|value| {
            parse_bool(value)
                .ok_or_else(|| ApiError::bad_request("recurrent must be true, false, 1 or 0"))
        })
        .transpose()?;

    let with_recurrent_dates = q
        .with_recurrent_dates
        .as_deref()
        .and_then(parse_bool)
        .unwrap_or(false);
    let requested_limit = q.limit.as_deref().and_then(|s| s.parse::<i64>().ok());
    if with_recurrent_dates && requested_limit.is_some_and(|limit| limit > 25) {
        return Err(ApiError::bad_request(
            "with_recurrent_dates requires limit <= 25",
        ));
    }
    let limit = requested_limit
        .map(|n| n.clamp(0, 500))
        .unwrap_or(if with_recurrent_dates { 25 } else { 500 });
    if limit == 0 {
        return Ok(None);
    }
    let offset = q
        .offset
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .map(|n| n.max(0))
        .unwrap_or(0);
    let mut list = match q.list.as_deref() {
        Some("all") => EventListType::All,
        Some("live") => EventListType::Live,
        Some("upcoming") => EventListType::Upcoming,
        Some("relevance") => EventListType::Relevance,
        _ => EventListType::Active,
    };
    let order = match q.order.as_deref() {
        Some("desc") => SortOrder::Desc,
        Some("asc") => SortOrder::Asc,
        _ if q.search.is_some() => SortOrder::Desc,
        _ => SortOrder::Asc,
    };
    let mut highlighted = q.highlighted.as_deref().and_then(parse_bool);
    if matches!(q.list.as_deref(), Some("highlight")) {
        highlighted = Some(true);
        list = EventListType::Active;
    }

    // getEventList.ts:204-230: `position` and every `positions[]` entry are
    // checked (an out-of-bounds one is `[]`), then `model.ts:168-179,242-249`
    // filters on `position` alone when it is set.
    let mut position: Option<(i32, i32)> = None;
    if let Some(p) = &q.position {
        match parse_position(p) {
            Some(pos) if is_inside_world_limits(pos.0, pos.1) => position = Some(pos),
            Some(_) => return Ok(None),
            None => return Err(ApiError::bad_request("invalid position")),
        }
    }
    let mut positions: Vec<(i32, i32)> = Vec::new();
    if let Some(ps) = &q.positions {
        for s in ps {
            match parse_position(s) {
                Some(pos) if is_inside_world_limits(pos.0, pos.1) => positions.push(pos),
                Some(_) => return Ok(None),
                None => return Err(ApiError::bad_request("invalid position in positions[]")),
            }
        }
    }
    if let Some(pos) = position {
        positions = vec![pos];
    }

    let mut places_ids = q.places_ids.clone().unwrap_or_default();
    places_ids.extend(body_place_ids);
    let community_id = q.community_id.clone().or(body_community_id);

    let from = q.from.as_deref().and_then(query_instant);
    let to = q.to.as_deref().and_then(query_instant);

    let search = match q.search.as_deref() {
        Some(s) if !has_three_word_chars(s) => return Ok(None),
        Some(s) => Some(s.to_string()),
        None => None,
    };

    let owner = resolve_owner(q);
    let creator = if owner {
        None
    } else {
        match q.creator.as_deref() {
            Some(c) if is_ethereum_address(c) => Some(c.to_lowercase()),
            Some(_) => return Ok(None),
            None => None,
        }
    };

    // getEventList.ts:240-248: `String(Number(query.estate_id))`, so `0042`
    // filters on `42`; a non-finite value is `[]`.
    let estate_id = match q.estate_id.as_deref() {
        Some(e) if !e.is_empty() => match number_from_str(e) {
            n if n.is_finite() => Some(number_to_string(n)),
            _ => return Ok(None),
        },
        _ => None,
    };

    Ok(Some(EventListFilters {
        limit,
        offset,
        list,
        order,
        highlighted,
        creator,
        world: q.world.as_deref().and_then(parse_bool),
        recurrent,
        world_names: q.world_names.clone().unwrap_or_default(),
        positions,
        estate_id,
        community_id,
        schedule: q.schedule.clone(),
        with_recurrent_dates,
        places_ids,
        destination_ids: Vec::new(),
        from,
        to,
        search,
        user: user.clone(),
        rejected: if admin {
            q.rejected.as_deref().and_then(parse_bool)
        } else {
            None
        },
        approved: if admin {
            q.approved.as_deref().and_then(parse_bool)
        } else {
            None
        },
        deleted: if admin {
            q.deleted.as_deref().and_then(parse_bool)
        } else {
            None
        },
        admin,
        include_rejected: admin,
        only_attendee: resolve_only_attendee(q) && user.is_some(),
        owner,
        include_own: true,
        include_deleted: admin
            && q.allow_deleted
                .as_deref()
                .and_then(parse_bool)
                .unwrap_or(false),
        include_unapproved,
    }))
}

/// JavaScript's falsy JSON values: `null`, `false`, `0`, `""`.
fn js_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// `/\w{3}/` (`getEventList.ts:193`): three ASCII word characters in a row.
fn has_three_word_chars(s: &str) -> bool {
    let mut run = 0;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            run += 1;
            if run >= 3 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn is_ethereum_address(s: &str) -> bool {
    let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) else {
        return false;
    };
    hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

fn resolve_only_attendee(q: &EventListQuery) -> bool {
    match q.only_attendee.as_deref() {
        Some(v) => parse_bool(v).unwrap_or(true),
        None => false,
    }
}

fn resolve_owner(q: &EventListQuery) -> bool {
    q.owner.as_deref().and_then(parse_bool).unwrap_or(false)
}

/// The bearer as a plain predicate: `true` only for the configured token.
/// The writes still read it this way; the reads refuse through [`admin_bearer`].
pub(crate) fn is_admin_request(headers: &HeaderMap, state: &SharedState) -> bool {
    crate::auth::admin_bearer::matches(state.settings.events_admin_token.as_deref(), headers)
}

/// The events `adminBearer` middleware (`Event/routes/middleware/adminBearer.ts:18-64`):
/// a signed request wins and the header is not read (`:45-47`); a header
/// that is not `Bearer …` is anonymous (`:49-55`); a `Bearer` while
/// `EVENTS_ADMIN_AUTH_TOKEN` is unset is 503 (`:19-24`); a mismatch is 401
/// "Unauthorized" (`:34-37`). Fixtures `events.get_events.bearer_wrong`,
/// `.bearer_unconfigured`, `events.get_event.signed_chain_overrides_bearer`,
/// `.bearer_non_bearer_scheme_is_anonymous`.
pub(crate) fn admin_bearer(
    state: &SharedState,
    headers: &HeaderMap,
    signed: bool,
) -> Result<bool, ApiError> {
    if signed {
        return Ok(false);
    }
    let Some(presented) = crate::auth::admin_bearer::bearer_token(headers) else {
        return Ok(false);
    };
    match crate::auth::admin_bearer::check_token(
        state.settings.events_admin_token.as_deref(),
        Some(presented),
    ) {
        Ok(()) => Ok(true),
        Err(crate::auth::admin_bearer::Refusal::NotConfigured) => {
            Err(ApiError::service_unavailable(ADMIN_TOKEN_UNCONFIGURED))
        }
        Err(_) => Err(ApiError::unauthorized("Unauthorized")),
    }
}

/// What a read may see (`getEventList.ts:166-186`, `getEvent.ts:98-108`):
/// the admin bearer lists rejected and deleted rows on request and reads
/// every event; a signed wallet holding `edit_any_event` or
/// `approve_any_event` (or a moderator, gatsby's `isAdmin`) gets
/// `allow_pending` only: pending events of every creator, rejected ones by
/// id but never in a list, no admin filters. Fixtures
/// `events.get_events.edit_any_allow_pending`, `.approve_any_allow_pending`,
/// `.bearer_admin_default`, `events.get_event.edit_any_sees_pending`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Access {
    pub bearer: bool,
    pub allow_pending: bool,
}

pub(crate) async fn access(
    state: &SharedState,
    headers: &HeaderMap,
    viewer: Option<&str>,
) -> Result<Access, ApiError> {
    if admin_bearer(state, headers, viewer.is_some())? {
        return Ok(Access {
            bearer: true,
            allow_pending: true,
        });
    }
    let allow_pending = match viewer {
        Some(v) => {
            let permissions = super::profiles::event_permissions(&state.pool, v).await?;
            permissions.edit_any || permissions.approve_any
        }
        None => false,
    };
    Ok(Access {
        bearer: false,
        allow_pending,
    })
}

async fn list_with(
    state: &SharedState,
    headers: &HeaderMap,
    method: &str,
    signed_path: &str,
    q: &EventListQuery,
    body_place_ids: Vec<String>,
    body_community_id: Option<String>,
) -> Result<Json<ApiData<EventListData>>, ApiError> {
    let user = state.viewer(headers, method, signed_path).await;
    let access = access(state, headers, user.as_deref()).await?;
    list_as(state, user, access, q, body_place_ids, body_community_id).await
}

/// The `/api` shape of a list: no `previous_place_id`.
fn api_list(data: EventListData) -> EventListData {
    match data {
        EventListData::Events(events) => {
            EventListData::Events(events.into_iter().map(EventRecord::api_shape).collect())
        }
        EventListData::WithTotal(EventListWithTotal { events, total }) => {
            EventListData::WithTotal(EventListWithTotal {
                events: events.into_iter().map(EventRecord::api_shape).collect(),
                total,
            })
        }
    }
}

async fn list_as(
    state: &SharedState,
    user: Option<String>,
    access: Access,
    q: &EventListQuery,
    body_place_ids: Vec<String>,
    body_community_id: Option<String>,
) -> Result<Json<ApiData<EventListData>>, ApiError> {
    // getEventList.ts:144-151.
    if resolve_owner(q) && user.is_none() {
        return Err(ApiError::unauthorized(
            "owner filter requires authentication",
        ));
    }
    let anonymous = user.is_none();
    let Some(filters) = parse_filters(
        q,
        body_place_ids,
        body_community_id,
        user,
        access.bearer,
        access.allow_pending,
    )?
    else {
        return Ok(Json(ApiData::ok(EventListData::Events(Vec::new()))));
    };
    // getEventList.ts:259-264: after the empty answers (`limit=0`, a short
    // search, a position outside the world, a bad estate or creator).
    if q.only_attendee.is_some() && anonymous {
        return Err(ApiError::unauthorized(
            "only_attendee filter requieres autentication",
        ));
    }
    let envelope_with_total = !filters.places_ids.is_empty() || filters.community_id.is_some();
    let (mut events, total) = state.events.query(&filters, envelope_with_total).await?;
    if q.with_connected_users
        .as_deref()
        .and_then(parse_bool)
        .unwrap_or(false)
    {
        fill_connected(&mut events, &state.live).await;
    }
    let data = if envelope_with_total {
        EventListData::WithTotal(EventListWithTotal { events, total })
    } else {
        EventListData::Events(events)
    };
    Ok(Json(ApiData::ok(data)))
}

pub(super) async fn fill_connected(
    events: &mut [EventRecord],
    live: &std::sync::Arc<crate::places::live::Live>,
) {
    let keys = events
        .iter()
        .map(|event| connected_key(event.world, event.server.as_deref(), event.x, event.y))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let connected = live.connected_by_key(keys).await;
    for e in events {
        let key = connected_key(e.world, e.server.as_deref(), e.x, e.y);
        // `addConnectedUsersToEvents` (`getEventList.ts:317-323`): every item
        // carries the array, an unknown observation as an empty one (upstream
        // fell back to `[]` when the gatekeeper failed).
        e.connected_addresses = Some(connected.get(&key).cloned().flatten().unwrap_or_default());
    }
}

fn connected_key(world: bool, server: Option<&str>, x: i32, y: i32) -> (bool, String) {
    match server.filter(|server| world && !server.is_empty()) {
        Some(server) => (true, server.to_ascii_lowercase()),
        None => (false, format!("{x},{y}")),
    }
}

/// `GET /api/events` in upstream's order (`Event/routes/index.ts:69-75`):
/// optional signature, the admin bearer middleware, then the handler's
/// validation (`getEventList.ts:139-142`).
pub async fn get_event_list(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<ApiData<EventListData>>, ApiError> {
    let user = state.viewer(&headers, "get", uri.path()).await;
    let access = access(&state, &headers, user.as_deref()).await?;
    let q = validate_event_list_query(&pairs, json!([]))?;
    let Json(data) = list_as(&state, user, access, &q, Vec::new(), None).await?;
    Ok(Json(ApiData::ok(api_list(data.data))))
}

/// `GET /v1/events`: dcl-one's own list, the lenient parse with its extra
/// `recurrent`, `with_recurrent_dates` and `allow_deleted` params.
pub async fn get_v1_event_list(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<ApiData<EventListData>>, ApiError> {
    let q = EventListQuery::from_pairs(&pairs);
    list_with(&state, &headers, "get", uri.path(), &q, Vec::new(), None).await
}

/// `POST /api/events/search` (`Event/routes/index.ts:77-83`): the body's
/// `placeIds` becomes the validated `places_ids` (`getEventList.ts:130-142`),
/// `communityId` the community filter. Fixtures `events.post_search.*`.
pub async fn post_event_search(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
    body: Option<Json<Value>>,
) -> Result<Json<ApiData<EventListData>>, ApiError> {
    let user = state.viewer(&headers, "post", uri.path()).await;
    let access = access(&state, &headers, user.as_deref()).await?;
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    // `req.body.placeIds || []` (`getEventList.ts:135`): a JSON-falsy value
    // (`null`, `false`, `0`, `""`) is the empty list.
    let place_ids = body
        .get("placeIds")
        .filter(|v| !js_falsy(v))
        .cloned()
        .unwrap_or(json!([]));
    let community_id = body
        .get("communityId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let q = validate_event_list_query(&pairs, place_ids)?;
    let body_place_ids = q.places_ids.clone().unwrap_or_default();
    let q = EventListQuery {
        places_ids: None,
        ..q
    };
    let Json(data) = list_as(&state, user, access, &q, body_place_ids, community_id).await?;
    Ok(Json(ApiData::ok(api_list(data.data))))
}

pub(crate) fn is_soft_deleted(evt: &EventRecord) -> bool {
    evt.deleted_by_user || evt.deleted_by_admin || evt.deleted_at.is_some()
}

/// `getEventWithOptions` (`getEvent.ts:61-79`): a pending or rejected event
/// is read by its owner, a moderator, an `approve_any_event` or
/// `edit_any_event` grant and the admin bearer; everyone reads an approved
/// one, soft-deleted or not (there is no deletion check there; fixture
/// `events.get_event.deleted_anonymous`).
pub(crate) fn visible_to_viewer(
    evt: &EventRecord,
    viewer: Option<&str>,
    allow_pending: bool,
) -> bool {
    if allow_pending {
        return true;
    }
    let owner =
        matches!((viewer, evt.user.as_deref()), (Some(v), Some(u)) if v.eq_ignore_ascii_case(u));
    if owner {
        return true;
    }
    evt.approved && !evt.rejected
}

fn not_found(event_id: &str) -> ApiError {
    ApiError::not_found(format!("Not found event \"{event_id}\""))
}

/// `getEvent` as the event routes and the attendee routes call it
/// (`index.ts:26-38`, `EventAttendee/routes.ts:39-42,61-85`): the bearer's
/// read with `contact`/`details`, else the viewer's with their attendance.
pub(crate) async fn load_visible_event(
    state: &SharedState,
    headers: &HeaderMap,
    event_id: &str,
    viewer: Option<&str>,
    bearer_honoured: bool,
) -> Result<EventRecord, ApiError> {
    let access = if bearer_honoured {
        access(state, headers, viewer).await?
    } else {
        // The attendee routes mount no `adminBearer`: a `Bearer` header is
        // simply an anonymous request there.
        let permissions = match viewer {
            Some(v) => Some(super::profiles::event_permissions(&state.pool, v).await?),
            None => None,
        };
        Access {
            bearer: false,
            allow_pending: permissions.is_some_and(|p| p.edit_any || p.approve_any),
        }
    };
    validate_event_id(event_id)?;
    let record = if access.bearer {
        state.events.get_for_admin(event_id).await?
    } else {
        state.events.get_for_viewer(event_id, viewer).await?
    };
    record
        .filter(|evt| visible_to_viewer(evt, viewer, access.allow_pending))
        .map(EventRecord::api_shape)
        .ok_or_else(|| not_found(event_id))
}

/// `GET /api/events/:event_id` (`Event/routes/index.ts:26-38,90-96`).
pub async fn get_event(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(event_id): Path<String>,
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    let viewer = state.viewer(&headers, "get", uri.path()).await;
    let evt = load_visible_event(&state, &headers, &event_id, viewer.as_deref(), true).await?;
    Ok(Json(ApiData::ok(evt)))
}

/// `GET /api/events/attending` (`getAttendingEventList.ts:6-10`): a signed
/// fetch (the bearer is not mounted here); every unfinished, non-rejected,
/// non-deleted event with the viewer's `attending` flag.
pub async fn get_attending_event_list(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<ApiData<Vec<EventRecord>>>, ApiError> {
    let user = state
        .viewer(&headers, "get", uri.path())
        .await
        .ok_or_else(|| ApiError::unauthorized("Unauthorized"))?;
    let events = state
        .events
        .attending(&user)
        .await?
        .into_iter()
        .map(EventRecord::api_shape)
        .collect();
    Ok(Json(ApiData::ok(events)))
}

pub async fn get_moderation_list(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<ApiData<Vec<EventRecord>>>, ApiError> {
    if !is_admin_request(&headers, &state) {
        return Err(ApiError::forbidden(
            if state.settings.events_admin_token.is_none() {
                "Admin token not configured"
            } else {
                "Invalid admin credentials"
            },
        ));
    }
    let limit = pairs
        .iter()
        .find(|(k, _)| k == "limit")
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .map(|n| n.clamp(0, 500))
        .unwrap_or(24);
    Ok(Json(ApiData::ok(
        state.events.moderation_pending(limit).await?,
    )))
}

pub async fn get_event_category_list() -> Json<ApiData<Vec<EventCategoryRecord>>> {
    Json(ApiData::ok(super::categories::list()))
}

/// `GET /api/events/:event_id/attendees` (`EventAttendee/routes.ts:21,39-42`):
/// `getEvent` first, so a hidden event is 404 and a non-UUID 400. The route
/// mounts no `auth()`, so no signature is read: a pending event is 404 for
/// its owner too. Fixtures `events.get_attendees.pending_anonymous`,
/// `.missing`, `.not_uuid`.
pub async fn get_event_attendees(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(event_id): Path<String>,
) -> Result<Json<ApiData<Vec<EventAttendeeRecord>>>, ApiError> {
    load_visible_event(&state, &headers, &event_id, None, false).await?;
    let list = super::attendees::list_for_event(&state.pool, &event_id).await?;
    Ok(Json(ApiData::ok(list)))
}

pub async fn get_schedule_list(
    State(state): State<SharedState>,
) -> Result<Json<ApiData<Vec<ScheduleRecord>>>, ApiError> {
    Ok(Json(ApiData::ok(state.schedules.list().await?)))
}

pub async fn get_schedule_by_id(
    State(state): State<SharedState>,
    Path(schedule_id): Path<String>,
) -> Result<Json<ApiData<ScheduleRecord>>, ApiError> {
    let s = state
        .schedules
        .get(&schedule_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Schedule \"{schedule_id}\" not found")))?;
    Ok(Json(ApiData::ok(s)))
}

/// `new Date(query.from)` (`getEventList.ts:289-299`): a bound the validator
/// passed and V8 parses, so a `±HHMM` offset counts, and one V8 makes `NaN`
/// (a bare `±HH`) drops the bound.
fn query_instant(s: &str) -> Option<DateTime<Utc>> {
    let parse = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    };
    if let Some(at) = parse(s) {
        return Some(at);
    }
    let bytes = s.as_bytes();
    let n = bytes.len().checked_sub(5)?;
    let compact = matches!(bytes[n], b'+' | b'-') && bytes[n + 1..].iter().all(u8::is_ascii_digit);
    compact
        .then(|| parse(&format!("{}:{}", &s[..n + 3], &s[n + 3..])))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_bounds_parse_as_v8_does() {
        let at = |s: &str| query_instant(s).map(|d| d.to_rfc3339());
        let utc = Some("2026-11-30T23:00:00+00:00".to_string());
        assert_eq!(at("2026-12-01T00:00:00+01:00"), utc);
        assert_eq!(at("2026-12-01T00:00:00+0100"), utc);
        assert_eq!(at("2026-12-01 00:00:00+0100"), utc);
        assert_eq!(
            at("2026-12-01T00:00:00.5z"),
            Some("2026-12-01T00:00:00.500+00:00".to_string())
        );
        assert_eq!(at("2026-12-01T00:00:00+01"), None, "`NaN` in V8");
        assert_eq!(at("2026-12-01T00:00:00+01:0"), None);
    }

    #[tokio::test]
    async fn signed_event_list_filters_use_current_trusted_admin_permissions() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let key = alloy_signer_local::PrivateKeySigner::random();
        let address = format!("{:#x}", key.address());
        let community = uuid::Uuid::new_v4().to_string();
        let pending = uuid::Uuid::new_v4().to_string();
        let public = uuid::Uuid::new_v4().to_string();
        for (id, approved) in [(&pending, false), (&public, true)] {
            super::super::mirror::upsert_event(&pool,&serde_json::json!({"id":id,"name":"Filter fixture","user":"0x1111111111111111111111111111111111111111","community_id":community,"approved":approved,"start_at":"2099-06-01T18:00:00Z","duration":3600000})).await.unwrap();
        }
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\",permissions,permissions_granted) VALUES($1,'[\"edit_any_event\"]',false)").bind(&address).execute(&pool).await.unwrap();
        let state = std::sync::Arc::new(crate::ExploreState::new(
            pool.clone(),
            crate::Settings::for_tests(),
            None,
        ));
        let headers = crate::shared::signed_fetch::test_support::signed_headers_by(
            &key,
            "get",
            "/api/events",
            "{}",
            true,
            &Utc::now().timestamp_millis().to_string(),
        )
        .await;
        let query = EventListQuery::from_pairs(&[
            ("community_id".into(), community),
            ("list".into(), "all".into()),
            ("approved".into(), "false".into()),
        ]);
        for granted in [false, true, false] {
            sqlx::query("UPDATE explore.event_profile_settings SET permissions_granted=$2 WHERE \"user\"=$1").bind(&address).bind(granted).execute(&pool).await.unwrap();
            let Json(body) =
                list_with(&state, &headers, "get", "/api/events", &query, vec![], None)
                    .await
                    .unwrap();
            let body = serde_json::to_value(body).unwrap();
            let ids: Vec<_> = body["data"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| event["id"].as_str().unwrap())
                .collect();
            // getEventList.ts:166-171: a granted wallet gets allow_pending, not
            // the admin filters, so `approved=false` is ignored for it.
            let mut ids = ids;
            ids.sort();
            let mut expected = if granted {
                vec![pending.as_str(), public.as_str()]
            } else {
                vec![public.as_str()]
            };
            expected.sort();
            assert_eq!(ids, expected);
        }
    }

    /// `Event/model.ts:183-190,238-241`: a signed viewer's default list has
    /// their pending events but not their rejected ones; `owner=true` lists
    /// both.
    #[tokio::test]
    async fn a_signed_viewer_lists_their_pending_events_but_not_their_rejected_ones() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let key = alloy_signer_local::PrivateKeySigner::random();
        let address = format!("{:#x}", key.address());
        let pending = uuid::Uuid::new_v4().to_string();
        let rejected = uuid::Uuid::new_v4().to_string();
        let public = uuid::Uuid::new_v4().to_string();
        for (id, approved, rejected_flag) in [
            (&pending, false, false),
            (&rejected, false, true),
            (&public, true, false),
        ] {
            super::super::mirror::upsert_event(&pool,&serde_json::json!({"id":id,"name":"Own rows","user":address,"approved":approved,"rejected":rejected_flag,"start_at":"2099-06-01T18:00:00Z","duration":3600000})).await.unwrap();
        }
        let state = std::sync::Arc::new(crate::ExploreState::new(
            pool.clone(),
            crate::Settings::for_tests(),
            None,
        ));
        let headers = crate::shared::signed_fetch::test_support::signed_headers_by(
            &key,
            "get",
            "/api/events",
            "{}",
            true,
            &Utc::now().timestamp_millis().to_string(),
        )
        .await;
        let seeded = [pending.clone(), rejected.clone(), public.clone()];
        let listed = |pairs: Vec<(String, String)>| {
            let state = state.clone();
            let headers = headers.clone();
            let seeded = seeded.clone();
            async move {
                let query = EventListQuery::from_pairs(&pairs);
                let Json(body) =
                    list_with(&state, &headers, "get", "/api/events", &query, vec![], None)
                        .await
                        .unwrap();
                let body = serde_json::to_value(body).unwrap();
                let mut ids: Vec<String> = body["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|event| event["id"].as_str().unwrap().to_string())
                    // The scratch database is shared with the other tests.
                    .filter(|id| seeded.contains(id))
                    .collect();
                ids.sort();
                ids
            }
        };
        let mut default_expected = vec![pending.clone(), public.clone()];
        default_expected.sort();
        assert_eq!(
            listed(vec![("list".into(), "all".into())]).await,
            default_expected
        );
        let mut owner_expected = vec![pending.clone(), rejected.clone(), public.clone()];
        owner_expected.sort();
        assert_eq!(
            listed(vec![
                ("list".into(), "all".into()),
                ("owner".into(), "true".into())
            ])
            .await,
            owner_expected
        );
    }

    fn filters_of(pairs: &[(&str, &str)]) -> Option<EventListFilters> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let q = EventListQuery::from_pairs(&pairs);
        parse_filters(&q, Vec::new(), None, None, false, false).unwrap()
    }

    /// `getEventList.ts:240-248`: `estate_id` is `String(Number(value))`.
    #[test]
    fn estate_id_is_normalised_as_number_does() {
        for (given, expected) in [
            ("0042", "42"),
            ("42", "42"),
            ("1e3", "1000"),
            ("0x10", "16"),
            ("1.5", "1.5"),
            (" 7 ", "7"),
            ("-3", "-3"),
            ("-0", "0"),
            ("\u{FEFF}5", "5"),
            ("0xfffffffffffffffff", "295147905179352830000"),
            ("1e21", "1e+21"),
            ("0.000001", "0.000001"),
        ] {
            let f = filters_of(&[("estate_id", given)]).unwrap();
            assert_eq!(f.estate_id.as_deref(), Some(expected), "{given}");
        }
        for not_finite in [
            "abc", "Infinity", "1e400", "+-1", "0x", "1_000", "nan", "inf", "0x+5", "-0x5",
            "\u{85}5",
        ] {
            assert!(
                filters_of(&[("estate_id", not_finite)]).is_none(),
                "{not_finite}"
            );
        }
        assert_eq!(filters_of(&[("estate_id", "")]).unwrap().estate_id, None);
        assert_eq!(
            filters_of(&[("estate_id", "   ")])
                .unwrap()
                .estate_id
                .as_deref(),
            Some("0")
        );
    }

    /// `getEventList.ts:204-230`, `model.ts:168-179,242-249`: `position` wins
    /// over `positions[]`, both are bounds-checked.
    #[test]
    fn position_wins_over_positions() {
        let f = filters_of(&[
            ("position", "1,2"),
            ("positions", "3,4"),
            ("positions", "5,6"),
        ])
        .unwrap();
        assert_eq!(f.positions, vec![(1, 2)]);
        let f = filters_of(&[("positions", "3,4"), ("positions[]", "5,6")]).unwrap();
        assert_eq!(f.positions, vec![(3, 4), (5, 6)]);
        assert!(filters_of(&[("position", "1,2"), ("positions", "999,0")]).is_none());
        assert!(filters_of(&[("position", "999,0"), ("positions", "1,2")]).is_none());
    }

    /// `/\w{3}/` (`getEventList.ts:193`).
    #[test]
    fn the_search_guard_counts_ascii_word_characters() {
        assert!(has_three_word_chars("abc"));
        assert!(has_three_word_chars("a_1 x"));
        assert!(!has_three_word_chars("ab c"));
        assert!(!has_three_word_chars("ñoñó"));
        assert!(has_three_word_chars("ñoñ abc"));
    }

    /// `req.body.placeIds || []` (`getEventList.ts:135`).
    #[test]
    fn falsy_place_ids_are_the_empty_list() {
        for falsy in [Value::Null, json!(false), json!(0), json!("")] {
            assert!(js_falsy(&falsy), "{falsy}");
        }
        for truthy in [
            json!([]),
            json!({}),
            json!(true),
            json!(1),
            json!("x"),
            json!([null]),
        ] {
            assert!(!js_falsy(&truthy), "{truthy}");
        }
    }

    /// A community_id query selects the WithTotal envelope. The Unity client
    /// (EventWithPlaceIdDTOListResponse) parses exactly
    /// `{"ok":true,"data":{"events":[...],"total":n}}` for this query, so the
    /// empty case must serialize to that byte sequence -- the untagged
    /// EventListData must not collapse to a bare array.
    #[test]
    fn envelopes_follow_the_query_shape() {
        let pairs = vec![
            (
                "community_id".to_string(),
                "11111111-2222-3333-4444-555555555555".to_string(),
            ),
            ("limit".to_string(), "10".to_string()),
        ];
        let q = EventListQuery::from_pairs(&pairs);
        let filters = parse_filters(
            &q,
            Vec::new(),
            None,
            Some("0xabc".to_string()),
            false,
            false,
        )
        .unwrap()
        .unwrap();
        assert!(filters.community_id.is_some());
        let body = ApiData::ok(EventListData::WithTotal(EventListWithTotal {
            events: Vec::new(),
            total: 0,
        }));
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"ok":true,"data":{"events":[],"total":0}}"#
        );

        let q = EventListQuery::from_pairs(&[("limit".to_string(), "10".to_string())]);
        let filters = parse_filters(&q, Vec::new(), None, None, false, false)
            .unwrap()
            .unwrap();
        assert!(filters.places_ids.is_empty() && filters.community_id.is_none());
        let body = ApiData::ok(EventListData::Events(Vec::new()));
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"ok":true,"data":[]}"#
        );
    }

    #[test]
    fn filters_follow_upstream_parsing_rules() {
        let q = EventListQuery::from_pairs(&[
            ("list".to_string(), "highlight".to_string()),
            ("position".to_string(), "10,-20".to_string()),
            ("positions[]".to_string(), "1,1".to_string()),
            (
                "creator".to_string(),
                "0xF39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string(),
            ),
            ("estate_id".to_string(), "6112".to_string()),
            ("search".to_string(), "party".to_string()),
            ("approved".to_string(), "false".to_string()),
        ]);
        let f = parse_filters(&q, vec!["p1".into()], None, None, false, false)
            .unwrap()
            .unwrap();
        assert_eq!(f.list, EventListType::Active);
        assert_eq!(f.highlighted, Some(true));
        // model.ts:168-179: `position` wins over `positions[]`.
        assert_eq!(f.positions, vec![(10, -20)]);
        assert_eq!(
            f.creator.as_deref(),
            Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
        );
        assert_eq!(f.estate_id.as_deref(), Some("6112"));
        assert_eq!(
            f.order,
            SortOrder::Desc,
            "a search sorts by rank descending"
        );
        assert_eq!(f.places_ids, vec!["p1"]);
        assert_eq!(f.approved, None, "status filters are admin-only");
        assert!(!f.only_attendee);

        let q = EventListQuery::from_pairs(&[("limit".to_string(), "0".to_string())]);
        assert!(
            parse_filters(&q, Vec::new(), None, None, false, false)
                .unwrap()
                .is_none()
        );
        let q = EventListQuery::from_pairs(&[("position".to_string(), "9999,0".to_string())]);
        assert!(
            parse_filters(&q, Vec::new(), None, None, false, false)
                .unwrap()
                .is_none()
        );
        let q = EventListQuery::from_pairs(&[("position".to_string(), "abc".to_string())]);
        assert!(parse_filters(&q, Vec::new(), None, None, false, false).is_err());
        let q = EventListQuery::from_pairs(&[("search".to_string(), "ab".to_string())]);
        assert!(
            parse_filters(&q, Vec::new(), None, None, false, false)
                .unwrap()
                .is_none()
        );
        let q =
            EventListQuery::from_pairs(&[("creator".to_string(), "not-an-address".to_string())]);
        assert!(
            parse_filters(&q, Vec::new(), None, None, false, false)
                .unwrap()
                .is_none()
        );

        let q = EventListQuery::from_pairs(&[
            ("owner".to_string(), "true".to_string()),
            (
                "creator".to_string(),
                "0xF39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string(),
            ),
            ("only_attendee".to_string(), "".to_string()),
            ("allow_deleted".to_string(), "true".to_string()),
        ]);
        let f = parse_filters(&q, Vec::new(), None, Some("0xabc".into()), true, true)
            .unwrap()
            .unwrap();
        assert!(f.owner && f.creator.is_none() && f.only_attendee && f.include_deleted && f.admin);
    }

    #[test]
    fn expanded_recurrence_pagination_rejects_oversized_limits_instead_of_skipping_rows() {
        let mut query =
            EventListQuery::from_pairs(&[("with_recurrent_dates".into(), "true".into())]);
        assert_eq!(
            parse_filters(&query, vec![], None, None, false, false)
                .unwrap()
                .unwrap()
                .limit,
            25
        );
        query.limit = Some("26".into());
        assert!(parse_filters(&query, vec![], None, None, false, false).is_err());
        query.limit = Some("25".into());
        query.offset = Some("25".into());
        let filters = parse_filters(&query, vec![], None, None, false, false)
            .unwrap()
            .unwrap();
        assert_eq!((filters.limit, filters.offset), (25, 25));
    }

    #[test]
    fn world_limits_and_admin_bearer_comparison() {
        assert!(is_inside_world_limits(0, 0));
        assert!(is_inside_world_limits(-150, 150));
        assert!(is_inside_world_limits(160, 155));
        assert!(is_inside_world_limits(151, 151));
        assert!(!is_inside_world_limits(151, 159));
        assert!(!is_inside_world_limits(200, 0));
    }
    #[test]
    fn invalid_recurrent_filters_are_rejected_instead_of_ignored() {
        let query = EventListQuery {
            recurrent: Some("yes".into()),
            ..Default::default()
        };
        assert!(parse_filters(&query, Vec::new(), None, None, false, false).is_err());
        let query = EventListQuery {
            search: Some("x".into()),
            ..query
        };
        assert!(parse_filters(&query, Vec::new(), None, None, false, false).is_err());
    }
}
