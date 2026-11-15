//! The photo events, recorded for in-process consumers in the append-only
//! outbox `camera_reel.photo_events` instead of being published to a
//! broker. Each row's `payload` is the body camera-reel-service published to
//! SNS, byte for byte in shape: `{type: "camera", subType, key, timestamp,
//! metadata}`.
//!
//! - `photo-taken` ([`photo_taken`]): written in the transaction that
//!   finalises an upload once both blobs are stored (the one that marks
//!   `ranking_published` and applies the community photo counts through
//!   `social::communities::ranking::record_photo`), so a refused or failed
//!   upload records none and a finalisation the repair job completes records
//!   it then. Public and private photos alike; `timestamp` is the upload's
//!   `created_at` in unix seconds.
//! - `photo-privacy-changed` ([`photo_privacy_changed`]): written in the
//!   statement that changes a photo's visibility, only when the owner's
//!   update changed it (a request for the visibility it already has records
//!   nothing).
//!
//! The Credits photo-goal worker reads `photo-taken` rows with a bounded
//! cursor and periodic reconciliation for transactions that commit late.
//! The test-only `read_after` helper checks the outbox's ordered read shape.
//! The outbox keeps no per-consumer state.
//! Rows older than [`RETENTION`] are deleted by the fleet-wide
//! `camera-photo-events-retention` job ([`spawn_retention_job`]), so a
//! consumer that falls further behind misses events. The Credits program
//! also records its own receipt and progress transactionally.

use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::dto::Metadata;
use crate::jobs::{Jobs, PeriodicCfg};

pub const PHOTO_TAKEN: &str = "photo-taken";
pub const PHOTO_PRIVACY_CHANGED: &str = "photo-privacy-changed";

/// How long an event stays readable.
pub const RETENTION: chrono::TimeDelta = chrono::TimeDelta::days(30);
/// Rows one retention statement deletes.
const RETENTION_BATCH: i64 = 1000;
/// Statements one retention pass runs before yielding to the next pass.
const RETENTION_MAX_BATCHES: usize = 100;
const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// One outbox row.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct PhotoEvent {
    pub id: i64,
    /// [`PHOTO_TAKEN`] or [`PHOTO_PRIVACY_CHANGED`].
    pub kind: String,
    pub photo_id: Uuid,
    /// Lowercase: the photo's owner.
    pub user_address: String,
    /// The upstream event body.
    pub payload: Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// The `photo-taken` body: `sceneId` carries the scene's name, as upstream
/// sends it; `users` lists every visible person with the address as sent
/// and `isEmoting` false when absent.
pub fn photo_taken(photo_id: Uuid, is_public: bool, metadata: &Metadata, timestamp: i64) -> Value {
    let users: Vec<Value> = metadata
        .visible_people
        .iter()
        .map(|user| {
            json!({
                "address": user.user_address,
                "isEmoting": user.is_emoting.unwrap_or(false),
            })
        })
        .collect();
    json!({
        "type": "camera",
        "subType": PHOTO_TAKEN,
        "key": photo_id.to_string(),
        "timestamp": timestamp,
        "metadata": {
            "sceneId": metadata.scene.name,
            "realm": metadata.realm,
            "userAddress": metadata.user_address.to_lowercase(),
            "isPublic": is_public,
            "photoId": photo_id.to_string(),
            "placeId": metadata.place_id,
            "users": users,
        },
    })
}

/// The `photo-privacy-changed` body; `userAddress` is the owner who changed it.
pub fn photo_privacy_changed(
    photo_id: Uuid,
    user_address: &str,
    is_public: bool,
    timestamp: i64,
) -> Value {
    json!({
        "type": "camera",
        "subType": PHOTO_PRIVACY_CHANGED,
        "key": photo_id.to_string(),
        "timestamp": timestamp,
        "metadata": {
            "photoId": photo_id.to_string(),
            "userAddress": user_address.to_lowercase(),
            "isPublic": is_public,
        },
    })
}

/// Records one event; the caller's executor is its transaction.
pub(crate) async fn record<'e>(
    executor: impl PgExecutor<'e>,
    kind: &str,
    photo_id: Uuid,
    user_address: &str,
    payload: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO camera_reel.photo_events (kind, photo_id, user_address, payload) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(kind)
    .bind(photo_id)
    .bind(user_address.to_lowercase())
    .bind(sqlx::types::Json(payload))
    .execute(executor)
    .await?;
    Ok(())
}

/// Up to `limit` events with an id above `after_id`, in id order. An id is
/// assigned when its transaction inserts the row, so a consumer that holds
/// its cursor past an id still being committed by a slower transaction
/// could skip it; read with a margin (re-read the last few ids, dedupe on
/// `id`) when that matters.
#[cfg(test)]
pub async fn read_after<'e>(
    executor: impl PgExecutor<'e>,
    after_id: i64,
    limit: i64,
) -> Result<Vec<PhotoEvent>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, kind, photo_id, user_address, payload, created_at \
         FROM camera_reel.photo_events WHERE id > $1 ORDER BY id LIMIT $2",
    )
    .bind(after_id)
    .bind(limit.max(0))
    .fetch_all(executor)
    .await
}

/// Deletes the events created before `cutoff`, at most
/// `RETENTION_BATCH * RETENTION_MAX_BATCHES` rows, one bounded statement at
/// a time; returns how many went.
pub async fn delete_expired(
    pool: &PgPool,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    let mut deleted = 0;
    for _ in 0..RETENTION_MAX_BATCHES {
        let n = sqlx::query(
            "DELETE FROM camera_reel.photo_events WHERE id IN ( \
                 SELECT id FROM camera_reel.photo_events WHERE created_at < $1 \
                 ORDER BY created_at LIMIT $2)",
        )
        .bind(cutoff)
        .bind(RETENTION_BATCH)
        .execute(pool)
        .await?
        .rows_affected();
        deleted += n;
        if n < RETENTION_BATCH as u64 {
            break;
        }
    }
    Ok(deleted)
}

/// The hourly retention pass, one process of the fleet at a time.
pub fn spawn_retention_job(state: &Arc<crate::State>, jobs: &Jobs) {
    let pool = state.db.pool().clone();
    jobs.spawn_periodic_leased(
        "camera-photo-events-retention",
        PeriodicCfg::every(RETENTION_INTERVAL)
            .with_jitter(Duration::from_secs(60))
            .with_error_backoff(Duration::from_secs(6 * 60 * 60)),
        move |ctx| {
            let pool = pool.clone();
            async move {
                let deleted = delete_expired(&pool, chrono::Utc::now() - RETENTION).await?;
                ctx.progress(format!("{deleted} expired photo events deleted"));
                Ok(())
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{Scene, User};

    #[test]
    fn photo_taken_is_the_upstream_sns_body() {
        let id = Uuid::parse_str("6f1c0f6a-1a2b-4c3d-8e4f-0123456789ab").unwrap();
        let metadata = Metadata {
            user_name: "tester".into(),
            user_address: "0xAbC0000000000000000000000000000000000001".into(),
            date_time: "1700000000".into(),
            realm: "main".into(),
            scene: Scene {
                name: "Genesis Plaza".into(),
                ..Scene::default()
            },
            visible_people: vec![
                User {
                    user_address: "0xDeF0000000000000000000000000000000000002".into(),
                    is_emoting: None,
                    ..User::default()
                },
                User {
                    user_address: "0xabc0000000000000000000000000000000000001".into(),
                    is_emoting: Some(true),
                    ..User::default()
                },
            ],
            place_id: "place-1".into(),
        };
        assert_eq!(
            photo_taken(id, false, &metadata, 1_700_000_123),
            json!({
                "type": "camera",
                "subType": "photo-taken",
                "key": "6f1c0f6a-1a2b-4c3d-8e4f-0123456789ab",
                "timestamp": 1_700_000_123,
                "metadata": {
                    "sceneId": "Genesis Plaza",
                    "realm": "main",
                    "userAddress": "0xabc0000000000000000000000000000000000001",
                    "isPublic": false,
                    "photoId": "6f1c0f6a-1a2b-4c3d-8e4f-0123456789ab",
                    "placeId": "place-1",
                    "users": [
                        { "address": "0xDeF0000000000000000000000000000000000002", "isEmoting": false },
                        { "address": "0xabc0000000000000000000000000000000000001", "isEmoting": true }
                    ]
                }
            })
        );
    }

    #[test]
    fn photo_privacy_changed_is_the_upstream_sns_body() {
        let id = Uuid::parse_str("6f1c0f6a-1a2b-4c3d-8e4f-0123456789ab").unwrap();
        assert_eq!(
            photo_privacy_changed(id, "0xAbC0000000000000000000000000000000000001", true, 42),
            json!({
                "type": "camera",
                "subType": "photo-privacy-changed",
                "key": "6f1c0f6a-1a2b-4c3d-8e4f-0123456789ab",
                "timestamp": 42,
                "metadata": {
                    "photoId": "6f1c0f6a-1a2b-4c3d-8e4f-0123456789ab",
                    "userAddress": "0xabc0000000000000000000000000000000000001",
                    "isPublic": true
                }
            })
        );
    }
}
