//! Event profile settings, schedule writes, web-push subscriptions and the
//! avatar redirects of the old events service (decentraland/events@b60d5bbd
//! and decentraland-gatsby@9.0.1). Existing clients use plain JSON plus
//! signed-fetch headers; federation envelopes remain supported.
//!
//! - `GET /api/profiles/settings` (rows holding permissions, for a moderator;
//!   `ProfileSettings/routes/listProfileSettings.ts`),
//!   `GET`/`PATCH /api/profiles/me/settings` (the signer's own;
//!   `getAuthProfileSettings.ts`, `updateMyProfileSettings.ts`) and
//!   `GET`/`PATCH /api/profiles/{id}/settings` (anyone's, for the same
//!   readers; `getProfileSettings.ts`, `updateProfileSettings.ts`);
//! - `POST /api/schedules` and `PATCH /api/schedules/{id}` (a moderator or
//!   an `edit_any_schedule` grant; `Schedule/routes.ts:48-97`), with signed
//!   JSON or a `ScheduleUpsert` envelope;
//! - `GET`/`POST`/`DELETE /api/profiles/subscriptions`: the deprecated
//!   web-push rows, kept in step with `notify_by_browser`
//!   (`ProfileSubscription/routes.ts:17-73`); nothing sends pushes;
//! - `GET /api/profile/{user}/{face,body}.png`: 302 to the avatar snapshot of
//!   the profile this node holds, else the default avatar (gatsby
//!   `entities/Profile/routes.js`).
//!
//! "isAdmin" (gatsby's `ADMIN_ADDRESSES`) is a wallet in `explore.moderators`;
//! the `edit_any_*` permissions count once a moderator granted them
//! (`permissions_granted`), except `edit_any_profile`: private profile reads
//! and permission grants stay moderator-only, the grant is stored and listed
//! but delegates nothing. A plain signed-fetch body needs no
//! `x-identity-metadata`; when it carries `body_sha256` the body must match
//! it, and when it carries a `nonce` a replay answers 409.
//! Every write is logged in `explore.signed_actions` and applied in one
//! transaction: the signer's own settings and subscriptions behind
//! `DATA_WRITES_ENABLED`, schedule and profile administration behind
//! `OPS_WRITES_ENABLED`, the switch of the node's moderation. Validation
//! failures answer the gatsby validator's 400, `Invalid data was sent to
//! the server` with the ajv `data.{errors,messages,body}` (gatsby
//! `dist/entities/Route/validate.js`; the schemas live in `crate::validate`).
//!
//! Legacy notify_by_email/notify_by_browser values are stored for profile API
//! compatibility. Social delivery uses its own subscriptions and per-message
//! preferences; these legacy profile values do not configure that worker.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
#[cfg(test)]
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};

use super::ScheduleRecord;
use crate::SharedState;
use crate::fed::{self, ProfileSettingsUpdate, ScheduleUpsert};
use crate::shared::errors::{ApiData, ApiError};
use crate::shared::identity::Address;
use crate::validate::{self, Flavour};

/// The list is capped (a table with one row per wallet that saved settings).
const SETTINGS_LIST_CAP: i64 = 10_000;

/// gatsby `createValidator` (`dist/entities/Route/validate.js`): every ajv
/// failure answers this text.
const INVALID_DATA: &str = "Invalid data was sent to the server";

const PROFILE_PERMISSIONS: [&str; 5] = [
    "approve_own_event",
    "approve_any_event",
    "edit_any_event",
    "edit_any_schedule",
    "edit_any_profile",
];

/// `Schedule/types.ts:20-24`, the `theme` enum beside `null`.
const SCHEDULE_THEMES: [&str; 3] = ["mvmf_2022", "mvfw_2023", "pride_2023"];

/// `Schedule/schema.ts:90-141`: the keys a schedule body may set, and the
/// keys a PATCH answers (`routes.ts:90-96`; `schedule_keys`).
#[cfg(test)]
const SCHEDULE_FIELDS: [&str; 8] = [
    "name",
    "description",
    "image",
    "theme",
    "background",
    "active",
    "active_since",
    "active_until",
];

/// gatsby `Profile/routes.js`: the avatar every miss redirects to, cached a day.
const DEFAULT_AVATAR: &str = "https://decentraland.org/images/male.png";
const AVATAR_TTL_SECS: u64 = 86_400;

pub async fn is_moderator(pool: &PgPool, address: &str) -> Result<bool, ApiError> {
    let row: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM explore.moderators WHERE address = $1")
        .bind(address.to_ascii_lowercase())
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

pub async fn require_moderator(pool: &PgPool, signer: &str) -> Result<(), ApiError> {
    if is_moderator(pool, signer).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden("Forbidden"))
    }
}

/// A moderator, or a wallet a moderator granted `permission`.
async fn has_granted(pool: &PgPool, signer: &str, permission: &str) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM explore.moderators WHERE address = $1) \
         OR EXISTS (SELECT 1 FROM explore.event_profile_settings WHERE \"user\" = $1 AND permissions_granted AND permissions ? $2)",
    )
    .bind(signer.to_ascii_lowercase())
    .bind(permission)
    .fetch_one(pool)
    .await?)
}

/// The readers and editors of other profiles: a moderator only. The old
/// service also admits an `edit_any_profile` grant (`getProfileSettings.ts:14-16`,
/// `listProfileSettings.ts:11-13`, `ProfileSettings/utils.ts:4-11`); here the
/// grant reads no email and grants nothing (accepted entries
/// `events.profiles.*_edit_any_profile`).
async fn require_profile_admin(pool: &PgPool, signer: &str) -> Result<(), ApiError> {
    if is_moderator(pool, signer).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden("Forbidden"))
    }
}

/// `isAdmin(user) || canEditAnySchedule(profile)` (`Schedule/routes.ts:54-56,80-82`).
async fn require_schedule_editor(pool: &PgPool, signer: &str) -> Result<(), ApiError> {
    if has_granted(pool, signer, "edit_any_schedule").await? {
        Ok(())
    } else {
        Err(ApiError::forbidden("Forbidden"))
    }
}

fn ok(data: Value) -> Json<Value> {
    Json(json!({ "ok": true, "data": data }))
}

fn invalid_data() -> ApiError {
    ApiError::bad_request(INVALID_DATA)
}

/// The asset checks both schedule encodings share past their schemas: past
/// ajv's `format: "uri"`, an image URL the website may render (an http(s)
/// scheme) and a theme it knows.
fn check_schedule_assets(image: Option<&str>, theme: Option<&str>) -> Result<(), ApiError> {
    if image.is_some_and(|image| crate::sanitize::sanitize_asset_url(image).is_err())
        || theme.is_some_and(|theme| !SCHEDULE_THEMES.contains(&theme))
    {
        return Err(invalid_data());
    }
    Ok(())
}

fn envelope(body: Value) -> Result<Value, ApiError> {
    if catalyst_fed::is_envelope(&body) {
        Ok(body)
    } else {
        Err(ApiError::bad_request("missing signed body"))
    }
}

/// The profile entity this node holds for `address` (`id`, deployed
/// metadata), as `/lambdas/profiles` reads it; `None` for a non-address or
/// a wallet without a profile here.
pub(super) async fn local_profile(
    pool: &PgPool,
    address: &str,
) -> Result<Option<(String, Value)>, ApiError> {
    let Ok(address) = Address::parse(address) else {
        return Ok(None);
    };
    let docs =
        crate::content::queries::active_entity_docs_by_pointers(pool, &[address.into_string()])
            .await?;
    Ok(docs
        .into_iter()
        .filter_map(|doc| doc.to_value())
        .find(|doc| doc.get("type").and_then(Value::as_str) == Some("profile"))
        .map(|doc| {
            (
                doc.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                doc.get("metadata").cloned().unwrap_or(Value::Null),
            )
        }))
}

/// gatsby `withAuthProfile()` (`dist/entities/Profile/middleware.js:33-47`):
/// a signer without a profile is 404 `Not found profile for "<user>"`.
pub(super) async fn require_profile(pool: &PgPool, signer: &str) -> Result<(), ApiError> {
    if local_profile(pool, signer).await?.is_none() {
        return Err(ApiError::not_found(format!(
            "Not found profile for \"{signer}\""
        )));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Snapshot {
    Face,
    Body,
}

/// `GET /api/profile/{user}/face.png`: 302 to `avatar.snapshots.face256 || face`.
pub async fn avatar_face(
    State(state): State<SharedState>,
    Path(user): Path<String>,
) -> Result<Response, ApiError> {
    avatar(&state, &user, Snapshot::Face).await
}

/// `GET /api/profile/{user}/body.png`: 302 to `avatar.snapshots.body`.
pub async fn avatar_body(
    State(state): State<SharedState>,
    Path(user): Path<String>,
) -> Result<Response, ApiError> {
    avatar(&state, &user, Snapshot::Body).await
}

async fn avatar(state: &SharedState, user: &str, which: Snapshot) -> Result<Response, ApiError> {
    let target = match local_profile(&state.pool, user).await? {
        Some((entity_id, metadata)) => snapshot_url(
            &metadata,
            &entity_id,
            which,
            &state.settings.events_asset_base_url,
        ),
        None => None,
    };
    Ok(avatar_redirect(target.as_deref()))
}

/// The snapshot a profile's first avatar names, as `/lambdas/profiles`
/// answers it: an absolute URL as stored, a content hash as this node's
/// `profile-images/entities/<id>/{face,body}.png` (which renders the
/// image when the node lacks the file).
fn snapshot_url(
    metadata: &Value,
    entity_id: &str,
    which: Snapshot,
    events_asset_base: &str,
) -> Option<String> {
    let snapshots = metadata
        .get("avatars")?
        .get(0)?
        .get("avatar")?
        .get("snapshots")?;
    let (keys, file): (&[&str], &str) = match which {
        Snapshot::Face => (&["face256", "face"], "face.png"),
        Snapshot::Body => (&["body"], "body.png"),
    };
    let value = keys.iter().find_map(|key| {
        snapshots
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    })?;
    // `events_asset_base` is `<PUBLIC_URL>/events` (`Settings::events_asset_base_for`).
    let own = events_asset_base
        .trim_end_matches('/')
        .strip_suffix("/events")
        .unwrap_or(events_asset_base);
    if value.contains("://") && trusted_snapshot_host(value, own) {
        return Some(value.to_owned());
    }
    // Any other host renders from this node instead of being redirected to.
    Some(format!("{own}/profile-images/entities/{entity_id}/{file}"))
}

/// An absolute snapshot is redirected to only on this node or a Decentraland
/// host, so the route never forwards to an arbitrary site.
fn trusted_snapshot_host(value: &str, own: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let own_host = url::Url::parse(own)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    matches!(url.scheme(), "http" | "https")
        && (own_host.as_deref() == Some(host.as_str())
            || [
                "decentraland.org",
                "decentraland.zone",
                "decentraland.today",
            ]
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}"))))
}

/// gatsby `redirectTo`: a non-URL falls back to the default avatar; the
/// answer is express's 302 with `Cache-Control: max-age=86400` (fixtures
/// `events.get_profile_face`, `events.get_profile_face.invalid`, `events.get_profile_body`).
fn avatar_redirect(url: Option<&str>) -> Response {
    let url = url
        .filter(|url| {
            url::Url::parse(url)
                .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
        })
        .unwrap_or(DEFAULT_AVATAR);
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, url.to_owned()),
            (header::CACHE_CONTROL, format!("max-age={AVATAR_TTL_SECS}")),
            (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
        ],
        format!("Found. Redirecting to {url}"),
    )
        .into_response()
}

/// The settings rows as the old `ProfileSettingsAttributes` (`types.ts:1-13`),
/// a wallet without a row as `DEFAULT_PROFILE_SETTINGS` (`types.ts:15-27`); a
/// moderator's `permissions` are the whole set even without a row.
async fn load_settings_many(
    conn: &mut PgConnection,
    users: &[String],
) -> Result<Vec<Value>, ApiError> {
    Ok(sqlx::query_scalar(
        r#"SELECT jsonb_build_object(
            'user',u.address,'email',s.email,'email_verified',COALESCE(s.email_verified,false),
            'email_verified_at',to_char(s.email_verified_at,$3),
            'email_updated_at',to_char(s.email_updated_at,$3),
            'use_local_time',COALESCE(s.use_local_time,true),'notify_by_email',COALESCE(s.notify_by_email,false),
            'notify_by_browser',COALESCE(s.notify_by_browser,false),
            'permissions',CASE WHEN m.address IS NOT NULL THEN $2::jsonb WHEN s.permissions_granted THEN s.permissions ELSE '[]'::jsonb END,
            'created_at',to_char(COALESCE(s.created_at,TIMESTAMP '1970-01-01'),$3),
            'updated_at',to_char(COALESCE(s.updated_at,TIMESTAMP '1970-01-01'),$3))
        FROM UNNEST($1::text[]) u(address)
        LEFT JOIN explore.event_profile_settings s ON s."user"=u.address
        LEFT JOIN explore.moderators m ON m.address=u.address ORDER BY u.address"#,
    )
    .bind(users)
    .bind(json!(PROFILE_PERMISSIONS))
    .bind("YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"")
    .fetch_all(conn)
    .await?)
}

/// Settings include a moderator's effective permissions even without a saved row.
pub async fn load_settings(conn: &mut PgConnection, user: &str) -> Result<Value, ApiError> {
    load_settings_many(conn, &[user.to_ascii_lowercase()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::internal("profile settings missing"))
}

/// The stored row, or `None` (`ProfileSettingsModel.findOne`).
async fn load_saved_settings(
    conn: &mut PgConnection,
    user: &str,
) -> Result<Option<Value>, ApiError> {
    let saved: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM explore.event_profile_settings WHERE \"user\" = $1)",
    )
    .bind(user)
    .fetch_one(&mut *conn)
    .await?;
    if !saved {
        return Ok(None);
    }
    load_settings(conn, user).await.map(Some)
}

/// What one settings write may change: the owner's five fields
/// (`updateMyProfileSettingsSchema`, `types.ts:79-101`) and the
/// administrator's `permissions` (`updateProfileSettingsSchema`, `types.ts:59-77`).
#[derive(Debug, Default, Clone)]
struct SettingsPatch {
    email: Option<String>,
    email_verified: Option<bool>,
    use_local_time: Option<bool>,
    notify_by_email: Option<bool>,
    notify_by_browser: Option<bool>,
    permissions: Option<Vec<String>>,
}

impl From<&ProfileSettingsUpdate> for SettingsPatch {
    fn from(m: &ProfileSettingsUpdate) -> Self {
        Self {
            email: m.email.clone(),
            email_verified: m.email_verified,
            use_local_time: m.use_local_time,
            notify_by_email: m.notify_by_email,
            notify_by_browser: m.notify_by_browser,
            permissions: m.permissions.clone(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct SettingsRow {
    email: Option<String>,
    email_verified: bool,
    email_verified_at: Option<NaiveDateTime>,
    email_updated_at: Option<NaiveDateTime>,
    use_local_time: bool,
    notify_by_email: bool,
    notify_by_browser: bool,
    permissions: Value,
    permissions_granted: bool,
    created_at: NaiveDateTime,
}

fn granted_permissions(permissions: &Value, granted: bool) -> Vec<String> {
    if !granted {
        return Vec::new();
    }
    permissions
        .as_array()
        .map(|p| {
            p.iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Applies `patch` to `target`'s row (locked, or the defaults when absent)
/// with the old email lifecycle (`updateMyProfileSettings.ts:33-63`): the
/// body's `email_verified: true` is discarded, a changed email is unverified
/// and loses `notify_by_email`, `email_verified: false` on the same email
/// un-verifies it, no email clears both marks; a `permissions` value becomes
/// authority (`permissions_granted`). Answers the stored settings and whether
/// the effective permission set changed (the old Slack trigger,
/// `updateProfileSettings.ts:31-33,43-48`).
async fn save_settings(
    conn: &mut PgConnection,
    target: &str,
    patch: &SettingsPatch,
) -> Result<(Value, bool), ApiError> {
    let target = target.to_ascii_lowercase();
    let now = Utc::now().naive_utc();
    let existing: Option<SettingsRow> = sqlx::query_as(
        "SELECT email, email_verified, email_verified_at, email_updated_at, use_local_time, notify_by_email, \
                notify_by_browser, permissions, permissions_granted, created_at \
         FROM explore.event_profile_settings WHERE \"user\" = $1 FOR UPDATE",
    )
    .bind(&target)
    .fetch_optional(&mut *conn)
    .await?;
    let previous = existing.as_ref().map(|row| {
        (
            row.email.clone(),
            granted_permissions(&row.permissions, row.permissions_granted),
        )
    });
    let (previous_email, previous_permissions) = previous.unwrap_or_default();
    let mut email = patch.email.clone().or_else(|| previous_email.clone());
    let mut email_verified = existing.as_ref().is_some_and(|row| row.email_verified);
    let mut email_verified_at = existing.as_ref().and_then(|row| row.email_verified_at);
    let mut email_updated_at = existing.as_ref().and_then(|row| row.email_updated_at);
    let mut notify_by_email = patch
        .notify_by_email
        .unwrap_or_else(|| existing.as_ref().is_some_and(|row| row.notify_by_email));
    if email.as_deref().is_some_and(str::is_empty) {
        email = None;
    }
    if email.is_none() {
        notify_by_email = false;
        email_updated_at = None;
        email_verified_at = None;
    }
    if previous_email != email {
        notify_by_email = false;
        email_verified = false;
        email_updated_at = Some(now);
        email_verified_at = None;
    }
    if email.is_some() && previous_email == email && patch.email_verified == Some(false) {
        email_verified = false;
        email_verified_at = None;
        email_updated_at = Some(now);
        notify_by_email = false;
    }
    let use_local_time = patch
        .use_local_time
        .unwrap_or_else(|| existing.as_ref().is_none_or(|row| row.use_local_time));
    let notify_by_browser = patch
        .notify_by_browser
        .unwrap_or_else(|| existing.as_ref().is_some_and(|row| row.notify_by_browser));
    let (permissions, granted) = match &patch.permissions {
        Some(permissions) => (json!(permissions), true),
        None => existing
            .as_ref()
            .map(|row| (row.permissions.clone(), row.permissions_granted))
            .unwrap_or_else(|| (json!([]), false)),
    };
    // A first write inserts the default row's `created_at` and `updated_at`
    // (both the epoch) and answers them; the old service spread
    // `findOrGetDefault` / `getDefault` into the new row and never bumped
    // `updated_at` (`updateMyProfileSettings.ts:28-35,66-71`,
    // `updateProfileSettings.ts:24-31`, `types.ts:25-26`). Fixture
    // `events.profiles.me_patch_success`.
    let created_at = existing
        .as_ref()
        .map(|row| row.created_at)
        .unwrap_or(DateTime::UNIX_EPOCH.naive_utc());
    sqlx::query(
        r#"INSERT INTO explore.event_profile_settings
               ("user", email, email_verified, email_verified_at, email_updated_at, use_local_time, notify_by_email,
                notify_by_browser, permissions, permissions_granted, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
           ON CONFLICT ("user") DO UPDATE SET
               email = EXCLUDED.email, email_verified = EXCLUDED.email_verified,
               email_verified_at = EXCLUDED.email_verified_at, email_updated_at = EXCLUDED.email_updated_at,
               use_local_time = EXCLUDED.use_local_time, notify_by_email = EXCLUDED.notify_by_email,
               notify_by_browser = EXCLUDED.notify_by_browser, permissions = EXCLUDED.permissions,
               permissions_granted = EXCLUDED.permissions_granted"#,
    )
    .bind(&target)
    .bind(&email)
    .bind(email_verified)
    .bind(email_verified_at)
    .bind(email_updated_at)
    .bind(use_local_time)
    .bind(notify_by_email)
    .bind(notify_by_browser)
    .bind(&permissions)
    .bind(granted)
    .bind(created_at)
    .bind(DateTime::UNIX_EPOCH.naive_utc())
    .execute(&mut *conn)
    .await?;
    let changed = patch.permissions.as_ref().is_some_and(|after| {
        let same = after.len() == previous_permissions.len()
            && after.iter().all(|p| previous_permissions.contains(p));
        !same
    });
    Ok((load_settings(conn, &target).await?, changed))
}

/// Records the Slack intent of a permission change (`Slack/utils.ts:322-358`)
/// through the moderation outbox, under the same switch as the event notices
/// (`events::notifications`: operator writes on and a webhook configured).
async fn notify_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &crate::Settings,
    settings: &Value,
) -> Result<(), ApiError> {
    if !config.operator_writes_enabled
        || config
            .events_slack_webhook
            .as_deref()
            .is_none_or(|endpoint| endpoint.trim().is_empty())
    {
        return Ok(());
    }
    let user = settings
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let permissions: Vec<String> = settings
        .get("permissions")
        .and_then(Value::as_array)
        .map(|p| {
            p.iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    super::hooks::record_profile_permissions(tx, user, &permissions).await?;
    Ok(())
}

/// Applies a `ProfileSettingsUpdate` envelope (the federation form).
async fn apply_profile_settings(
    state: &SharedState,
    signed: &catalyst_fed::Signed<ProfileSettingsUpdate>,
    signer: &str,
) -> Result<Value, ApiError> {
    let m = &signed.message;
    let target = profile_address(&m.target)?;
    let hash = signed.signature_hash();
    let payload = serde_json::to_value(m).unwrap_or_else(|_| json!({}));
    let mut tx = state.pool.begin().await?;
    fed::record_action(
        &mut tx,
        &fed::Action {
            scope: fed::Scope::Events,
            signature_hash: &hash,
            signer,
            subject_id: Some(&target),
            action_type: "ProfileSettingsUpdate",
            payload: &payload,
            signed_at: signed.signed_at,
        },
    )
    .await?;
    let (settings, changed) = save_settings(&mut tx, &target, &SettingsPatch::from(m)).await?;
    if changed {
        notify_permissions(&mut tx, &state.settings, &settings).await?;
    }
    tx.commit().await?;
    Ok(settings)
}

/// `getProfileSettings.ts:18-24`: the path's wallet, lowercased, or 404.
fn profile_address(raw: &str) -> Result<String, ApiError> {
    let lower = raw.to_ascii_lowercase();
    Address::parse(&lower)
        .map(Address::into_string)
        .map_err(|_| ApiError::not_found(format!("Not found \"{lower}\" profile: invalid address")))
}

/// `GET /api/profiles/settings`: the rows holding permissions
/// (`ProfileSettingsModel.list()`, `model.ts:47-57`), for a moderator
/// (fixtures `events.profiles.list_*`).
pub async fn list_profile_settings(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = state.signer(&headers, method.as_str(), uri.path()).await?;
    require_profile_admin(&state.pool, &user).await?;
    let users: Vec<String> = sqlx::query_scalar(
        r#"SELECT s."user" FROM explore.event_profile_settings s
           LEFT JOIN explore.moderators m ON m.address = s."user"
           WHERE m.address IS NOT NULL
              OR (s.permissions_granted AND jsonb_typeof(s.permissions) = 'array' AND jsonb_array_length(s.permissions) > 0)
           ORDER BY s."user" ASC LIMIT $1"#,
    )
    .bind(SETTINGS_LIST_CAP)
    .fetch_all(&state.pool)
    .await?;
    let mut conn = state.pool.acquire().await?;
    Ok(ok(json!(load_settings_many(&mut conn, &users).await?)))
}

/// `GET /api/profiles/me/settings`: the signer's own, with its push
/// `subscriptions` endpoints (`getAuthProfileSettings.ts:7-17`,
/// `ProfileSubscription/model.ts:144-153`; fixture `events.profiles.me_get`).
pub async fn get_my_profile_settings(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = state.signer(&headers, method.as_str(), uri.path()).await?;
    let mut conn = state.pool.acquire().await?;
    let mut settings = load_settings(&mut conn, &user).await?;
    let endpoints: Vec<String> = sqlx::query_scalar(
        "SELECT endpoint FROM explore.event_profile_subscriptions WHERE \"user\" = $1 ORDER BY created_at, endpoint",
    )
    .bind(user.to_ascii_lowercase())
    .fetch_all(&mut *conn)
    .await?;
    if let Some(obj) = settings.as_object_mut() {
        obj.insert("subscriptions".into(), json!(endpoints));
    }
    Ok(ok(settings))
}

/// `updateMyProfileSettingsSchema` (`types.ts:79-101`): the five own fields,
/// `additionalProperties: false`, `email` in ajv's `email` format; the ajv
/// 400 carries `data.{errors,messages,body}` (fixtures
/// `events.profiles.me_patch_email_invalid`, `me_patch_unknown_field`).
fn own_settings_patch(body: &Value) -> Result<SettingsPatch, ApiError> {
    validate::validate(
        &validate::UPDATE_MY_PROFILE_SETTINGS_BODY,
        body,
        Flavour::Gatsby,
    )?;
    let body = body.as_object().ok_or_else(invalid_data)?;
    let mut patch = SettingsPatch::default();
    for (key, value) in body {
        match key.as_str() {
            "email" => {
                let email = value.as_str().ok_or_else(invalid_data)?;
                if !is_email(email) {
                    return Err(invalid_data());
                }
                patch.email = Some(email.to_owned());
            }
            "email_verified" => {
                patch.email_verified = Some(value.as_bool().ok_or_else(invalid_data)?)
            }
            "use_local_time" => {
                patch.use_local_time = Some(value.as_bool().ok_or_else(invalid_data)?)
            }
            "notify_by_email" => {
                patch.notify_by_email = Some(value.as_bool().ok_or_else(invalid_data)?)
            }
            "notify_by_browser" => {
                patch.notify_by_browser = Some(value.as_bool().ok_or_else(invalid_data)?)
            }
            _ => return Err(invalid_data()),
        }
    }
    Ok(patch)
}

/// ajv-formats' `email` (`validate::Format::Email`), for the envelope path.
fn is_email(email: &str) -> bool {
    validate::Format::Email.accepts(email)
}

/// `updateProfileSettingsSchema` (`types.ts:59-77`): `permissions` only, each
/// one of the five grantable values; the ajv 400 carries
/// `data.{errors,messages,body}` (fixtures `events.profiles.patch_by_id_unknown_field`,
/// `patch_by_id_invalid_permission`).
fn permissions_patch(body: &Value) -> Result<SettingsPatch, ApiError> {
    validate::validate(
        &validate::UPDATE_PROFILE_SETTINGS_BODY,
        body,
        Flavour::Gatsby,
    )?;
    let body = body.as_object().ok_or_else(invalid_data)?;
    let mut patch = SettingsPatch::default();
    for (key, value) in body {
        if key != "permissions" {
            return Err(invalid_data());
        }
        let permissions = value
            .as_array()
            .ok_or_else(invalid_data)?
            .iter()
            .map(|p| {
                p.as_str()
                    .filter(|p| PROFILE_PERMISSIONS.contains(p))
                    .map(str::to_owned)
                    .ok_or_else(invalid_data)
            })
            .collect::<Result<Vec<_>, _>>()?;
        patch.permissions = Some(permissions);
    }
    Ok(patch)
}

/// `PATCH /api/profiles/me/settings`: the signer's own fields
/// (`updateMyProfileSettings.ts:26-78`; fixtures `events.profiles.me_patch_*`),
/// plain signed JSON or a `ProfileSettingsUpdate` envelope.
pub async fn update_my_profile_settings(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    raw_body: Bytes,
) -> Result<Json<Value>, ApiError> {
    state.user_writes()?;
    let body: Value = serde_json::from_slice(&raw_body).map_err(|_| invalid_data())?;
    if !catalyst_fed::is_envelope(&body) {
        let signer = state.signer(&headers, "patch", uri.path()).await?;
        let patch = own_settings_patch(&body)?;
        let mut tx = state.pool.begin().await?;
        super::audit::record_signed_request(
            &mut tx,
            &headers,
            &signer,
            &signer,
            "patch",
            uri.path(),
            &raw_body,
            super::audit::SignedWrite::Profile,
        )
        .await?;
        let (settings, _) = save_settings(&mut tx, &signer, &patch).await?;
        tx.commit().await?;
        return Ok(ok(settings));
    }
    let body = envelope(body)?;
    let (signed, signer) =
        fed::preflight::<ProfileSettingsUpdate>(&state, &headers, body, fed::Scope::Events).await?;
    if !signed.message.target.eq_ignore_ascii_case(&signer) {
        return Err(ApiError::forbidden(
            "me/settings only edits the signer's own profile",
        ));
    }
    if signed.message.permissions.is_some() {
        return Err(ApiError::forbidden(
            "Only a moderator may change profile permissions",
        ));
    }
    if signed
        .message
        .email
        .as_deref()
        .is_some_and(|email| !is_email(email))
    {
        return Err(invalid_data());
    }
    Ok(ok(apply_profile_settings(&state, &signed, &signer).await?))
}

#[derive(Debug, Default)]
pub(super) struct EventPermissions {
    pub edit_any: bool,
    pub approve_any: bool,
    pub approve_own: bool,
}

/// Profile permissions are read from local authority, never request metadata.
pub(super) async fn event_permissions(
    pool: &PgPool,
    signer: &str,
) -> Result<EventPermissions, ApiError> {
    let (moderator, permissions): (bool, Value) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM explore.moderators WHERE address = $1), \
         COALESCE((SELECT permissions FROM explore.event_profile_settings WHERE \"user\" = $1 AND permissions_granted), '[]'::jsonb)",
    ).bind(signer.to_ascii_lowercase()).fetch_one(pool).await?;
    let has = |permission: &str| {
        moderator
            || permissions
                .as_array()
                .is_some_and(|p| p.iter().any(|p| p.as_str() == Some(permission)))
    };
    Ok(EventPermissions {
        edit_any: has("edit_any_event"),
        approve_any: has("approve_any_event"),
        approve_own: has("approve_own_event"),
    })
}

/// `GET /api/profiles/{profile_id}/settings` (`getProfileSettings.ts:12-27`):
/// a moderator's read of the stored row,
/// `null` when there is none; a non-address is 404 (fixtures
/// `events.profiles.get_by_id_*`).
pub async fn get_profile_settings(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(profile_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let user = state.signer(&headers, method.as_str(), uri.path()).await?;
    require_profile_admin(&state.pool, &user).await?;
    let target = profile_address(&profile_id)?;
    let mut conn = state.pool.acquire().await?;
    Ok(ok(load_saved_settings(&mut conn, &target)
        .await?
        .unwrap_or(Value::Null)))
}

/// `PATCH /api/profiles/{profile_id}/settings` (`updateProfileSettings.ts:19-50`):
/// the target's `permissions`, by a moderator,
/// with the Slack notice when the set changed (fixtures
/// `events.profiles.patch_by_id_*`). The `ProfileSettingsUpdate` envelope,
/// which may also set the target's own fields, stays a moderator's.
pub async fn update_profile_settings(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(profile_id): Path<String>,
    raw_body: Bytes,
) -> Result<Json<Value>, ApiError> {
    state.operator_writes()?;
    let body: Value = serde_json::from_slice(&raw_body).map_err(|_| invalid_data())?;
    if !catalyst_fed::is_envelope(&body) {
        let signer = state.signer(&headers, "patch", uri.path()).await?;
        require_profile_admin(&state.pool, &signer).await?;
        let target = profile_address(&profile_id)?;
        let patch = permissions_patch(&body)?;
        let mut tx = state.pool.begin().await?;
        super::audit::record_signed_request(
            &mut tx,
            &headers,
            &signer,
            &target,
            "patch",
            uri.path(),
            &raw_body,
            super::audit::SignedWrite::Profile,
        )
        .await?;
        let (settings, changed) = save_settings(&mut tx, &target, &patch).await?;
        if changed {
            notify_permissions(&mut tx, &state.settings, &settings).await?;
        }
        tx.commit().await?;
        return Ok(ok(settings));
    }
    let body = envelope(body)?;
    let (signed, signer) =
        fed::preflight::<ProfileSettingsUpdate>(&state, &headers, body, fed::Scope::Events).await?;
    if !signed.message.target.eq_ignore_ascii_case(&profile_id) {
        return Err(ApiError::bad_request(
            "target does not match path profile_id",
        ));
    }
    require_moderator(&state.pool, &signer).await?;
    let m = &signed.message;
    if m.permissions.as_ref().is_some_and(|permissions| {
        permissions
            .iter()
            .any(|permission| !PROFILE_PERMISSIONS.contains(&permission.as_str()))
    }) || m.email.as_deref().is_some_and(|email| !is_email(email))
    {
        return Err(invalid_data());
    }
    Ok(ok(apply_profile_settings(&state, &signed, &signer).await?))
}

/// `GET /api/profiles/subscriptions`: the signer's rows (`routes.ts:37-40`;
/// fixture `events.subscriptions.get`).
pub async fn get_profile_subscriptions(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = state.signer(&headers, method.as_str(), uri.path()).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('user', \"user\", 'endpoint', endpoint, 'p256dh', p256dh, 'auth', auth) \
         FROM explore.event_profile_subscriptions WHERE \"user\" = $1 ORDER BY created_at, endpoint",
    )
    .bind(user.to_ascii_lowercase())
    .fetch_all(&state.pool)
    .await?;
    Ok(ok(json!(rows)))
}

/// `profileSubscriptionSchema` (`ProfileSubscription/types.ts:10-25`): the
/// three required strings and nothing else; the ajv 400 carries
/// `data.{errors,messages,body}` (fixtures `events.subscriptions.post_missing_field`,
/// `post_unknown_field`). The endpoint length bound is dcl-one's.
fn subscription_body(body: &Value) -> Result<(String, String, String), ApiError> {
    validate::validate(&validate::PROFILE_SUBSCRIPTION_BODY, body, Flavour::Gatsby)?;
    let body = body.as_object().ok_or_else(invalid_data)?;
    if body
        .keys()
        .any(|key| !["endpoint", "p256dh", "auth"].contains(&key.as_str()))
    {
        return Err(invalid_data());
    }
    let field = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(invalid_data)
    };
    let endpoint = field("endpoint")?;
    if endpoint.is_empty() || endpoint.len() > 2048 {
        return Err(invalid_data());
    }
    Ok((endpoint, field("p256dh")?, field("auth")?))
}

/// `POST /api/profiles/subscriptions` (`routes.ts:45-62`): stores the
/// browser's subscription and turns the owner's `notify_by_browser` on;
/// answers `true` (fixtures `events.subscriptions.post*`).
pub async fn create_profile_subscription(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    raw_body: Bytes,
) -> Result<Json<Value>, ApiError> {
    state.user_writes()?;
    let user = state.signer(&headers, "post", uri.path()).await?;
    let body: Value = serde_json::from_slice(&raw_body).map_err(|_| invalid_data())?;
    let (endpoint, p256dh, auth) = subscription_body(&body)?;
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO explore.event_profile_subscriptions (endpoint, \"user\", p256dh, auth) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (endpoint) DO UPDATE SET \"user\" = EXCLUDED.\"user\", p256dh = EXCLUDED.p256dh, auth = EXCLUDED.auth",
    )
    .bind(&endpoint)
    .bind(user.to_ascii_lowercase())
    .bind(&p256dh)
    .bind(&auth)
    .execute(&mut *tx)
    .await?;
    set_notify_by_browser(&mut tx, &user, true).await?;
    tx.commit().await?;
    Ok(ok(json!(true)))
}

/// `DELETE /api/profiles/subscriptions` (`routes.ts:65-73`): removes the
/// signer's rows and turns `notify_by_browser` off; answers `true`
/// (fixture `events.subscriptions.delete`).
pub async fn delete_profile_subscription(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    state.user_writes()?;
    let user = state.signer(&headers, "delete", uri.path()).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM explore.event_profile_subscriptions WHERE \"user\" = $1")
        .bind(user.to_ascii_lowercase())
        .execute(&mut *tx)
        .await?;
    set_notify_by_browser(&mut tx, &user, false).await?;
    tx.commit().await?;
    Ok(ok(json!(true)))
}

/// `ProfileSettingsModel.update({notify_by_browser}, {user})`: the stored
/// row only; a wallet without one keeps its defaults.
async fn set_notify_by_browser(
    conn: &mut PgConnection,
    user: &str,
    on: bool,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE explore.event_profile_settings SET notify_by_browser = $2 WHERE \"user\" = $1",
    )
    .bind(user.to_ascii_lowercase())
    .bind(on)
    .execute(conn)
    .await?;
    Ok(())
}

/// A validated schedule body: the eight keys of `createScheduleSchema`
/// (`Schedule/schema.ts:86-142`), each present only when the body set it;
/// other keys are ignored (no `additionalProperties: false`).
#[derive(Debug, Default)]
struct ScheduleBody {
    name: Option<String>,
    description: Option<Option<String>>,
    image: Option<Option<String>>,
    theme: Option<Option<String>>,
    background: Option<Vec<String>>,
    active: Option<bool>,
    active_since: Option<DateTime<Utc>>,
    active_until: Option<DateTime<Utc>>,
}

/// ajv's `date-time` format: RFC 3339, `T` or a space between date and time.
fn parse_date_time(value: &str) -> Option<DateTime<Utc>> {
    super::moderation::parse_rfc3339(value).or_else(|| {
        let (date, time) = value.split_once(' ')?;
        super::moderation::parse_rfc3339(&format!("{date}T{time}"))
    })
}

/// `createScheduleValidator` / `updateScheduleValidator`
/// (`Schedule/routes.ts:21-24`, `schema.ts:86-147`): `create` adds the
/// `required` triple; the ajv 400 carries `data.{errors,messages,body}`
/// (fixtures `events.schedules.post_*`, `patch_invalid_theme`, `patch_empty_body`).
fn schedule_body(body: &Value, create: bool) -> Result<ScheduleBody, ApiError> {
    let schema = if create {
        &validate::CREATE_SCHEDULE_BODY
    } else {
        &validate::UPDATE_SCHEDULE_BODY
    };
    validate::validate(schema, body, Flavour::Gatsby)?;
    let body = body.as_object().ok_or_else(invalid_data)?;
    let mut out = ScheduleBody::default();
    let text = |value: &Value, max: usize| -> Result<String, ApiError> {
        value
            .as_str()
            .filter(|s| s.chars().count() <= max)
            .map(str::to_owned)
            .ok_or_else(invalid_data)
    };
    if let Some(value) = body.get("name") {
        out.name = Some(text(value, 50)?);
    }
    if let Some(value) = body.get("description") {
        out.description = Some(Some(text(value, 255)?));
    }
    if let Some(value) = body.get("image") {
        out.image = Some(match value {
            Value::Null => None,
            Value::String(image) => {
                check_schedule_assets(Some(image), None)?;
                Some(image.clone())
            }
            _ => return Err(invalid_data()),
        });
    }
    if let Some(value) = body.get("theme") {
        out.theme = Some(match value {
            Value::Null => None,
            Value::String(theme) => {
                check_schedule_assets(None, Some(theme))?;
                Some(theme.clone())
            }
            _ => return Err(invalid_data()),
        });
    }
    if let Some(value) = body.get("background") {
        out.background = Some(
            value
                .as_array()
                .ok_or_else(invalid_data)?
                .iter()
                .map(|color| text(color, 255))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    if let Some(value) = body.get("active") {
        out.active = Some(value.as_bool().ok_or_else(invalid_data)?);
    }
    for (key, slot) in [
        ("active_since", &mut out.active_since),
        ("active_until", &mut out.active_until),
    ] {
        if let Some(value) = body.get(key) {
            *slot = Some(
                value
                    .as_str()
                    .and_then(parse_date_time)
                    .ok_or_else(invalid_data)?,
            );
        }
    }
    Ok(out)
}

async fn apply_schedule(
    state: &SharedState,
    headers: &HeaderMap,
    body: Value,
    raw_body: &[u8],
    path_id: Option<String>,
    method: &str,
    path: &str,
) -> Result<Json<Value>, ApiError> {
    state.operator_writes()?;
    if !catalyst_fed::is_envelope(&body) {
        return apply_legacy_schedule(state, headers, body, raw_body, path_id, method, path).await;
    }
    let body = envelope(body)?;
    let (signed, signer) =
        fed::preflight::<ScheduleUpsert>(state, headers, body, fed::Scope::Events).await?;
    if let Some(id) = &path_id
        && signed.message.schedule_id.as_ref() != Some(id)
    {
        return Err(ApiError::bad_request(
            "schedule_id in body does not match path",
        ));
    }
    require_schedule_editor(&state.pool, &signer).await?;
    let m = &signed.message;
    check_schedule_assets(m.image.as_deref(), m.theme.as_deref())?;
    let id = m
        .schedule_id
        .clone()
        .unwrap_or_else(|| fed::schedule_id(&signer, &m.name, &signed.nonce));
    let hash = signed.signature_hash();
    let payload = serde_json::to_value(m).unwrap_or_else(|_| json!({}));
    let at = |secs: i64| DateTime::from_timestamp(secs, 0).map(|t| t.naive_utc());
    let now = Utc::now().naive_utc();
    let mut tx = state.pool.begin().await?;
    fed::record_action(
        &mut tx,
        &fed::Action {
            scope: fed::Scope::Events,
            signature_hash: &hash,
            signer: &signer,
            subject_id: Some(&id),
            action_type: "ScheduleUpsert",
            payload: &payload,
            signed_at: signed.signed_at,
        },
    )
    .await?;
    sqlx::query(
        r#"INSERT INTO explore.schedules_local (id, name, description, image, theme, background, active_since, active_until, active, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)
           ON CONFLICT (id) DO UPDATE SET
               name = EXCLUDED.name, description = EXCLUDED.description, image = EXCLUDED.image, theme = EXCLUDED.theme,
               background = EXCLUDED.background, active_since = EXCLUDED.active_since, active_until = EXCLUDED.active_until,
               active = EXCLUDED.active, updated_at = EXCLUDED.updated_at"#,
    )
    .bind(&id)
    .bind(&m.name)
    .bind(m.description.as_deref())
    .bind(m.image.as_deref())
    .bind(m.theme.as_deref())
    .bind(json!(m.background))
    .bind(at(m.active_since))
    .bind(at(m.active_until))
    .bind(m.active)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    state.schedules.invalidate();
    let schedule = state
        .schedules
        .get(&id)
        .await?
        .ok_or_else(|| ApiError::internal("schedule vanished after write"))?;
    Ok(Json(json!(ApiData::ok(schedule))))
}

/// The stored row as `pick(schedule, keys)` (`Schedule/routes.ts:91`).
fn schedule_keys(schedule: &ScheduleRecord) -> Map<String, Value> {
    let mut out = Map::new();
    let stamp = |at: Option<DateTime<Utc>>| {
        json!(at.map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true)))
    };
    out.insert("name".into(), json!(schedule.name));
    out.insert("description".into(), json!(schedule.description));
    out.insert("image".into(), json!(schedule.image));
    out.insert("theme".into(), json!(schedule.theme));
    out.insert("background".into(), json!(schedule.background));
    out.insert("active".into(), json!(schedule.active));
    out.insert("active_since".into(), stamp(schedule.active_since));
    out.insert("active_until".into(), stamp(schedule.active_until));
    out
}

/// `Object.assign(pick(schedule, keys), pick(data, keys))` (`routes.ts:90-93`).
fn merge_schedule(stored: &ScheduleRecord, patch: &ScheduleBody) -> Map<String, Value> {
    let mut merged = schedule_keys(stored);
    if let Some(name) = &patch.name {
        merged.insert("name".into(), json!(name));
    }
    if let Some(description) = &patch.description {
        merged.insert("description".into(), json!(description));
    }
    if let Some(image) = &patch.image {
        merged.insert("image".into(), json!(image));
    }
    if let Some(theme) = &patch.theme {
        merged.insert("theme".into(), json!(theme));
    }
    if let Some(background) = &patch.background {
        merged.insert("background".into(), json!(background));
    }
    if let Some(active) = patch.active {
        merged.insert("active".into(), json!(active));
    }
    for (key, value) in [
        ("active_since", patch.active_since),
        ("active_until", patch.active_until),
    ] {
        if let Some(at) = value {
            merged.insert(
                key.into(),
                json!(at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            );
        }
    }
    merged
}

/// The signed-fetch JSON contract of `Schedule/routes.ts:48-97`, in its
/// order: the body is validated (400) before the permission check (403); a
/// PATCH of an absent schedule is 404 `Schedule "<id>" not found`. A create
/// stores `theme: null` whatever the body said (`routes.ts:63`) and answers
/// the whole record; a PATCH rewrites the merged eight keys and answers
/// only them (`routes.ts:90-96`).
async fn apply_legacy_schedule(
    state: &SharedState,
    headers: &HeaderMap,
    body: Value,
    raw_body: &[u8],
    path_id: Option<String>,
    method: &str,
    path: &str,
) -> Result<Json<Value>, ApiError> {
    let signer = state.signer(headers, method, path).await?;
    let patch = schedule_body(&body, path_id.is_none())?;
    require_schedule_editor(&state.pool, &signer).await?;
    let mut tx = state.pool.begin().await?;
    let (id, existing) = match path_id {
        Some(id) => {
            let row: Option<Value> = sqlx::query_scalar(
                "SELECT to_jsonb(s) FROM explore.schedules_local s WHERE id = $1 FOR UPDATE",
            )
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
            let row =
                row.ok_or_else(|| ApiError::not_found(format!("Schedule \"{id}\" not found")))?;
            (id, Some(stored_schedule(row)?))
        }
        None => (uuid::Uuid::new_v4().to_string(), None),
    };
    super::audit::record_signed_request(
        &mut tx,
        headers,
        &signer,
        &id,
        method,
        path,
        raw_body,
        super::audit::SignedWrite::Schedule,
    )
    .await?;
    let answer = match existing {
        None => {
            sqlx::query(
                "INSERT INTO explore.schedules_local (id,name,description,image,theme,background,active_since,active_until,active) \
                 VALUES ($1,$2,$3,$4,NULL,$5,$6,$7,$8)",
            )
            .bind(&id)
            .bind(patch.name.as_deref().unwrap_or_default())
            .bind(patch.description.clone().flatten())
            .bind(patch.image.clone().flatten())
            .bind(json!(patch.background.clone().unwrap_or_default()))
            .bind(patch.active_since.map(|at| at.naive_utc()))
            .bind(patch.active_until.map(|at| at.naive_utc()))
            .bind(patch.active.unwrap_or(true))
            .execute(&mut *tx)
            .await?;
            None
        }
        Some(stored) => {
            let merged = merge_schedule(&stored, &patch);
            let text = |key: &str| merged.get(key).and_then(Value::as_str).map(str::to_owned);
            let at = |key: &str| {
                text(key)
                    .and_then(|s| parse_date_time(&s))
                    .map(|at| at.naive_utc())
            };
            sqlx::query(
                "UPDATE explore.schedules_local SET name=$2,description=$3,image=$4,theme=$5,background=$6, \
                 active_since=$7,active_until=$8,active=$9,updated_at=timezone('UTC',now()) WHERE id=$1",
            )
            .bind(&id)
            .bind(text("name").unwrap_or_default())
            .bind(text("description"))
            .bind(text("image"))
            .bind(text("theme"))
            .bind(merged.get("background").cloned().unwrap_or_else(|| json!([])))
            .bind(at("active_since"))
            .bind(at("active_until"))
            .bind(merged.get("active").and_then(Value::as_bool).unwrap_or(true))
            .execute(&mut *tx)
            .await?;
            Some(Value::Object(merged))
        }
    };
    tx.commit().await?;
    state.schedules.invalidate();
    match answer {
        Some(merged) => Ok(ok(merged)),
        None => {
            let schedule = state
                .schedules
                .get(&id)
                .await?
                .ok_or_else(|| ApiError::internal("schedule vanished after write"))?;
            Ok(Json(json!(ApiData::ok(schedule))))
        }
    }
}

/// A `to_jsonb(schedules_local)` row as the record the reads answer.
fn stored_schedule(row: Value) -> Result<ScheduleRecord, ApiError> {
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    let at = |key: &str| {
        text(key).and_then(|s| {
            NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|t| t.and_utc())
        })
    };
    Ok(ScheduleRecord {
        id: text("id").ok_or_else(|| ApiError::internal("schedule row without id"))?,
        name: text("name").unwrap_or_default(),
        description: text("description"),
        image: text("image"),
        theme: text("theme"),
        background: row
            .get("background")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        active_since: at("active_since"),
        active_until: at("active_until"),
        active: row.get("active").and_then(Value::as_bool).unwrap_or(true),
        created_at: at("created_at"),
        updated_at: at("updated_at"),
    })
}

/// `POST /api/schedules`: a signed body (`Schedule/routes.ts:48-71`) or a
/// `ScheduleUpsert` envelope (an id minted when absent).
pub async fn create_schedule(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    raw_body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = serde_json::from_slice(&raw_body).map_err(|_| invalid_data())?;
    apply_schedule(&state, &headers, body, &raw_body, None, "post", uri.path()).await
}

/// `PATCH /api/schedules/{schedule_id}` (`Schedule/routes.ts:73-97`); the
/// envelope must name the same id.
pub async fn patch_schedule(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(schedule_id): Path<String>,
    raw_body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = serde_json::from_slice(&raw_body).map_err(|_| invalid_data())?;
    apply_schedule(
        &state,
        &headers,
        body,
        &raw_body,
        Some(schedule_id),
        "patch",
        uri.path(),
    )
    .await
}

#[cfg(test)]
pub(crate) mod test_support {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    /// A profile deployment this node holds for `address`, its first avatar
    /// carrying `avatar` (the `{"v": ...}`-wrapped metadata the deployers store).
    /// A wallet that already has a profile keeps it (the shared test wallet is
    /// seeded by several tests); the answer is the entity id inserted.
    pub async fn deploy_profile(pool: &PgPool, address: &str, avatar: Value) -> String {
        let address = address.to_ascii_lowercase();
        let entity_id = format!("bafyprofile{}", uuid::Uuid::new_v4().simple());
        let metadata =
            json!({"v": {"avatars": [{"name": "Tester", "userId": address, "avatar": avatar}]}});
        sqlx::query(
            "INSERT INTO content.deployments (deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) \
             VALUES ($1,'v3','profile',$2,$3::json,timezone('UTC',now()),ARRAY[$1],timezone('UTC',now()),'[]')",
        )
        .bind(&address)
        .bind(&entity_id)
        .bind(metadata)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO content.active_pointers (pointer,entity_id,entity_type) VALUES ($1,$2,'profile') \
             ON CONFLICT (pointer) DO NOTHING",
        )
        .bind(&address)
        .bind(&entity_id)
        .execute(pool)
        .await
        .unwrap();
        entity_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    type Wallet = alloy_signer_local::PrivateKeySigner;

    fn app(pool: &PgPool) -> axum::Router {
        app_with_webhook(pool, None)
    }

    fn app_with_webhook(pool: &PgPool, webhook: Option<&str>) -> axum::Router {
        crate::router(Arc::new(crate::ExploreState::new(
            pool.clone(),
            crate::Settings {
                user_writes_enabled: true,
                operator_writes_enabled: true,
                events_slack_webhook: webhook.map(str::to_owned),
                ..crate::Settings::for_tests()
            },
            None,
        )))
    }

    fn address(wallet: &Wallet) -> String {
        format!("{:#x}", wallet.address())
    }

    async fn moderator(pool: &PgPool) -> Wallet {
        let wallet = Wallet::random();
        sqlx::query("INSERT INTO explore.moderators (address,added_at) VALUES ($1,0)")
            .bind(address(&wallet))
            .execute(pool)
            .await
            .unwrap();
        wallet
    }

    /// A plain signed-fetch call with `metadata` (`{}`: no body hash, no nonce).
    async fn call_with(
        app: &axum::Router,
        wallet: &Wallet,
        method: &str,
        path: &str,
        body: Option<&Value>,
        metadata: &str,
    ) -> (StatusCode, Value) {
        let headers = crate::shared::signed_fetch::test_support::signed_headers_by(
            wallet,
            method,
            path,
            metadata,
            true,
            &Utc::now().timestamp_millis().to_string(),
        )
        .await;
        let mut request = Request::builder()
            .method(method.to_uppercase().as_str())
            .uri(path)
            .header("content-type", "application/json");
        for (key, value) in headers.iter() {
            request = request.header(key, value);
        }
        let body = body.map(Value::to_string).unwrap_or_default();
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn call(
        app: &axum::Router,
        wallet: &Wallet,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
        call_with(app, wallet, method, path, body, "{}").await
    }

    fn with_nonce(body: &Value) -> String {
        json!({"body_sha256":hex::encode(Sha256::digest(body.to_string().as_bytes())),"nonce":uuid::Uuid::new_v4().to_string()}).to_string()
    }

    /// `listProfileSettings.ts`, `getProfileSettings.ts`, `updateProfileSettings.ts`:
    /// a moderator ("isAdmin") reads and administers other profiles; the list
    /// holds the rows with permissions; an `edit_any_profile` grantee is listed
    /// but stays 403 like a plain user, who cannot grant itself through `/me`.
    #[tokio::test]
    async fn profile_administration_is_moderator_only_and_lists_the_granted_rows() {
        // Queues moderation-outbox rows: serialized with the outbox tests.
        let _outbox = crate::moderation_hooks::TEST_LOCK.lock().await;
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let moderator = moderator(&pool).await;
        let editor = Wallet::random();
        let plain = Wallet::random();
        let target = Wallet::random();
        let app = app_with_webhook(&pool, Some("https://hooks.example.org/test"));
        let (status, settings) =
            call(&app, &moderator, "get", "/api/profiles/me/settings", None).await;
        assert_eq!(status, StatusCode::OK, "{settings}");
        assert_eq!(settings["data"]["permissions"], json!(PROFILE_PERMISSIONS));
        assert_eq!(settings["data"]["subscriptions"], json!([]));
        assert!(settings["data"]["email_verified_at"].is_null());
        assert!(settings["data"]["email_updated_at"].is_null());
        assert_eq!(settings["data"]["created_at"], "1970-01-01T00:00:00.000Z");
        // `/me` never takes `permissions` (`types.ts:79-101`, additionalProperties).
        let (status, body) = call(
            &app,
            &editor,
            "patch",
            "/api/profiles/me/settings",
            Some(&json!({"permissions":["edit_any_profile"]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], INVALID_DATA);
        for (wallet, path) in [
            (&plain, "/api/profiles/settings".to_owned()),
            (
                &plain,
                format!("/api/profiles/{}/settings", address(&target)),
            ),
            (&editor, "/api/profiles/settings".to_owned()),
        ] {
            let (status, body) = call(&app, wallet, "get", &path, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(body["error"], "Forbidden");
        }
        // A moderator grants; the grant is live at once and lists the row.
        let editor_path = format!("/api/profiles/{}/settings", address(&editor));
        let (status, body) = call(
            &app,
            &moderator,
            "patch",
            &editor_path,
            Some(&json!({"permissions":["edit_any_profile","edit_any_event"]})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            body["data"]["permissions"],
            json!(["edit_any_profile", "edit_any_event"])
        );
        // The grant is listed but delegates nothing: the grantee stays 403 on
        // the list, the read and the write of other profiles. The old service
        // admits it (`ProfileSettings/utils.ts:4-11`): accepted entries
        // `events.profiles.*_edit_any_profile`.
        let target_lower = format!("/api/profiles/{}/settings", address(&target));
        for (verb, path, body) in [
            ("get", "/api/profiles/settings".to_owned(), None),
            ("get", target_lower.clone(), None),
            (
                "patch",
                target_lower,
                Some(json!({"permissions":["edit_any_schedule"]})),
            ),
        ] {
            let (status, answer) = call(&app, &editor, verb, &path, body.as_ref()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
            assert_eq!(answer["error"], "Forbidden");
        }
        let (status, body) = call(&app, &moderator, "get", "/api/profiles/settings", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listed: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["user"].as_str().unwrap())
            .collect();
        assert!(listed.contains(&address(&editor).as_str()), "{body}");
        assert!(
            !listed.contains(&address(&moderator).as_str()),
            "a moderator without a row is not a row: {body}"
        );
        // `getProfileSettings.ts:25`: no row is `data: null`; the address is lowercased.
        let target_path = format!(
            "/api/profiles/{}/settings",
            address(&target).to_uppercase().replace("0X", "0x")
        );
        let (status, body) = call(&app, &moderator, "get", &target_path, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["data"].is_null(), "{body}");
        let (status, body) = call(
            &app,
            &moderator,
            "get",
            "/api/profiles/NotAnAddress/settings",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(
            body["error"],
            "Not found \"notanaddress\" profile: invalid address"
        );
        // `updateProfileSettings.ts:19-34`: a moderator's grant creates
        // the row from the defaults and queues the Slack notice.
        let (status, body) = call(
            &app,
            &moderator,
            "patch",
            &target_path,
            Some(&json!({"permissions":["edit_any_schedule"]})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"]["user"], address(&target));
        assert_eq!(body["data"]["permissions"], json!(["edit_any_schedule"]));
        assert_eq!(body["data"]["use_local_time"], true);
        // The same set again changes nothing and queues no second notice;
        // an invalid value and a non-permission key are 400.
        let (status, _) = call(
            &app,
            &moderator,
            "patch",
            &target_path,
            Some(&json!({"permissions":["edit_any_schedule"]})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        for body in [
            json!({"permissions":["test_any_notification"]}),
            json!({"email":"x@example.com"}),
        ] {
            let (status, answer) = call(&app, &moderator, "patch", &target_path, Some(&body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
            assert_eq!(answer["error"], INVALID_DATA);
        }
        let (status, body) = call(
            &app,
            &plain,
            "patch",
            &target_path,
            Some(&json!({"permissions":[]})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        // The wallet is the row's subject; a permission notice is about no
        // event (explore-change-6).
        let hooks: Vec<(String, Value)> = sqlx::query_as(
            "SELECT subject, payload FROM explore.moderation_hooks WHERE channel='events' AND kind='profile_permissions' AND subject IN ($1,$2) ORDER BY created_at",
        )
        .bind(address(&editor))
        .bind(address(&target))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(hooks.len(), 2, "{hooks:?}");
        assert_eq!(hooks[1].0, address(&target));
        assert_eq!(hooks[1].1["permissions"], json!(["edit_any_schedule"]));
        let (status, body) = call(
            &app,
            &moderator,
            "patch",
            &target_path,
            Some(&json!({"permissions":[]})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"]["permissions"], json!([]));
        let lost: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE channel='events' AND kind='profile_permissions' AND subject=$1 AND payload->'permissions'='[]'::jsonb",
        )
        .bind(address(&target))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(lost, 1);
        sqlx::query("DELETE FROM explore.moderation_hooks WHERE channel='events' AND kind='profile_permissions' AND subject IN ($1,$2)")
            .bind(address(&editor)).bind(address(&target)).execute(&pool).await.unwrap();
    }

    /// `updateMyProfileSettings.ts:26-78` and `types.ts:79-101`: the body's
    /// `email_verified: true` is discarded, a changed email is unverified and
    /// stamped, `email_verified: false` un-verifies the same email, and the
    /// ajv `email` format, `additionalProperties` and `type` rules are 400.
    #[tokio::test]
    async fn own_settings_follow_the_old_email_lifecycle_and_schema() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let wallet = Wallet::random();
        let app = app(&pool);
        let me = "/api/profiles/me/settings";
        let (status, body) = call(
            &app,
            &wallet,
            "patch",
            me,
            Some(&json!({"use_local_time":false,"notify_by_browser":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"]["use_local_time"], false);
        assert_eq!(body["data"]["notify_by_browser"], true);
        assert!(body["data"].get("subscriptions").is_none());
        // The first write keeps the default row's epoch
        // (`updateMyProfileSettings.ts:28-35,66-71`, `types.ts:25`).
        assert_eq!(body["data"]["created_at"], "1970-01-01T00:00:00.000Z");
        let (status, body) = call(
            &app,
            &wallet,
            "patch",
            me,
            Some(&json!({"email":"new@example.com","notify_by_email":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"]["created_at"], "1970-01-01T00:00:00.000Z");
        assert_eq!(body["data"]["email"], "new@example.com");
        assert_eq!(body["data"]["email_verified"], false);
        assert_eq!(body["data"]["notify_by_email"], false);
        assert!(body["data"]["email_updated_at"].is_string(), "{body}");
        assert!(body["data"]["email_verified_at"].is_null());
        let first_update = body["data"]["email_updated_at"].clone();
        // Pretend a verification happened; `email_verified: true` in a body
        // is discarded (`updateMyProfileSettings.ts:36`).
        sqlx::query("UPDATE explore.event_profile_settings SET email_verified=true, email_verified_at=timezone('UTC',now()), notify_by_email=true WHERE \"user\"=$1")
            .bind(address(&wallet)).execute(&pool).await.unwrap();
        let (status, body) = call(
            &app,
            &wallet,
            "patch",
            me,
            Some(&json!({"email_verified":true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"]["email_verified"], true);
        assert_eq!(body["data"]["notify_by_email"], true);
        assert_eq!(body["data"]["email_updated_at"], first_update);
        // The same email with `email_verified: false` un-verifies it (lines 53-63).
        let (status, body) = call(
            &app,
            &wallet,
            "patch",
            me,
            Some(&json!({"email_verified":false})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"]["email_verified"], false);
        assert!(body["data"]["email_verified_at"].is_null());
        assert_eq!(body["data"]["notify_by_email"], false);
        assert_ne!(body["data"]["email_updated_at"], first_update);
        for body in [
            json!({"email":"not-an-email"}),
            json!({"email":null}),
            json!({"foo":1}),
            json!({"use_local_time":"yes"}),
            json!([]),
        ] {
            let (status, answer) = call(&app, &wallet, "patch", me, Some(&body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {answer}");
            assert_eq!(answer["error"], INVALID_DATA);
        }
        assert!(is_email("a.b+c@sub.example.org"));
        assert!(!is_email("a@b"));
        assert!(!is_email("@example.org"));
    }

    /// A plain signed fetch without `x-identity-metadata`
    /// writes; a signed `body_sha256` must match (400) and a signed `nonce`
    /// is replay-protected (409); every write is audited once.
    #[tokio::test]
    async fn signed_metadata_is_optional_and_binding_when_present() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let moderator = moderator(&pool).await;
        let target = Wallet::random();
        let app = axum::Router::new().nest("/events", app(&pool));
        let path = format!("/events/api/profiles/{}/settings", address(&target));
        let body = json!({"permissions":["edit_any_event"]});
        let changed = json!({"permissions":["edit_any_profile"]});
        let bound = with_nonce(&body);
        let (status, answer) =
            call_with(&app, &moderator, "patch", &path, Some(&changed), &bound).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert_eq!(
            answer["error"],
            "Profile body must match signed body_sha256"
        );
        let (status, _) = call_with(&app, &moderator, "patch", &path, Some(&body), &bound).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, answer) =
            call_with(&app, &moderator, "patch", &path, Some(&body), &bound).await;
        assert_eq!(status, StatusCode::CONFLICT, "{answer}");
        let (status, answer) = call_with(
            &app,
            &moderator,
            "patch",
            &path,
            Some(&body),
            &json!({"nonce":"not-a-uuid"}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        // No metadata at all (the upstream client): accepted, twice.
        for _ in 0..2 {
            let (status, answer) = call(&app, &moderator, "patch", &path, Some(&changed)).await;
            assert_eq!(status, StatusCode::CREATED, "{answer}");
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM explore.signed_actions WHERE signer=$1 AND action_type='ProfileSettingsUpdate'")
            .bind(address(&moderator)).fetch_one(&pool).await.unwrap();
        assert!(count >= 2, "{count}");
        let schedules = "/events/api/schedules";
        let schedule = json!({"name":"Plain","active_since":"2031-01-01T00:00:00.000Z","active_until":"2031-02-01T00:00:00.000Z"});
        let (status, answer) = call(&app, &moderator, "post", schedules, Some(&schedule)).await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let bound = with_nonce(&schedule);
        let (status, _) =
            call_with(&app, &moderator, "post", schedules, Some(&schedule), &bound).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, answer) =
            call_with(&app, &moderator, "post", schedules, Some(&schedule), &bound).await;
        assert_eq!(status, StatusCode::CONFLICT, "{answer}");
        assert_eq!(answer["error"], "Schedule request already applied");
    }

    /// `Schedule/routes.ts:48-97`, `schema.ts:86-147`: the required triple,
    /// the `theme` enum, `image` as a URI, ignored unknown keys, `theme: null`
    /// on create, the merged eight keys on PATCH, the 404 text, and
    /// validation (400) before permission (403).
    #[tokio::test]
    async fn schedule_writes_follow_the_old_schema_order_and_answers() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let editor = Wallet::random();
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\", permissions, permissions_granted) VALUES ($1,'[\"edit_any_schedule\"]',true)")
            .bind(address(&editor)).execute(&pool).await.unwrap();
        let plain = Wallet::random();
        let app = app(&pool);
        let valid = json!({"name":"Season","active_since":"2031-01-01T00:00:00.000Z","active_until":"2031-02-01T00:00:00.000Z","description":"d","image":"https://example.com/i.png","background":["#fff"],"theme":"mvfw_2023","foo":1});
        let (status, body) = call(&app, &editor, "post", "/api/schedules", Some(&valid)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let created = &body["data"];
        let id = created["id"].as_str().unwrap().to_owned();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert!(
            created["theme"].is_null(),
            "theme is ignored on create: {body}"
        );
        assert_eq!(created["background"], json!(["#fff"]));
        assert_eq!(created["active"], true);
        assert_eq!(created["description"], "d");
        assert_eq!(created["active_since"], "2031-01-01T00:00:00.000Z");
        assert!(created.get("foo").is_none());
        // A plain user with a missing name: 400 before 403 (`routes.ts:51-56`).
        let (status, body) = call(
            &app,
            &plain,
            "post",
            "/api/schedules",
            Some(&json!({"active_since":"2031-01-01T00:00:00.000Z","active_until":"2031-02-01T00:00:00.000Z"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], INVALID_DATA);
        let (status, body) = call(
            &app,
            &plain,
            "post",
            "/api/schedules",
            Some(&json!({"name":"Season","active_since":"2031-01-01T00:00:00.000Z","active_until":"2031-02-01T00:00:00.000Z"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "Forbidden");
        for invalid in [
            json!({"name":"Season","active_until":"2031-02-01T00:00:00.000Z"}),
            json!({"name":"Season","active_since":"2031-01-01T00:00:00.000Z"}),
            json!({"name":"x".repeat(51),"active_since":"2031-01-01T00:00:00.000Z","active_until":"2031-02-01T00:00:00.000Z"}),
            json!({"name":"Season","active_since":"2031-01-01T00:00:00.000Z","active_until":"2031-02-01T00:00:00.000Z","image":"not a uri"}),
            json!({"name":"Season","active_since":"2031-01-01T00:00:00.000Z","active_until":"2031-02-01T00:00:00.000Z","theme":"other"}),
            json!({"name":"Season","active_since":"soon","active_until":"2031-02-01T00:00:00.000Z"}),
        ] {
            let (status, body) =
                call(&app, &editor, "post", "/api/schedules", Some(&invalid)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid} -> {body}");
            assert_eq!(body["error"], INVALID_DATA);
        }
        // PATCH: the merged eight keys, theme persisted, `{}` accepted.
        let path = format!("/api/schedules/{id}");
        let (status, body) = call(
            &app,
            &editor,
            "patch",
            &path,
            Some(&json!({"name":"Renamed","theme":"pride_2023"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let mut keys: Vec<&str> = body["data"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = SCHEDULE_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(body["data"]["name"], "Renamed");
        assert_eq!(body["data"]["theme"], "pride_2023");
        assert_eq!(body["data"]["description"], "d");
        assert_eq!(body["data"]["active_until"], "2031-02-01T00:00:00.000Z");
        let (status, body) = call(&app, &editor, "patch", &path, Some(&json!({}))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["data"]["name"], "Renamed");
        let (status, body) = call(&app, &editor, "get", &path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["theme"], "pride_2023");
        let (status, body) = call(
            &app,
            &editor,
            "patch",
            &path,
            Some(&json!({"theme":"nope"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = call(&app, &plain, "patch", &path, Some(&json!({"name":"x"}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let missing = uuid::Uuid::new_v4().to_string();
        let (status, body) = call(
            &app,
            &editor,
            "patch",
            &format!("/api/schedules/{missing}"),
            Some(&json!({"name":"x"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], format!("Schedule \"{missing}\" not found"));
    }

    /// Both schedule encodings share the granted authority, revoked at once.
    #[tokio::test]
    async fn schedule_encodings_share_granted_authority_and_immediate_revocation() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let key = Wallet::random();
        let signer = address(&key);
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\", permissions, permissions_granted) VALUES ($1,'[\"edit_any_schedule\"]',false)").bind(&signer).execute(&pool).await.unwrap();
        let existing = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO explore.schedules_local(id,name) VALUES ($1,'Existing schedule')")
            .bind(&existing)
            .execute(&pool)
            .await
            .unwrap();
        let app = app(&pool);
        async fn write(
            app: &axum::Router,
            key: &Wallet,
            envelope: bool,
            id: Option<&str>,
            name: &str,
        ) -> StatusCode {
            let now = Utc::now().timestamp();
            let (method, path) = match id {
                Some(id) => ("patch", format!("/api/schedules/{id}")),
                None => ("post", "/api/schedules".to_owned()),
            };
            let body = if envelope {
                let message: ScheduleUpsert = serde_json::from_value(json!({"schedule_id":id,"name":name,"active_since":now,"active_until":now+3600,"signed_at":now})).unwrap();
                serde_json::to_value(catalyst_fed::test_support::sign(
                    key,
                    catalyst_fed::domains::events(),
                    message,
                    now,
                ))
                .unwrap()
            } else {
                json!({"name":name,"active_since":"2031-01-01T00:00:00Z","active_until":"2031-02-01T00:00:00Z"})
            };
            call_with(app, key, method, &path, Some(&body), &with_nonce(&body))
                .await
                .0
        }
        for (stage, granted, moderator) in [
            ("untrusted", false, false),
            ("granted", true, false),
            ("revoked", false, false),
            ("moderator", false, true),
        ] {
            sqlx::query("UPDATE explore.event_profile_settings SET permissions_granted=$2 WHERE \"user\"=$1").bind(&signer).bind(granted).execute(&pool).await.unwrap();
            if moderator {
                sqlx::query("INSERT INTO explore.moderators(address,added_at) VALUES($1,0)")
                    .bind(&signer)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            let expected = if granted || moderator {
                StatusCode::CREATED
            } else {
                StatusCode::FORBIDDEN
            };
            for envelope in [false, true] {
                assert_eq!(
                    write(&app, &key, envelope, None, stage).await,
                    expected,
                    "{stage}, envelope={envelope}, create"
                );
                assert_eq!(
                    write(&app, &key, envelope, Some(&existing), stage).await,
                    expected,
                    "{stage}, envelope={envelope}, patch"
                );
            }
            let stored: String =
                sqlx::query_scalar("SELECT name FROM explore.schedules_local WHERE id=$1")
                    .bind(&existing)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                stored,
                match stage {
                    "untrusted" => "Existing schedule",
                    "revoked" => "granted",
                    _ => stage,
                }
            );
        }
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM explore.signed_actions WHERE signer=$1 AND action_type='ScheduleUpsert'").bind(&signer).fetch_one(&pool).await.unwrap();
        assert_eq!(audits, 8, "only authorized creates and patches are applied");
    }

    /// The signed envelope passes the asset checks of the JSON body: an
    /// image URL the website must never render or a theme it does not know
    /// is refused and nothing is stored.
    #[tokio::test]
    async fn a_signed_schedule_envelope_passes_the_asset_checks() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let key = Wallet::random();
        let signer = address(&key);
        sqlx::query("INSERT INTO explore.event_profile_settings (\"user\", permissions, permissions_granted) VALUES ($1,'[\"edit_any_schedule\"]',true)")
            .bind(&signer)
            .execute(&pool)
            .await
            .unwrap();
        let app = app(&pool);
        let name = format!("Signed {}", uuid::Uuid::new_v4());
        let now = Utc::now().timestamp();
        for (field, value, expected) in [
            ("image", "javascript:alert(1)", StatusCode::BAD_REQUEST),
            ("theme", "nope", StatusCode::BAD_REQUEST),
            (
                "image",
                "https://cdn.example/poster.png",
                StatusCode::CREATED,
            ),
        ] {
            let mut message =
                json!({"name":name,"active_since":now,"active_until":now+3600,"signed_at":now});
            message[field] = json!(value);
            let message: ScheduleUpsert = serde_json::from_value(message).unwrap();
            let body = serde_json::to_value(catalyst_fed::test_support::sign(
                &key,
                catalyst_fed::domains::events(),
                message,
                now,
            ))
            .unwrap();
            let (status, reply) = call_with(
                &app,
                &key,
                "post",
                "/api/schedules",
                Some(&body),
                &with_nonce(&body),
            )
            .await;
            assert_eq!(status, expected, "{field}={value}: {reply}");
        }
        let stored: i64 =
            sqlx::query_scalar("SELECT count(*) FROM explore.schedules_local WHERE name=$1")
                .bind(&name)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, 1, "only the safe image is stored");
    }

    /// `ProfileSubscription/routes.ts:17-73`: the three verbs, the schema, and
    /// `notify_by_browser` following the rows.
    #[tokio::test]
    async fn subscriptions_keep_rows_and_the_browser_flag_in_step() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let wallet = Wallet::random();
        let app = app(&pool);
        let path = "/api/profiles/subscriptions";
        let (status, body) = call(&app, &wallet, "get", path, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"ok":true,"data":[]}));
        for invalid in [
            json!({"endpoint":"https://push.example/abc"}),
            json!({"endpoint":"e","p256dh":"k","auth":"a","foo":1}),
            json!({"endpoint":1,"p256dh":"k","auth":"a"}),
        ] {
            let (status, body) = call(&app, &wallet, "post", path, Some(&invalid)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid} -> {body}");
            assert_eq!(body["error"], INVALID_DATA);
        }
        // Without a settings row the flag has nothing to update (model `update`).
        let subscription =
            json!({"endpoint":"https://push.example/abc","p256dh":"key","auth":"secret"});
        let (status, body) = call(&app, &wallet, "post", path, Some(&subscription)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body, json!({"ok":true,"data":true}));
        let (status, body) = call(&app, &wallet, "get", "/api/profiles/me/settings", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["notify_by_browser"], false);
        assert_eq!(
            body["data"]["subscriptions"],
            json!(["https://push.example/abc"])
        );
        let (status, _) = call(
            &app,
            &wallet,
            "patch",
            "/api/profiles/me/settings",
            Some(&json!({"use_local_time":false})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = call(&app, &wallet, "post", path, Some(&subscription)).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, body) = call(&app, &wallet, "get", path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["data"],
            json!([{"user":address(&wallet),"endpoint":"https://push.example/abc","p256dh":"key","auth":"secret"}])
        );
        let (status, body) = call(&app, &wallet, "get", "/api/profiles/me/settings", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["notify_by_browser"], true);
        let (status, body) = call(&app, &wallet, "delete", path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok":true,"data":true}));
        let (status, body) = call(&app, &wallet, "get", "/api/profiles/me/settings", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["notify_by_browser"], false);
        assert_eq!(body["data"]["subscriptions"], json!([]));
    }

    /// gatsby `Profile/routes.js`: the face and body snapshots of the profile
    /// this node holds, the default avatar for a non-address or an unknown
    /// wallet, `Cache-Control: max-age=86400`.
    #[tokio::test]
    async fn avatar_redirects_follow_the_held_profile_or_the_default() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let app = app(&pool);
        async fn get(app: &axum::Router, uri: &str) -> (StatusCode, HeaderMap, String) {
            let response = app
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
        }
        let unknown = address(&Wallet::random());
        for uri in [
            format!("/api/profile/{unknown}/face.png"),
            format!("/api/profile/{unknown}/body.png"),
            "/api/profile/nope/face.png".to_owned(),
        ] {
            let (status, headers, body) = get(&app, &uri).await;
            assert_eq!(status, StatusCode::FOUND, "{uri}");
            assert_eq!(headers["location"], DEFAULT_AVATAR);
            assert_eq!(headers["cache-control"], "max-age=86400");
            assert!(
                headers["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("text/plain")
            );
            assert_eq!(body, format!("Found. Redirecting to {DEFAULT_AVATAR}"));
        }
        let known = address(&Wallet::random());
        let entity_id = test_support::deploy_profile(
            &pool,
            &known,
            json!({"snapshots":{"face256":"bafface256","face":"https://cdn.example/face.png","body":"https://cdn.example/body.png"}}),
        )
        .await;
        let (status, headers, _) = get(
            &app,
            &format!(
                "/api/profile/{}/face.png",
                known.to_uppercase().replace("0X", "0x")
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(
            headers["location"],
            format!("http://localhost/profile-images/entities/{entity_id}/face.png")
        );
        let (_, headers, _) = get(&app, &format!("/api/profile/{known}/body.png")).await;
        assert_eq!(
            headers["location"],
            format!("http://localhost/profile-images/entities/{entity_id}/body.png"),
            "a foreign snapshot host is rendered here, not redirected to"
        );
        assert!(
            avatar_redirect(Some("javascript:alert(1)")).headers()["location"] == DEFAULT_AVATAR
        );
    }

    #[test]
    fn a_body_without_the_envelope_keys_is_refused() {
        assert_eq!(
            envelope(json!({ "email": "a@b.c" }))
                .unwrap_err()
                .to_string(),
            "missing signed body"
        );
        assert!(envelope(json!({ "domain": {}, "message": {}, "signature": "0x" })).is_ok());
    }

    #[test]
    fn schedule_bodies_validate_as_the_old_schema_and_merge_like_pick() {
        let ok = schedule_body(
            &json!({"name":"S","active_since":"2031-01-01 00:00:00Z","active_until":"2031-02-01T00:00:00+02:00","theme":null,"image":null,"ignored":true}),
            true,
        )
        .unwrap();
        assert_eq!(ok.theme, Some(None));
        assert_eq!(ok.image, Some(None));
        assert_eq!(
            ok.active_until
                .unwrap()
                .to_rfc3339_opts(SecondsFormat::Millis, true),
            "2031-01-31T22:00:00.000Z"
        );
        assert!(schedule_body(&json!({"name":"S"}), true).is_err());
        assert!(schedule_body(&json!({"name":"S"}), false).is_ok());
        assert!(schedule_body(&json!({"background":["x",1]}), false).is_err());
        assert!(schedule_body(&json!({"image":"javascript:alert(1)"}), false).is_err());
        assert!(schedule_body(&json!("S"), false).is_err());
        let stored = ScheduleRecord {
            id: "id".into(),
            name: "Old".into(),
            description: Some("d".into()),
            image: None,
            theme: None,
            background: vec![],
            active_since: Some("2031-01-01T00:00:00Z".parse().unwrap()),
            active_until: Some("2031-02-01T00:00:00Z".parse().unwrap()),
            active: true,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        };
        let merged = merge_schedule(
            &stored,
            &schedule_body(&json!({"name":"New","theme":"pride_2023"}), false).unwrap(),
        );
        assert_eq!(merged.len(), SCHEDULE_FIELDS.len());
        assert_eq!(merged["name"], "New");
        assert_eq!(merged["theme"], "pride_2023");
        assert_eq!(merged["description"], "d");
        assert_eq!(merged["active_since"], "2031-01-01T00:00:00.000Z");
        assert!(merged.get("id").is_none());
    }

    #[test]
    fn snapshots_resolve_to_held_urls_or_this_nodes_profile_images() {
        let metadata = json!({"avatars":[{"avatar":{"snapshots":{"face256":"bafface","body":"https://cdn.example/body.png"}}}]});
        assert_eq!(
            snapshot_url(
                &metadata,
                "bafyid",
                Snapshot::Face,
                "https://node.example/prefix/events"
            ),
            Some("https://node.example/prefix/profile-images/entities/bafyid/face.png".into())
        );
        assert_eq!(
            snapshot_url(
                &metadata,
                "bafyid",
                Snapshot::Body,
                "https://node.example/events"
            ),
            Some("https://node.example/profile-images/entities/bafyid/body.png".into()),
            "a foreign host is not redirected to"
        );
        for held in [
            "https://profile-images.decentraland.org/entities/bafyid/body.png",
            "https://node.example/content/contents/bafbody",
        ] {
            let metadata = json!({"avatars":[{"avatar":{"snapshots":{"body":held}}}]});
            assert_eq!(
                snapshot_url(
                    &metadata,
                    "bafyid",
                    Snapshot::Body,
                    "https://node.example/events"
                ),
                Some(held.into())
            );
        }
        for foreign in [
            "https://decentraland.org.evil.example/x.png",
            "https://evil.example/?decentraland.org",
            "ftp://decentraland.org/x.png",
        ] {
            let metadata = json!({"avatars":[{"avatar":{"snapshots":{"body":foreign}}}]});
            assert_eq!(
                snapshot_url(
                    &metadata,
                    "bafyid",
                    Snapshot::Body,
                    "https://node.example/events"
                ),
                Some("https://node.example/profile-images/entities/bafyid/body.png".into()),
                "{foreign}"
            );
        }
        assert_eq!(
            snapshot_url(
                &json!({"avatars":[]}),
                "x",
                Snapshot::Face,
                "http://localhost/events"
            ),
            None
        );
        assert_eq!(
            profile_address("NotAnAddress").unwrap_err().to_string(),
            "Not found \"notanaddress\" profile: invalid address"
        );
    }
}
