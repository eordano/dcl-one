//! `GET`/`PUT /world/{world_name}/settings`: the owner (or a world-wide
//! deployer) edits the world's settings through a multipart form under the
//! shared upload budgets; a thumbnail must be a real raster image.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, OriginalUri, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::comms::world_identity::Parcel;
use crate::policy::Permission;
use crate::shared::identity::PermissionDecision;
use crate::shared::signed_fetch::SignedPath;

use super::SharedState;
use super::auth::{map_auth_error_api, require_verified};
use super::deploy_validate::{MAX_PARCEL_COORDINATE, MIN_PARCEL_COORDINATE};
use super::http::ApiError;
use super::permissions::parse_world;
use super::settings_policy::{
    DESCRIPTION_MAX_LENGTH, DESCRIPTION_MIN_LENGTH, MAX_CATEGORIES, TITLE_MAX_LENGTH,
    TITLE_MIN_LENGTH, VALID_RATINGS, detect_image_format, storable_skybox_time, text_len,
};
use super::store::{self, WorldSettingsRow, WorldSettingsUpdate};
use super::upload_limits::{self, too_large};

/// A world thumbnail is at most 1 MiB, uploaded or mirrored.
pub(crate) const MAX_THUMBNAIL_BYTES: usize = 1024 * 1024;
const REALM_NAME_MAX_LENGTH: usize = 64;
const MAX_PREVIEW_WEARABLES: usize = 8;

/// The realm name reaches the client as a bare token it may interpolate into a
/// URL path, so anything that could change that URL's shape is refused here
/// rather than at whatever consumes it later.
fn is_valid_realm_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= REALM_NAME_MAX_LENGTH
        && name
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '/' && c != '\\' && c != '?' && c != '#')
}

const MAX_SETTINGS_UPLOAD_BYTES: usize = 2 * 1024 * 1024;

/// Wire-size cap for the whole multipart body; also this route's axum body limit so our 400 fires instead of axum's 413.
pub const MAX_SETTINGS_UPLOAD_WIRE_BYTES: usize = MAX_SETTINGS_UPLOAD_BYTES + 10 * 1024 * 1024;

const _: () = assert!(MAX_SETTINGS_UPLOAD_WIRE_BYTES >= MAX_SETTINGS_UPLOAD_BYTES);

/// A settings refusal. Upstream's handlers answer their own `{error:
/// <message>}` for a field that fails validation (400), a signer without the
/// right (403) and a world without settings (404); everything before the
/// handler (signed fetch, multipart parser, upload budgets) answers the
/// worlds envelope.
#[derive(Debug)]
pub enum SettingsError {
    Handler { status: StatusCode, message: String },
    Api(ApiError),
}

impl SettingsError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Handler {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl From<ApiError> for SettingsError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl IntoResponse for SettingsError {
    fn into_response(self) -> Response {
        match self {
            Self::Handler { status, message } => {
                (status, Json(json!({ "error": message }))).into_response()
            }
            Self::Api(e) => e.into_response(),
        }
    }
}

/// `UnauthorizedError`'s text (`logic/settings/errors.ts`).
pub const NOT_ALLOWED_MESSAGE: &str =
    "Unauthorized. You do not have permission to update settings for this world.";

/// The fields upstream's parser lets repeat (`repeatableFields`), and the
/// catalyst-only list field.
const REPEATABLE_FIELDS: [&str; 2] = ["categories", "preview_wearable_urns"];

/// GET /world/{world_name}/settings
pub async fn get_world_settings(
    State(state): State<SharedState>,
    Path(world_name): Path<String>,
) -> Result<Json<Value>, SettingsError> {
    let world = parse_world(&world_name)?;
    let settings = store::get_world_settings(&state.pool, &world)
        .await?
        .ok_or_else(|| SettingsError::Handler {
            status: StatusCode::NOT_FOUND,
            message: format!("World \"{world_name}\" not found or has no settings configured."),
        })?;
    Ok(Json(settings_json(&settings)))
}

/// Upstream's `toSnakeCaseSettings`: a NULL or empty text column and NULL
/// categories are left out, not `null`. `realm_name_override` and
/// `preview_wearable_urns` are this node's own.
fn settings_json(s: &WorldSettingsRow) -> Value {
    let mut v = json!({
        "skybox_time": s.skybox_time,
        "single_player": s.single_player,
        "show_in_places": s.show_in_places,
        "access_type": s.access_type,
        "realm_name_override": s.realm_name_override,
        "preview_wearable_urns": s.preview_wearable_urns,
        "settings_version": s.settings_version,
    });
    let text = [
        ("title", &s.title),
        ("description", &s.description),
        ("content_rating", &s.content_rating),
        ("spawn_coordinates", &s.spawn_coordinates),
        ("thumbnail_hash", &s.thumbnail_hash),
    ];
    for (key, value) in text {
        if let Some(value) = value.as_deref().filter(|t| !t.is_empty()) {
            v[key] = json!(value);
        }
    }
    if let Some(categories) = &s.categories {
        v["categories"] = json!(categories);
    }
    v
}

/// PUT /world/{world_name}/settings. The body is read only after the signed
/// fetch, so an unsigned request of any content type gets the signed-fetch
/// refusal, as upstream's middleware order gives.
pub async fn update_world_settings(
    State(state): State<SharedState>,
    Path(world_name): Path<String>,
    OriginalUri(original): OriginalUri,
    request: Request,
) -> Result<Json<Value>, SettingsError> {
    let world = parse_world(&world_name)?;
    let headers = request.headers().clone();
    let auth = require_verified(
        &headers,
        "put",
        &SignedPath::from_uris(&original, request.uri()),
        &[],
        state.eip1654.as_deref(),
    )
    .await
    .map_err(map_auth_error_api)?;
    let signer = auth.signer;
    let limits = &state.settings.upload_limits;

    let declared = match upload_limits::declared_content_length(&headers) {
        upload_limits::DeclaredContentLength::Invalid => {
            return Err(
                ApiError::bad_request(upload_limits::INVALID_CONTENT_LENGTH_MESSAGE).into(),
            );
        }
        upload_limits::DeclaredContentLength::Bytes(len)
            if len > MAX_SETTINGS_UPLOAD_WIRE_BYTES as u64 =>
        {
            return Err(too_large(upload_limits::PAYLOAD_TOO_LARGE_MESSAGE).into());
        }
        upload_limits::DeclaredContentLength::Bytes(len) => Some(len),
        upload_limits::DeclaredContentLength::Absent => None,
    };
    let _source = upload_limits::admit_source(
        &headers,
        limits,
        "world-settings",
        MAX_SETTINGS_UPLOAD_BYTES as u64,
    )
    .map_err(|_| ApiError::too_many(upload_limits::SOURCE_LIMIT_MESSAGE, 5))?;
    let _slot =
        upload_limits::try_acquire_upload_slot(limits.max_concurrent_uploads).ok_or_else(|| {
            tracing::warn!(
                active = upload_limits::active_uploads(),
                max = limits.max_concurrent_uploads,
                "PUT /world/:world_name/settings shed: concurrent-upload cap exceeded"
            );
            ApiError::UploadShed(upload_limits::CONCURRENCY_SHED_MESSAGE.to_string())
        })?;
    let mut bytes_lease = upload_limits::reserve_in_flight();
    let mut files_lease = upload_limits::reserve_in_flight_files();

    let received = Arc::new(AtomicU64::new(0));
    let multipart = read_multipart(request, received.clone()).await?;
    let timeout_ms = limits.multipart_upload_timeout_ms;
    let (mut input, thumbnail) = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        parse_multipart(
            multipart,
            limits.max_in_flight_upload_bytes,
            limits.max_in_flight_upload_files,
            &mut bytes_lease,
            &mut files_lease,
        ),
    )
    .await
    .map_err(|_| {
        tracing::warn!(
            timeout_ms,
            "PUT /world/:world_name/settings: multipart upload timed out"
        );
        ApiError::RequestTimeout(describe_upload_timeout(
            timeout_ms,
            received.load(Ordering::Relaxed),
            declared,
        ))
    })??;

    let timeout_ms = limits.deployment_processing_timeout_ms;
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        // `holder`: who the edit is recorded for, the signer when it holds the name, else (a
        // collaborator with world-wide deployment) the holder this node knows, if any.
        let (authorized, holder) = match state.policy.is_owner(&world, &signer).await {
            PermissionDecision::Allowed => (true, Some(signer.clone())),
            PermissionDecision::Refused(crate::shared::identity::AuthorityNotEstablished::RefusedLacksAuthority { .. }) => {
                let allowed = state.policy.has_world_wide_permission(&world, Permission::Deployment, &signer).await.map_err(ApiError::from)?;
                let holder = match allowed {
                    true => state.policy.current_owner(&world).await.ok().flatten(),
                    false => None,
                };
                (allowed, holder)
            }
            PermissionDecision::Refused(undetermined) => return Err(ApiError::from(undetermined).into()),
        };
        if !authorized {
            return Err(SettingsError::Handler { status: StatusCode::FORBIDDEN, message: NOT_ALLOWED_MESSAGE.to_string() });
        }
        if let Some(bytes) = thumbnail {
            let hash = crate::auth::hash_bytes_v1(&bytes);
            state
                .storage
                .store(&hash, bytes::Bytes::from(bytes))
                .await
                .map_err(|e| ApiError::internal(format!("failed to store thumbnail: {e}")))?;
            input.thumbnail_hash = Some(hash);
        }
        // The two spawn refusals are the store's only 400s: upstream's `ValidationError`s.
        let (settings, _old_spawn) = store::update_world_settings(&state.pool, &world, &signer, holder.as_ref(), &input)
            .await
            .map_err(|e| match e.is_bad_request() {
                true => SettingsError::invalid(e.public_message()),
                false => e.into(),
            })?;
        Ok(Json(json!({ "message": "World settings updated successfully", "settings": settings_json(&settings) })))
    })
    .await
    .map_err(|_| {
        tracing::warn!(timeout_ms, "PUT /world/:world_name/settings: settings processing timed out");
        ApiError::RequestTimeout(format!("Deployment processing exceeded the {timeout_ms}ms deadline."))
    })?
}

/// The multipart reader over the body, counting the bytes received for the
/// timeout message. A refused content type answers busboy 1.6.0's text, as
/// upstream's parser does.
async fn read_multipart(request: Request, received: Arc<AtomicU64>) -> Result<Multipart, ApiError> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let Some(content_type) = content_type else {
        return Err(ApiError::bad_request("Missing Content-Type"));
    };
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if media != "multipart/form-data" {
        return Err(ApiError::bad_request(format!(
            "Unsupported content type: {content_type}"
        )));
    }
    let (parts, body) = request.into_parts();
    let counted = Body::from_stream(body.into_data_stream().inspect(move |chunk| {
        if let Ok(chunk) = chunk {
            received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    }));
    Multipart::from_request(Request::from_parts(parts, counted), &())
        .await
        .map_err(|_| ApiError::bad_request("Multipart: Boundary not found"))
}

/// upstream `formatBytes`.
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} bytes")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else if b < KIB * KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else {
        format!("{:.1} GiB", b / (KIB * KIB * KIB))
    }
}

/// upstream `describeUploadTimeout` (`controllers/multipart/errors.ts`).
pub(super) fn describe_upload_timeout(
    timeout_ms: u64,
    received: u64,
    declared: Option<u64>,
) -> String {
    let seconds = timeout_ms as f64 / 1000.0;
    let got = match declared {
        Some(d) => format!("{} of {}", format_bytes(received), format_bytes(d)),
        None => format_bytes(received),
    };
    let rate = received as f64 / 1024.0 / seconds;
    format!(
        "The upload did not finish within {seconds} s: received {got} (about {rate:.1} KiB/s). \
         Retry on a faster connection or send smaller batches."
    )
}

fn account_settings_bytes(
    total_bytes: &mut usize,
    added: usize,
    bytes_lease: &mut upload_limits::InFlightBytesGuard,
    max_in_flight_bytes: u64,
) -> Result<(), ApiError> {
    match upload_limits::account_payload_bytes(
        total_bytes,
        added,
        MAX_SETTINGS_UPLOAD_BYTES,
        bytes_lease,
        max_in_flight_bytes,
    ) {
        Ok(()) => Ok(()),
        Err(upload_limits::PayloadAccountError::PayloadTooLarge) => {
            Err(too_large(upload_limits::PAYLOAD_TOO_LARGE_MESSAGE))
        }
        Err(upload_limits::PayloadAccountError::BudgetExhausted) => {
            tracing::warn!(
                total_bytes,
                in_flight = upload_limits::in_flight_upload_bytes(),
                max = max_in_flight_bytes,
                "PUT /world/:world_name/settings shed: aggregate in-flight upload budget exceeded"
            );
            Err(ApiError::UploadShed(
                upload_limits::BYTES_SHED_MESSAGE.to_string(),
            ))
        }
    }
}

/// Validates the form; the thumbnail bytes come back unstored so nothing is
/// written before the caller is authorized.
async fn parse_multipart(
    mut multipart: Multipart,
    max_in_flight_bytes: u64,
    max_in_flight_files: u64,
    bytes_lease: &mut upload_limits::InFlightBytesGuard,
    files_lease: &mut upload_limits::InFlightFilesGuard,
) -> Result<(WorldSettingsUpdate, Option<Vec<u8>>), SettingsError> {
    let mut fields: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashMap<String, bool> = HashMap::new();
    let mut thumbnail: Option<Vec<u8>> = None;
    let mut total_bytes: usize = 0;
    let mut total_files: u64 = 0;
    let mut part_count: usize = 0;
    let mut field_count: usize = 0;

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return Err(too_large(upload_limits::PAYLOAD_TOO_LARGE_MESSAGE).into());
            }
            Err(e) => {
                return Err(ApiError::bad_request(format!("Invalid multipart form: {e}")).into());
            }
        };
        let name = field.name().unwrap_or("").to_string();
        let is_file = field.file_name().is_some();

        part_count += 1;
        if part_count > upload_limits::MAX_MULTIPART_PARTS {
            return Err(too_large(upload_limits::TOO_MANY_PARTS_MESSAGE).into());
        }
        // A name seen before is refused unless both are fields of a repeatable name.
        match seen.insert(name.clone(), is_file) {
            Some(was_file)
                if is_file || was_file || !REPEATABLE_FIELDS.contains(&name.as_str()) =>
            {
                return Err(ApiError::bad_request(format!("Duplicate form field '{name}'")).into());
            }
            _ => {}
        }

        if is_file {
            if !files_lease.try_resize(total_files + 1, max_in_flight_files) {
                tracing::warn!(
                    request_files = total_files + 1,
                    in_flight = upload_limits::in_flight_upload_files(),
                    max = max_in_flight_files,
                    "PUT /world/:world_name/settings shed: aggregate in-flight upload-file budget exceeded"
                );
                return Err(
                    ApiError::UploadShed(upload_limits::FILES_SHED_MESSAGE.to_string()).into(),
                );
            }
            total_files += 1;
            let mut buf = BytesMut::new();
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        if buf.len().saturating_add(chunk.len()) > MAX_SETTINGS_UPLOAD_BYTES {
                            return Err(too_large(
                                "An uploaded file exceeds the maximum allowed size.",
                            )
                            .into());
                        }
                        account_settings_bytes(
                            &mut total_bytes,
                            chunk.len(),
                            bytes_lease,
                            max_in_flight_bytes,
                        )?;
                        buf.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                        return Err(too_large(upload_limits::PAYLOAD_TOO_LARGE_MESSAGE).into());
                    }
                    Err(e) => {
                        return Err(ApiError::bad_request(format!(
                            "Failed to read file data: {e}"
                        ))
                        .into());
                    }
                }
            }
            let data = buf.freeze();
            if name == "thumbnail" {
                if data.len() > MAX_THUMBNAIL_BYTES {
                    return Err(SettingsError::invalid(format!(
                        "Invalid thumbnail: size {} bytes exceeds maximum of {MAX_THUMBNAIL_BYTES} bytes (1MB).",
                        data.len()
                    )));
                }
                thumbnail = Some(data.to_vec());
            }
        } else {
            field_count += 1;
            if field_count > upload_limits::MAX_MULTIPART_FIELDS {
                return Err(too_large(upload_limits::too_many_fields_message(
                    upload_limits::MAX_MULTIPART_FIELDS,
                ))
                .into());
            }
            let mut buf = BytesMut::new();
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        if buf.len().saturating_add(chunk.len())
                            > upload_limits::MAX_MULTIPART_FIELD_VALUE_BYTES
                        {
                            return Err(too_large(format!(
                                "Field '{name}' is too large. The maximum allowed size per field is {} bytes.",
                                upload_limits::MAX_MULTIPART_FIELD_VALUE_BYTES
                            ))
                            .into());
                        }
                        account_settings_bytes(
                            &mut total_bytes,
                            chunk.len(),
                            bytes_lease,
                            max_in_flight_bytes,
                        )?;
                        buf.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                        return Err(too_large(upload_limits::PAYLOAD_TOO_LARGE_MESSAGE).into());
                    }
                    Err(e) => {
                        return Err(
                            ApiError::bad_request(format!("Invalid form field: {e}")).into()
                        );
                    }
                }
            }
            fields
                .entry(name)
                .or_default()
                .push(String::from_utf8_lossy(&buf).into_owned());
        }
    }

    let mut input = WorldSettingsUpdate::default();

    if let Some(title) = first_value(&fields, "title") {
        if !(TITLE_MIN_LENGTH..=TITLE_MAX_LENGTH).contains(&text_len(&title)) {
            let shown = if title.is_empty() {
                "title cannot be empty"
            } else {
                &title
            };
            return Err(SettingsError::invalid(format!(
                "Invalid title: {shown}. Expected between {TITLE_MIN_LENGTH} and {TITLE_MAX_LENGTH} characters."
            )));
        }
        input.title = Some(title);
    }

    if let Some(description) = first_value(&fields, "description") {
        if !(DESCRIPTION_MIN_LENGTH..=DESCRIPTION_MAX_LENGTH).contains(&text_len(&description)) {
            let shown = if description.is_empty() {
                "description cannot be empty"
            } else {
                &description
            };
            return Err(SettingsError::invalid(format!(
                "Invalid description: {shown}. Expected between {DESCRIPTION_MIN_LENGTH} and {DESCRIPTION_MAX_LENGTH} characters."
            )));
        }
        input.description = Some(description);
    }

    if let Some(rating) = first_defined(&fields, "content_rating") {
        if !VALID_RATINGS.contains(&rating.as_str()) {
            return Err(SettingsError::invalid(format!(
                "Invalid content rating: {rating}. Expected one of: {}",
                VALID_RATINGS.join(", ")
            )));
        }
        input.content_rating = Some(rating);
    }

    if let Some(spawn) = first_defined(&fields, "spawn_coordinates") {
        if parse_coordinate(&spawn).is_none() {
            return Err(SettingsError::invalid(format!(
                "Invalid spawnCoordinates format: \"{spawn}\"."
            )));
        }
        input.spawn_coordinates = Some(spawn);
    }

    if let Some(value) = first_defined(&fields, "skybox_time") {
        input.skybox_time_provided = true;
        input.skybox_time = if value == "null" {
            None
        } else {
            let time = value.trim().parse::<f64>().ok().and_then(storable_skybox_time).ok_or_else(|| {
                SettingsError::invalid(format!("Invalid skybox_time: {value}. Expected an integer between {} and {}, or null.", i32::MIN, i32::MAX))
            })?;
            Some(time)
        };
    }

    // Catalyst-only field (no upstream counterpart): only an empty first value is skipped.
    if let Some(value) = first_nonempty(&fields, "realm_name_override") {
        input.realm_name_override_provided = true;
        input.realm_name_override = if value == "null" {
            None
        } else {
            let name = value.trim().to_string();
            if !is_valid_realm_name(&name) {
                return Err(SettingsError::invalid(format!(
                    "Invalid realm_name_override: \"{name}\". Expected 1-{REALM_NAME_MAX_LENGTH} printable characters with no whitespace or \"/\", or null."
                )));
            }
            Some(name)
        };
    }

    if let Some(values) = defined(&fields, "categories") {
        input.categories_provided = true;
        if values.len() == 1 && values[0] == "null" {
            input.categories = Some(Vec::new());
        } else {
            if values.len() > MAX_CATEGORIES {
                return Err(SettingsError::invalid(format!(
                    "Invalid categories: {} items. Expected at most {MAX_CATEGORIES}",
                    values.len()
                )));
            }
            input.categories = Some(values.clone());
        }
    }

    if let Some(values) = fields.get("preview_wearable_urns") {
        input.preview_wearable_urns_provided = true;
        input.preview_wearable_urns = if values.len() == 1 && values[0] == "null" {
            Some(Vec::new())
        } else {
            if values.len() > MAX_PREVIEW_WEARABLES {
                return Err(SettingsError::invalid(format!(
                    "Invalid preview_wearable_urns: {} items. Expected at most {MAX_PREVIEW_WEARABLES}",
                    values.len()
                )));
            }
            Some(values.iter().map(|v| v.trim().to_string()).collect())
        };
    }

    if let Some(value) = first_defined(&fields, "single_player") {
        input.single_player = Some(value == "true");
    }
    if let Some(value) = first_defined(&fields, "show_in_places") {
        input.show_in_places = Some(value == "true");
    }

    if let Some(bytes) = &thumbnail {
        if bytes.len() > MAX_THUMBNAIL_BYTES {
            return Err(SettingsError::invalid(format!(
                "Invalid thumbnail: size {} bytes exceeds maximum of {MAX_THUMBNAIL_BYTES} bytes (1MB).",
                bytes.len()
            )));
        }
        if detect_image_format(bytes).is_none() {
            return Err(SettingsError::invalid(
                "Invalid thumbnail: expected a PNG, JPEG, GIF or WebP image.",
            ));
        }
    }

    Ok((input, thumbnail))
}

fn first_value(fields: &HashMap<String, Vec<String>>, key: &str) -> Option<String> {
    fields.get(key).and_then(|v| v.first()).cloned()
}

/// Upstream's `isDefinedMultipartField`: present with every value non-empty;
/// otherwise the field is not an update.
fn defined<'a>(fields: &'a HashMap<String, Vec<String>>, key: &str) -> Option<&'a Vec<String>> {
    fields
        .get(key)
        .filter(|v| !v.is_empty() && v.iter().all(|s| !s.is_empty()))
}

fn first_defined(fields: &HashMap<String, Vec<String>>, key: &str) -> Option<String> {
    defined(fields, key).map(|v| v[0].clone())
}

fn first_nonempty(fields: &HashMap<String, Vec<String>>, key: &str) -> Option<String> {
    first_value(fields, key).filter(|s| !s.is_empty())
}

fn parse_coordinate(s: &str) -> Option<(i32, i32)> {
    let p = Parcel::parse(s).ok()?;
    if !(MIN_PARCEL_COORDINATE..=MAX_PARCEL_COORDINATE).contains(&i64::from(p.x))
        || !(MIN_PARCEL_COORDINATE..=MAX_PARCEL_COORDINATE).contains(&i64::from(p.y))
    {
        return None;
    }
    Some((p.x, p.y))
}

#[cfg(test)]
mod tests {
    use super::super::deploy_form::test_support::multipart_from;
    use super::*;

    #[test]
    fn coordinate_parse_and_bounds() {
        assert_eq!(parse_coordinate("0,0"), Some((0, 0)));
        assert_eq!(parse_coordinate(" -5 , 10 "), Some((-5, 10)));
        assert_eq!(parse_coordinate("150,-150"), Some((150, -150)));
        assert!(parse_coordinate("151,0").is_none());
        assert!(parse_coordinate("0,-151").is_none());
        assert!(parse_coordinate("abc").is_none());
        assert!(parse_coordinate("1,2,3").is_none());
    }

    #[test]
    fn settings_json_reports_effective_defaults_for_unexpressed_booleans() {
        let v = settings_json(&WorldSettingsRow {
            show_in_places: true,
            ..WorldSettingsRow::default()
        });
        assert_eq!(v["single_player"], serde_json::json!(false));
        assert_eq!(v["show_in_places"], serde_json::json!(true));
        assert_eq!(v["settings_version"], serde_json::json!(0));
        assert!(v["access_type"].is_null());
        // `toSnakeCaseSettings` leaves NULL text and categories out.
        for key in [
            "title",
            "description",
            "content_rating",
            "spawn_coordinates",
            "thumbnail_hash",
            "categories",
        ] {
            assert!(v.get(key).is_none(), "{key} is absent: {v}");
        }

        let v = settings_json(&WorldSettingsRow {
            single_player: true,
            show_in_places: false,
            settings_version: 3,
            access_type: Some("unrestricted".into()),
            ..Default::default()
        });
        assert_eq!(v["single_player"], serde_json::json!(true));
        assert_eq!(v["show_in_places"], serde_json::json!(false));
        assert_eq!(v["settings_version"], serde_json::json!(3));
        assert_eq!(v["access_type"], serde_json::json!("unrestricted"));
    }

    /// A handler 400, `{error: <message>}`.
    fn bad_request_message(err: SettingsError) -> String {
        match err {
            SettingsError::Handler {
                status: StatusCode::BAD_REQUEST,
                message,
            } => message,
            other => panic!("expected a validation 400, got {other:?}"),
        }
    }

    /// A parser refusal in the worlds envelope: its status and message.
    fn parser_refusal(err: SettingsError) -> (u16, String) {
        match err {
            SettingsError::Api(e) => (e.status(), e.public_message()),
            other => panic!("expected a parser refusal, got {other:?}"),
        }
    }

    async fn parse_settings_form(
        parts: &[(&str, Option<&str>, &str)],
    ) -> Result<WorldSettingsUpdate, SettingsError> {
        let multipart = multipart_from(parts).await;
        let mut bytes_lease = upload_limits::reserve_in_flight();
        let mut files_lease = upload_limits::reserve_in_flight_files();
        parse_multipart(
            multipart,
            u64::MAX,
            u64::MAX,
            &mut bytes_lease,
            &mut files_lease,
        )
        .await
        .map(|(input, _)| input)
    }

    #[tokio::test]
    async fn parse_multipart_streams_fields_and_accounts_payload() {
        let multipart = multipart_from(&[
            ("title", None, "Gate World"),
            ("other", Some("a.bin"), "abc"),
        ])
        .await;
        let mut bytes_lease = upload_limits::reserve_in_flight();
        let mut files_lease = upload_limits::reserve_in_flight_files();
        let (input, _) = parse_multipart(
            multipart,
            u64::MAX,
            u64::MAX,
            &mut bytes_lease,
            &mut files_lease,
        )
        .await
        .expect("form parses");
        assert_eq!(input.title.as_deref(), Some("Gate World"));
        assert_eq!(bytes_lease.reserved(), 10 + 3);
        assert_eq!(files_lease.reserved(), 1);
    }

    #[tokio::test]
    async fn parse_multipart_rejects_malformed_skybox_time() {
        for value in ["1.5", "12abc", "3000000000", "-2147483649", "NaN"] {
            let err = parse_settings_form(&[("skybox_time", None, value)])
                .await
                .expect_err("out-of-policy skybox_time must be a 400");
            assert_eq!(
                bad_request_message(err),
                format!(
                    "Invalid skybox_time: {value}. Expected an integer between {} and {}, or null.",
                    i32::MIN,
                    i32::MAX
                )
            );
        }
    }

    #[tokio::test]
    async fn parse_multipart_accepts_integer_and_null_skybox_time() {
        let input = parse_settings_form(&[("skybox_time", None, "36000")])
            .await
            .expect("integer parses");
        assert!(input.skybox_time_provided);
        assert_eq!(input.skybox_time, Some(36000));

        let input = parse_settings_form(&[("skybox_time", None, "null")])
            .await
            .expect("explicit null clears");
        assert!(input.skybox_time_provided);
        assert_eq!(input.skybox_time, None);

        let input = parse_settings_form(&[("skybox_time", None, "")])
            .await
            .expect("empty field is not an update");
        assert!(!input.skybox_time_provided);
    }

    #[tokio::test]
    async fn parse_multipart_measures_text_in_utf16_code_units() {
        let err = parse_settings_form(&[("title", None, "\u{65E5}\u{672C}")])
            .await
            .expect_err("2 UTF-16 units is under the 3-unit title minimum");
        assert!(bad_request_message(err).starts_with("Invalid title"));

        let description = "\u{30C7}".repeat(400);
        let input = parse_settings_form(&[("description", None, description.as_str())])
            .await
            .expect("400 UTF-16 units fits the 1000-unit description bound despite 1200 bytes");
        assert_eq!(input.description.as_deref(), Some(description.as_str()));
    }

    #[tokio::test]
    async fn parse_multipart_caps_each_field_value_at_one_megabyte() {
        let big = "a".repeat(upload_limits::MAX_MULTIPART_FIELD_VALUE_BYTES + 1);
        let multipart = multipart_from(&[("description", None, big.as_str())]).await;
        let mut bytes_lease = upload_limits::reserve_in_flight();
        let mut files_lease = upload_limits::reserve_in_flight_files();
        let err = parse_multipart(
            multipart,
            u64::MAX,
            u64::MAX,
            &mut bytes_lease,
            &mut files_lease,
        )
        .await
        .expect_err("oversized field must be rejected");
        assert_eq!(
            parser_refusal(err),
            (
                413,
                "Field 'description' is too large. The maximum allowed size per field is 1048576 bytes.".to_string()
            )
        );
        assert!(bytes_lease.reserved() <= upload_limits::MAX_MULTIPART_FIELD_VALUE_BYTES as u64);
    }

    #[tokio::test]
    async fn parse_multipart_rejects_more_than_one_hundred_fields() {
        let names: Vec<String> = (0..=upload_limits::MAX_MULTIPART_FIELDS)
            .map(|i| format!("f{i}"))
            .collect();
        let parts: Vec<(&str, Option<&str>, &str)> =
            names.iter().map(|n| (n.as_str(), None, "v")).collect();
        let err = parse_settings_form(&parts)
            .await
            .expect_err("101st field must be rejected");
        assert_eq!(
            parser_refusal(err),
            (
                413,
                "The multipart request has too many fields. The maximum allowed is 100."
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn parse_multipart_rejects_more_than_the_parts_limit() {
        let names: Vec<String> = (0..=upload_limits::MAX_MULTIPART_PARTS)
            .map(|i| format!("f{i}"))
            .collect();
        let parts: Vec<(&str, Option<&str>, &str)> = names
            .iter()
            .map(|n| (n.as_str(), Some("a.bin"), ""))
            .collect();
        let err = parse_settings_form(&parts)
            .await
            .expect_err("part 10101 must be rejected");
        assert_eq!(
            parser_refusal(err),
            (413, upload_limits::TOO_MANY_PARTS_MESSAGE.to_string())
        );
    }

    #[tokio::test]
    async fn a_thumbnail_must_be_a_raster_image_and_is_returned_unstored() {
        let err = parse_settings_form(&[("thumbnail", Some("t.png"), "<svg xmlns=\"a\"/>")])
            .await
            .expect_err("svg refused");
        assert!(bad_request_message(err).contains("expected a PNG, JPEG, GIF or WebP"));
        let png = [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];
        let (body, content_type) = super::super::deploy_form::test_support::multipart_body(&[(
            "thumbnail",
            Some("t.png"),
            &png,
        )]);
        use axum::extract::FromRequest;
        let req = axum::http::Request::builder()
            .header("content-type", content_type)
            .body(axum::body::Body::from(body))
            .unwrap();
        let multipart = Multipart::from_request(req, &()).await.unwrap();
        let mut bytes_lease = upload_limits::reserve_in_flight();
        let mut files_lease = upload_limits::reserve_in_flight_files();
        let (input, thumbnail) = parse_multipart(
            multipart,
            u64::MAX,
            u64::MAX,
            &mut bytes_lease,
            &mut files_lease,
        )
        .await
        .unwrap();
        assert_eq!(thumbnail.as_deref(), Some(&png[..]));
        assert!(
            input.thumbnail_hash.is_none(),
            "stored only after authorization"
        );
    }

    #[tokio::test]
    async fn empty_values_follow_the_upstream_rules() {
        let err = parse_settings_form(&[("title", None, "")])
            .await
            .expect_err("empty title");
        assert_eq!(
            bad_request_message(err),
            "Invalid title: title cannot be empty. Expected between 3 and 100 characters."
        );
        let err = parse_settings_form(&[("description", None, "")])
            .await
            .expect_err("empty description");
        assert_eq!(
            bad_request_message(err),
            "Invalid description: description cannot be empty. Expected between 3 and 1000 characters."
        );
        let input = parse_settings_form(&[
            ("content_rating", None, ""),
            ("spawn_coordinates", None, ""),
            ("single_player", None, ""),
            ("show_in_places", None, ""),
            ("categories", None, "art"),
            ("categories", None, ""),
        ])
        .await
        .expect("empty values are not updates");
        assert!(input.content_rating.is_none());
        assert!(input.spawn_coordinates.is_none());
        assert!(input.single_player.is_none());
        assert!(input.show_in_places.is_none());
        assert!(!input.categories_provided);
        assert!(input.categories.is_none());
    }

    #[test]
    fn settings_json_keeps_set_values_and_drops_empty_text() {
        let v = settings_json(&WorldSettingsRow {
            title: Some("A World".into()),
            description: Some(String::new()),
            categories: Some(Vec::new()),
            ..Default::default()
        });
        assert_eq!(v["title"], "A World");
        assert!(v.get("description").is_none());
        assert_eq!(
            v["categories"],
            serde_json::json!([]),
            "an empty list is kept"
        );
    }

    /// `multipart/middleware.ts` `rejectIfDuplicate`: only `categories` repeats
    /// (and this node's `preview_wearable_urns`).
    #[tokio::test]
    async fn a_repeated_form_field_is_refused() {
        for parts in [
            vec![("title", None, "One World"), ("title", None, "Two World")],
            vec![
                ("thumbnail", Some("a.png"), "x"),
                ("thumbnail", Some("b.png"), "y"),
            ],
            vec![("title", None, "One World"), ("title", Some("t.txt"), "x")],
            vec![
                ("categories", None, "art"),
                ("categories", Some("c.txt"), "x"),
            ],
        ] {
            let name = parts[0].0;
            let err = parse_settings_form(&parts).await.expect_err("a repeat");
            assert_eq!(
                parser_refusal(err),
                (400, format!("Duplicate form field '{name}'"))
            );
        }
        let input =
            parse_settings_form(&[("categories", None, "art"), ("categories", None, "game")])
                .await
                .expect("categories repeat");
        assert_eq!(
            input.categories,
            Some(vec!["art".to_string(), "game".into()])
        );
    }

    #[test]
    fn the_timeout_message_is_upstreams() {
        assert_eq!(
            describe_upload_timeout(300_000, 2_048, Some(4_096)),
            "The upload did not finish within 300 s: received 2.0 KiB of 4.0 KiB (about 0.0 KiB/s). Retry on a faster connection or send smaller batches."
        );
        assert_eq!(
            describe_upload_timeout(1_500, 512, None),
            "The upload did not finish within 1.5 s: received 512 bytes (about 0.3 KiB/s). Retry on a faster connection or send smaller batches."
        );
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
