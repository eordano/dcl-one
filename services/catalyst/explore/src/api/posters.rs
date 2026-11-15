//! The event poster bucket: `GET /poster/{file}` and `/poster-vertical/{file}`
//! as the old gateway fronted them on `events-assets-099ac00.<domain>`, at
//! the root (the vhost's own paths) and under `/events-assets-099ac00` (the
//! gateway prefix the explorer-boot corpus maps the host to). Mirrored event
//! records carry `https://events-assets-099ac00.decentraland.org/poster/...`
//! URLs, rewritten to this deployment's domain on read (`explore::events`).
//!
//! The old vhost asked the events service's own poster store first and, on
//! its 404, proxied the upstream bucket on the same origin (never a redirect:
//! the browser's resource policy and original-image recovery need the same
//! origin), caching nothing. Here a name is looked up in `explore.poster`, a
//! keyed index into the ONE blob store; a miss reads the upstream bucket
//! through `http::Outbound` once per name at a time, lands the bytes in the
//! store and indexes them, so the next request never leaves the node. An
//! upstream miss is remembered for a minute. Only the four image types the
//! events service accepted pass (sniffed from the bytes); the upstream's own
//! `Content-Type` is served back as it came (`image/*` or
//! `application/octet-stream`, which the bucket answers for most files).
//! The read-through runs where the old one did: when the events catalogue is
//! mirrored (`EVENTS_MIRROR_UPSTREAM`), from `EVENTS_POSTER_UPSTREAM_URL`.
//! Local uploads (`origin = 'local'`, the old `<sha256>.<ext>` names) are
//! rows of the same index, written by the events write routes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use parking_lot::Mutex;
use serde_json::json;

use super::Shared;
use super::mirror::{self, Conditional};

/// The vhost this bucket answered on, and the gateway prefix it maps to.
pub const HOST_PREFIX: &str = "events-assets-099ac00";

const BODY_CAP: usize = 16 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const MISS_TTL: Duration = Duration::from_secs(60);
const MISS_MAX_ENTRIES: usize = 16_384;
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Poster,
    Vertical,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Poster => "poster",
            Kind::Vertical => "poster-vertical",
        }
    }
}

/// A poster file name: one plain path segment (the old vhost's `[^/]+`),
/// restricted to the characters the bucket's names use (uuids, hex hashes,
/// an extension), so a key can never address anything but one object.
pub fn valid_file(file: &str) -> bool {
    let bytes = file.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 200
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The image type the bytes are, among the four the events service accepted.
pub fn sniff(body: &[u8]) -> Option<&'static str> {
    if body.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if body.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if body.len() >= 12 && &body[..4] == b"RIFF" && &body[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The type served for a landed object: the upstream's when it is one of
/// the image types or the bucket's `application/octet-stream`, else the sniffed one.
fn served_type(upstream: Option<&str>, sniffed: &'static str) -> String {
    let declared = upstream.map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    match declared.as_deref() {
        Some(
            t @ ("image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "application/octet-stream"),
        ) => t.to_string(),
        _ => sniffed.to_string(),
    }
}

#[derive(Default)]
pub struct PosterState {
    locks: crate::keyed_locks::KeyedLocks<(Kind, String)>,
    misses: Mutex<HashMap<(Kind, String), Instant>>,
}

impl PosterState {
    fn is_known_miss(&self, key: &(Kind, String)) -> bool {
        let mut misses = self.misses.lock();
        match misses.get(key) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                misses.remove(key);
                false
            }
            None => false,
        }
    }

    fn remember_miss(&self, key: (Kind, String)) {
        let mut misses = self.misses.lock();
        let now = Instant::now();
        if misses.len() >= MISS_MAX_ENTRIES {
            misses.retain(|_, until| *until > now);
            if misses.len() >= MISS_MAX_ENTRIES {
                misses.clear();
            }
        }
        misses.insert(key, now + MISS_TTL);
    }
}

pub enum Served {
    Hit {
        body: Bytes,
        content_type: String,
        cid: String,
    },
    Miss,
}

async fn lookup(state: &Shared, kind: Kind, file: &str) -> anyhow::Result<Option<Served>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT blob_cid, content_type FROM explore.poster WHERE kind = $1 AND file = $2",
    )
    .bind(kind.as_str())
    .bind(file)
    .fetch_optional(&state.pool)
    .await
    .context("reading explore.poster")?;
    let Some((cid, content_type)) = row else {
        return Ok(None);
    };
    match state
        .content
        .storage
        .retrieve(&cid)
        .await
        .context("reading the poster blob")?
    {
        Some(body) => Ok(Some(Served::Hit {
            body,
            content_type,
            cid,
        })),
        None => {
            tracing::warn!(kind = kind.as_str(), file, blob = %cid, "poster blob missing from the store; reading through again");
            Ok(None)
        }
    }
}

/// The index, then (once per name at a time) the upstream bucket.
pub async fn serve(state: &Shared, kind: Kind, file: &str) -> anyhow::Result<Served> {
    if let Some(hit) = lookup(state, kind, file).await? {
        return Ok(hit);
    }
    let Some(upstream) = state.settings.poster_upstream.clone() else {
        return Ok(Served::Miss);
    };
    let key = (kind, file.to_string());
    if state.posters.is_known_miss(&key) {
        return Ok(Served::Miss);
    }
    let lock = state.posters.locks.claim(&key);
    let result = async {
        let _guard = lock.lock().await;
        if let Some(hit) = lookup(state, kind, file).await? {
            return Ok(hit);
        }
        let url = format!("{}/{}/{file}", upstream.trim_end_matches('/'), kind.as_str());
        match mirror::fetch_conditional(&state.http, &state.settings, &url, None, None, BODY_CAP, FETCH_TIMEOUT, mirror::Reach::Public).await? {
            Conditional::Fetched { body, content_type, .. } => {
                let sniffed = sniff(&body).with_context(|| format!("{} is not a poster image", mirror::shown(&url)))?;
                let content_type = served_type(content_type.as_deref(), sniffed);
                let body = Bytes::from(body);
                let cid = {
                    let body = body.clone();
                    tokio::task::spawn_blocking(move || crate::auth::hash::hash_bytes_v1(&body)).await.context("poster hash task")?
                };
                let storage = &state.content.storage;
                if !storage.exist(&cid).await.context("probing the poster blob")? {
                    storage.store(&cid, body.clone()).await.context("storing the poster blob")?;
                }
                sqlx::query(
                    "INSERT INTO explore.poster (kind, file, blob_cid, content_type, byte_size, origin, fetched_at)
                     VALUES ($1, $2, $3, $4, $5, 'upstream', timezone('UTC', now()))
                     ON CONFLICT (kind, file) DO UPDATE
                     SET blob_cid = EXCLUDED.blob_cid, content_type = EXCLUDED.content_type,
                         byte_size = EXCLUDED.byte_size, fetched_at = EXCLUDED.fetched_at
                     WHERE explore.poster.origin = 'upstream'",
                )
                .bind(kind.as_str())
                .bind(file)
                .bind(&cid)
                .bind(&content_type)
                .bind(body.len() as i64)
                .execute(&state.pool)
                .await
                .context("recording explore.poster")?;
                Ok(Served::Hit { body, content_type, cid })
            }
            Conditional::Missing(_) => {
                state.posters.remember_miss(key.clone());
                Ok(Served::Miss)
            }
            Conditional::NotModified => anyhow::bail!("{} answered 304 to an unconditional GET", mirror::shown(&url)),
        }
    }
    .await;
    result
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({ "ok": false, "error": "Not found" })),
    )
        .into_response()
}

async fn answer(state: Shared, kind: Kind, file: String) -> Response {
    if !valid_file(&file) {
        return not_found();
    }
    match serve(&state, kind, &file).await {
        Ok(Served::Hit {
            body,
            content_type,
            cid,
        }) => {
            let mut resp = Response::new(axum::body::Body::from(body));
            let h = resp.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&content_type) {
                h.insert(header::CONTENT_TYPE, v);
            }
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
            if let Ok(v) = HeaderValue::from_str(&format!("\"{cid}\"")) {
                h.insert(header::ETAG, v);
            }
            h.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            resp
        }
        Ok(Served::Miss) => not_found(),
        Err(e) => {
            tracing::warn!(kind = kind.as_str(), file, error = %format!("{e:#}"), "poster read-through failed");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(json!({ "ok": false, "error": "the poster bucket could not be read" })),
            )
                .into_response()
        }
    }
}

async fn get_poster(State(state): State<Shared>, Path(file): Path<String>) -> Response {
    answer(state, Kind::Poster, file).await
}

async fn get_poster_vertical(State(state): State<Shared>, Path(file): Path<String>) -> Response {
    answer(state, Kind::Vertical, file).await
}

/// The bucket's routes (the vhost's own paths).
pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/poster/{file}", get(get_poster))
        .route("/poster-vertical/{file}", get(get_poster_vertical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_one_plain_segment() {
        for ok in [
            "85c9f954-9529-453a-9777-90778bf46f94.webp",
            "1ad0d5dd6a03ac29.jpg",
            "a",
            "x_y.PNG",
        ] {
            assert!(valid_file(ok), "{ok}");
        }
        for bad in [
            "",
            ".hidden",
            "../etc/passwd",
            "a/b",
            "a b",
            "a%2Fb",
            "a\\b",
            &"x".repeat(201),
        ] {
            assert!(!valid_file(bad), "{bad}");
        }
    }

    #[test]
    fn only_the_four_poster_types_pass_and_the_bucket_type_is_kept() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a..."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg xmlns=..."), None);
        assert_eq!(sniff(b"<html>"), None);
        assert_eq!(
            served_type(Some("application/octet-stream"), "image/jpeg"),
            "application/octet-stream"
        );
        assert_eq!(served_type(Some("image/webp"), "image/webp"), "image/webp");
        assert_eq!(
            served_type(Some("text/html; charset=utf-8"), "image/png"),
            "image/png",
            "a type the bucket should not send is replaced by the sniffed one"
        );
        assert_eq!(served_type(None, "image/gif"), "image/gif");
    }
}
