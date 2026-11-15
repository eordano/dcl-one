use std::io::Cursor;
use std::time::Duration;

use tokio::time::Instant;

use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, OriginalUri, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use bytes::Bytes;
use image::io::{Limits, Reader as ImageReader};
use image::{ImageFormat, guess_format};
use uuid::Uuid;

use crate::db::{BlobVerdict, DeletedImage, NewImage};
use crate::dto::{
    Image, Metadata, UpdateReview, UpdateVisibility, UploadResponse, UserDataResponse,
};
use crate::http::{ApiError, JsonBody};
use crate::storage::{Storage, StorageError};
use crate::{SharedState, signed_path};

pub const MAX_JSON_IMAGE_BYTES: usize = 15 * 1024 * 1024;
pub const MAX_MULTIPART_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_MULTIPART_MEMORY_BYTES: usize = MAX_MULTIPART_IMAGE_BYTES + 1024 * 1024;

/// Decode bounds: an upload's pixel count, not its byte size, sets the cost.
pub const MAX_IMAGE_DIMENSION: u32 = 8192;
pub const MAX_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

/// Decodes run off the async workers, a few at a time.
static DECODES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Runs `work` on the blocking pool holding a `permits` permit until it
/// finishes, even when the caller is dropped first.
pub(crate) async fn decode_with<T: Send + 'static>(
    permits: &'static tokio::sync::Semaphore,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    let permit = permits
        .acquire()
        .await
        .map_err(|_| ApiError::Internal("decoder unavailable".to_string()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| ApiError::Internal("couldn't create thumbnail".to_string()))
}

pub async fn upload_image(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<Response, ApiError> {
    let address = state
        .require_auth(&headers, "post", &signed_path(&uri, "/api/images"))
        .await?;

    let mut multipart = multipart.map_err(|_| {
        multipart_error(match headers.get(header::CONTENT_TYPE) {
            None => "No Content-Type header found",
            Some(value) if value.to_str().ok().is_none_or(|value| !value.contains('/')) => {
                "Can not parse Content-Type header"
            }
            Some(_) => "Multipart boundary is not found",
        })
    })?;
    let mut memory_remaining = MAX_MULTIPART_MEMORY_BYTES;
    let mut image_bytes: Option<Bytes> = None;
    let mut image_content_type: Option<String> = None;
    let mut image_file_name: Option<String> = None;
    let mut metadata_bytes: Option<Bytes> = None;
    let mut is_public = None;

    while let Some(mut field) = multipart.next_field().await.map_err(multipart_read_error)? {
        if field.name().is_none()
            || !field
                .headers()
                .get(header::CONTENT_DISPOSITION)
                .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("form-data"))
        {
            return Err(multipart_error("No Content-Disposition `form-data` header"));
        }
        if field
            .content_type()
            .is_some_and(|value| value.starts_with("multipart/"))
        {
            return Err(multipart_error("Nested multipart is not supported"));
        }
        match field.name() {
            Some("image") if image_bytes.is_none() => {
                image_content_type = field.content_type().map(str::to_owned);
                image_file_name = field.file_name().map(str::to_owned);
                let bytes = read_form_bytes(field, &mut memory_remaining).await?;
                if bytes.len() > MAX_MULTIPART_IMAGE_BYTES {
                    return Err(multipart_error("Payload reached size limit."));
                }
                image_bytes = Some(bytes);
            }
            Some("metadata") if metadata_bytes.is_none() => {
                metadata_bytes = Some(read_form_bytes(field, &mut memory_remaining).await?);
            }
            Some("is_public") if is_public.is_none() => {
                if field.content_type().is_some_and(|content_type| {
                    content_type
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .rsplit('+')
                        .next()
                        != Some("plain")
                }) {
                    return Err(multipart_error(
                        "An error occurred processing field `is_public`: Content type error",
                    ));
                }
                let bytes = read_form_bytes(field, &mut memory_remaining).await?;
                let text = std::str::from_utf8(&bytes).map_err(|error| multipart_error(format!(
                    "An error occurred processing field `is_public`: UTF-8 decoding error: {error}"
                )))?;
                is_public = Some(text.parse::<bool>().map_err(|error| multipart_error(format!(
                    "An error occurred processing field `is_public`: Plain text deserialize error: cannot parse boolean: {error}"
                )))?);
            }
            _ => {
                // Unknown and duplicate fields are ignored; the first value wins.
                while field.chunk().await.map_err(multipart_read_error)?.is_some() {}
            }
        }
    }

    let image_bytes =
        image_bytes.ok_or_else(|| multipart_error("Field with name `image` is required"))?;
    let metadata_bytes =
        metadata_bytes.ok_or_else(|| multipart_error("Field with name `metadata` is required"))?;

    let max_images = state.settings.max_images_per_user;
    if state
        .db
        .get_user_images_count(&address, false)
        .await
        .unwrap_or(0)
        >= max_images
    {
        return Err(max_limit_reached(max_images));
    }

    let metadata: Metadata = serde_json::from_slice(&metadata_bytes).map_err(|e| {
        tracing::error!("failed to parse metadata: {e}");
        ApiError::BadRequest("invalid metadata".to_string())
    })?;

    finalize_upload(
        &state,
        address.as_str(),
        image_bytes,
        image_content_type,
        image_file_name.as_deref(),
        metadata,
        is_public.unwrap_or(false),
    )
    .await
}

fn multipart_error(message: impl Into<String>) -> ApiError {
    ApiError::PlainText(StatusCode::BAD_REQUEST, message.into())
}

fn multipart_read_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        multipart_error("Payload reached size limit.")
    } else {
        multipart_error("Multipart stream is incomplete")
    }
}

async fn read_form_bytes(
    mut field: axum::extract::multipart::Field<'_>,
    remaining: &mut usize,
) -> Result<Bytes, ApiError> {
    let mut bytes = bytes::BytesMut::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_read_error)? {
        *remaining = remaining
            .checked_sub(chunk.len())
            .ok_or_else(|| multipart_error("payload reached size limit"))?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes.freeze())
}

#[derive(serde::Deserialize)]
pub struct JsonUpload {
    pub image: String,
    pub content_type: String,
    pub metadata: Metadata,
    #[serde(default)]
    pub is_public: bool,
}

pub async fn upload_image_json(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(req): Json<JsonUpload>,
) -> Result<Response, ApiError> {
    let address = state
        .require_auth(&headers, "post", &signed_path(&uri, "/api/images-json"))
        .await?;

    let image_bytes = Bytes::from(
        base64::engine::general_purpose::STANDARD
            .decode(req.image.trim())
            .map_err(|_| ApiError::BadRequest("invalid base64 image".to_string()))?,
    );
    if image_bytes.len() > MAX_JSON_IMAGE_BYTES {
        return Err(ApiError::BadRequest("image too large".to_string()));
    }

    finalize_upload(
        &state,
        address.as_str(),
        image_bytes,
        Some(req.content_type),
        None,
        req.metadata,
        req.is_public,
    )
    .await
}

/// The 640x360 thumbnail in the image's own format, which must be the
/// declared one; decode failures and oversized images are the client's (400).
pub fn thumbnail_of(image_bytes: &[u8], declared: ImageFormat) -> Result<Bytes, ApiError> {
    let format = guess_format(image_bytes)
        .map_err(|_| ApiError::BadRequest("invalid image format".to_string()))?;
    if format != declared {
        return Err(ApiError::BadRequest("invalid image format".to_string()));
    }
    thumbnail_for_format(image_bytes, format)
}

/// Exercises the pinned source codec corpus without making those formats
/// reachable through the upload endpoints.
#[cfg(test)]
pub fn thumbnail_source_compat(image_bytes: &[u8]) -> Result<Bytes, ApiError> {
    let format = guess_format(image_bytes)
        .map_err(|_| ApiError::BadRequest("invalid image format".to_string()))?;
    thumbnail_for_format(image_bytes, format)
}

fn thumbnail_for_format(image_bytes: &[u8], format: ImageFormat) -> Result<Bytes, ApiError> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(image_bytes), format);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| {
        tracing::error!("failed to parse image: {e}");
        ApiError::BadRequest("invalid image".to_string())
    })?;
    let thumb = img.thumbnail(640, 360);
    let mut buffer = Cursor::new(Vec::new());
    thumb.write_to(&mut buffer, format).map_err(|e| {
        tracing::error!("couldn't generate thumbnail: {e}");
        ApiError::BadRequest("couldn't create thumbnail".to_string())
    })?;
    Ok(Bytes::from(buffer.into_inner()))
}

fn max_limit_reached(max_images: u64) -> ApiError {
    ApiError::MaxLimitReached(format!(
        "you have reached the limit of {max_images} max images"
    ))
}

/// `hash` must be `auth::hash_bytes_v1(&data)`; an existing blob with that
/// hash is the same bytes, so it is left alone.
async fn store_blob(storage: &Storage, hash: &str, data: Bytes) -> Result<(), StorageError> {
    if storage.exist(hash).await? {
        return Ok(());
    }
    storage.store(hash, data).await
}

async fn finalize_upload(
    state: &SharedState,
    address: &str,
    image_bytes: Bytes,
    image_content_type: Option<String>,
    image_file_name: Option<&str>,
    metadata: Metadata,
    is_public: bool,
) -> Result<Response, ApiError> {
    if !metadata.screen_rects_are_valid() {
        return Err(ApiError::BadRequest("invalid metadata".to_string()));
    }
    if !metadata.user_address.eq_ignore_ascii_case(address) {
        return Err(ApiError::BadRequest("invalid user address".to_string()));
    }

    let content_type = image_content_type
        .ok_or_else(|| ApiError::BadRequest("invalid content type".to_string()))?;
    let declared = match content_type.as_str() {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        _ => return Err(ApiError::BadRequest("unsupported content type".to_string())),
    };

    if image_file_name.is_some_and(|name| name.contains('/') || name.contains('\\')) {
        return Err(ApiError::BadRequest("invalid file name".to_string()));
    }

    // JSON uploads too are refused at the cap before the decode; the insert stays the gate.
    let max_images = state.settings.max_images_per_user;
    let owned = state
        .db
        .get_user_images_count(address, false)
        .await
        .map_err(|e| {
            tracing::error!("failed to count user images: {e}");
            ApiError::Internal("failed to store image metadata".to_string())
        })?;
    if owned >= max_images {
        return Err(max_limit_reached(max_images));
    }
    let bytes = image_bytes.clone();
    let thumbnail = decode_with(&DECODES, move || thumbnail_of(&bytes, declared)).await??;

    let new_image = NewImage {
        id: Uuid::new_v4(),
        image_hash: crate::auth::hash_bytes_v1(&image_bytes),
        thumbnail_hash: crate::auth::hash_bytes_v1(&thumbnail),
        is_public,
        metadata,
    };
    let original_filename = image_file_name.unwrap_or("image.png");
    let image = new_image.wire(&state.settings.api_url, Some(original_filename));

    // The row is the limit gate, so it lands before the blobs: a refused upload
    // never touches the store, and a failed blob write takes its row back out.
    let current_images = state
        .db
        .insert_image_within_limit_named(
            &new_image,
            max_images,
            Some(original_filename),
            Some(&content_type),
        )
        .await
        .map_err(|e| {
            tracing::error!("failed to store image metadata: {e}");
            ApiError::Internal("failed to store image metadata".to_string())
        })?
        .ok_or_else(|| max_limit_reached(max_images))?;

    let stored = async {
        store_blob(&state.storage, &new_image.image_hash, image_bytes)
            .await
            .map_err(|error| {
                tracing::error!(%error, "failed to store image");
                ApiError::Internal("failed to upload image".to_owned())
            })?;
        store_blob(&state.storage, &new_image.thumbnail_hash, thumbnail)
            .await
            .map_err(|error| {
                tracing::error!(%error, "failed to store thumbnail");
                ApiError::Internal("failed to upload image".to_owned())
            })
    }
    .await;
    if let Err(err) = stored {
        let _ = state.db.delete_image(new_image.id).await;
        return Err(err);
    }

    {
        let _verdict = state.blob_verdict.lock().await;
        let released = state
            .db
            .release_deleted_upload(new_image.id)
            .await
            .map_err(|error| {
                tracing::error!(%error, "failed to restore uploaded image availability");
                ApiError::Internal("failed to upload image".to_owned())
            })?;
        state.apply_verdict(&released);
    }

    // PhotoTaken's counter handler is best-effort upstream. Retain the false
    // publication marker on failure so the repair pass can retry after restart.
    if let Err(error) = state.db.publish_ranking(new_image.id).await {
        tracing::error!(%error, image_id=%new_image.id, "photo ranking publication deferred");
    }

    let response = UploadResponse {
        image,
        user_data: UserDataResponse {
            current_images,
            max_images,
        },
    };
    Ok((StatusCode::OK, Json(response)).into_response())
}

// Candidate retry policy: recent uploads get the existing minute cadence; old
// incomplete uploads are probed again an hour after a complete cold sweep.
const RANKING_RECENT_AGE: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
const RANKING_TICK: Duration = Duration::from_secs(60);
const RANKING_COLD_RETRY: Duration = Duration::from_secs(60 * 60);
const RANKING_PASS_BUDGET: Duration = Duration::from_secs(3);
// Candidate slow-row isolation budget; the pass deadline still takes priority.
const RANKING_ROW_BUDGET: Duration = Duration::from_secs(1);
const RANKING_MAX_PAGES: usize = 32;

pub(crate) struct RankingSweep {
    pub(crate) after: Uuid,
    pub(crate) cutoff: Option<chrono::NaiveDateTime>,
    pub(crate) next_due: Instant,
}

impl RankingSweep {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            after: Uuid::nil(),
            cutoff: None,
            next_due: now,
        }
    }
}

pub(crate) struct RankingSchedule {
    // Fixed slots: recent first, cold second. Only these two sweep records persist.
    pub(crate) sweeps: [RankingSweep; 2],
    next: usize,
}

impl RankingSchedule {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            sweeps: [RankingSweep::new(now), RankingSweep::new(now)],
            next: 0,
        }
    }

    pub(crate) fn due(&mut self, now: Instant) -> Option<usize> {
        for offset in 0..2 {
            let index = (self.next + offset) % 2;
            if self.sweeps[index].next_due <= now {
                self.next = (index + 1) % 2;
                return Some(index);
            }
        }
        None
    }
}

#[derive(Debug, Default)]
pub(crate) struct RankingRepairProgress {
    pub(crate) pages: usize,
    pub(crate) fetched: usize,
    pub(crate) inspected: usize,
    pub(crate) deferred: usize,
    pub(crate) probes: usize,
    pub(crate) published: usize,
    pub(crate) errors: usize,
    pub(crate) timed_out: bool,
}

impl crate::State {
    pub(crate) async fn repair_ranking_publications(
        &self,
        sweep: &mut RankingSweep,
        cold: bool,
        deadline: Instant,
    ) -> anyhow::Result<RankingRepairProgress> {
        let cutoff = *sweep
            .cutoff
            .get_or_insert_with(|| chrono::Utc::now().naive_utc() - RANKING_RECENT_AGE);
        let mut progress = RankingRepairProgress::default();
        let page = async {
            let rows = self.db.pending_rankings(sweep.after).await?;
            progress.fetched = rows.len();
            if rows.is_empty() {
                sweep.after = Uuid::nil();
                sweep.cutoff = None;
                sweep.next_due = Instant::now()
                    + if cold {
                        RANKING_COLD_RETRY
                    } else {
                        RANKING_TICK
                    };
            }
            for (id, image, thumbnail, created_at) in rows {
                if Instant::now() >= deadline {
                    progress.timed_out = true;
                    break;
                }
                if (created_at < cutoff) != cold {
                    progress.deferred += 1;
                } else {
                    // No database transaction or row lock is held during storage I/O.
                    let row_deadline = Instant::now() + RANKING_ROW_BUDGET;
                    let publication = async {
                        progress.probes += 1;
                        if !self.storage.exist(&image).await? {
                            return Ok(false);
                        }
                        progress.probes += 1;
                        if !self.storage.exist(&thumbnail).await? {
                            return Ok(false);
                        }
                        self.db
                            .publish_ranking_before(id, Some(row_deadline))
                            .await?;
                        Ok::<_, anyhow::Error>(true)
                    };
                    let publication = match tokio::time::timeout_at(row_deadline, publication).await
                    {
                        Ok(result) => result,
                        Err(error) => Err(error.into()),
                    };
                    // A row budget is a completed retryable failure. A global
                    // deadline retains this row even when both timers fire together.
                    if Instant::now() >= deadline {
                        progress.timed_out = true;
                        break;
                    }
                    match publication {
                        Ok(true) => progress.published += 1,
                        Ok(false) => {}
                        Err(error) => {
                            progress.errors += 1;
                            tracing::error!(%id, %error, "photo ranking row repair failed");
                        }
                    }
                    progress.inspected += 1;
                }
                // Only completed/deferred rows advance. Dropping this future on
                // deadline or cancellation retains the in-flight row and unvisited tail.
                sweep.after = id;
            }
            Ok::<_, anyhow::Error>(())
        };
        match tokio::time::timeout_at(deadline, page).await {
            Ok(result) => result?,
            Err(_) => progress.timed_out = true,
        }
        Ok(progress)
    }

    pub(crate) async fn repair_ranking_pass(
        &self,
        schedule: &mut RankingSchedule,
        deadline: Instant,
    ) -> anyhow::Result<RankingRepairProgress> {
        let mut progress = RankingRepairProgress::default();
        for _ in 0..RANKING_MAX_PAGES {
            if Instant::now() >= deadline {
                progress.timed_out = true;
                break;
            }
            let Some(index) = schedule.due(Instant::now()) else {
                break;
            };
            let page = self
                .repair_ranking_publications(&mut schedule.sweeps[index], index == 1, deadline)
                .await?;
            progress.pages += 1;
            progress.fetched += page.fetched;
            progress.inspected += page.inspected;
            progress.deferred += page.deferred;
            progress.probes += page.probes;
            progress.published += page.published;
            progress.errors += page.errors;
            if page.timed_out {
                progress.timed_out = true;
                break;
            }
        }
        Ok(progress)
    }

    pub fn spawn_ranking_job(self: &std::sync::Arc<Self>, jobs: &crate::jobs::Jobs) {
        let state = self.clone();
        jobs.spawn_leased("camera-photo-ranking", move |ctx| {
            let state = state.clone();
            async move {
                let mut schedule = RankingSchedule::new(Instant::now());
                loop {
                    let result = tokio::select! {
                        _ = ctx.cancel.cancelled() => return Ok(()),
                        result = state.repair_ranking_pass(
                            &mut schedule, Instant::now() + RANKING_PASS_BUDGET,
                        ) => result,
                    };
                    match result {
                        Ok(progress) => ctx.progress(format!(
                            "{} photo ranking publications repaired; {} pages, {} inspected, {} deferred, {} row errors; pass timed out: {}",
                            progress.published, progress.pages, progress.inspected,
                            progress.deferred, progress.errors, progress.timed_out,
                        )),
                        Err(error) => {
                            ctx.error(error.to_string());
                            tracing::error!(%error, "photo ranking repair pass failed");
                        }
                    }
                    ctx.pass_done();
                    tokio::select! {
                        _ = ctx.cancel.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(RANKING_TICK) => {},
                    }
                }
            }
        });
    }
}

pub async fn delete_image(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(image_id): Path<String>,
) -> Result<Response, ApiError> {
    let address = state
        .require_auth(
            &headers,
            "delete",
            &signed_path(&uri, format!("/api/images/{image_id}")),
        )
        .await?;

    let deleted = delete_image_row(&state, &image_id, Some(address.as_str())).await?;
    if !deleted.deleted {
        return Err(ApiError::Forbidden("forbidden".to_string()));
    }

    Ok((
        StatusCode::OK,
        Json(UserDataResponse {
            current_images: deleted.remaining,
            max_images: state.settings.max_images_per_user,
        }),
    )
        .into_response())
}

/// The row goes; the blobs stay, withheld. The ONE store is shared with
/// content, worlds and builder files and keeps hash-addressed bytes
/// immutable, so nothing here unlinks (the old service's per-row refcount
/// over its private tree is gone); the row was what made the hash reachable
/// through `/api/images/{hash}`, and the `withheld_blobs` record written in
/// the same transaction is what keeps it off `/contents/{hash}` everywhere.
async fn delete_image_row(
    state: &SharedState,
    image_id: &str,
    owner: Option<&str>,
) -> Result<DeletedImage, ApiError> {
    let _verdict = state.blob_verdict.lock().await;
    let deleted = state
        .db
        .delete_image_owned(image_id, owner, chrono::Utc::now().naive_utc())
        .await
        .map_err(|e| {
            tracing::error!("failed to delete image metadata: {e}");
            ApiError::Internal("failed to delete image".to_string())
        })?
        .ok_or_else(|| ApiError::NotFound("image not found".to_string()))?;
    state.apply_verdict(&BlobVerdict {
        withheld: deleted.withheld.clone(),
        released: Vec::new(),
    });
    Ok(deleted)
}

pub async fn update_image_visibility(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(image_id): Path<String>,
    update: Result<JsonBody<UpdateVisibility>, ApiError>,
) -> Result<Response, ApiError> {
    let address = state
        .require_auth(
            &headers,
            "patch",
            &signed_path(&uri, format!("/api/images/{image_id}/visibility")),
        )
        .await?;

    let JsonBody(update) = update?;
    let owned = state
        .db
        .set_image_visibility(&image_id, address.as_str(), update.is_public)
        .await
        .map_err(|e| {
            tracing::error!("failed to update image metadata: {e}");
            ApiError::Internal("failed to update image metadata".to_string())
        })?
        .ok_or_else(|| ApiError::NotFound("image not found".to_string()))?;
    if !owned {
        return Err(ApiError::Forbidden("forbidden".to_string()));
    }

    Ok(StatusCode::OK.into_response())
}

/// The public image URL redirects even when its target does not exist.
/// The configured bucket owns its object responses; the local byte endpoint
/// resolves photo aliases and enforces the shared store's moderation policy.
pub async fn get_image(
    State(state): State<SharedState>,
    Path(image_id): Path<String>,
) -> Result<Response, ApiError> {
    if image_id.is_empty() || image_id.bytes().any(|b| b.is_ascii_control()) {
        return Err(ApiError::NotFound("image not found".to_string()));
    }
    // A hash this node withholds (rejected, deleted or denied) is never sent
    // to an origin that may still hold a copy; the in-memory denylist needs
    // no database.
    if state.storage.withholds(&image_id) {
        return Err(ApiError::NotFound("image not found".to_string()));
    }
    let bucket_url = state
        .settings
        .bucket_url
        .as_deref()
        .and_then(|value| url::Url::parse(value).ok())
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .map(|url| url.to_string().trim_end_matches('/').to_owned());
    let location = match bucket_url {
        Some(bucket_url) => format!("{bucket_url}/{image_id}"),
        None => format!(
            "{}/api/files/{image_id}",
            state.settings.api_url.trim_end_matches('/')
        ),
    };
    Ok((
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, location)],
    )
        .into_response())
}

/// Serves only a stored photo alias or legacy hash whose bytes remain readable.
/// Resolving the alias before checking the denylist keeps UUID filenames from
/// bypassing a verdict recorded against the underlying immutable blob hash.
pub async fn get_image_bytes(
    State(state): State<SharedState>,
    Path(image_id): Path<String>,
) -> Result<Response, ApiError> {
    let image_blob = state
        .db
        .image_blob_for_name(&image_id)
        .await
        .map_err(|e| {
            tracing::error!("failed to resolve image bytes: {e}");
            ApiError::NotFound("image not found".to_string())
        })?
        .ok_or_else(|| ApiError::NotFound("image not found".to_string()))?;
    if state.storage.withholds(&image_blob.hash) {
        return Err(ApiError::NotFound("image not found".to_string()));
    }

    let bytes = state
        .storage
        .retrieve(&image_blob.hash)
        .await
        .map_err(|e| match e {
            StorageError::InvalidId(_) | StorageError::PathTraversal(_) => {
                ApiError::BadRequest(format!("invalid image id: {e}"))
            }
            other => ApiError::Internal(format!("failed to read image: {other}")),
        })?
        .ok_or_else(|| ApiError::NotFound("image not found".to_string()))?;

    let content_type =
        image_blob
            .content_type
            .as_deref()
            .unwrap_or_else(|| match guess_format(&bytes) {
                Ok(image::ImageFormat::Png) => "image/png",
                Ok(image::ImageFormat::Jpeg) => "image/jpeg",
                _ => "application/octet-stream",
            });

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        Body::from(bytes),
    )
        .into_response())
}

pub async fn admin_delete_image(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
) -> Result<Response, ApiError> {
    state.authorize_admin(&headers)?;

    let deleted = delete_image_row(&state, &image_id, None).await?;

    Ok((
        StatusCode::OK,
        Json(UserDataResponse {
            current_images: deleted.remaining,
            max_images: state.settings.max_images_per_user,
        }),
    )
        .into_response())
}

pub async fn admin_update_image_review(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
    Json(update): Json<UpdateReview>,
) -> Result<Response, ApiError> {
    state.authorize_admin(&headers)?;

    if !update.is_valid() {
        return Err(ApiError::BadRequest(
            "reviewStatus must be one of: ok, flagged, rejected".to_string(),
        ));
    }

    let _verdict = state.blob_verdict.lock().await;
    let verdict = state
        .db
        .update_image_review_status(
            &image_id,
            &update.review_status,
            chrono::Utc::now().naive_utc(),
        )
        .await
        .map_err(|e| {
            tracing::error!("failed to update image review status: {e}");
            ApiError::Internal("failed to update image review status".to_string())
        })?
        .ok_or_else(|| ApiError::NotFound("image not found".to_string()))?;
    state.apply_verdict(&verdict);

    Ok(StatusCode::OK.into_response())
}

/// Public metadata: private images answer too (upstream parity; the
/// `metadata_public` contract), a rejected or a store-withheld one is 404.
pub async fn get_metadata(
    State(state): State<SharedState>,
    Path(image_id): Path<String>,
) -> Result<Response, ApiError> {
    if state.storage.withholds(&image_id) {
        return Err(ApiError::NotFound("image not found".to_string()));
    }
    let db_image = match state.db.get_image(&image_id).await {
        Ok(img) => img,
        Err(sqlx::Error::ColumnDecode { source, .. }) => {
            tracing::debug!("couldn't decode image metadata: {source:?}");
            return Err(ApiError::Internal("couldn't decode image".to_string()));
        }
        Err(e) => {
            tracing::debug!("image not found: {e:?}");
            return Err(ApiError::NotFound("image not found".to_string()));
        }
    };

    if db_image.review_status == "rejected" {
        return Err(ApiError::NotFound("image not found".to_string()));
    }

    let image: Image = db_image.into();
    Ok((StatusCode::OK, Json(image)).into_response())
}

#[cfg(test)]
#[path = "writes_parity_tests.rs"]
mod parity_tests;

#[cfg(test)]
#[path = "image_format_tests.rs"]
mod image_format_tests;
