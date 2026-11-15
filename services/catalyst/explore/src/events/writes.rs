//! The events write routes: `POST /api/events`, `PATCH`/`DELETE
//! /api/events/{id}` and the RSVPs `POST`/`DELETE /api/events/{id}/attendees`,
//! as the old events service answered them (`decentraland/events@b60d5bbd`,
//! `Event/routes/{index,createEvent,updateEvent,deleteEvent,admin}.ts`,
//! `EventAttendee/routes.ts`).
//!
//! Two callers. The events admin bearer (`EVENTS_ADMIN_AUTH_TOKEN`; the
//! `adminBearer` middleware, [`super::handlers::admin_bearer`]) is honoured on
//! PATCH and DELETE only (`index.ts:97-108`), never on POST or the attendee
//! routes: it moderates with upstream's state body (`admin.ts:253-308`:
//! `approved`/`rejected` with `actor` and `reason`), edits content with the
//! editable attributes (`admin.ts:310-387`) and soft-deletes as its actor
//! (default `admin`), behind `OPS_WRITES_ENABLED`. A signed wallet
//! creates its own event (pending approval; the signer needs a profile this
//! node holds, `withAuthProfile()`), edits its own (or any, with
//! `edit_any_event`), deletes its own (or any, with `edit_any_event`) and
//! RSVPs, behind `DATA_WRITES_ENABLED`. Unknown body fields are dropped on
//! PATCH (`lodash/pick`), refused on POST (`newEventSchema`,
//! `additionalProperties: false`). A plain signed fetch needs no
//! `x-identity-metadata`; a signed `body_sha256` must match
//! and a signed `nonce` refuses its replay (409).
//!
//! Every write lands in `explore.event` with its `explore.events_local`
//! audit in one transaction (`events::moderation::store_locked`). Only events
//! created here (`origin = 'local'`) take an edit or an owner's delete: on a
//! mirrored event those answer 409 naming the upstream (the mirror writes its
//! fields; dcl-one-only). The admin's moderation and delete of a mirrored
//! event are local policy (`explore.event_moderation`, re-applied by every
//! pass; the delete hides it from the public reads), and an RSVP to it is a
//! local row the count adds; the mirror deletes both with the event.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method};
use chrono::Utc;
use serde_json::{Map, Value, json};
#[cfg(test)]
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

use super::moderation::{
    self, AdminState, CreateEvent, MAX_EVENT_DURATION_MS, PAST_FINISH_AT_MESSAGE,
    PAST_RECURRENT_UNTIL_MESSAGE, check_world_limits, is_soft_deleted, max_duration_message,
    normalized_series, parse_rfc3339, public_body, resolve_poster, validate_event_url,
};
use super::profiles::EventPermissions;
use super::{EventAttendeeRecord, EventRecord};
use crate::SharedState;
use crate::shared::errors::{ApiData, ApiError};
use crate::validate::{self, Flavour};

/// `admin.ts:35`.
pub const DEFAULT_ADMIN_ACTOR: &str = "admin";
/// `Event/types.ts:102`.
const MAX_ADMIN_ACTOR_LENGTH: usize = 42;
/// `Event/types.ts:100-101`.
const MAX_REASON_LENGTH: usize = 500;

/// `editEventAttributes` (`Event/types.ts:276-300`).
const EDIT_EVENT_ATTRIBUTES: &[&str] = &[
    "image",
    "image_vertical",
    "rejected",
    "name",
    "description",
    "start_at",
    "duration",
    "all_day",
    "x",
    "y",
    "server",
    "recurrent",
    "recurrent_frequency",
    "recurrent_setpos",
    "recurrent_monthday",
    "recurrent_weekday_mask",
    "recurrent_month_mask",
    "recurrent_interval",
    "recurrent_count",
    "recurrent_until",
    "categories",
    "world",
    "featured_item",
];
/// `editOwnEventAttributes` (`:306-310`).
const EDIT_OWN_EVENT_ATTRIBUTES: &[&str] = &["contact", "details", "community_id"];
/// `editAnyEventAttributes` (`:312-317`).
const EDIT_ANY_EVENT_ATTRIBUTES: &[&str] = &["highlighted", "trending", "schedules", "url"];
/// `approveEventAttributes` (`:319`).
const APPROVE_EVENT_ATTRIBUTES: &[&str] = &["approved"];
/// `NON_MODERATED_EDITABLE_FIELDS` (`updateEvent.ts:75-89`): the editable
/// fields whose change does not re-open moderation.
const NON_MODERATED_EDITABLE_FIELDS: &[&str] = &[
    "start_at",
    "duration",
    "all_day",
    "recurrent",
    "recurrent_frequency",
    "recurrent_setpos",
    "recurrent_monthday",
    "recurrent_weekday_mask",
    "recurrent_month_mask",
    "recurrent_interval",
    "recurrent_count",
    "recurrent_until",
    "rejected",
];
/// `STATE_ONLY_FIELDS` (`admin.ts:63`).
const STATE_ONLY_FIELDS: &[&str] = &["actor", "approved", "rejected", "reason"];
/// `touchesDates` (`updateEvent.ts:400-412`).
const DATE_FIELDS: &[&str] = &[
    "start_at",
    "duration",
    "recurrent",
    "recurrent_frequency",
    "recurrent_interval",
    "recurrent_count",
    "recurrent_until",
    "recurrent_setpos",
    "recurrent_monthday",
    "recurrent_weekday_mask",
    "recurrent_month_mask",
];
/// `touchesRecurrence` (`updateEvent.ts:270-277`).
const RECURRENCE_FIELDS: &[&str] = &[
    "start_at",
    "recurrent",
    "recurrent_frequency",
    "recurrent_interval",
    "recurrent_count",
    "recurrent_until",
];
/// The body keys `newEventSchema` accepts that a create never stores from
/// the body (`createEvent.ts:235-264` overrides them).
const CREATE_OVERRIDDEN_FIELDS: &[&str] = &[
    "approved",
    "rejected",
    "rejection_reason",
    "deleted_by_user",
    "deleted_by_admin",
    "deleted_reason",
    "highlighted",
    "trending",
];

// ---------------------------------------------------------------------------
// Bodies

/// `req.body` as an object (`admin.ts:70-74`, `bodyAsRecord`; `lodash/pick`
/// of anything else is `{}`).
fn body_object(bytes: &[u8]) -> Map<String, Value> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn is_falsy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => !b,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::String(s)) => s.is_empty(),
        Some(_) => false,
    }
}

/// The body fields the old schema typed `integer` (`Event/schemas.ts:594-617`)
/// or stored in integer columns (`validate::EVENT_INTEGER_FIELDS`).
const INTEGER_FIELDS: &[&str] = &[
    "x",
    "y",
    "duration",
    "recurrent_setpos",
    "recurrent_monthday",
    "recurrent_weekday_mask",
    "recurrent_month_mask",
    "recurrent_interval",
    "recurrent_count",
];

/// A whole float (`11.0`) is the integer `11`, as JavaScript read it, so the
/// readers (`as_i64`, the typed create body) see no float; a fraction stays
/// for the schema to refuse (`validate::EVENT_INTEGER_FIELDS`).
fn normalize_integers(body: &mut Map<String, Value>) {
    for key in INTEGER_FIELDS {
        if let Some(value) = body.get_mut(*key)
            && value.as_i64().is_none()
            && let Some(whole) = value.as_f64().filter(|f| f.fract() == 0.0)
            && (i64::MIN as f64..=i64::MAX as f64).contains(&whole)
        {
            *value = json!(whole as i64);
        }
    }
}

fn pick(source: &Map<String, Value>, keys: &[&str], into: &mut Map<String, Value>) {
    for key in keys {
        if let Some(value) = source.get(*key) {
            into.insert((*key).to_string(), value.clone());
        }
    }
}

/// `getActor` (`admin.ts:76-98`). Fixtures `events.admin_patch.actor_*`.
fn actor_of(body: &Map<String, Value>) -> Result<String, ApiError> {
    match body.get("actor") {
        None => Ok(DEFAULT_ADMIN_ACTOR.to_string()),
        Some(Value::String(actor)) => {
            let actor = actor.trim();
            if actor.is_empty() {
                return Err(ApiError::bad_request("actor cannot be empty"));
            }
            if actor.chars().count() > MAX_ADMIN_ACTOR_LENGTH {
                return Err(ApiError::bad_request(format!(
                    "actor must be {MAX_ADMIN_ACTOR_LENGTH} characters or less"
                )));
            }
            Ok(actor.to_string())
        }
        Some(_) => Err(ApiError::bad_request("actor must be a string")),
    }
}

/// `validateRejectionReason` (`Event/utils.ts:559-604`).
fn validate_rejection_reason(
    value: Option<&Value>,
    field: &str,
    allow_null: bool,
) -> Result<Option<String>, ApiError> {
    match value {
        Some(Value::Null) if allow_null => Ok(None),
        None | Some(Value::Null) => Err(ApiError::bad_request(format!("{field} is required"))),
        Some(Value::String(reason)) => {
            let reason = reason.trim();
            if reason.is_empty() {
                return Err(ApiError::bad_request(format!("{field} cannot be empty")));
            }
            if reason.chars().count() > MAX_REASON_LENGTH {
                return Err(ApiError::bad_request(format!(
                    "{field} must be {MAX_REASON_LENGTH} characters or less"
                )));
            }
            Ok(Some(reason.to_string()))
        }
        Some(_) => Err(ApiError::bad_request(format!("{field} must be a string"))),
    }
}

/// `validateDeletedReason` (`Event/utils.ts:608-633`): optional, blank is
/// null. Fixtures `events.delete.reason_*`, `.bearer_reason_too_long`.
fn validate_deleted_reason(value: Option<&Value>) -> Result<Option<String>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(reason)) => {
            let reason = reason.trim();
            if reason.is_empty() {
                return Ok(None);
            }
            if reason.chars().count() > MAX_REASON_LENGTH {
                return Err(ApiError::bad_request(format!(
                    "deleted_reason must be {MAX_REASON_LENGTH} characters or less"
                )));
            }
            Ok(Some(reason.to_string()))
        }
        Some(_) => Err(ApiError::bad_request("deleted_reason must be a string")),
    }
}

/// `getBooleanField` (`admin.ts:259-272`).
fn boolean_field(body: &Map<String, Value>, field: &str) -> Result<Option<bool>, ApiError> {
    match body.get(field) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ApiError::bad_request(format!("{field} must be a boolean"))),
    }
}

/// `getEventParamsSchema` on the `:event_id` of a write (`admin.ts:53-55,105`,
/// `getEvent.ts:19-20,35`): a non-UUID is the ajv 400. Fixtures
/// `events.patch.non_uuid`, `events.admin_patch.non_uuid`, `events.delete.non_uuid`.
fn validate_event_id(event_id: &str) -> Result<(), ApiError> {
    validate::validate(
        &validate::EVENT_PARAMS,
        &json!({ "event_id": event_id }),
        Flavour::Gatsby,
    )
}

fn not_found(id: &str) -> ApiError {
    ApiError::not_found(format!("Not found event \"{id}\""))
}

// ---------------------------------------------------------------------------
// Answers

/// `toPublic(event, profile)` for a viewer: the owner sees `contact`/`details`.
async fn answer(
    state: &SharedState,
    id: &str,
    viewer: Option<&str>,
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    let record = state
        .events
        .get_for_viewer(id, viewer)
        .await?
        .ok_or_else(|| not_found(id))?;
    Ok(Json(ApiData::ok(record.api_shape())))
}

/// `toPublic(event, adminServiceProfile(event.user))` (`admin.ts:100-102`):
/// `contact`/`details` present, `attending` false.
async fn answer_admin(
    state: &SharedState,
    id: &str,
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    let record = state
        .events
        .get_for_admin(id)
        .await?
        .ok_or_else(|| not_found(id))?;
    Ok(Json(ApiData::ok(record.api_shape())))
}

// ---------------------------------------------------------------------------
// POST /api/events

/// `POST /api/events` (`Event/routes/index.ts:76`, `createEvent.ts:46-271`):
/// a signed fetch (the admin bearer is not honoured here; fixture
/// `events.create.bearer_not_accepted`) by a wallet whose profile this node
/// holds (`withAuthProfile()`, 404 `Not found profile for "<user>"`); the
/// body null-defaulted (`:61-79`), validated against `newEventSchema` (the
/// ajv 400 echoes the defaulted body), then upstream's checks in its order
/// (`moderation::create_as`). `schedules` are always stored `[]` (`:260`); a
/// missing `categories` is `[]` here where upstream threw (its 500 is not
/// reproduced).
pub async fn create_event(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ApiData<Value>>, ApiError> {
    state.user_writes()?;
    let signer = state.signer(&headers, method.as_str(), uri.path()).await?;
    super::profiles::require_profile(&state.pool, &signer).await?;
    let mut data = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    if data.is_empty() {
        // createEvent.ts:53-59
        let echoed_headers: Map<String, Value> = headers
            .iter()
            .filter(|(name, _)| name.as_str() != "authorization")
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    json!(value.to_str().unwrap_or_default()),
                )
            })
            .collect();
        return Err(ApiError::with_data(
            400,
            "Empty event data",
            json!({ "body": data, "headers": echoed_headers, "user": signer }),
        ));
    }
    // createEvent.ts:61-79: falsy image/image_vertical/server/featured_item
    // are null; a falsy url is the generated jump-in link.
    for key in ["image", "image_vertical", "server", "featured_item"] {
        if is_falsy(data.get(key)) {
            data.insert(key.to_string(), Value::Null);
        }
    }
    normalize_integers(&mut data);
    let has_custom_url = !is_falsy(data.get("url"));
    if !has_custom_url {
        let x = data.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
        let y = data.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
        let server = data
            .get("server")
            .and_then(Value::as_str)
            .map(str::to_owned);
        data.insert(
            "url".to_string(),
            json!(super::metadata::jump_in_url(server.as_deref(), x, y)),
        );
    }
    validate::validate(
        &validate::NEW_EVENT_BODY,
        &Value::Object(data.clone()),
        Flavour::Gatsby,
    )?;
    validate::validate(
        &validate::EVENT_INTEGER_FIELDS,
        &Value::Object(data.clone()),
        Flavour::Gatsby,
    )?;
    for key in CREATE_OVERRIDDEN_FIELDS {
        data.remove(*key);
    }
    if data.get("categories").is_none_or(Value::is_null) {
        data.insert("categories".to_string(), json!([]));
    }
    let mut event: CreateEvent = serde_json::from_value(Value::Object(data))
        .map_err(|_| ApiError::bad_request(validate::GATSBY_INVALID_DATA))?;
    // createEvent.ts:260: schedules are never taken from the body.
    event.schedules = None;
    if state.storage.is_none()
        && (event.image.as_deref().is_some_and(|i| !i.is_empty())
            || event
                .image_vertical
                .as_deref()
                .is_some_and(|i| !i.is_empty()))
    {
        // validateImageUrl without a bucket (`Event/utils.ts:838-843`): this
        // node has no blob store to hold the poster.
        return Err(ApiError::service_unavailable(
            "Poster uploads are unavailable because this node has no blob store configured",
        ));
    }
    let id = moderation::create_as(
        &state.pool,
        &event,
        &signer,
        &signer,
        &state.settings,
        Some(&state.places),
    )
    .await?;
    // createEvent.ts:235-270: the answer is the object the route built
    // (`created_at: now`, no `updated_at`; the column's `now()` default shows
    // on later reads only). Fixtures `events.create.*_success`.
    let Json(created) = answer(&state, &id, Some(&signer)).await?;
    let mut data = serde_json::to_value(created.data)
        .map_err(|e| ApiError::internal(format!("event record: {e}")))?;
    if let Some(record) = data.as_object_mut() {
        record.remove("updated_at");
    }
    Ok(Json(ApiData::ok(data)))
}

// ---------------------------------------------------------------------------
// PATCH /api/events/{id}

/// Who edits (`updateEventWithOptions`, `updateEvent.ts:159-175`).
enum Editor {
    /// The admin bearer with its actor (`admin.ts:378-386`).
    Admin { actor: String },
    /// A signed wallet with its grants; `owner` when it created the event.
    Wallet {
        signer: String,
        permissions: EventPermissions,
        owner: bool,
    },
}

impl Editor {
    fn user(&self) -> &str {
        match self {
            Editor::Admin { actor } => actor,
            Editor::Wallet { signer, .. } => signer,
        }
    }
}

/// `PATCH /api/events/{id}` (`index.ts:49-55,97-102`): `requireAuthOrAdmin`
/// (a signed chain wins over the bearer), then `patchEventAdmin` for the
/// bearer or `updateEvent` for the wallet.
pub async fn patch_event(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(event_id): Path<String>,
    body: Bytes,
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    let signed = state.viewer(&headers, method.as_str(), uri.path()).await;
    let bearer = super::handlers::admin_bearer(&state, &headers, signed.is_some())?;
    match signed {
        None if bearer => {
            state.operator_writes()?;
            patch_event_as_admin(
                &state,
                &headers,
                method.as_str(),
                uri.path(),
                &event_id,
                &body,
            )
            .await
        }
        None => Err(ApiError::unauthorized("Unauthorized")),
        Some(signer) => {
            state.user_writes()?;
            patch_event_as_wallet(
                &state,
                &headers,
                method.as_str(),
                uri.path(),
                &event_id,
                signer,
                &body,
            )
            .await
        }
    }
}

/// The top-level keys of a JSON object in document order (`Object.keys`),
/// without `preserve_order`: serde hands a map's entries to the visitor as
/// they appear. Empty when the body is not an object.
fn document_keys(bytes: &[u8]) -> Vec<String> {
    struct Keys(Vec<String>);
    impl<'de> serde::Deserialize<'de> for Keys {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = Keys;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a JSON object")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut m: A,
                ) -> Result<Keys, A::Error> {
                    let mut keys: Vec<String> = Vec::new();
                    while let Some(key) = m.next_key::<String>()? {
                        m.next_value::<serde::de::IgnoredAny>()?;
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                    }
                    Ok(Keys(keys))
                }
            }
            d.deserialize_map(V)
        }
    }
    serde_json::from_slice::<Keys>(bytes)
        .map(|k| k.0)
        .unwrap_or_default()
}

/// `patchEventAdmin` (`admin.ts:367-387`): the event first (400 non-UUID,
/// 404 missing, deleted ones included), a state-only body through
/// `applyStateChange`, else the content body through `validateAdminPatchBody`
/// and the shared edit with `admin: true`.
async fn patch_event_as_admin(
    state: &SharedState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    event_id: &str,
    bytes: &[u8],
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    validate_event_id(event_id)?;
    if state.events.get(event_id).await?.is_none() {
        return Err(not_found(event_id));
    }
    let body = body_object(bytes);
    // `Object.keys(body)` (`admin.ts:310-321`): document order, as upstream
    // lists the unsupported ones.
    let keys = document_keys(bytes);
    // isStateOnlyBody (admin.ts:253-257)
    if !body.is_empty() && keys.iter().all(|k| STATE_ONLY_FIELDS.contains(&k.as_str())) {
        let approved = boolean_field(&body, "approved")?;
        let rejected = boolean_field(&body, "rejected")?;
        if approved.is_some() && rejected.is_some() {
            return Err(ApiError::bad_request(
                "approved and rejected cannot be set in the same request",
            ));
        }
        let actor = actor_of(&body)?;
        let change = match (approved, rejected) {
            (Some(true), _) => AdminState::Approve,
            (Some(false), _) => AdminState::Unapprove,
            (_, Some(true)) => AdminState::Reject(
                validate_rejection_reason(body.get("reason"), "rejection reason", false)?
                    .unwrap_or_default(),
            ),
            (_, Some(false)) => AdminState::Unreject,
            (None, None) => {
                return Err(ApiError::bad_request(
                    "approved or rejected field is required",
                ));
            }
        };
        if !moderation::moderate_state(&state.pool, event_id, &change, &actor, &state.settings)
            .await?
        {
            return Err(not_found(event_id));
        }
        return answer_admin(state, event_id).await;
    }
    // validateAdminPatchBody (admin.ts:310-333)
    let unsupported: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|k| {
            *k != "actor"
                && !EDIT_EVENT_ATTRIBUTES.contains(k)
                && !EDIT_OWN_EVENT_ATTRIBUTES.contains(k)
                && !EDIT_ANY_EVENT_ATTRIBUTES.contains(k)
                || *k == "rejected"
        })
        .collect();
    if !unsupported.is_empty() {
        return Err(ApiError::bad_request(format!(
            "Unsupported admin event fields: {}",
            unsupported.join(", ")
        )));
    }
    if keys.iter().all(|k| k == "actor") {
        return Err(ApiError::bad_request(
            "At least one admin event field is required",
        ));
    }
    let actor = actor_of(&body)?;
    update_event(
        state,
        headers,
        method,
        path,
        event_id,
        body,
        Editor::Admin { actor },
        bytes,
    )
    .await
}

/// `updateEvent` (`updateEvent.ts:159-175,597-599`): `getEvent` (400
/// non-UUID, 404 missing or hidden), the profile's grants, 403 unless the
/// owner, a moderator or an `edit_any_event` grant. Fixtures
/// `events.patch.non_owner_forbidden`, `.pending_event_other_user_404`,
/// `.approve_any_only_non_owner_forbidden`.
async fn patch_event_as_wallet(
    state: &SharedState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    event_id: &str,
    signer: String,
    bytes: &[u8],
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    let event =
        super::handlers::load_visible_event(state, headers, event_id, Some(&signer), true).await?;
    let permissions = super::profiles::event_permissions(&state.pool, &signer).await?;
    let owner = event
        .user
        .as_deref()
        .is_some_and(|u| u.eq_ignore_ascii_case(&signer));
    if !owner && !permissions.edit_any {
        return Err(ApiError::forbidden(
            "You don't have permission to edit this event",
        ));
    }
    let body = body_object(bytes);
    update_event(
        state,
        headers,
        method,
        path,
        event_id,
        body,
        Editor::Wallet {
            signer,
            permissions,
            owner,
        },
        bytes,
    )
    .await
}

/// Lock and load the document before applying any mutation. Its owner comes
/// from the document or projected creator; mirrored authority is returned so
/// the caller can reject an owner write or apply operator policy explicitly.
async fn load_raw(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> Result<Option<(Map<String, Value>, String, bool)>, ApiError> {
    let row: Option<(Value, Option<String>, String)> = sqlx::query_as(
        "SELECT raw, user_creator, origin FROM explore.event WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let mirrored = row
        .as_ref()
        .is_some_and(|(_, _, origin)| origin != moderation::LOCAL);
    Ok(row.map(|(raw, creator, _)| {
        let owner = raw
            .get("user")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(creator)
            .unwrap_or_default()
            .to_lowercase();
        let map = match raw {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        (map, owner, mirrored)
    }))
}

/// `isModeratedFieldChanged` (`updateEvent.ts:109-128`).
fn moderated_field_changed(field: &str, next: &Value, current: &Value) -> bool {
    if field == "description" {
        let text =
            |v: &Value| crate::sanitize::sanitize_event_description(v.as_str().unwrap_or_default());
        return text(next) != text(current);
    }
    if next.is_array() || current.is_array() {
        let sorted = |v: &Value| {
            let mut items: Vec<String> = v
                .as_array()
                .map(|a| a.iter().map(Value::to_string).collect())
                .unwrap_or_default();
            items.sort();
            items
        };
        return sorted(next) != sorted(current);
    }
    next != current
}

fn flag(raw: &Map<String, Value>, key: &str) -> bool {
    moderation::flag(raw, key)
}

fn text(raw: &Map<String, Value>, key: &str) -> Option<String> {
    raw.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `updateEventWithOptions` (`updateEvent.ts:159-595`) over the locked
/// document, in upstream's order: the picked attribute sets per editor, the
/// url regeneration, the ajv validation of the merged attributes, the
/// recurrence cap, the posters, the schedules, the duration, the world
/// limits, the categories, the enrichment, the recurrence, the date checks,
/// the rejection reset, the re-moderation of changed content, the actors of
/// the transitions and the community check.
#[allow(clippy::too_many_arguments)]
async fn update_event(
    state: &SharedState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    event_id: &str,
    mut body: Map<String, Value>,
    editor: Editor,
    bytes: &[u8],
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    let admin = matches!(editor, Editor::Admin { .. });
    super::mirror::reclaim_local(&state.pool).await?;
    let mut tx = state.pool.begin().await?;
    let Some((raw, _, mirrored)) = load_raw(&mut tx, event_id).await? else {
        return Err(not_found(event_id));
    };
    if mirrored {
        tx.rollback().await?;
        return Err(moderation::mirrored_event(&state.pool, event_id).await);
    }
    let before = raw;
    let user = editor.user().to_string();
    let (moderator, permissions, is_owner) = match &editor {
        Editor::Admin { .. } => (
            true,
            EventPermissions {
                edit_any: true,
                approve_any: true,
                approve_own: true,
            },
            false,
        ),
        Editor::Wallet {
            permissions, owner, ..
        } => (
            false,
            EventPermissions {
                edit_any: permissions.edit_any,
                approve_any: permissions.approve_any,
                approve_own: permissions.approve_own,
            },
            *owner,
        ),
    };
    if let Editor::Wallet { signer, .. } = &editor {
        // dcl-one: another wallet's edit is an operator write; every wallet
        // edit is logged, its signed digest and nonce honoured.
        if !is_owner {
            state.operator_writes()?;
        }
        super::audit::record_signed_request(
            &mut tx,
            headers,
            signer,
            event_id,
            method,
            path,
            bytes,
            super::audit::SignedWrite::Event,
        )
        .await?;
    }

    // updateEvent.ts:176-181
    if body.get("featured_item").and_then(Value::as_str) == Some("") {
        body.insert("featured_item".into(), Value::Null);
    }

    // updateEvent.ts:183-236: the picked attribute sets.
    let mut updated: Map<String, Value> = Map::new();
    pick(&before, EDIT_EVENT_ATTRIBUTES, &mut updated);
    let edit_without_rejected: Vec<&str> = EDIT_EVENT_ATTRIBUTES
        .iter()
        .copied()
        .filter(|k| *k != "rejected")
        .collect();
    // dcl-one: another reviewer's rejection holds the event for its owner.
    // While it stands the owner's `rejected` and `approved` stay as stored,
    // so neither the owner, their `approve_own_event` grant nor their
    // `edit_any_event` grant can clear it (updateEvent.ts:196-220 let the
    // grants re-approve over it); an `approve_any_event` grant is a reviewer
    // and may. A mirrored row never reaches here: its edit answers 409 above.
    let held = !admin
        && is_owner
        && !permissions.approve_any
        && flag(&before, "rejected")
        && !text(&before, "rejected_by").is_some_and(|by| by.eq_ignore_ascii_case(&user));
    if admin {
        pick(&body, &edit_without_rejected, &mut updated);
        pick(&before, EDIT_OWN_EVENT_ATTRIBUTES, &mut updated);
        pick(&body, EDIT_OWN_EVENT_ATTRIBUTES, &mut updated);
        pick(&before, EDIT_ANY_EVENT_ATTRIBUTES, &mut updated);
        pick(&body, EDIT_ANY_EVENT_ATTRIBUTES, &mut updated);
    } else if is_owner {
        if held {
            pick(&body, &edit_without_rejected, &mut updated);
        } else {
            pick(&body, EDIT_EVENT_ATTRIBUTES, &mut updated);
        }
        pick(&before, EDIT_OWN_EVENT_ATTRIBUTES, &mut updated);
        pick(&body, EDIT_OWN_EVENT_ATTRIBUTES, &mut updated);
        if (moderator || permissions.approve_own) && !held {
            pick(&before, APPROVE_EVENT_ATTRIBUTES, &mut updated);
            pick(&body, APPROVE_EVENT_ATTRIBUTES, &mut updated);
        }
    }
    if !admin && permissions.edit_any {
        let editable = if held {
            edit_without_rejected.as_slice()
        } else {
            EDIT_EVENT_ATTRIBUTES
        };
        pick(&body, editable, &mut updated);
        pick(&before, EDIT_ANY_EVENT_ATTRIBUTES, &mut updated);
        pick(&body, EDIT_ANY_EVENT_ATTRIBUTES, &mut updated);
    }
    if !admin && permissions.approve_any {
        pick(&before, APPROVE_EVENT_ATTRIBUTES, &mut updated);
        pick(&body, APPROVE_EVENT_ATTRIBUTES, &mut updated);
        if let Some(reason) = body.get("rejection_reason") {
            // updateEvent.ts:229-235
            updated.insert(
                "rejection_reason".into(),
                json!(validate_rejection_reason(
                    Some(reason),
                    "rejection_reason",
                    true
                )?),
            );
        }
    }
    // updateEvent.ts:238-243: an empty or generated url follows the location.
    // The stored custom url is kept as the merged attribute (upstream's pick
    // left it out of the owner's merge and regenerated it; the documented
    // rule is "regenerated when empty or under JUMP_IN_SITE_URL").
    if !updated.contains_key("url") {
        pick(&before, &["url"], &mut updated);
    }
    super::metadata::refresh_target(&mut updated, &before);
    let body_has = |key: &str| body.contains_key(key);
    // dcl-one's own provenance: an explicit poster stops the generated refresh.
    if body_has("image") && body.get("image") != before.get("image") {
        updated.insert("_image_source".into(), json!("explicit"));
    }

    // updateEvent.ts:245-254: the merged attributes validated whole. Stored
    // posters are router-local paths here (`/poster/<file>`, the default
    // asset); the schema sees them on the events mount, as the reads answer
    // them. A stored interval of 0 (an older local document) is upstream's 1.
    normalize_integers(&mut updated);
    if updated
        .get("recurrent_interval")
        .is_none_or(|v| v.is_null() || v.as_i64() == Some(0))
    {
        updated.insert("recurrent_interval".into(), json!(1));
    }
    let mut echo = updated.clone();
    echo.remove("_image_source");
    for field in ["image", "image_vertical"] {
        if let Some(path) = echo.get(field).and_then(Value::as_str)
            && path.starts_with('/')
        {
            let absolute = format!(
                "{}{path}",
                state.settings.events_asset_base_url.trim_end_matches('/')
            );
            echo.insert(field.into(), json!(absolute));
        }
    }
    validate::validate(
        &validate::NEW_EVENT_BODY,
        &Value::Object(echo.clone()),
        Flavour::Gatsby,
    )?;
    validate::validate(
        &validate::EVENT_INTEGER_FIELDS,
        &Value::Object(echo.clone()),
        Flavour::Gatsby,
    )?;
    let echo = Value::Object(echo);
    let refuse = |message: &str| ApiError::with_data(400, message, json!({ "body": echo }));

    // updateEvent.ts:256-258
    if body_has("url")
        && let Some(url) = updated.get("url").and_then(Value::as_str)
    {
        validate_event_url(url)?;
    }
    // updateEvent.ts:270-289
    let touches_recurrence = RECURRENCE_FIELDS.iter().any(|k| body_has(k));
    if touches_recurrence
        && flag(&updated, "recurrent")
        && super::recurrence::estimate_past_iterations(&updated, Utc::now())
            > super::recurrence::MAX_RECURRENT_PAST_ITERATIONS
    {
        return Err(refuse(super::recurrence::TOO_MANY_PAST_ITERATIONS_MESSAGE));
    }
    // updateEvent.ts:291-300: a changed poster must be one this node serves.
    for (field, kind) in [("image", "poster"), ("image_vertical", "poster-vertical")] {
        let next = body
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty());
        let stored = before.get(field).and_then(Value::as_str);
        if let Some(next) = next
            && stored != Some(next)
        {
            let spelled = |value: &str| {
                super::posters::normalize_image_input(value, &state.settings, kind).ok()
            };
            let same_poster = matches!(
                (stored.and_then(spelled), spelled(next)),
                (Some(stored), Some(next)) if stored == next
            );
            if same_poster {
                // The same poster spelled absolutely: not a change.
                updated.insert(field.into(), json!(stored));
                if field == "image" {
                    updated.remove("_image_source");
                }
            } else {
                let normalized = resolve_poster(&mut *tx, next, &state.settings, kind).await?;
                updated.insert(field.into(), json!(normalized));
            }
        }
    }
    // updateEvent.ts:302-323
    if let Some(Value::Array(schedules)) = updated.get("schedules") {
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<Value> = schedules
            .iter()
            .filter(|s| seen.insert(s.to_string()))
            .cloned()
            .collect();
        updated.insert("schedules".into(), Value::Array(unique));
        let mut check = Map::new();
        check.insert("schedules".into(), updated["schedules"].clone());
        moderation::validate_references(&mut *tx, &check)
            .await
            .map_err(|e| with_body(e, &echo))?;
    }
    // updateEvent.ts:328-337
    let stored_duration = before.get("duration").and_then(Value::as_i64).unwrap_or(0);
    if let Some(duration) = updated.get("duration").and_then(Value::as_i64)
        && duration > 0
        && duration > stored_duration.max(MAX_EVENT_DURATION_MS)
    {
        return Err(refuse(&max_duration_message()));
    }
    // updateEvent.ts:349-357
    let x = updated.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
    let y = updated.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
    check_world_limits(x, y, &echo)?;
    // updateEvent.ts:362-373
    if updated.get("categories").is_some() {
        let mut check = Map::new();
        check.insert("categories".into(), updated["categories"].clone());
        moderation::validate_references(&mut *tx, &check)
            .await
            .map_err(|e| with_body(e, &echo))?;
    }

    // The merged document.
    let mut raw = before.clone();
    for (key, value) in &updated {
        raw.insert(key.clone(), value.clone());
    }
    if let Some(Value::Null) = raw.get("categories") {
        raw.insert("categories".into(), json!([]));
    }
    if let Some(Value::Null) = raw.get("schedules") {
        raw.insert("schedules".into(), json!([]));
    }
    raw.insert("coordinates".into(), json!([x, y]));
    raw.insert("position".into(), json!([x, y]));
    if let Some(start_at) = raw
        .get("start_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
    {
        raw.insert("start_at".into(), json!(start_at.to_rfc3339()));
    }
    if let Some(until) = raw
        .get("recurrent_until")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
    {
        raw.insert("recurrent_until".into(), json!(until.to_rfc3339()));
    }
    // updateEvent.ts:375-390: the location's place and estate, the world
    // default poster.
    let location_touched = ["x", "y", "server", "world"].iter().any(|k| body_has(k));
    if location_touched {
        let world = flag(&raw, "world");
        let server = text(&raw, "server");
        let (place_id, world) =
            moderation::resolve_location(&mut *tx, world, server.as_deref(), x, y).await?;
        raw.insert(
            "place_id".into(),
            place_id.map(Value::String).unwrap_or(Value::Null),
        );
        raw.insert("world".into(), json!(world));
    }
    super::metadata::enrich(
        &mut tx,
        &mut raw,
        Some(&before),
        false,
        &state.settings.events_base_url,
        Some(&state.places),
    )
    .await?;
    if flag(&raw, "world") && is_falsy(raw.get("image")) {
        raw.insert(
            "image".into(),
            json!(super::metadata::default_image(
                &raw,
                &state.settings.events_base_url
            )),
        );
        raw.insert("_image_source".into(), json!("generated"));
    }
    // updateEvent.ts:392-395
    super::recurrence::recompute(&mut raw, Utc::now())?;
    // updateEvent.ts:397-433
    if DATE_FIELDS.iter().any(|k| body_has(k)) {
        let now = Utc::now();
        if raw
            .get("finish_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339)
            .is_some_and(|f| f <= now)
        {
            return Err(refuse(PAST_FINISH_AT_MESSAGE));
        }
        let until = text(&raw, "recurrent_until").and_then(|u| parse_rfc3339(&u));
        if normalized_series(
            flag(&raw, "recurrent"),
            raw.get("recurrent_frequency").and_then(Value::as_str),
            raw.get("recurrent_count").and_then(Value::as_i64),
            until,
        ) && until.is_some_and(|u| u <= now)
        {
            return Err(refuse(PAST_RECURRENT_UNTIL_MESSAGE));
        }
    }
    // A new schedule cannot mint historical occurrence facts (dcl-one).
    if [
        "start_at",
        "recurrent",
        "recurrent_frequency",
        "recurrent_interval",
        "recurrent_weekday_mask",
        "recurrent_month_mask",
        "recurrent_setpos",
        "recurrent_monthday",
        "recurrent_count",
        "recurrent_until",
    ]
    .iter()
    .any(|key| before.get(*key) != raw.get(*key))
    {
        let cutoff: chrono::NaiveDateTime =
            sqlx::query_scalar("SELECT clock_timestamp() AT TIME ZONE 'UTC'")
                .fetch_one(&mut *tx)
                .await?;
        moderation::preserve_metric_starts(&before, &mut raw, cutoff.and_utc());
    }
    // updateEvent.ts:445-450
    if flag(&raw, "rejected") {
        raw.insert("approved".into(), json!(false));
        raw.insert("highlighted".into(), json!(false));
        raw.insert("trending".into(), json!(false));
    }
    // updateEvent.ts:452-477: re-queue changed content unless the actor may approve.
    let actor_can_approve =
        admin || moderator || permissions.approve_any || (is_owner && permissions.approve_own);
    let moderated_content_changed = EDIT_EVENT_ATTRIBUTES
        .iter()
        .filter(|field| !NON_MODERATED_EDITABLE_FIELDS.contains(field))
        .any(|field| {
            body.get(*field).is_some_and(|next| {
                moderated_field_changed(field, next, before.get(*field).unwrap_or(&Value::Null))
            })
        });
    if flag(&before, "approved") && moderated_content_changed && !actor_can_approve {
        raw.insert("approved".into(), json!(false));
        raw.insert("approved_by".into(), Value::Null);
        raw.insert("highlighted".into(), json!(false));
        raw.insert("trending".into(), json!(false));
    }
    // updateEvent.ts:485-491: the actors of the transitions.
    if !flag(&before, "approved") && flag(&raw, "approved") {
        raw.insert("approved_by".into(), json!(user));
    } else if !flag(&before, "rejected") && flag(&raw, "rejected") {
        raw.insert("rejected_by".into(), json!(user));
    }
    // updateEvent.ts:493-534: the community the merged event links.
    if !admin && updated.contains_key("community_id") {
        let to_validate = match body.get("community_id") {
            Some(Value::Null) | None => text(&before, "community_id"),
            Some(value) => value.as_str().map(str::to_string),
        };
        if let Some(community) = to_validate {
            let access = if body.get("community_id") == Some(&Value::Null) {
                "permission to detach this event from"
            } else {
                "permission to access"
            };
            let merged = public_body(&raw);
            moderation::validate_community(&mut *tx, &community, &user, access, &merged).await?;
        }
    }
    raw.insert("updated_at".into(), json!(Utc::now().to_rfc3339()));
    moderation::store_locked(
        &mut tx,
        event_id,
        &before,
        raw,
        &user,
        json!({ "edit": body }),
        &state.settings,
    )
    .await?;
    tx.commit().await?;
    match editor {
        Editor::Admin { .. } => answer_admin(state, event_id).await,
        Editor::Wallet { signer, .. } => answer(state, event_id, Some(&signer)).await,
    }
}

/// A 400 of `validate_references` with this request's merged attributes.
fn with_body(error: ApiError, echo: &Value) -> ApiError {
    match error {
        ApiError::HttpData {
            status, message, ..
        } => ApiError::with_data(status, message, json!({ "body": echo })),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/events/{id}

/// `DELETE /api/events/{id}` (`index.ts:57-63,103-108`): the bearer's
/// `adminDeleteEvent` (`admin.ts:230-251,359-365`) or the wallet's
/// `deleteEvent` (`deleteEvent.ts:22-92`); a terminal soft delete, answered
/// idempotently once deleted.
pub async fn delete_event(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(event_id): Path<String>,
    body: Bytes,
) -> Result<Json<ApiData<EventRecord>>, ApiError> {
    let signed = state.viewer(&headers, method.as_str(), uri.path()).await;
    let bearer = super::handlers::admin_bearer(&state, &headers, signed.is_some())?;
    let signer = match signed {
        None if bearer => None,
        None => return Err(ApiError::unauthorized("Unauthorized")),
        Some(signer) => Some(signer),
    };
    match signer.as_deref() {
        None => state.operator_writes()?,
        Some(_) => state.user_writes()?,
    }
    validate_event_id(&event_id)?;
    let permissions = match signer.as_deref() {
        Some(signer) => super::profiles::event_permissions(&state.pool, signer).await?,
        None => EventPermissions::default(),
    };
    let fields = body_object(&body);
    super::mirror::reclaim_local(&state.pool).await?;
    let mut tx = state.pool.begin().await?;
    let Some((mut raw, owner, mirrored)) = load_raw(&mut tx, &event_id).await? else {
        return Err(not_found(&event_id));
    };
    let is_owner = signer.as_deref().is_some_and(|s| s == owner);
    if let Some(signer) = signer.as_deref() {
        // deleteEvent.ts:44-52
        if !is_owner && !permissions.edit_any {
            return Err(ApiError::forbidden(
                "You don't have permission to delete this event",
            ));
        }
        if mirrored && is_owner && !permissions.edit_any {
            tx.rollback().await?;
            return Err(moderation::mirrored_event(&state.pool, &event_id).await);
        }
        if !is_owner || mirrored {
            // dcl-one: another wallet's delete is an operator write, logged.
            state.operator_writes()?;
            super::audit::record_signed_request(
                &mut tx,
                &headers,
                signer,
                &event_id,
                method.as_str(),
                uri.path(),
                &body,
                super::audit::SignedWrite::Event,
            )
            .await?;
        }
    }
    // admin.ts:235-238, deleteEvent.ts:54-57: already deleted, no event
    // write; the signed request stays logged (its nonce consumed).
    if is_soft_deleted(&raw) {
        tx.commit().await?;
        return match signer.as_deref() {
            None => answer_admin(&state, &event_id).await,
            Some(signer) => answer(&state, &event_id, Some(signer)).await,
        };
    }
    let before = raw.clone();
    let now = Utc::now().to_rfc3339();
    let actor = match signer.as_deref() {
        None => {
            // admin.ts:359-365: the actor and the optional reason.
            let actor = actor_of(&fields)?;
            let reason = validate_deleted_reason(fields.get("reason"))?;
            raw.insert("deleted_by_admin".into(), json!(true));
            raw.insert("deleted_by".into(), json!(actor));
            raw.insert("deleted_reason".into(), json!(reason));
            actor
        }
        Some(signer) if is_owner && !mirrored => {
            // deleteEvent.ts:62-67: the creator's own delete, no reason.
            raw.insert("deleted_by_user".into(), json!(true));
            raw.insert("deleted_by".into(), json!(signer));
            signer.to_string()
        }
        Some(signer) => {
            // deleteEvent.ts:68-78: an editor's delete with its reason.
            let reason = validate_deleted_reason(fields.get("reason"))?;
            raw.insert("deleted_by_admin".into(), json!(true));
            raw.insert("deleted_by".into(), json!(signer));
            raw.insert("deleted_reason".into(), json!(reason));
            signer.to_string()
        }
    };
    raw.insert("deleted_at".into(), json!(now));
    raw.insert("updated_at".into(), json!(now));
    if mirrored {
        moderation::hide_mirrored_locked(&mut tx, &event_id, &before, raw, &actor, &state.settings)
            .await?;
    } else {
        moderation::store_locked(
            &mut tx,
            &event_id,
            &before,
            raw,
            &actor,
            json!({ "delete": { "reason": fields.get("reason"), "actor": fields.get("actor") } }),
            &state.settings,
        )
        .await?;
    }
    tx.commit().await?;
    match signer.as_deref() {
        None => answer_admin(&state, &event_id).await,
        Some(signer) => answer(&state, &event_id, Some(signer)).await,
    }
}

// ---------------------------------------------------------------------------
// Attendees

/// `EventAttendee/routes.ts:61-85`: a signed fetch, `getEvent` (400
/// non-UUID, 404 missing or hidden; the bearer is not mounted here), the RSVP
/// row and the list. The POST needs the signer's profile
/// (`withAuthProfile()`, 404 `Not found profile for "<user>"`), whose name
/// is the row's `user_name`.
async fn rsvp(
    state: SharedState,
    method: Method,
    path: &str,
    headers: HeaderMap,
    event_id: String,
    going: bool,
) -> Result<Json<ApiData<Vec<EventAttendeeRecord>>>, ApiError> {
    state.user_writes()?;
    let signer = state.signer(&headers, method.as_str(), path).await?;
    let user_name = if going {
        let (_, metadata) = super::profiles::local_profile(&state.pool, &signer)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("Not found profile for \"{signer}\"")))?;
        metadata
            .get("avatars")
            .and_then(|a| a.get(0))
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    } else {
        None
    };
    super::handlers::load_visible_event(&state, &headers, &event_id, Some(&signer), false).await?;
    let list = if going {
        super::attendees::rsvp_going(&state.pool, &event_id, &signer, user_name.as_deref()).await?
    } else {
        super::attendees::rsvp_cancel(&state.pool, &event_id, &signer).await?
    };
    Ok(Json(ApiData::ok(list)))
}

/// `POST /api/events/{id}/attendees`: the signer is going.
pub async fn create_event_attendee(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(event_id): Path<String>,
) -> Result<Json<ApiData<Vec<EventAttendeeRecord>>>, ApiError> {
    rsvp(state, method, uri.path(), headers, event_id, true).await
}

/// `DELETE /api/events/{id}/attendees`: the signer is not.
pub async fn delete_event_attendee(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(event_id): Path<String>,
) -> Result<Json<ApiData<Vec<EventAttendeeRecord>>>, ApiError> {
    rsvp(state, method, uri.path(), headers, event_id, false).await
}

#[cfg(test)]
mod tests {
    use super::moderation::AdminPatch;
    use super::*;

    fn raw() -> Map<String, Value> {
        json!({ "name": "e", "x": 1, "y": 2, "duration": 3_600_000, "start_at": "2099-01-01T00:00:00+00:00", "approved": true }).as_object().unwrap().clone()
    }

    /// `admin.ts:76-98,259-272`, `Event/utils.ts:559-633`: the bearer body
    /// helpers and their messages. Fixtures `events.admin_patch.actor_*`,
    /// `.approved_not_boolean`, `.reject_*_reason*`, `events.delete.reason_*`.
    #[test]
    fn admin_body_helpers_answer_upstream_messages() {
        let body = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(actor_of(&body(json!({}))).unwrap(), DEFAULT_ADMIN_ACTOR);
        assert_eq!(
            actor_of(&body(json!({"actor":" ops-bot "}))).unwrap(),
            "ops-bot"
        );
        let message = |r: Result<String, ApiError>| r.unwrap_err().to_string();
        assert_eq!(
            message(actor_of(&body(json!({"actor":"  "})))),
            "actor cannot be empty"
        );
        assert_eq!(
            message(actor_of(&body(json!({"actor":"x".repeat(43)})))),
            "actor must be 42 characters or less"
        );
        assert_eq!(
            message(actor_of(&body(json!({"actor":1})))),
            "actor must be a string"
        );
        assert_eq!(
            message(actor_of(&body(json!({"actor":null})))),
            "actor must be a string"
        );
        let reason = |v: Option<Value>, allow_null: bool| {
            validate_rejection_reason(v.as_ref(), "rejection reason", allow_null)
                .map_err(|e| e.to_string())
        };
        assert_eq!(
            reason(None, false),
            Err("rejection reason is required".into())
        );
        assert_eq!(reason(Some(Value::Null), true), Ok(None));
        assert_eq!(
            reason(Some(json!("   ")), false),
            Err("rejection reason cannot be empty".into())
        );
        assert_eq!(
            reason(Some(json!(7)), false),
            Err("rejection reason must be a string".into())
        );
        assert_eq!(
            reason(Some(json!("x".repeat(501))), false),
            Err("rejection reason must be 500 characters or less".into())
        );
        assert_eq!(
            reason(Some(json!(" Spam ")), false),
            Ok(Some("Spam".into()))
        );
        let deleted =
            |v: Option<Value>| validate_deleted_reason(v.as_ref()).map_err(|e| e.to_string());
        assert_eq!(deleted(None), Ok(None));
        assert_eq!(deleted(Some(json!("   "))), Ok(None));
        assert_eq!(
            deleted(Some(json!(3))),
            Err("deleted_reason must be a string".into())
        );
        assert_eq!(
            deleted(Some(json!("x".repeat(501)))),
            Err("deleted_reason must be 500 characters or less".into())
        );
        assert_eq!(
            boolean_field(&body(json!({"approved":"yes"})), "approved")
                .unwrap_err()
                .to_string(),
            "approved must be a boolean"
        );
        assert_eq!(boolean_field(&body(json!({})), "approved").unwrap(), None);
        // the keys a body yields are sorted, whatever order it was sent in
        assert_eq!(
            body_object(br#"{"rejected":true,"reason":"x","description":"y"}"#)
                .keys()
                .collect::<Vec<_>>(),
            ["description", "reason", "rejected"]
        );
        assert!(body_object(b"[1]").is_empty() && body_object(b"\"nope\"").is_empty());
    }

    /// `isModeratedFieldChanged` (`updateEvent.ts:109-128`): sanitized
    /// descriptions and sorted arrays compare by value. Fixture
    /// `events.patch.owner_same_categories_no_requeue`.
    #[test]
    fn moderated_field_changes_compare_by_value() {
        assert!(!moderated_field_changed(
            "categories",
            &json!(["music", "art"]),
            &json!(["art", "music"])
        ));
        assert!(moderated_field_changed(
            "categories",
            &json!(["art"]),
            &json!(["art", "music"])
        ));
        assert!(!moderated_field_changed(
            "description",
            &json!("Join <b>us</b>"),
            &json!("Join us")
        ));
        assert!(moderated_field_changed(
            "name",
            &json!("Changed"),
            &json!("Event")
        ));
        assert!(!moderated_field_changed(
            "featured_item",
            &Value::Null,
            &Value::Null
        ));
        let stored = raw();
        assert!(moderated_field_changed(
            "x",
            &json!(10),
            stored.get("x").unwrap()
        ));
    }

    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::shared::signed_fetch::test_support::{address, signed_headers};
    use crate::{ExploreState, Settings, router};

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        match auth {
            Some("signed") => {
                let signed = path.split('?').next().unwrap();
                let metadata = if signed.starts_with("/api/schedules")
                    || (signed.starts_with("/api/events/") && matches!(method, "PATCH" | "DELETE"))
                {
                    use sha2::{Digest, Sha256};
                    json!({"name":"Alice","body_sha256":hex::encode(Sha256::digest(body.as_ref().map(Value::to_string).unwrap_or_default().as_bytes())),"nonce":uuid::Uuid::new_v4().to_string()}).to_string()
                } else {
                    r#"{"name":"Alice"}"#.to_string()
                };
                for (k, v) in signed_headers(&method.to_lowercase(), signed, &metadata, true)
                    .await
                    .iter()
                {
                    req = req.header(k, v);
                }
            }
            Some(token) => req = req.header("authorization", format!("Bearer {token}")),
            None => {}
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
        )
    }

    fn new_event() -> Value {
        json!({ "name": "Launch party", "start_at": "2099-06-01T18:00:00Z", "duration": 3_600_000, "x": 10, "y": -20, "description": "<b>come</b>" })
    }

    /// Uploads through `POST /api/poster[-vertical]` as the test wallet, which
    /// needs a profile this node holds (`withAuthProfile()`); the old envelope's
    /// `data.url` is absolute on the events mount, and the event routes store
    /// its router-local path, which is what this answers.
    async fn upload_native_poster(
        pool: &sqlx::PgPool,
        app: &axum::Router,
        vertical: bool,
    ) -> (String, Vec<u8>) {
        if super::super::profiles::local_profile(pool, &address())
            .await
            .unwrap()
            .is_none()
        {
            super::super::profiles::test_support::deploy_profile(pool, &address(), json!({})).await;
        }
        let path = if vertical {
            "/api/poster-vertical"
        } else {
            "/api/poster"
        };
        let png = tiny_skia::Pixmap::new(2, 2).unwrap().encode_png().unwrap();
        let mut multipart=b"--native\r\nContent-Disposition: form-data; name=\"poster\"; filename=\"poster.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
        multipart.extend_from_slice(&png);
        multipart.extend_from_slice(b"\r\n--native--\r\n");
        let mut request =
            Request::post(path).header("content-type", "multipart/form-data; boundary=native");
        for (key, value) in signed_headers("post", path, "{}", true).await.iter() {
            request = request.header(key, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(multipart)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let url = body["data"]["url"].as_str().unwrap();
        let segment = format!("{}/", path.replace("/api", ""));
        let local = url
            .find(&segment)
            .filter(|_| url.starts_with("http"))
            .map(|at| &url[at..])
            .unwrap_or_else(|| panic!("poster url is absolute on the asset base: {url}"));
        (local.to_owned(), png)
    }

    /// `withAuthProfile()`: the test wallet needs a profile this node holds to
    /// create events and RSVP.
    async fn ensure_profile(pool: &sqlx::PgPool) {
        if super::super::profiles::local_profile(pool, &address())
            .await
            .unwrap()
            .is_none()
        {
            super::super::profiles::test_support::deploy_profile(pool, &address(), json!({})).await;
        }
    }

    /// `admin.ts:253-387`: the bearer's state body on a mirrored and on a local
    /// event, its no-ops and refusals. Fixtures `events.admin_patch.*`.
    #[tokio::test]
    async fn admin_bearer_state_changes_follow_upstream_on_mirrored_and_local_events() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let mirrored = uuid::Uuid::new_v4().to_string();
        super::super::mirror::upsert_event(&pool, &json!({"id":mirrored,"name":"Mirrored","user":"0x0000000000000000000000000000000000000abc","approved":true,"highlighted":true,"trending":true,"start_at":"2099-06-01T18:00:00Z","duration":3600000})).await.unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_admin_token: Some("admin-test".into()),
                ..Settings::for_tests()
            },
            None,
        )));
        let path = format!("/api/events/{mirrored}");
        // The old vocabulary and mixed bodies are refused (admin.ts:310-333).
        for (body, message) in [
            (
                json!({"action":"reject","reason":"spam"}),
                "Unsupported admin event fields: action, reason",
            ),
            (
                json!({"rejected":true,"rejection_reason":"spam"}),
                "Unsupported admin event fields: rejected, rejection_reason",
            ),
            (
                json!({"name":"content","approved":true}),
                "Unsupported admin event fields: approved",
            ),
            // the names are listed sorted, whatever order the body had
            (
                json!({"rejected":true,"reason":"x","description":"y"}),
                "Unsupported admin event fields: reason, rejected",
            ),
            (json!({}), "At least one admin event field is required"),
            (
                json!({"actor":"ops-bot"}),
                "approved or rejected field is required",
            ),
            (
                json!({"approved":true,"rejected":true}),
                "approved and rejected cannot be set in the same request",
            ),
            (json!({"approved":"yes"}), "approved must be a boolean"),
            (json!({"rejected":true}), "rejection reason is required"),
            (
                json!({"rejected":true,"reason":"  "}),
                "rejection reason cannot be empty",
            ),
            (
                json!({"approved":true,"actor":"  "}),
                "actor cannot be empty",
            ),
        ] {
            let (status, answer) = call(&app, "PATCH", &path, Some("admin-test"), Some(body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
            assert_eq!(answer["error"], message);
        }
        // A rejection on the mirrored event is a local decision; its own
        // fields stay the upstream's (409).
        let (status, rejected) = call(
            &app,
            "PATCH",
            &path,
            Some("admin-test"),
            Some(json!({"rejected":true,"reason":"spam","actor":"ops-bot"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{rejected}");
        assert_eq!(rejected["data"]["rejected"], true);
        assert_eq!(rejected["data"]["rejected_by"], "ops-bot");
        assert_eq!(rejected["data"]["rejection_reason"], "spam");
        assert_eq!(
            rejected["data"]["highlighted"], false,
            "a rejection clears the flags (admin.ts:163-164)"
        );
        assert_eq!(rejected["data"]["trending"], false);
        assert_eq!(
            rejected["data"]["contact"],
            Value::Null,
            "adminServiceProfile: the key is present"
        );
        let (status, _) = call(
            &app,
            "PATCH",
            &path,
            Some("admin-test"),
            Some(json!({"name":"content"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // A local event: approve, re-approve (no-op), reject with the default
        // actor, the same reason again (no-op), a new reason, unreject.
        let (_, created) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(new_event()),
        )
        .await;
        let local = format!("/api/events/{}", created["data"]["id"].as_str().unwrap());
        let (status, approved) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"approved":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{approved}");
        assert_eq!(approved["data"]["approved_by"], DEFAULT_ADMIN_ACTOR);
        let (_, again) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"approved":true,"actor":"second"})),
        )
        .await;
        assert_eq!(
            again["data"]["approved_by"], DEFAULT_ADMIN_ACTOR,
            "re-approval is a no-op (admin.ts:135-137)"
        );
        let (_, rejected) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"rejected":true,"reason":"A"})),
        )
        .await;
        assert_eq!(
            (
                rejected["data"]["rejected"].clone(),
                rejected["data"]["approved"].clone(),
                rejected["data"]["rejected_by"].clone()
            ),
            (json!(true), json!(false), json!(DEFAULT_ADMIN_ACTOR))
        );
        let notices = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM social.event_notification_intents WHERE event_id=$1 AND kind='event_rejected'")
                .bind(created["data"]["id"].as_str().unwrap()).fetch_one(&pool).await.unwrap()
        };
        let before = notices().await;
        let (_, same) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"rejected":true,"reason":"A","actor":"other"})),
        )
        .await;
        assert_eq!(
            same["data"]["rejected_by"], DEFAULT_ADMIN_ACTOR,
            "the same reason is a no-op (admin.ts:153-155)"
        );
        let (_, other) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"rejected":true,"reason":"B","actor":"other"})),
        )
        .await;
        assert_eq!(other["data"]["rejection_reason"], "B");
        assert_eq!(other["data"]["rejected_by"], "other");
        assert_eq!(
            notices().await,
            before,
            "a second rejection sends no notice (admin.ts:209-212)"
        );
        let (_, unrejected) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"rejected":false})),
        )
        .await;
        assert_eq!(
            (
                unrejected["data"]["rejected"].clone(),
                unrejected["data"]["rejected_by"].clone(),
                unrejected["data"]["rejection_reason"].clone(),
                unrejected["data"]["approved"].clone()
            ),
            (json!(false), Value::Null, Value::Null, json!(false))
        );
        let (_, content) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"name":"Edited by admin","actor":"editor"})),
        )
        .await;
        assert_eq!(content["data"]["name"], "Edited by admin");
        assert_eq!(content["data"]["attending"], false);
        assert!(
            content["data"].get("details").is_some(),
            "the bearer's answers carry contact/details"
        );
        // A soft-deleted event is still patched (admin.ts:104-118).
        call(
            &app,
            "DELETE",
            &local,
            Some("admin-test"),
            Some(json!({"reason":"gone"})),
        )
        .await;
        let (status, revived) = call(
            &app,
            "PATCH",
            &local,
            Some("admin-test"),
            Some(json!({"approved":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{revived}");
        assert_eq!(revived["data"]["deleted_by_admin"], true);
        assert_eq!(revived["data"]["approved"], true);
    }

    /// `createEvent.ts:77-85`: any signer may set a public `url`; `schedules`
    /// never come from the body (`:260`). Fixtures `events.create.custom_url_kept`,
    /// `.url_unsafe`, `.schedules_dropped`, `.no_catalyst_profile`.
    #[tokio::test]
    async fn any_signer_sets_a_public_url_and_schedules_are_dropped_on_create() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let address = format!("{:#x}", owner.address());
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        )));
        let mut body = new_event();
        body["url"] = json!("https://example.org/custom-join");
        body["schedules"] = json!(["11111111-1111-4111-8111-111111111111"]);
        let (status, missing) = call_as(&app, "POST", "/api/events", &owner, body.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
        assert_eq!(
            missing["error"],
            format!("Not found profile for \"{address}\"")
        );
        super::super::profiles::test_support::deploy_profile(&pool, &address, json!({})).await;
        let (status, created) = call_as(&app, "POST", "/api/events", &owner, body.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["data"]["url"], body["url"]);
        assert_eq!(created["data"]["schedules"], json!([]));
        assert_eq!(created["data"]["user"], address);
        assert!(created["data"].get("previous_place_id").is_none());
        body["url"] = json!("http://127.0.0.1/admin");
        let (status, unsafe_url) = call_as(&app, "POST", "/api/events", &owner, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            unsafe_url["error"],
            "Invalid event url http://127.0.0.1/admin; only public HTTP(S) URLs are allowed"
        );
        body["url"] = json!("javascript:alert(1)");
        let (status, schema) = call_as(&app, "POST", "/api/events", &owner, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            schema["data"]["errors"][0]["keyword"], "pattern",
            "{schema}"
        );
        let (status, empty) = call_as(&app, "POST", "/api/events", &owner, json!({})).await;
        assert_eq!(
            (status, empty["error"].clone()),
            (StatusCode::BAD_REQUEST, json!("Empty event data"))
        );
        assert!(empty["data"]["headers"].get("authorization").is_none());
        let mut world = new_event();
        world["world"] = json!(true);
        world["server"] = json!("w.dcl.eth");
        world["x"] = json!(165);
        let (status, outside) = call_as(&app, "POST", "/api/events", &owner, world).await;
        assert_eq!(
            (status, outside["error"].clone()),
            (
                StatusCode::BAD_REQUEST,
                json!("Event is outside the world limits")
            )
        );
        assert!(outside["data"]["body"].is_object());
    }

    /// The old schema typed `x`, `y` and `duration` as `number`
    /// (`Event/schemas.ts:580-583,622-631`) over INTEGER and BIGINT columns, so
    /// a fraction failed its insert. Both routes refuse it as the `integer`
    /// type error, and a whole float is read as its integer: the generated
    /// link, the world-limit check and the duration cap see the value, not 0.
    /// Fixtures `events.create.x_decimal`, `events.patch.duration_decimal`.
    #[tokio::test]
    async fn fractional_coordinates_and_durations_are_refused_and_whole_floats_are_integers() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let address = format!("{:#x}", owner.address());
        super::super::profiles::test_support::deploy_profile(&pool, &address, json!({})).await;
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        )));
        let mut body = new_event();
        body["x"] = json!(10.5);
        let (status, fraction) = call_as(&app, "POST", "/api/events", &owner, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{fraction}");
        assert_eq!(fraction["error"], "Invalid data was sent to the server");
        assert_eq!(fraction["data"]["errors"][0]["instancePath"], "/x");
        assert_eq!(
            fraction["data"]["errors"][0]["params"],
            json!({"type": "integer"})
        );
        assert_eq!(
            fraction["data"]["messages"][0],
            "x must be integer {\"type\":\"integer\"}"
        );
        body["x"] = json!(11.0);
        body["duration"] = json!(7200000.0);
        let (status, created) = call_as(&app, "POST", "/api/events", &owner, body).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["data"]["x"], 11);
        assert_eq!(created["data"]["coordinates"], json!([11, -20]));
        assert_eq!(created["data"]["duration"], 7200000);
        assert_eq!(
            created["data"]["url"],
            "https://decentraland.org/jump/events?position=11%2C-20"
        );
        let path = format!("/api/events/{}", created["data"]["id"].as_str().unwrap());
        let (status, fraction) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"duration": 90000000.5}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{fraction}");
        assert_eq!(fraction["data"]["errors"][0]["instancePath"], "/duration");
        assert_eq!(fraction["data"]["errors"][0]["keyword"], "type");
        let (status, fraction) =
            call_as(&app, "PATCH", &path, &owner, json!({"x": 165.5, "y": 0.25})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{fraction}");
        assert_eq!(fraction["data"]["errors"][0]["instancePath"], "/x");
        let (status, edited) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"x": 12.0, "y": -21.0, "duration": 3600000.0}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{edited}");
        assert_eq!(edited["data"]["coordinates"], json!([12, -21]));
        assert_eq!(edited["data"]["duration"], 3600000);
        let (status, capped) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"duration": 90000000.0}),
        )
        .await;
        assert_eq!(
            (status, capped["error"].clone()),
            (
                StatusCode::BAD_REQUEST,
                json!("Maximum allowed duration 24Hrs")
            )
        );
    }

    /// `updateEvent.ts:169-174`: an `approve_any_event` grant alone cannot
    /// edit another creator's event; with `edit_any_event` it approves and the
    /// stored rejection fields stay as upstream left them. Fixtures
    /// `events.patch.approve_any_only_non_owner_forbidden`,
    /// `.edit_any_and_approve_any_approves`.
    #[tokio::test]
    async fn approval_only_grants_cannot_edit_another_creators_event() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let reviewer = alloy::signers::local::PrivateKeySigner::random();
        let owner_address = format!("{:#x}", owner.address());
        let reviewer_address = format!("{:#x}", reviewer.address());
        let id = moderation::create_as(
            &pool,
            &serde_json::from_value(new_event()).unwrap(),
            &owner_address,
            "test",
            &Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\",permissions_granted,permissions) VALUES ($1,true,'[\"approve_any_event\"]')")
            .bind(&reviewer_address).execute(&pool).await.unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        )));
        let path = format!("/api/events/{id}");
        for body in [
            json!({"name":"Forged title","approved":true}),
            json!({"approved":true}),
        ] {
            let (status, refused) = call_as(&app, "PATCH", &path, &reviewer, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
            assert_eq!(
                refused["error"],
                "You don't have permission to edit this event"
            );
        }
        sqlx::query("UPDATE explore.event_profile_settings SET permissions='[\"approve_any_event\",\"edit_any_event\"]' WHERE \"user\"=$1").bind(&reviewer_address).execute(&pool).await.unwrap();
        let (status, approved) = call_as(
            &app,
            "PATCH",
            &path,
            &reviewer,
            json!({"approved":true,"foo":"ignored"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{approved}");
        assert_eq!(approved["data"]["approved"], true);
        assert_eq!(approved["data"]["approved_by"], reviewer_address);
        assert!(approved["data"].get("contact").is_none(), "not the owner");
        let (_, rejected) = call_as(
            &app,
            "PATCH",
            &path,
            &reviewer,
            json!({"rejected":true,"rejection_reason":" Hold "}),
        )
        .await;
        assert_eq!(
            (
                rejected["data"]["rejected"].clone(),
                rejected["data"]["approved"].clone(),
                rejected["data"]["rejection_reason"].clone()
            ),
            (json!(true), json!(false), json!("Hold"))
        );
        assert_eq!(rejected["data"]["rejected_by"], reviewer_address);
        let (_, invalid) = call_as(
            &app,
            "PATCH",
            &path,
            &reviewer,
            json!({"rejection_reason":5}),
        )
        .await;
        assert_eq!(invalid["error"], "rejection_reason must be a string");
        let (_, approved_again) =
            call_as(&app, "PATCH", &path, &reviewer, json!({"approved":true})).await;
        assert_eq!(
            approved_again["data"]["approved"], false,
            "a rejected event stays rejected until rejected:false is sent (updateEvent.ts:445-450)"
        );
        let (_, cleared) = call_as(
            &app,
            "PATCH",
            &path,
            &reviewer,
            json!({"approved":true,"rejected":false,"rejection_reason":null}),
        )
        .await;
        assert_eq!(
            (
                cleared["data"]["approved"].clone(),
                cleared["data"]["rejected"].clone(),
                cleared["data"]["rejection_reason"].clone()
            ),
            (json!(true), json!(false), Value::Null)
        );
    }

    /// `updateEvent.ts:362-373,493-534`: the merged categories are validated
    /// on every edit and a community link must still be managed by the owner.
    /// Fixtures `events.patch.community_not_member`, `.community_detach_not_member`.
    #[tokio::test]
    async fn stale_categories_and_unmanaged_communities_refuse_owner_edits() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let wallet = alloy::signers::local::PrivateKeySigner::random();
        let owner = format!("{:#x}", wallet.address());
        let id = moderation::create_as(
            &pool,
            &serde_json::from_value(new_event()).unwrap(),
            &owner,
            "test",
            &Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        let foreign = uuid::Uuid::new_v4().to_string();
        sqlx::query("UPDATE explore.event SET raw=raw || $2 WHERE id=$1")
            .bind(&id)
            .bind(json!({"categories":["retired-category"],"community_id":foreign}))
            .execute(&pool)
            .await
            .unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        )));
        let path = format!("/api/events/{id}");
        let (status, stale) =
            call_as(&app, "PATCH", &path, &wallet, json!({"name":"Typo fixed"})).await;
        assert_eq!(
            (status, stale["error"].clone()),
            (
                StatusCode::BAD_REQUEST,
                json!("Invalid category tag supplied")
            ),
            "{stale}"
        );
        assert!(stale["data"]["body"].is_object());
        let (status, unmanaged) = call_as(
            &app,
            "PATCH",
            &path,
            &wallet,
            json!({"name":"Typo fixed","categories":["art"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{unmanaged}");
        assert_eq!(
            unmanaged["error"],
            format!(
                "Failed to validate community ownership: Community \"{foreign}\" not found or you don't have permission to access it"
            )
        );
        let (status, detach) = call_as(
            &app,
            "PATCH",
            &path,
            &wallet,
            json!({"community_id":null,"categories":["art"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{detach}");
        assert_eq!(
            detach["error"],
            format!(
                "Failed to validate community ownership: Community \"{foreign}\" not found or you don't have permission to detach this event from it"
            )
        );
        sqlx::query(
            "INSERT INTO social.communities (id,name,owner_address) VALUES ($1::uuid,'Managed',$2)",
        )
        .bind(&foreign)
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
        let (status, detached) = call_as(
            &app,
            "PATCH",
            &path,
            &wallet,
            json!({"community_id":null,"categories":["art"],"name":"Typo fixed"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{detached}");
        assert!(detached["data"]["community_id"].is_null());
        assert_eq!(detached["data"]["name"], "Typo fixed");
        let (status, unknown) = call_as(
            &app,
            "PATCH",
            &path,
            &wallet,
            json!({"categories":["invalid-new-category"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{unknown}");
    }

    #[tokio::test]
    async fn generated_defaults_use_the_node_asset_mount_without_persisting_its_origin() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let settings = Settings {
            events_asset_base_url: Settings::events_asset_base_for("http://localhost:8080"),
            user_writes_enabled: true,
            operator_writes_enabled: true,
            events_admin_token: Some("admin-test".into()),
            ..Settings::for_tests()
        };
        assert_eq!(settings.events_base_url, "https://events.decentraland.org");
        let app = axum::Router::new().nest(
            "/events",
            router(Arc::new(ExploreState::new(
                pool.clone(),
                settings.clone(),
                None,
            ))),
        );
        let (status, created) = call(
            &app,
            "POST",
            "/events/api/events",
            Some("signed"),
            Some(new_event()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(
            created["data"]["image"], "https://api.decentraland.org/v1/parcels/10/-20/map.png",
            "createEvent.ts:215-219: the parcel map is the default poster"
        );
        let expected = "http://localhost:8080/events/assets/event-default.webp";
        assert_eq!(
            expected,
            super::super::posters::default_image_url(&settings)
        );
        let image_path = url::Url::parse(expected).unwrap();
        let asset = app
            .clone()
            .oneshot(
                Request::get(image_path.path())
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(asset.headers()["content-type"], "image/webp");
        assert!(
            !axum::body::to_bytes(asset.into_body(), 1 << 20)
                .await
                .unwrap()
                .is_empty()
        );
        let id = created["data"]["id"].as_str().unwrap();
        let path = format!("/events/api/events/{id}");
        let (status, projected) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"image":expected,"image_vertical":expected})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{projected}");
        assert_eq!(projected["data"]["image_vertical"], expected);
        let (status, approved) = call(
            &app,
            "PATCH",
            &path,
            Some("admin-test"),
            Some(json!({"approved":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{approved}");
        let raw: Value = sqlx::query_scalar("SELECT raw FROM explore.event WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(raw["image"], super::super::posters::DEFAULT_EVENT_IMAGE);
        assert_eq!(
            raw["_image_source"], "explicit",
            "the organizer chose the node's default asset explicitly"
        );
        let image: String = sqlx::query_scalar("SELECT event->>'image' FROM social.event_notification_intents WHERE event_id=$1 AND kind='event_approved'").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(image, super::super::posters::DEFAULT_EVENT_IMAGE);
        let changed = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                events_asset_base_url: Settings::events_asset_base_for("http://localhost:9090"),
                ..settings
            },
            None,
        )));
        let (status, response) = call(
            &changed,
            "GET",
            &format!("/api/events/{id}"),
            Some("signed"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(
            response["data"]["image"],
            "http://localhost:9090/events/assets/event-default.webp"
        );
        assert_eq!(
            response["data"]["image_vertical"],
            "http://localhost:9090/events/assets/event-default.webp"
        );
        let unchanged: Value = sqlx::query_scalar("SELECT raw FROM explore.event WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            unchanged, raw,
            "a read/base change must not rewrite authoring data"
        );
    }

    #[tokio::test]
    async fn unchanged_legacy_posters_keep_their_provenance_and_equivalent_absolute_spelling() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                events_asset_base_url: "https://native.example.org/events".into(),
                ..Settings::for_tests()
            },
            None,
        )));
        let (_, created) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(new_event()),
        )
        .await;
        let id = created["data"]["id"].as_str().unwrap();
        let path = format!("/api/events/{id}");
        sqlx::query(r#"UPDATE explore.event SET raw=raw || '{"image":"/poster/legacy.webp","_image_source":"explicit"}'::jsonb WHERE id=$1"#).bind(id).execute(&pool).await.unwrap();
        let (status, _) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"image":"https://native.example.org/events/poster/legacy.webp"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        sqlx::query(r#"UPDATE explore.event SET raw=raw || '{"image":"https://third.example.org/scene.png","_image_source":"generated"}'::jsonb WHERE id=$1"#).bind(id).execute(&pool).await.unwrap();
        let (status, _) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"image":"https://third.example.org/scene.png"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let source: String =
            sqlx::query_scalar("SELECT raw->>'_image_source' FROM explore.event WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(source, "generated");
        let (_, moved) = call(&app, "PATCH", &path, Some("signed"), Some(json!({"x":12}))).await;
        assert_eq!(
            moved["data"]["image"], "https://api.decentraland.org/v1/parcels/12/-20/map.png",
            "a generated poster follows the move (createEvent.ts:215-219)"
        );
    }

    #[tokio::test]
    async fn native_uploaded_posters_and_broad_admin_fields_preserve_privilege_boundaries() {
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let dir =
            std::env::temp_dir().join(format!("native-event-authoring-{}", uuid::Uuid::new_v4()));
        let storage = Arc::new(crate::storage::Storage::from_config(&dir).await.unwrap());
        let settings = Settings {
            user_writes_enabled: true,
            operator_writes_enabled: true,
            events_admin_token: Some("native-admin".into()),
            events_asset_base_url: "https://native.example.org".into(),
            content_public_url: "https://content.example.org".into(),
            ..Settings::for_tests()
        };
        let app = router(Arc::new(
            ExploreState::new(pool.clone(), settings.clone(), None).with_storage(storage.clone()),
        ));
        let (poster, bytes) = upload_native_poster(&pool, &app, false).await;
        let (vertical, _) = upload_native_poster(&pool, &app, true).await;
        let cid: String = sqlx::query_scalar(
            "SELECT blob_cid FROM explore.poster WHERE kind='poster' AND file=$1",
        )
        .bind(poster.strip_prefix("/poster/").unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            storage.retrieve(&cid).await.unwrap().unwrap().as_ref(),
            bytes.as_slice()
        );
        let mut event = new_event();
        event["image"] = json!(format!("https://native.example.org{poster}"));
        event["image_vertical"] = json!(format!("https://content.example.org{vertical}"));
        let mut invalid = event.clone();
        invalid["image_vertical"] = json!(poster);
        assert_eq!(
            call(&app, "POST", "/api/events", Some("signed"), Some(invalid))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let (status, created) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(event.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["data"]["image"], poster);
        assert_eq!(created["data"]["image_vertical"], vertical);
        let path = format!("/api/events/{}", created["data"]["id"].as_str().unwrap());
        let schedule = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO explore.schedules_local (id,name) VALUES ($1,'Native calendar')")
            .bind(&schedule)
            .execute(&pool)
            .await
            .unwrap();
        let mut campaign_event = new_event();
        campaign_event["schedules"] = json!([schedule]);
        let (status, ordinary) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(campaign_event.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{ordinary}");
        assert_eq!(
            ordinary["data"]["schedules"],
            json!([]),
            "createEvent.ts:260: a created event has no schedules"
        );
        let (status, privileged) = call(
            &app,
            "POST",
            "/api/events",
            Some("native-admin"),
            Some(campaign_event),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{privileged}: the bearer is not honoured on POST (index.ts:76)"
        );
        let (status,edited)=call(&app,"PATCH",&path,Some("native-admin"),Some(json!({
            "actor":"automation","name":"Administrative edit","description":"Reviewed description","start_at":"2099-07-01T18:00:00Z","duration":7200000,
            "image":format!("https://native.example.org{poster}"),"image_vertical":format!("https://content.example.org{vertical}"),"categories":["art"],"schedules":[schedule],"url":"https://example.org/join",
            "contact":"organizer@example.org","details":"Private instructions","world":true,"server":"native-world.dcl.eth"
        }))).await;
        assert_eq!(status, StatusCode::CREATED, "{edited}");
        let (status, approved) = call(
            &app,
            "PATCH",
            &path,
            Some("native-admin"),
            Some(json!({"approved":true,"actor":"automation"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{approved}");
        assert_eq!(edited["data"]["duration"], 7200000);
        assert_eq!(edited["data"]["image_vertical"], vertical);
        assert_eq!(edited["data"]["categories"], json!(["art"]));
        assert_eq!(edited["data"]["schedules"], json!([schedule]));
        assert_eq!(edited["data"]["url"], "https://example.org/join");
        let (status, restored) = call(
            &app,
            "PATCH",
            &path,
            Some("native-admin"),
            Some(json!({"url":null})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{restored}");
        assert_eq!(
            restored["data"]["url"],
            "https://decentraland.org/jump/events?position=10%2C-20&realm=native-world.dcl.eth"
        );
        assert_eq!(approved["data"]["approved_by"], "automation");
        assert_eq!(
            edited["data"]["user"],
            address(),
            "admin editing never becomes ownership transfer"
        );
        assert_eq!(
            edited["data"]["contact"], "organizer@example.org",
            "adminServiceProfile(event.user) keeps contact/details (admin.ts:100-102)"
        );
        let (_, owner) = call(&app, "GET", &path, Some("signed"), None).await;
        assert_eq!(owner["data"]["contact"], "organizer@example.org");
        assert_eq!(owner["data"]["details"], "Private instructions");
        let (status, cleared) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"contact":null,"details":null})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{cleared}");
        assert!(
            cleared["data"]["contact"].is_null(),
            "the owner's view keeps the key"
        );
        assert!(cleared["data"]["details"].is_null());
        assert_eq!(
            cleared["data"]["approved"], true,
            "clearing private organizer text is not a public content edit"
        );
        let (status, _) = call(&app, "PATCH", &path, None, Some(json!({"duration":3000}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let other: CreateEvent = serde_json::from_value(new_event()).unwrap();
        let other = moderation::create_as(
            &pool,
            &other,
            "0x0123456789012345678901234567890123456789",
            "test",
            &Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        let (status, _) = call(
            &app,
            "PATCH",
            &format!("/api/events/{other}"),
            Some("signed"),
            Some(json!({"image":poster})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "another creator's pending event is hidden before the 403 (getEvent.ts:61-79)"
        );
        let (status, owner_edit) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"image_vertical":null,"approved":true,"highlighted":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{owner_edit}");
        assert_eq!(owner_edit["data"]["image_vertical"], Value::Null);
        assert_eq!(
            owner_edit["data"]["approved"], false,
            "content edits cannot self-approve"
        );
        assert_eq!(owner_edit["data"]["highlighted"], false);
        let closed = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                operator_writes_enabled: false,
                ..settings.clone()
            },
            None,
        )));
        let (status, _) = call(
            &closed,
            "PATCH",
            &path,
            Some("native-admin"),
            Some(json!({"duration":4000})),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        for image in [
            format!("https://foreign.example.org{poster}"),
            "/poster/missing.png".into(),
            "https://foreign.example.org/cover.png".into(),
        ] {
            event["image"] = json!(image);
            let (status, body) = call(
                &app,
                "POST",
                "/api/events",
                Some("signed"),
                Some(event.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        let mut world = new_event();
        world["world"] = json!(true);
        world["server"] = json!("native-world.dcl.eth");
        let (status, default) =
            call(&app, "POST", "/api/events", Some("signed"), Some(world)).await;
        assert_eq!(status, StatusCode::CREATED, "{default}");
        assert_eq!(
            default["data"]["image"], "https://events.decentraland.org/images/event-default.jpg",
            "createEvent.ts:223: the events site's default for a world event"
        );
        let default = app
            .clone()
            .oneshot(
                Request::get(super::super::posters::DEFAULT_EVENT_IMAGE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(default.status(), StatusCode::OK);
        assert_eq!(default.headers()["content-type"], "image/webp");
        let bytes = axum::body::to_bytes(default.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn recurring_filter_runs_before_server_pagination() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let app = router(Arc::new(ExploreState::new(
            pool,
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_admin_token: Some("recurring-filter-admin".into()),
                ..Settings::for_tests()
            },
            None,
        )));
        for recurring in [false, false, true, true] {
            let mut event = new_event();
            event["name"] = json!("RecurringServerPage");
            if recurring {
                event["recurrent"] = json!(true);
                event["recurrent_frequency"] = json!("DAILY");
                event["recurrent_count"] = json!(3);
            }
            let (_, event) = call(&app, "POST", "/api/events", Some("signed"), Some(event)).await;
            let path = format!("/api/events/{}", event["data"]["id"].as_str().unwrap());
            let (status, body) = call(
                &app,
                "PATCH",
                &path,
                Some("recurring-filter-admin"),
                Some(json!({"approved":true})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let mut ids = Vec::new();
        for offset in [0, 1] {
            let (status, page) = call(
                &app,
                "GET",
                &format!(
                    "/v1/events?search=RecurringServerPage&recurrent=true&limit=1&offset={offset}"
                ),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(page["data"].as_array().unwrap().len(), 1);
            assert_eq!(page["data"][0]["recurrent"], true);
            ids.push(page["data"][0]["id"].clone());
        }
        assert_ne!(ids[0], ids[1]);
        let (_, ordinary) = call(
            &app,
            "GET",
            "/v1/events?search=RecurringServerPage&recurrent=false",
            None,
            None,
        )
        .await;
        assert_eq!(ordinary["data"].as_array().unwrap().len(), 2);
        assert!(
            ordinary["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|event| event["recurrent"] == false)
        );
    }

    async fn wait_for_event_lock(pool: &sqlx::PgPool, blocker_pid: i32, count: i64) {
        tokio::time::timeout(std::time::Duration::from_secs(5),async {
            loop {
                let blocked: i64 = sqlx::query_scalar("WITH RECURSIVE blocked(pid) AS (
                    SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid))
                    UNION SELECT a.pid FROM pg_stat_activity a JOIN blocked b ON b.pid=ANY(pg_blocking_pids(a.pid))
                ) SELECT count(*) FROM pg_stat_activity a JOIN blocked b USING(pid)
                WHERE a.datname=current_database() AND a.wait_event_type='Lock' AND a.query LIKE '%explore.event%FOR UPDATE%' ")
                    .bind(blocker_pid).fetch_one(pool).await.unwrap();
                if blocked >= count { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("event mutations reach the held row lock");
    }

    #[tokio::test]
    async fn authoring_enriches_known_metadata_and_updates_generated_targets_while_preserving_posters()
     {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let dir =
            std::env::temp_dir().join(format!("event-metadata-posters-{}", uuid::Uuid::new_v4()));
        let storage = Arc::new(crate::storage::Storage::from_config(&dir).await.unwrap());
        let profile_id = format!("event-profile-{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO content.deployments (deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) VALUES ($1,'v3','profile',$2,'{\"v\":{\"avatars\":[{\"name\":\"Known Creator\"}]}}',timezone('UTC',now()),ARRAY[$1],timezone('UTC',now()),'[]')")
            .bind(address()).bind(&profile_id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO content.active_pointers (pointer,entity_id,entity_type) VALUES ($1,$2,'profile') ON CONFLICT (pointer) DO UPDATE SET entity_id=EXCLUDED.entity_id")
            .bind(address()).bind(&profile_id).execute(&pool).await.unwrap();
        let first_place = format!("event-place-{}", uuid::Uuid::new_v4());
        let second_place = format!("event-place-{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO explore.place (id,base_position,title,raw) VALUES ($1,'41,42','Known scene','{\"positions\":[\"41,42\"],\"title\":\"Known scene\",\"estate_id\":\"known-estate\",\"estate_name\":\"Known estate\",\"image\":\"https://cdn.example.org/scene.png\"}'),($2,'43,44','Other scene','{\"positions\":[\"43,44\"],\"title\":\"Other scene\"}')")
            .bind(&first_place).bind(&second_place).execute(&pool).await.unwrap();
        let app = router(Arc::new(
            ExploreState::new(
                pool.clone(),
                Settings {
                    user_writes_enabled: true,
                    ..Settings::for_tests()
                },
                None,
            )
            .with_storage(storage),
        ));
        let (poster, _) = upload_native_poster(&pool, &app, false).await;
        let mut event = new_event();
        event["x"] = json!(41);
        event["y"] = json!(42);
        let (status, created) =
            call(&app, "POST", "/api/events", Some("signed"), Some(event)).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["data"]["user_name"], "Known Creator");
        assert_eq!(created["data"]["scene_name"], "Known scene");
        assert_eq!(created["data"]["estate_id"], "known-estate");
        assert_eq!(
            created["data"]["image"],
            "https://api.decentraland.org/v1/estates/known-estate/map.png",
            "createEvent.ts:215-219: the estate map once the estate is known"
        );
        let path = format!("/api/events/{}", created["data"]["id"].as_str().unwrap());
        let (status, generated) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"x":43,"y":44})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{generated}");
        assert_eq!(
            generated["data"]["image"],
            "https://api.decentraland.org/v1/parcels/43/44/map.png"
        );
        let (status, moved) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"x":43,"y":44,"image":poster})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{moved}");
        assert_eq!(moved["data"]["place_id"], second_place);
        assert_eq!(moved["data"]["scene_name"], "Other scene");
        assert_eq!(
            moved["data"]["estate_id"],
            Value::Null,
            "unknown estate facts are never inferred from the scene name"
        );
        assert_eq!(moved["data"]["image"], poster);
        assert_eq!(
            moved["data"]["url"],
            "https://decentraland.org/jump/events?position=43%2C44"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn generated_posters_follow_moves_and_explicit_null_clears_survive_edits() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        ensure_profile(&pool).await;
        let dir =
            std::env::temp_dir().join(format!("event-generated-posters-{}", uuid::Uuid::new_v4()));
        let storage = Arc::new(crate::storage::Storage::from_config(&dir).await.unwrap());
        let app = router(Arc::new(
            ExploreState::new(
                pool.clone(),
                Settings {
                    user_writes_enabled: true,
                    ..Settings::for_tests()
                },
                None,
            )
            .with_storage(storage),
        ));
        let (status, created) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(new_event()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let path = format!("/api/events/{}", created["data"]["id"].as_str().unwrap());
        assert_eq!(
            created["data"]["image"],
            "https://api.decentraland.org/v1/parcels/10/-20/map.png"
        );
        let (status, moved) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"x":123,"y":124})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{moved}");
        assert_eq!(
            moved["data"]["image"],
            "https://api.decentraland.org/v1/parcels/123/124/map.png"
        );
        let (status, cleared) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"image":null})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{cleared}");
        assert!(cleared["data"]["image"].is_null());
        let (status, moved) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"x":125,"name":"Still cleared"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{moved}");
        assert!(moved["data"]["image"].is_null());
        let (uploaded, _) = upload_native_poster(&pool, &app, false).await;
        let (status, poster) = call(
            &app,
            "PATCH",
            &path,
            Some("signed"),
            Some(json!({"image":uploaded})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{poster}");
        assert_eq!(poster["data"]["image"], uploaded);
        let (_, moved) = call(&app, "PATCH", &path, Some("signed"), Some(json!({"y":126}))).await;
        assert_eq!(moved["data"]["image"], uploaded);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn mirrored_deletion_by_its_owner_with_edit_any_requires_operator_writes_and_a_single_use_nonce()
     {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let wallet = alloy::signers::local::PrivateKeySigner::random();
        let signer = format!("{:#x}", wallet.address());
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\",permissions_granted,permissions) VALUES ($1,true,'[\"edit_any_event\"]') ON CONFLICT (\"user\") DO UPDATE SET permissions_granted=true,permissions=EXCLUDED.permissions")
            .bind(&signer).execute(&pool).await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        super::super::mirror::upsert_event(&pool,&json!({"id":id,"name":"Mirrored owner","user":signer,"approved":true,"start_at":"2099-06-01T18:00:00Z","duration":3600000})).await.unwrap();
        let path = format!("/api/events/{id}");
        let make_app = |operator_writes_enabled| {
            router(Arc::new(ExploreState::new(
                pool.clone(),
                Settings {
                    user_writes_enabled: true,
                    operator_writes_enabled,
                    ..Settings::for_tests()
                },
                None,
            )))
        };
        let (status, _) = call_as(&make_app(false), "DELETE", &path, &wallet, json!({})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let app = make_app(true);
        let nonce = uuid::Uuid::new_v4().to_string();
        for (metadata, expected) in [
            (
                // No nonce, no refusal.
                json!({"body_sha256":hex::encode(Sha256::digest(b""))}),
                StatusCode::OK,
            ),
            (
                json!({"body_sha256":hex::encode(Sha256::digest(b"")),"nonce":nonce}),
                StatusCode::OK,
            ),
            (
                json!({"body_sha256":hex::encode(Sha256::digest(b"")),"nonce":nonce}),
                StatusCode::CONFLICT,
            ),
        ] {
            let mut req = Request::builder().method("DELETE").uri(&path);
            for (key, value) in crate::shared::signed_fetch::test_support::signed_headers_by(
                &wallet,
                "delete",
                &path,
                &metadata.to_string(),
                true,
                &Utc::now().timestamp_millis().to_string(),
            )
            .await
            .iter()
            {
                req = req.header(key, value);
            }
            let response = app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let raw: Value = sqlx::query_scalar("SELECT raw FROM explore.event WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(raw["deleted_by_admin"], true);
        assert_ne!(raw["deleted_by_user"], true);
        let actions:i64=sqlx::query_scalar("SELECT count(*) FROM explore.signed_actions WHERE signer=$1 AND action_type='EventPrivilegedWrite' AND subject_id=$2").bind(&signer).bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!(
            actions, 2,
            "the delete and its idempotent repeat; the replay is refused"
        );
    }

    #[tokio::test]
    async fn owner_edits_preserve_committed_moderation_and_concurrent_fields_without_exposing_private_details()
     {
        let Some(pool) = crate::db::scratch::pool(8).await else {
            return;
        };
        ensure_profile(&pool).await;
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_admin_token: Some("concurrent-admin".into()),
                ..Settings::for_tests()
            },
            None,
        )));
        let mut event = new_event();
        event["contact"] = json!("private@example.org");
        event["details"] = json!("Organizer instructions");
        let (status, created) =
            call(&app, "POST", "/api/events", Some("signed"), Some(event)).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["data"]["contact"], "private@example.org");
        let id = created["data"]["id"].as_str().unwrap().to_owned();
        let path = format!("/api/events/{id}");
        call(
            &app,
            "PATCH",
            &path,
            Some("concurrent-admin"),
            Some(json!({"approved":true})),
        )
        .await;
        let (_, public) = call(&app, "GET", &path, None, None).await;
        assert!(public["data"].get("contact").is_none());
        assert!(public["data"].get("details").is_none());
        let (_, owner) = call(&app, "GET", &path, Some("signed"), None).await;
        assert_eq!(owner["data"]["details"], "Organizer instructions");
        let (_, admin) = call(&app, "GET", &path, Some("concurrent-admin"), None).await;
        assert_eq!(
            admin["data"]["contact"], "private@example.org",
            "adminServiceProfile(event.user): the bearer reads contact/details (index.ts:26-36)"
        );

        let mut moderator = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM explore.event WHERE id=$1 FOR UPDATE")
            .bind(&id)
            .execute(&mut *moderator)
            .await
            .unwrap();
        let edit = {
            let (app, path) = (app.clone(), path.clone());
            tokio::spawn(async move {
                call(
                    &app,
                    "PATCH",
                    &path,
                    Some("signed"),
                    Some(json!({"contact":"updated@example.org"})),
                )
                .await
            })
        };
        let blocker_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *moderator)
            .await
            .unwrap();
        wait_for_event_lock(&pool, blocker_pid, 1).await;
        sqlx::query("UPDATE explore.event SET approved=false,raw=raw || '{\"approved\":false,\"rejected\":true,\"rejection_reason\":\"Moderator reason\"}'::jsonb WHERE id=$1")
            .bind(&id).execute(&mut *moderator).await.unwrap();
        moderator.commit().await.unwrap();
        let (status, edited) = edit.await.unwrap();
        assert_eq!(status, StatusCode::CREATED, "{edited}");
        assert_eq!(edited["data"]["rejected"], true);
        assert_eq!(edited["data"]["rejection_reason"], "Moderator reason");
        assert_eq!(edited["data"]["contact"], "updated@example.org");
        assert_eq!(edited["data"]["approved"], false);

        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM explore.event WHERE id=$1 FOR UPDATE")
            .bind(&id)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let first = {
            let (app, path) = (app.clone(), path.clone());
            tokio::spawn(async move {
                call(
                    &app,
                    "PATCH",
                    &path,
                    Some("signed"),
                    Some(json!({"name":"Concurrent name"})),
                )
                .await
            })
        };
        let second = {
            let (app, path) = (app.clone(), path.clone());
            tokio::spawn(async move {
                call(
                    &app,
                    "PATCH",
                    &path,
                    Some("signed"),
                    Some(json!({"description":"Concurrent description"})),
                )
                .await
            })
        };
        let blocker_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        wait_for_event_lock(&pool, blocker_pid, 2).await;
        blocker.rollback().await.unwrap();
        assert_eq!(first.await.unwrap().0, StatusCode::CREATED);
        assert_eq!(second.await.unwrap().0, StatusCode::CREATED);
        let (_, result) = call(&app, "GET", &path, Some("signed"), None).await;
        assert_eq!(result["data"]["name"], "Concurrent name");
        assert_eq!(result["data"]["description"], "Concurrent description");
        let leaked: i64=sqlx::query_scalar("SELECT count(*) FROM social.event_notification_intents WHERE event_id=$1 AND event ?| ARRAY['contact','details','recurrent_dates']")
            .bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!(
            leaked, 0,
            "notification snapshots contain no private data or recurrence arrays"
        );
    }

    async fn call_as(
        app: &axum::Router,
        method: &str,
        path: &str,
        wallet: &alloy::signers::local::PrivateKeySigner,
        body: Value,
    ) -> (StatusCode, Value) {
        let headers = crate::shared::signed_fetch::test_support::signed_headers_by(
            wallet,
            &method.to_lowercase(),
            path,
            &json!({"body_sha256":hex::encode(Sha256::digest(body.to_string().as_bytes())),"nonce":uuid::Uuid::new_v4().to_string()}).to_string(),
            true,
            &Utc::now().timestamp_millis().to_string(),
        )
        .await;
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        for (key, value) in headers.iter() {
            req = req.header(key, value)
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn http_patch_permissions_preserve_rejection_provenance_and_remoderate_changed_communities()
     {
        let Some(pool) = crate::db::scratch::pool(1).await else {
            return;
        };
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let editor = alloy::signers::local::PrivateKeySigner::random();
        let owner_address = format!("{:#x}", owner.address());
        let editor_address = format!("{:#x}", editor.address());
        let event = moderation::create_as(
            &pool,
            &serde_json::from_value(new_event()).unwrap(),
            &owner_address,
            "test",
            &Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        let path = format!("/api/events/{event}");
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_admin_token: Some("patch-review-admin".into()),
                ..Settings::for_tests()
            },
            None,
        )));
        let (status, _) = call(
            &app,
            "PATCH",
            &path,
            Some("patch-review-admin"),
            Some(json!({"approved":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, date) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"start_at":"2099-07-01T18:00:00Z"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{date}");
        assert_eq!(
            date["data"]["approved"], true,
            "scheduling fields do not re-open moderation (updateEvent.ts:75-89)"
        );
        let (_, renamed) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"name":"Changed public title"}),
        )
        .await;
        assert_eq!(renamed["data"]["approved"], false);
        let (_, rejected) = call(
            &app,
            "PATCH",
            &path,
            Some("patch-review-admin"),
            Some(json!({"rejected":true,"reason":"Moderator decision"})),
        )
        .await;
        let rejecting_actor = rejected["data"]["rejected_by"].clone();
        let (status, owner_edit) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"rejected":false,"rejection_reason":"Forged","details":"Owner instructions"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{owner_edit}");
        assert_eq!(
            owner_edit["data"]["rejected"], true,
            "another reviewer's rejection holds: the owner's `rejected` is not picked while it stands"
        );
        assert_eq!(owner_edit["data"]["rejected_by"], rejecting_actor);
        assert_eq!(owner_edit["data"]["rejection_reason"], "Moderator decision");
        assert_eq!(
            owner_edit["data"]["details"], "Owner instructions",
            "the rest of the edit applies"
        );
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\",permissions_granted,permissions) VALUES ($1,true,'[\"edit_any_event\"]')").bind(&editor_address).execute(&pool).await.unwrap();
        let (status, edited) = call_as(
            &app,
            "PATCH",
            &path,
            &editor,
            json!({"name":"Editor title","rejected":false,"rejection_reason":"Also forged"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{edited}");
        assert_eq!(edited["data"]["rejected_by"], rejecting_actor);
        assert_eq!(edited["data"]["rejection_reason"], "Moderator decision");
        sqlx::query("UPDATE explore.event_profile_settings SET permissions='[\"edit_any_event\",\"approve_any_event\"]' WHERE \"user\"=$1").bind(&editor_address).execute(&pool).await.unwrap();
        let (status, approved) =
            call_as(&app, "PATCH", &path, &editor, json!({"approved":true})).await;
        assert_eq!(status, StatusCode::CREATED, "{approved}");
        assert_eq!(approved["data"]["approved"], true);
        assert_eq!(approved["data"]["rejected"], false);
        assert_eq!(
            approved["data"]["rejected_by"], rejecting_actor,
            "a wallet's approval leaves the rejection fields as they were (updateEvent.ts:485-491)"
        );
        assert_eq!(approved["data"]["rejection_reason"], "Moderator decision");
        let community = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO social.communities (id,name,owner_address) VALUES ($1,'Managed community',$2)").bind(community).bind(&owner_address).execute(&pool).await.unwrap();
        let (status, linked) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"community_id":community.to_string()}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{linked}");
        assert_eq!(
            linked["data"]["approved"], true,
            "community_id is not a moderated field (updateEvent.ts:91-100)"
        );
        call(
            &app,
            "PATCH",
            &path,
            Some("patch-review-admin"),
            Some(json!({"approved":true})),
        )
        .await;
        let (_, unchanged) = call_as(
            &app,
            "PATCH",
            &path,
            &owner,
            json!({"community_id":community.to_string()}),
        )
        .await;
        assert_eq!(
            unchanged["data"]["approved"], true,
            "unchanged community does not reopen moderation"
        );
    }

    #[tokio::test]
    async fn a_waiting_owner_patch_reads_the_committed_moderation_under_the_same_row_lock() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let wallet = alloy::signers::local::PrivateKeySigner::random();
        let owner = format!("{:#x}", wallet.address());
        let event = moderation::create_as(
            &pool,
            &serde_json::from_value(new_event()).unwrap(),
            &owner,
            "test",
            &Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        )));
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM explore.event WHERE id=$1 FOR UPDATE")
            .bind(&event)
            .execute(&mut *tx)
            .await
            .unwrap();
        let path = format!("/api/events/{event}");
        let mut patch = tokio::spawn(async move {
            call_as(
                &app,
                "PATCH",
                &path,
                &wallet,
                json!({"name":"Concurrent owner name"}),
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut patch)
                .await
                .is_err()
        );
        sqlx::query("UPDATE explore.event SET approved=false,raw=raw || '{\"approved\":false,\"rejected\":true,\"rejected_by\":\"concurrent-reviewer\",\"rejection_reason\":\"Locked decision\"}'::jsonb WHERE id=$1")
            .bind(&event).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let (status, result) = patch.await.unwrap();
        assert_eq!(status, StatusCode::CREATED, "{result}");
        assert_eq!(result["data"]["name"], "Concurrent owner name");
        assert_eq!(result["data"]["rejected_by"], "concurrent-reviewer");
        assert_eq!(result["data"]["rejection_reason"], "Locked decision");
        assert_eq!(result["data"]["approved"], false);
    }

    #[tokio::test]
    async fn lists_preserve_a_bounded_future_window_and_ignore_invalid_legacy_dates() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let id = uuid::Uuid::new_v4().to_string();
        let creator = format!(
            "{:#x}",
            alloy::signers::local::PrivateKeySigner::random().address()
        );
        let now = chrono::DateTime::<Utc>::from_timestamp(Utc::now().timestamp(), 0).unwrap();
        let expired = now - chrono::Duration::hours(2);
        let active = now - chrono::Duration::minutes(15);
        let next = now + chrono::Duration::hours(1);
        let later = next + chrono::Duration::hours(1);
        let dates = json!([
            later.to_rfc3339(),
            "not-a-date",
            expired.to_rfc3339(),
            null,
            active.to_rfc3339(),
            "infinity",
            next.to_rfc3339()
        ]);
        super::super::mirror::upsert_event(&pool, &json!({"id":id,"name":"Projection fixture","user":creator,"approved":true,"recurrent":true,"start_at":expired.to_rfc3339(),"next_start_at":expired.to_rfc3339(),"duration":3600000,"finish_at":(later+chrono::Duration::hours(1)).to_rfc3339(),"recurrent_dates":dates})).await.unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings::for_tests(),
            None,
        )));
        let path = format!("/v1/events?list=all&creator={creator}");
        let (status, list) = call(&app, "GET", &path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        let actual: Vec<i64> = list["data"][0]["recurrent_dates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|date| {
                chrono::DateTime::parse_from_rfc3339(date.as_str().unwrap())
                    .unwrap()
                    .timestamp()
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                expired.timestamp(),
                active.timestamp(),
                next.timestamp(),
                later.timestamp()
            ],
            "the start first (`Event/model.ts:138-140`), then the in-progress occurrence and the sorted future dates; invalid dates dropped"
        );
        for date in list["data"][0]["recurrent_dates"].as_array().unwrap() {
            assert!(
                date.as_str().unwrap().ends_with('Z'),
                "UTC dates use the Z suffix: {date}"
            );
        }
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(
                list["data"][0]["next_start_at"].as_str().unwrap()
            )
            .unwrap()
            .timestamp(),
            active.timestamp(),
            "stale snapshot rollover still uses the complete series"
        );
        let (status, live) = call(
            &app,
            "GET",
            &format!("/api/events?list=live&creator={creator}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{live}");
        assert_eq!(live["data"][0]["id"], id);
        let (_, detail) = call(&app, "GET", &format!("/api/events/{id}"), None, None).await;
        assert_eq!(
            detail["data"]["recurrent_dates"].as_array().unwrap().len(),
            4,
            "the start, then the occurrences that have not ended (`Event/model.ts:138-140`, utils.ts:191-233)"
        );
        // The first thousand stored entries are all past. Both list windows
        // must select upcoming dates before applying their output bound.
        let historical: Vec<String> = (0..1000)
            .chain((1000..2300).rev())
            .map(|index| (now + chrono::Duration::hours(index - 1000)).to_rfc3339())
            .collect();
        sqlx::query(
            "UPDATE explore.event SET raw=jsonb_set(raw,'{recurrent_dates}',$2) WHERE id=$1",
        )
        .bind(&id)
        .bind(json!(historical))
        .execute(&pool)
        .await
        .unwrap();
        // The stored series holds the next `MAX_EVENT_RECURRENT` dates and
        // `EventModel.build` adds the start in front (`Event/model.ts:138-140`).
        let at = |window: &[Value], index: usize| {
            chrono::DateTime::parse_from_rfc3339(window[index].as_str().unwrap())
                .unwrap()
                .timestamp()
        };
        let (_, standard) = call(&app, "GET", &path, None, None).await;
        let window = standard["data"][0]["recurrent_dates"].as_array().unwrap();
        assert_eq!(window.len(), super::super::MAX_EVENT_RECURRENT + 1);
        assert_eq!(at(window, 0), expired.timestamp());
        assert_eq!(at(window, 1), now.timestamp());
        assert_eq!(
            at(window, 10),
            (now + chrono::Duration::hours(9)).timestamp()
        );
        let (_, expanded) = call(
            &app,
            "GET",
            &format!("{path}&with_recurrent_dates=true"),
            None,
            None,
        )
        .await;
        let window = expanded["data"][0]["recurrent_dates"].as_array().unwrap();
        assert_eq!(window.len(), 1001);
        assert_eq!(at(window, 0), expired.timestamp());
        assert_eq!(at(window, 1), now.timestamp());
        assert_eq!(
            at(window, 1000),
            (now + chrono::Duration::hours(999)).timestamp()
        );
        for ended_or_invalid in [
            json!([expired.to_rfc3339()]),
            json!({"invalid":"legacy recurrence document"}),
        ] {
            sqlx::query(
                "UPDATE explore.event SET raw=jsonb_set(raw,'{recurrent_dates}',$2) WHERE id=$1",
            )
            .bind(&id)
            .bind(ended_or_invalid)
            .execute(&pool)
            .await
            .unwrap();
            let (status, empty) = call(&app, "GET", &path, None, None).await;
            assert_eq!(status, StatusCode::OK, "{empty}");
            assert_eq!(
                empty["data"][0]["recurrent_dates"].as_array().map(Vec::len),
                Some(1),
                "an ended or invalid series falls back to the start (utils.ts:436-438)"
            );
        }
    }

    #[tokio::test]
    async fn privileged_event_mutations_bind_bodies_and_consume_nonces_only_on_commit() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let wallet = alloy::signers::local::PrivateKeySigner::random();
        let signer = format!("{:#x}", wallet.address());
        sqlx::query("INSERT INTO explore.moderators(address,added_at) VALUES($1,0)")
            .bind(&signer)
            .execute(&pool)
            .await
            .unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                ..Settings::for_tests()
            },
            None,
        )));
        async fn request(
            app: &axum::Router,
            method: &str,
            path: &str,
            headers: &HeaderMap,
            body: &str,
        ) -> StatusCode {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json");
            for (name, value) in headers {
                request = request.header(name, value);
            }
            app.clone()
                .oneshot(request.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap()
                .status()
        }
        for (method, body) in [("PATCH", r#"{"approved":true}"#), ("DELETE", "")] {
            let id = moderation::create_as(
                &pool,
                &serde_json::from_value(new_event()).unwrap(),
                "0xowner",
                "test",
                &Settings::for_tests(),
                None,
            )
            .await
            .unwrap();
            let path = format!("/api/events/{id}");
            let metadata = json!({"body_sha256":hex::encode(Sha256::digest(body.as_bytes())),"nonce":uuid::Uuid::new_v4().to_string()}).to_string();
            let timestamp = Utc::now().timestamp_millis().to_string();
            let headers = crate::shared::signed_fetch::test_support::signed_headers_by(
                &wallet,
                &method.to_lowercase(),
                &path,
                &metadata,
                true,
                &timestamp,
            )
            .await;
            let unbound = crate::shared::signed_fetch::test_support::signed_headers_by(
                &wallet,
                &method.to_lowercase(),
                &path,
                "{}",
                true,
                &timestamp,
            )
            .await;
            assert_eq!(
                request(&app, method, &path, &unbound, body).await,
                // A plain signed fetch is accepted.
                if method == "DELETE" {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                }
            );
            assert_eq!(
                request(&app, method, &path, &headers, r#"{"approved":false}"#).await,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                request(&app, method, &path, &headers, body).await,
                // gatsby `handle`: PATCH answers 201, DELETE 200
                if method == "DELETE" {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                }
            );
            assert_eq!(
                request(&app, method, &path, &headers, body).await,
                StatusCode::CONFLICT
            );
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM explore.signed_actions WHERE signer=$1 AND action_type='EventPrivilegedWrite'").bind(&signer).fetch_one(&pool).await.unwrap();
        assert_eq!(
            count, 4,
            "two unbound and two bound requests; the replays are refused"
        );

        // Content-only edits retain approval through either owner approval grant,
        // so they need the same body binding and replay protection as explicit approval.
        for permission in ["approve_own_event", "approve_any_event"] {
            let owner = alloy::signers::local::PrivateKeySigner::random();
            let address = format!("{:#x}", owner.address());
            sqlx::query("INSERT INTO explore.event_profile_settings (\"user\",permissions_granted,permissions) VALUES ($1,true,$2)")
                .bind(&address).bind(json!([permission])).execute(&pool).await.unwrap();
            let id = moderation::create_as(
                &pool,
                &serde_json::from_value(new_event()).unwrap(),
                &address,
                "test",
                &Settings::for_tests(),
                None,
            )
            .await
            .unwrap();
            moderation::moderate(
                &pool,
                &id,
                &AdminPatch {
                    approved: Some(true),
                    ..Default::default()
                },
                "test",
                &Settings::for_tests(),
            )
            .await
            .unwrap();
            let path = format!("/api/events/{id}");
            let body = r#"{"name":"Approved content edit"}"#;
            let metadata = json!({"body_sha256":hex::encode(Sha256::digest(body.as_bytes())),"nonce":uuid::Uuid::new_v4().to_string()}).to_string();
            let timestamp = Utc::now().timestamp_millis().to_string();
            let headers = crate::shared::signed_fetch::test_support::signed_headers_by(
                &owner, "patch", &path, &metadata, true, &timestamp,
            )
            .await;
            let unbound = crate::shared::signed_fetch::test_support::signed_headers_by(
                &owner, "patch", &path, "{}", true, &timestamp,
            )
            .await;
            assert_eq!(
                request(&app, "PATCH", &path, &unbound, body).await,
                StatusCode::CREATED,
                "a plain signed fetch needs no metadata"
            );
            assert_eq!(
                request(
                    &app,
                    "PATCH",
                    &path,
                    &headers,
                    r#"{"name":"Tampered content edit"}"#
                )
                .await,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                request(&app, "PATCH", &path, &headers, body).await,
                StatusCode::CREATED
            );
            assert_eq!(
                request(&app, "PATCH", &path, &headers, body).await,
                StatusCode::CONFLICT
            );
            let raw: Value = sqlx::query_scalar("SELECT raw FROM explore.event WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(raw["name"], "Approved content edit");
            assert_eq!(raw["approved"], true);
        }
    }

    #[tokio::test]
    async fn self_approval_cannot_clear_a_moderator_rejection_but_approve_any_can() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let wallet = alloy::signers::local::PrivateKeySigner::random();
        let owner = format!("{:#x}", wallet.address());
        let event = moderation::create_as(
            &pool,
            &serde_json::from_value(new_event()).unwrap(),
            &owner,
            "test",
            &Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\",permissions_granted,permissions) VALUES ($1,true,'[\"approve_own_event\"]')").bind(&owner).execute(&pool).await.unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_admin_token: Some("rejection-reviewer".into()),
                ..Settings::for_tests()
            },
            None,
        )));
        let path = format!("/api/events/{event}");
        let (status, approved) =
            call_as(&app, "PATCH", &path, &wallet, json!({"approved":true})).await;
        assert_eq!(status, StatusCode::CREATED, "{approved}");
        assert_eq!(
            approved["data"]["approved"], true,
            "self-approval still works before moderation rejection"
        );
        // The same grant keeps an approved event approved through a content
        // edit (updateEvent.ts:452-477: the actor may approve, no re-queue).
        let (status, edited) = call_as(
            &app,
            "PATCH",
            &path,
            &wallet,
            json!({"name":"Renamed by its approving owner"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{edited}");
        assert_eq!(edited["data"]["approved"], true);
        assert_eq!(edited["data"]["name"], "Renamed by its approving owner");
        let (_, own) = call_as(&app, "PATCH", &path, &wallet, json!({"rejected":true})).await;
        assert_eq!(own["data"]["rejected"], true, "{own}");
        let (status, undone) = call_as(
            &app,
            "PATCH",
            &path,
            &wallet,
            json!({"approved":true,"rejected":false}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{undone}");
        assert_eq!(
            undone["data"]["rejected"], false,
            "an owner can undo their own rejection"
        );
        assert_eq!(undone["data"]["approved"], true);
        call_as(&app, "PATCH", &path, &wallet, json!({"rejected":true})).await;
        let (_, rejected) = call(
            &app,
            "PATCH",
            &path,
            Some("rejection-reviewer"),
            Some(json!({"rejected":true,"reason":"Moderator hold"})),
        )
        .await;
        let actor = rejected["data"]["rejected_by"].clone();
        // Another reviewer's rejection holds the event: the owner's branch
        // picks neither `approved` nor `rejected` while it stands, whatever
        // the body sends; `rejection_reason` is never theirs to set.
        for body in [
            json!({"approved":true}),
            json!({"rejected":false}),
            json!({"approved":true,"rejected":false,"rejection_reason":"Forged reason"}),
        ] {
            let (status, held) = call_as(&app, "PATCH", &path, &wallet, body).await;
            assert_eq!(status, StatusCode::CREATED, "{held}");
            assert_eq!(held["data"]["approved"], false);
            assert_eq!(held["data"]["rejected"], true);
            assert_eq!(held["data"]["rejected_by"], actor);
            assert_eq!(held["data"]["rejection_reason"], "Moderator hold");
        }
        // An owner who also holds `edit_any_event` edits as the owner they
        // are: the hold stands for them too (the grant picks `rejected` for
        // a non-owner as `updateEvent.ts:214-217` does).
        sqlx::query("UPDATE explore.event_profile_settings SET permissions='[\"approve_own_event\",\"edit_any_event\"]' WHERE \"user\"=$1").bind(&owner).execute(&pool).await.unwrap();
        for body in [
            json!({"rejected":false}),
            json!({"approved":true,"rejected":false}),
        ] {
            let (status, held) = call_as(&app, "PATCH", &path, &wallet, body).await;
            assert_eq!(status, StatusCode::CREATED, "{held}");
            assert_eq!(
                held["data"]["rejected"], true,
                "edit_any_event does not lift another reviewer's hold for the owner: {held}"
            );
            assert_eq!(held["data"]["approved"], false);
            assert_eq!(held["data"]["rejected_by"], actor);
        }
        sqlx::query("UPDATE explore.event_profile_settings SET permissions='[\"approve_any_event\"]' WHERE \"user\"=$1").bind(&owner).execute(&pool).await.unwrap();
        let (status, cleared) = call_as(
            &app,
            "PATCH",
            &path,
            &wallet,
            json!({"approved":true,"rejected":false,"rejection_reason":null}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{cleared}");
        assert_eq!(cleared["data"]["approved"], true);
        assert_eq!(cleared["data"]["rejected"], false);
        assert!(cleared["data"]["rejection_reason"].is_null());
    }

    #[tokio::test]
    async fn legacy_schedule_patches_preserve_fields_and_event_lists_apply_schedule_filter() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\", permissions_granted, permissions) VALUES ($1,true, '[\"edit_any_schedule\"]'::jsonb) ON CONFLICT (\"user\") DO UPDATE SET permissions = EXCLUDED.permissions, permissions_granted = true")
            .bind(address()).execute(&pool).await.unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_admin_token: Some("schedule-test-admin".into()),
                ..Settings::for_tests()
            },
            None,
        )));
        let (status, schedule) = call(&app, "POST", "/api/schedules", Some("signed"), Some(json!({
            "name":"Calendar", "description":"Retained", "active_since":"2099-01-01T00:00:00Z", "active_until":"2099-12-31T00:00:00Z"
        }))).await;
        assert_eq!(status, StatusCode::CREATED, "{schedule}");
        let schedule_id = schedule["data"]["id"].as_str().unwrap();
        let (status, changed) = call(
            &app,
            "PATCH",
            &format!("/api/schedules/{schedule_id}"),
            Some("signed"),
            Some(json!({"name":"Renamed"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{changed}");
        assert_eq!(changed["data"]["description"], "Retained");
        // Every answer uses the old service's Date serialization
        // (milliseconds, `events::json_date`); the stored instant is unchanged.
        assert_eq!(changed["data"]["active_since"], "2099-01-01T00:00:00.000Z");
        assert_eq!(schedule["data"]["active_since"], "2099-01-01T00:00:00.000Z");
        let mut chosen = new_event();
        chosen["schedules"] = json!([schedule_id]);
        let (status, chosen) =
            call(&app, "POST", "/api/events", Some("signed"), Some(chosen)).await;
        assert_eq!(status, StatusCode::CREATED, "{chosen}");
        let chosen_id = chosen["data"]["id"].as_str().unwrap();
        assert_eq!(chosen["data"]["schedules"], json!([]), "createEvent.ts:260");
        let (status, assigned) = call(
            &app,
            "PATCH",
            &format!("/api/events/{chosen_id}"),
            Some("schedule-test-admin"),
            Some(json!({"schedules":[schedule_id]})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{assigned}");
        assert_eq!(assigned["data"]["schedules"], json!([schedule_id]));
        let (status, other) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(new_event()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{other}");
        for id in [chosen_id, other["data"]["id"].as_str().unwrap()] {
            let (status, body) = call(
                &app,
                "PATCH",
                &format!("/api/events/{id}"),
                Some("schedule-test-admin"),
                Some(json!({"approved":true})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (status, body) = call(
            &app,
            "GET",
            &format!("/api/events?list=all&schedule={schedule_id}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
        assert_eq!(body["data"][0]["id"], chosen_id);
        let (status, body) = call(
            &app,
            "PATCH",
            "/api/schedules/missing",
            Some("signed"),
            Some(json!({"name":"No"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[tokio::test]
    async fn locked_event_edits_and_deletes_capture_only_their_committed_transitions() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let owner = alloy::signers::local::PrivateKeySigner::random();
        let address = format!("{:#x}", owner.address());
        let communities = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
        for community in communities {
            sqlx::query("INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Event transition fixture',$2)").bind(community).bind(&address).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\",permissions,permissions_granted) VALUES ($1,'[\"approve_own_event\"]',true)").bind(&address).execute(&pool).await.unwrap();
        let mut body = new_event();
        body["community_id"] = json!(communities[0]);
        let id = moderation::create_as(
            &pool,
            &serde_json::from_value(body).unwrap(),
            &address,
            &address,
            &Settings::for_tests(),
            None,
        )
        .await
        .unwrap();
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_admin_token: Some("transition-admin".into()),
                ..Settings::for_tests()
            },
            None,
        )));
        let path = format!("/api/events/{id}");
        for edit in [
            json!({"approved":true}),
            json!({"community_id":communities[1]}),
            json!({"name":"Still approved"}),
        ] {
            let (status, body) = call_as(&app, "PATCH", &path, &owner, edit).await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let created: Vec<String> = sqlx::query_scalar("SELECT event->>'community_id' FROM social.event_notification_intents WHERE event_id=$1 AND kind='event_created' ORDER BY occurred_at,id").bind(&id).fetch_all(&pool).await.unwrap();
        assert_eq!(created, communities.map(|id| id.to_string()));
        for _ in 0..2 {
            let (status, body) = call(
                &app,
                "DELETE",
                &path,
                Some("transition-admin"),
                Some(json!({"reason":"Removed","actor":"moderator"})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let deleted: Vec<Value> = sqlx::query_scalar("SELECT event FROM social.event_notification_intents WHERE event_id=$1 AND kind='event_deleted'").bind(&id).fetch_all(&pool).await.unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0]["deleted_reason"], "Removed");
        assert_eq!(deleted[0]["name"], "Still approved");
        let mirrored = uuid::Uuid::new_v4().to_string();
        super::super::mirror::upsert_event(&pool,&json!({"id":mirrored,"name":"Mirrored transition","user":address,"approved":true,"start_at":"2099-06-01T18:00:00Z","duration":3600000})).await.unwrap();
        for _ in 0..2 {
            let (status, body) = call(
                &app,
                "DELETE",
                &format!("/api/events/{mirrored}"),
                Some("transition-admin"),
                Some(json!({"reason":"Hidden","actor":"moderator"})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let notices: Vec<(String,bool,Value)> = sqlx::query_as("SELECT kind,local_event,event FROM social.event_notification_intents WHERE event_id=$1").bind(&mirrored).fetch_all(&pool).await.unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "event_deleted");
        assert!(!notices[0].1);
        assert_eq!(notices[0].2["deleted_reason"], "Hidden");
    }

    /// The events write surface over the real schema: a wallet creates,
    /// edits and deletes its own event and RSVPs; another wallet may not;
    /// the admin bearer moderates; the switches answer 503; the sitemaps and
    /// the v1 reads see the result.
    #[tokio::test]
    async fn events_are_written_by_their_owner_and_moderated_by_the_admin() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let settings = Settings {
            user_writes_enabled: true,
            operator_writes_enabled: true,
            events_admin_token: Some("events-admin".into()),
            ..Settings::for_tests()
        };
        let app = router(Arc::new(ExploreState::new(
            pool.clone(),
            settings.clone(),
            None,
        )));
        let closed = router(Arc::new(ExploreState::new(
            pool.clone(),
            Settings {
                user_writes_enabled: false,
                ..settings.clone()
            },
            None,
        )));

        let (status, _) = call(
            &closed,
            "POST",
            "/api/events",
            Some("signed"),
            Some(new_event()),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (status, _) = call(&app, "POST", "/api/events", None, Some(new_event())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(json!({ "name": "x", "nope": 1 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = call(
            &app,
            "POST",
            "/api/events",
            Some("signed"),
            Some(new_event()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["data"]["id"].as_str().unwrap().to_string();
        assert_eq!(
            (
                body["data"]["user"].clone(),
                body["data"]["approved"].clone()
            ),
            (json!(address()), json!(false))
        );

        // the admin approves, the owner's content edit sends it back to approval
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/events/{id}"),
            Some("events-admin"),
            Some(json!({ "approved": true, "actor": "0xmod" })),
        )
        .await;
        assert_eq!(
            (status, body["data"]["approved"].clone()),
            (StatusCode::CREATED, json!(true)),
            "{body}"
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/events/{id}"),
            Some("signed"),
            Some(json!({ "name": "Launch party II" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            (
                body["data"]["name"].clone(),
                body["data"]["approved"].clone()
            ),
            (json!("Launch party II"), json!(false))
        );
        let (status, _) = call(
            &app,
            "PATCH",
            &format!("/api/events/{id}"),
            Some("events-admin"),
            Some(json!({ "approved": true })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // another owner's event is not editable; an unknown one is a 404
        sqlx::query("UPDATE explore.event SET raw = raw || '{\"user\": \"0x0000000000000000000000000000000000000abc\"}', user_creator = '0x0000000000000000000000000000000000000abc' WHERE id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/events/{id}"),
            Some("signed"),
            Some(json!({ "name": "hijack" })),
        )
        .await;
        assert_eq!(
            (status, body["error"].clone()),
            (
                StatusCode::FORBIDDEN,
                json!("You don't have permission to edit this event")
            )
        );
        sqlx::query("UPDATE explore.event SET raw = raw || jsonb_build_object('user', $2::text), user_creator = $2 WHERE id = $1").bind(&id).bind(address()).execute(&pool).await.unwrap();
        let (status, _) = call(
            &app,
            "PATCH",
            "/api/events/00000000-0000-0000-0000-000000000000",
            Some("signed"),
            Some(json!({ "name": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/events/{id}/attendees"),
            Some("signed"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"][0]["user"], json!(address()));
        assert!(
            body["data"][0]["user_name"].is_null() || body["data"][0]["user_name"].is_string(),
            "user_name comes from the signer's profile, not the request metadata (routes.ts:63)"
        );
        let (_, body) = call(
            &app,
            "DELETE",
            &format!("/api/events/{id}/attendees"),
            Some("signed"),
            None,
        )
        .await;
        assert_eq!(body["data"], json!([]));

        let (status, body) = call(&app, "GET", &format!("/v1/events/{id}"), None, None).await;
        assert_eq!(
            (status, body["data"]["id"].clone()),
            (StatusCode::OK, json!(id))
        );
        let (status, body) = call(&app, "GET", "/v1/events/not-a-uuid", None, None).await;
        assert_eq!(
            (status, body["error"].clone()),
            (StatusCode::NOT_FOUND, json!("Event not found: not-a-uuid"))
        );
        let (status, body) = call(&app, "GET", "/events/sitemap.xml", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.as_str().unwrap().contains("sitemap.events.xml?page=0"),
            "{body}"
        );
        let (_, body) = call(&app, "GET", "/events/sitemap.events.xml?page=0", None, None).await;
        assert!(
            body.as_str().unwrap().contains(&format!("/event/?id={id}")),
            "{body}"
        );
        let (_, body) = call(
            &app,
            "GET",
            "/events/sitemap.events.xml?page=00",
            None,
            None,
        )
        .await;
        assert!(
            !body.as_str().unwrap().contains("<url>"),
            "a non-canonical page number answers an empty set"
        );

        // the owner deletes; a delete again answers the deleted record
        let (status, body) = call(
            &app,
            "DELETE",
            &format!("/api/events/{id}"),
            Some("signed"),
            None,
        )
        .await;
        assert_eq!(
            (status, body["data"]["deleted_by_user"].clone()),
            (StatusCode::OK, json!(true)),
            "{body}"
        );
        let (status, _) = call(
            &app,
            "DELETE",
            &format!("/api/events/{id}"),
            Some("signed"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &app,
            "POST",
            &format!("/api/events/{id}/attendees"),
            Some("signed"),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "getEvent has no deletion check: an approved deleted event still takes RSVPs"
        );
        let overlay: String =
            sqlx::query_scalar("SELECT signer FROM explore.events_local WHERE id = $1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(overlay, address());
        let (status, body) = call(&app, "GET", "/api/profiles/subscriptions", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }

    /// A mirrored event is the upstream's: its owner's edit and delete (and
    /// an admin patch of its name) answer 409 and write nothing, so the next
    /// pass still writes it. The admin's moderation and delete are local
    /// policy the pass keeps: a rejected or deleted event leaves the public
    /// list and detail while the admin still reads it. An RSVP is a local row
    /// the count adds to the upstream's.
    #[tokio::test]
    async fn a_mirrored_event_refuses_its_owner_keeps_the_admins_moderation_and_counts_local_rsvps()
    {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        ensure_profile(&pool).await;
        let settings = Settings {
            user_writes_enabled: true,
            operator_writes_enabled: true,
            events_admin_token: Some("events-admin".into()),
            ..Settings::for_tests()
        };
        let app = router(Arc::new(ExploreState::new(pool.clone(), settings, None)));
        let id = uuid::Uuid::new_v4().to_string();
        let community = uuid::Uuid::new_v4().to_string();
        let upstream = |name: &str| {
            json!({ "id": id, "name": name, "user": address(), "approved": true,
                    "community_id": community,
                    "start_at": "2099-06-01T18:00:00Z", "finish_at": "2099-06-01T19:00:00Z",
                    "duration": 3_600_000, "total_attendees": 7,
                    "latest_attendees": ["0x00000000000000000000000000000000000000a7"] })
        };
        let pass = |name: &'static str| {
            let pool = pool.clone();
            let doc = upstream(name);
            async move {
                assert!(
                    super::super::mirror::upsert_event(&pool, &doc)
                        .await
                        .unwrap()
                );
            }
        };
        pass("Upstream party").await;

        let path = format!("/api/events/{id}");
        let listed = || {
            let app = app.clone();
            let community = community.clone();
            async move {
                let (_, body) = call(
                    &app,
                    "GET",
                    &format!("/api/events?list=all&community_id={community}"),
                    None,
                    None,
                )
                .await;
                let shown = body["data"]["events"].as_array().map_or(0, Vec::len);
                assert_eq!(body["data"]["total"], json!(shown), "{body}");
                shown
            }
        };
        assert_eq!(listed().await, 1);
        let edit = json!({ "name": "renamed here" });
        let refusals = [
            call(&app, "PATCH", &path, Some("signed"), Some(edit.clone())).await,
            call(&app, "DELETE", &path, Some("signed"), None).await,
            call(
                &app,
                "PATCH",
                &path,
                Some("events-admin"),
                Some(edit.clone()),
            )
            .await,
        ];
        for (status, body) in refusals {
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["code"], json!("conflict"));
            assert!(
                body["error"].as_str().unwrap().contains("mirrored from"),
                "{body}"
            );
        }
        let (name, raw, audits, overlays): (String, Value, i64, i64) = sqlx::query_as(
            "SELECT name, raw, (SELECT count(*) FROM explore.events_local WHERE id = $1), \
                    (SELECT count(*) FROM explore.event_moderation WHERE event_id = $1) \
             FROM explore.event WHERE id = $1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (
                name.as_str(),
                raw.get("deleted_by_user"),
                raw.get("rejected"),
                audits,
                overlays
            ),
            ("Upstream party", None, None, 0, 0)
        );

        // the admin rejects it: gone from the public reads, kept by a pass
        let (status, body) = call(
            &app,
            "PATCH",
            &path,
            Some("events-admin"),
            Some(json!({ "rejected": true, "reason": "Spam" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        pass("Renamed upstream").await;
        assert_eq!(listed().await, 0, "a rejected event is not listed");
        let (status, _) = call(&app, "GET", &path, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(&app, "GET", &path, Some("events-admin"), None).await;
        assert_eq!(
            (
                status,
                body["data"]["name"].clone(),
                body["data"]["rejected"].clone()
            ),
            (StatusCode::OK, json!("Renamed upstream"), json!(true)),
            "{body}"
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &path,
            Some("events-admin"),
            Some(json!({ "approved": true })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(listed().await, 1);

        let (status, body) = call(
            &app,
            "POST",
            &format!("{path}/attendees"),
            Some("signed"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (_, body) = call(&app, "GET", &path, None, None).await;
        assert_eq!(
            body["data"]["total_attendees"],
            json!(8),
            "the upstream's 7 and this node's 1: {body}"
        );
        pass("Renamed again").await;
        let (_, body) = call(&app, "GET", &path, None, None).await;
        assert_eq!(
            (
                body["data"]["name"].clone(),
                body["data"]["total_attendees"].clone()
            ),
            (json!("Renamed again"), json!(8)),
            "neither the RSVP nor the moderation freezes the upstream's fields"
        );

        // the admin deletes it: hidden from every public read across passes
        let operator = "0x00000000000000000000000000000000000000ad";
        let (status, body) = call(
            &app,
            "DELETE",
            &path,
            Some("events-admin"),
            Some(json!({ "reason": "duplicate", "actor": operator })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        pass("Renamed once more").await;
        assert_eq!(listed().await, 0, "a hidden event is not listed");
        let (status, body) = call(&app, "GET", &path, None, None).await;
        assert_eq!(
            (status, body["data"]["deleted_by_admin"].clone()),
            (StatusCode::OK, json!(true)),
            "getEvent.ts:61-79 has no deletion check: the id read still answers it, deleted"
        );
        let (status, body) = call(&app, "GET", &path, Some("events-admin"), None).await;
        assert_eq!(
            (
                status,
                body["data"]["name"].clone(),
                body["data"]["deleted_by_admin"].clone(),
                body["data"]["deleted_by"].clone(),
                body["data"]["deleted_reason"].clone()
            ),
            (
                StatusCode::OK,
                json!("Renamed once more"),
                json!(true),
                json!(operator),
                json!("duplicate")
            ),
            "the console still reads it, with the delete's actor and reason: {body}"
        );
        let (hidden, approved, audits): (Option<bool>, Option<bool>, i64) = sqlx::query_as(
            "SELECT hidden, approved, (SELECT count(*) FROM explore.events_local WHERE id = $1) \
             FROM explore.event_moderation WHERE event_id = $1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((hidden, approved, audits), (Some(true), Some(true), 0));

        sqlx::query("DELETE FROM explore.event_attendance_local WHERE event_id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM explore.event_moderation WHERE event_id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM explore.event WHERE id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
    }
}
