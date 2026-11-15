//! `/scene-stream-access` (GET/POST/PUT/DELETE, scene signed): the RTMP
//! ingress credential of a place's stream. The signer must own or
//! administer the place the signed metadata stands on. List, add and reset
//! require the signed scene to be the current deployment on its parcel and
//! authorize against that deployment's footprint and the place based on its
//! base parcel; remove needs only the place (a world by its realm name, a
//! Genesis place by its parcel), so a key can still be revoked after a
//! redeploy. List, add and reset are refused to
//! a platform-banned wallet BEFORE that check, because LiveKit honours the
//! key without re-checking the wallet, and remove stays ungated (upstream's
//! `*-scene-stream-access-handler.ts`). One active row per place in
//! `social.scene_stream_access`; the ingress itself is LiveKit's (twirp
//! `livekit.Ingress`), and the LiveKit webhook keeps the active row's
//! `streaming` flag. `/cast/*` (Cast 2.0) was a 501 stub in the old service
//! and answers the same here.

use crate::shared::now_utc;
use axum::Json;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::admission::scene::{self, PlaceLookup, meta_str, realm_name_from_metadata};
use super::auth::{SCENE_SIGNER, device_identifier, verify_signed_fetch};
use super::errors::{ApiError, forbidden, service_unavailable};
use super::livekit::IngressClient;
use super::moderation::context::Context;
use super::moderation::store::PLATFORM_BANNED_MSG;
use super::{CommsState, SharedState};
use crate::shared::signed_fetch::SignedPath;
use crate::world_identity::{WorldName, is_world_realm_name};
use crate::{Address, AuthorityNotEstablished, PermissionDecision};

#[path = "streaming_notifications.rs"]
pub(super) mod notifications;

#[path = "streaming_jobs.rs"]
mod jobs;
pub(super) use jobs::spawn_jobs;

pub const PATH: &str = "/scene-stream-access";
pub const FOUR_DAYS_MS: i64 = 4 * 24 * 60 * 60 * 1000;
pub const STREAM_ACCESS_NOT_FOUND_MSG: &str = "No active streaming access found for place";
pub const STREAM_RESET_FAILED_MSG: &str = "Failed to reset scene stream access";
pub const NOT_AUTHORIZED_TO_STREAM_MSG: &str = "you are not authorized to stream to this scene";
pub const CAST_NOT_PORTED_MSG: &str = "Cast 2.0 WebRTC presenter surface (stream links, streamer/watcher/bot tokens, presenter \
     promote/demote) is not served; RTMP scene-stream ingress is served by /scene-stream-access";

/// The wire body (upstream's `SceneStreamAccessResponse`), times in ms.
#[derive(Serialize, PartialEq, Eq)]
pub struct SceneStreamAccessResponse {
    pub streaming_url: String,
    pub streaming_key: String,
    pub created_at: i64,
    pub ends_at: i64,
}

impl std::fmt::Debug for SceneStreamAccessResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneStreamAccessResponse")
            .field("streaming_url", &self.streaming_url)
            .field("streaming_key", &"<set>")
            .field("created_at", &self.created_at)
            .field("ends_at", &self.ends_at)
            .finish()
    }
}

#[derive(sqlx::FromRow)]
struct AccessRow {
    id: Uuid,
    room_id: Option<String>,
    streaming: bool,
    streaming_url: String,
    streaming_key: String,
    ingress_id: Option<String>,
    created_at: NaiveDateTime,
    expiration_time: Option<NaiveDateTime>,
}

impl AccessRow {
    fn response(self) -> SceneStreamAccessResponse {
        let created_ms = self.created_at.and_utc().timestamp_millis();
        let ends_ms = self
            .expiration_time
            .map(|t| t.and_utc().timestamp_millis())
            .unwrap_or(created_ms + FOUR_DAYS_MS);
        SceneStreamAccessResponse {
            streaming_url: self.streaming_url,
            streaming_key: self.streaming_key,
            created_at: created_ms,
            ends_at: ends_ms,
        }
    }
}

const ACCESS_COLUMNS: &str =
    "id, room_id, streaming, streaming_url, streaming_key, ingress_id, created_at, expiration_time";

pub async fn cast_any() -> ApiError {
    ApiError::http(501, CAST_NOT_PORTED_MSG)
}

pub async fn scene_stream_access(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !state.livekit.configured {
        return Err(service_unavailable(
            "LiveKit ingress is not configured (set LIVEKIT_API_KEY / LIVEKIT_API_SECRET)",
        ));
    }
    let verb = method.as_str().to_ascii_lowercase();
    let sf = verify_signed_fetch(
        &headers,
        &verb,
        &SignedPath::at(&uri, PATH),
        &[SCENE_SIGNER],
        state.eip1654(),
    )
    .await?;
    let realm_name = realm_name_from_metadata(&sf.metadata)
        .ok_or_else(|| ApiError::bad_request("invalid signed-fetch request, no realmName"))?;
    let device_id = device_identifier(&sf.metadata);
    if method != Method::DELETE {
        refuse_if_platform_banned(&state, &sf.signer, device_id.as_deref()).await?;
        if state.deny_list.is_denied(sf.signer.as_str()).await {
            return Err(forbidden(NOT_AUTHORIZED_TO_STREAM_MSG));
        }
    }
    let (place_id, place, room) = if method == Method::DELETE {
        // Revocation needs no current deployment: the place row's own footprint authorizes it.
        let world = if is_world_realm_name(&realm_name) {
            Some(
                WorldName::parse(&realm_name)
                    .map_err(|_| ApiError::bad_request("Invalid realm name"))?,
            )
        } else {
            None
        };
        let parcel = meta_str(&sf.metadata, "parcel");
        if world.is_none() && parcel.is_none() {
            return Err(ApiError::bad_request(
                "invalid signed-fetch request, no parcel",
            ));
        }
        let place_id = resolve_place_id(
            &state.pool,
            world.as_ref(),
            parcel.as_deref().unwrap_or_default(),
            false,
        )
        .await?
        .ok_or_else(|| ApiError::not_found("place not found for this scene"))?;
        let place = PlaceLookup::new(&state, &place_id);
        (place_id, place, None)
    } else {
        // One resolution binds the signed scene to its parcel; the room and the
        // authorizing footprint both come from it.
        let bound = Context::resolve(&state, &sf.metadata).await?;
        let world = bound.world_scene.as_ref().map(|(world, _)| world);
        let base = bound.footprint.base_position.clone().unwrap_or_default();
        let place_id = resolve_place_id(&state.pool, world, &base, true)
            .await?
            .ok_or_else(|| ApiError::not_found("place not found for this scene"))?;
        let place = PlaceLookup::resolved(&state, &place_id, bound.footprint);
        (place_id, place, Some(bound.room))
    };
    match scene::scene_owner_or_admin_decision(&state, &place, &sf.signer).await {
        PermissionDecision::Allowed => {}
        PermissionDecision::Refused(AuthorityNotEstablished::RefusedLacksAuthority { .. }) => {
            return Err(forbidden(NOT_AUTHORIZED_TO_STREAM_MSG));
        }
        PermissionDecision::Refused(why) => return Err(why.into()),
    }
    let room = room.unwrap_or_default();
    let ingress = state.ingress();
    match method {
        Method::GET => match active_access(&state.pool, &place_id).await? {
            Some(row) => {
                let mut resp = row.response();
                resp.ends_at = resp.created_at + FOUR_DAYS_MS;
                Ok(Json(resp).into_response())
            }
            None => Err(ApiError::not_found(STREAM_ACCESS_NOT_FOUND_MSG)),
        },
        Method::POST => {
            if let Some(existing) = active_access(&state.pool, &place_id).await? {
                match renewal(&existing, &room, now_utc()) {
                    Renewal::Reuse => return Ok(Json(existing.response()).into_response()),
                    Renewal::Conflict => return Err(renewal_conflict()),
                    Renewal::Replace => {
                        if !jobs::enqueue(
                            &state,
                            existing.id,
                            jobs::CleanupCommand::IdleReplacement,
                        )
                        .await?
                        {
                            return Err(renewal_conflict());
                        }
                        jobs::cleanup_access(&state, existing.id).await?;
                    }
                }
            }
            Ok(
                Json(create_access(&state.pool, &ingress, &place_id, &room, &sf.signer).await?)
                    .into_response(),
            )
        }
        Method::PUT => reset_access(&state, &ingress, &place_id, &room, &sf.signer)
            .await
            .map(|resp| Json(resp).into_response()),
        Method::DELETE => remove_access(&state, &ingress, &place_id).await,
        _ => Ok(StatusCode::METHOD_NOT_ALLOWED.into_response()),
    }
}

async fn refuse_if_platform_banned(
    state: &CommsState,
    signer: &Address,
    device_id: Option<&str>,
) -> Result<(), ApiError> {
    match crate::gatekeeper::moderation::admission_ban(
        state.policy.as_ref(),
        state.bans.as_ref(),
        signer,
        None,
        device_id,
        chrono::Utc::now().naive_utc(),
    )
    .await
    {
        Ok(None) => Ok(()),
        Ok(Some(_)) => {
            tracing::warn!(address = %signer, "rejected a stream key request from a platform-banned user");
            Err(forbidden(PLATFORM_BANNED_MSG))
        }
        Err(e) => Err(e.into()),
    }
}

/// The place the bound scene stands on: its world (published here or
/// mirrored) by name, else the Genesis place holding `parcel`. With
/// `based_there` only a place based on `parcel` counts, so a catalogue that
/// has not caught up with a redeploy never lends another scene's admins.
async fn resolve_place_id(
    pool: &PgPool,
    world: Option<&WorldName>,
    parcel: &str,
    based_there: bool,
) -> Result<Option<String>, ApiError> {
    if let Some(world) = world {
        return Ok(sqlx::query_scalar(
            "SELECT id FROM explore.place WHERE world AND lower(world_name) = lower($1) \
             UNION ALL \
             SELECT id FROM explore.place_world_local WHERE world AND lower(world_name) = lower($1) \
             LIMIT 1",
        )
        .bind(world.display())
        .fetch_optional(pool)
        .await?);
    }
    // A live Genesis place on the parcel, preferring the one based there.
    Ok(sqlx::query_scalar(
        "SELECT id FROM explore.place WHERE NOT world AND NOT disabled \
         AND raw->'positions' @> jsonb_build_array($1::text) \
         AND (NOT $2 OR base_position = $1) \
         ORDER BY base_position = $1 DESC, deployed_at DESC NULLS LAST, id LIMIT 1",
    )
    .bind(parcel)
    .bind(based_there)
    .fetch_optional(pool)
    .await?)
}

async fn active_access(pool: &PgPool, place_id: &str) -> Result<Option<AccessRow>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCESS_COLUMNS} FROM social.scene_stream_access WHERE place_id = $1 AND active"
    )))
    .bind(place_id)
    .fetch_optional(pool)
    .await
}

/// Mints (or reuses) the room's ingress and records it as the place's
/// active credential. A concurrent mint that recorded first wins: its row is
/// answered, and an ingress this call created for nothing is deleted.
async fn create_access(
    pool: &PgPool,
    ingress: &IngressClient<'_>,
    place_id: &str,
    room: &str,
    signer: &Address,
) -> Result<SceneStreamAccessResponse, ApiError> {
    let participant_identity = format!("{}-streamer", Uuid::new_v4());
    let info = ingress
        .create_ingress(room, &participant_identity)
        .await
        .map_err(|e| ApiError::internal(format!("livekit ingress create: {e}")))?;
    let (Some(url), Some(stream_key)) = (info.url.as_deref(), info.stream_key.as_deref()) else {
        return Err(ApiError::internal(
            "livekit ingress response missing url or stream key",
        ));
    };
    let now = now_utc();
    let inserted: Option<AccessRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO social.scene_stream_access \
             (place_id, streaming_url, streaming_key, ingress_id, room_id, generated_by, created_at, expiration_time, active) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7 + make_interval(secs => $8::double precision / 1000), true) \
         ON CONFLICT (place_id) WHERE active DO NOTHING \
         RETURNING {ACCESS_COLUMNS}"
    )))
    .bind(place_id)
    .bind(url)
    .bind(stream_key)
    .bind(&info.ingress_id)
    .bind(room)
    .bind(signer)
    .bind(now)
    .bind(FOUR_DAYS_MS)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = inserted {
        return Ok(row.response());
    }
    let winner = active_access(pool, place_id).await?.ok_or_else(|| {
        ApiError::internal("scene stream access vanished during a concurrent mint")
    })?;
    if !info.ingress_id.is_empty()
        && winner.ingress_id.as_deref() != Some(info.ingress_id.as_str())
        && let Err(e) = ingress.delete_ingress(&info.ingress_id).await
    {
        tracing::warn!(error = %e, place_id, "failed to delete an ingress that lost a concurrent mint");
    }
    if renewal(&winner, room, now_utc()) == Renewal::Reuse {
        Ok(winner.response())
    } else {
        Err(renewal_conflict())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Renewal {
    Reuse,
    Replace,
    Conflict,
}

fn renewal(row: &AccessRow, room: &str, now: NaiveDateTime) -> Renewal {
    let expiry = row
        .expiration_time
        .unwrap_or(row.created_at + chrono::Duration::days(4));
    if row.room_id.as_deref() == Some(room) && expiry > now {
        Renewal::Reuse
    } else if row.streaming {
        Renewal::Conflict
    } else {
        Renewal::Replace
    }
}

fn renewal_conflict() -> ApiError {
    ApiError::http(409, "Cannot renew streaming access while streaming")
}

async fn reset_access(
    state: &CommsState,
    ingress: &IngressClient<'_>,
    place_id: &str,
    room: &str,
    signer: &Address,
) -> Result<SceneStreamAccessResponse, ApiError> {
    let result = async {
        let row = active_access(&state.pool, place_id)
            .await?
            .ok_or_else(|| ApiError::not_found(STREAM_ACCESS_NOT_FOUND_MSG))?;
        if !jobs::enqueue(state, row.id, jobs::CleanupCommand::Reset).await? {
            return Err(renewal_conflict());
        }
        // The durable intent owns the old ingress's deletion and the reset notification,
        // delivered only once the replacement credential exists; it also covers a crash here.
        let response = create_access(&state.pool, ingress, place_id, room, signer).await?;
        jobs::cleanup_access(state, row.id).await?;
        Ok(response)
    }
    .await;
    result.map_err(|error: ApiError| {
        tracing::error!(place_id, error = %error, "scene stream access reset failed");
        ApiError::http(500, STREAM_RESET_FAILED_MSG)
    })
}

async fn remove_access(
    state: &CommsState,
    _ingress: &IngressClient<'_>,
    place_id: &str,
) -> Result<Response, ApiError> {
    let row = active_access(&state.pool, place_id)
        .await?
        .ok_or_else(|| ApiError::not_found("no active stream access for this scene"))?;
    if !jobs::enqueue(state, row.id, jobs::CleanupCommand::Revoke).await? {
        return Err(renewal_conflict());
    }
    jobs::cleanup_access(state, row.id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) enum IngressEvent {
    Started,
    Ended,
}

/// The LiveKit webhook's ingress events: the active row of that ingress
/// starts streaming (keeping the first start time) or stops.
pub(super) async fn ingress_event(
    state: &CommsState,
    event: IngressEvent,
    ingress_id: Option<&str>,
) -> Result<(), ApiError> {
    let Some(ingress_id) = ingress_id.filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    match event {
        IngressEvent::Started => {
            sqlx::query(
                "UPDATE social.scene_stream_access SET streaming = true, streaming_start_time = CASE WHEN streaming THEN COALESCE(streaming_start_time,$2) ELSE $2 END, streaming_state_version=streaming_state_version+1 \
                 WHERE ingress_id = $1 AND active",
            )
            .bind(ingress_id)
            .bind(now_utc())
            .execute(&state.pool)
            .await?;
        }
        IngressEvent::Ended => {
            sqlx::query("UPDATE social.scene_stream_access SET streaming = false, streaming_state_version=streaming_state_version+1 WHERE ingress_id = $1 AND active").bind(ingress_id).execute(&state.pool).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::gatekeeper::{Settings, router};
    use crate::http::Outbound;
    use crate::interfaces::test_support::{StubPolicy, StubScenes};

    #[test]
    fn the_access_body_is_upstreams_shape_and_hides_the_key_from_debug() {
        let body = SceneStreamAccessResponse {
            streaming_url: "rtmp://ingest/x".into(),
            streaming_key: "sk_abc".into(),
            created_at: 1_700_000_000_000,
            ends_at: 1_700_345_600_000,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({ "streaming_url": "rtmp://ingest/x", "streaming_key": "sk_abc", "created_at": 1_700_000_000_000i64, "ends_at": 1_700_345_600_000i64 })
        );
        assert!(!format!("{body:?}").contains("sk_abc"));
        assert_eq!(FOUR_DAYS_MS, 345_600_000);
    }

    #[test]
    fn renewal_preserves_valid_keys_and_refuses_only_conflicting_streams() {
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .naive_utc();
        let mut row = AccessRow {
            id: Uuid::nil(),
            room_id: Some("room".into()),
            streaming: true,
            streaming_url: "rtmp://example.org".into(),
            streaming_key: "key".into(),
            ingress_id: None,
            created_at: now,
            expiration_time: None,
        };
        assert_eq!(renewal(&row, "room", now), Renewal::Reuse);
        assert_eq!(renewal(&row, "new-room", now), Renewal::Conflict);
        assert_eq!(
            renewal(&row, "room", now + chrono::Duration::days(4)),
            Renewal::Conflict
        );
        row.streaming = false;
        assert_eq!(renewal(&row, "new-room", now), Renewal::Replace);
        assert_eq!(
            renewal(&row, "room", now + chrono::Duration::days(4)),
            Renewal::Replace
        );
    }

    #[tokio::test]
    async fn cast_answers_the_old_501_and_an_unconfigured_livekit_503() {
        let pool = PgPool::connect_lazy("postgres://postgres@127.0.0.1:1/postgres").unwrap();
        let unconfigured = Settings {
            livekit: super::super::LiveKit {
                configured: false,
                ..Settings::for_tests().livekit
            },
            ..Settings::for_tests()
        };
        let app = router(Arc::new(CommsState::new(
            pool,
            Outbound::for_tests(),
            unconfigured,
            None,
            None,
            Arc::new(StubPolicy::default()),
            Arc::new(StubScenes::default()),
            Arc::new(crate::presence::Presence::new()),
            Arc::new(crate::interfaces::test_support::StubDirectory::default()),
        )));
        let resp = app
            .clone()
            .oneshot(
                Request::get("/cast/presenters")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let resp = app
            .oneshot(Request::get(PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

#[cfg(test)]
mod moderation_boundary_tests {
    use super::*;
    use crate::gatekeeper::moderation::test_support::{StubBanStatus, state};
    use crate::interfaces::test_support::StubPolicy;
    #[tokio::test]
    async fn operator_block_refuses_streaming_without_ingress_or_permission_io() {
        let state = state(
            StubPolicy {
                blocked: Ok(true),
                ..Default::default()
            },
            StubBanStatus(Err(crate::AuthorityNotEstablished::store_unavailable(
                "unexpected raw read",
                &sqlx::Error::PoolClosed,
            ))),
        );
        let wallet = Address::parse("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(
            refuse_if_platform_banned(&state, &wallet, None)
                .await
                .unwrap_err()
                .code,
            403
        );
    }
    #[tokio::test]
    async fn block_authority_fault_keeps_streaming_unavailable() {
        let state = state(
            StubPolicy {
                blocked: Err(crate::AuthorityNotEstablished::store_unavailable(
                    "worlds.blocked_wallets",
                    &sqlx::Error::PoolClosed,
                )),
                ..Default::default()
            },
            StubBanStatus::default(),
        );
        let wallet = Address::parse("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(
            refuse_if_platform_banned(&state, &wallet, None)
                .await
                .unwrap_err()
                .code,
            503
        );
    }
}
