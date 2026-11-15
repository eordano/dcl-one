//! The old crate's contract tests over the module as `serve` mounts it
//! (nested under `/camera-reel-service`), plus the route-level walks the
//! consolidation adds: the explorer boot corpus shapes (rows 2 and 34 of
//! `tests/explorer-boot.md`), the write gate, an upload over signed fetch
//! served back from the ONE blob store, the keyset pages and the moderator's
//! review. DB-backed tests use the process's scratch database
//! (`CATALYST_TEST_DATABASE_URL`), rows scoped by random wallets and
//! uuid-suffixed hashes, and delete their rows at the end; without the
//! variable they skip.

use std::sync::Arc;

use alloy::signers::{Signer as _, local::PrivateKeySigner};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::db::{BlobVerdict, Database, NewImage, Page, WITHHELD_DELETED, WITHHELD_REJECTED};
use super::dto::{Image, Metadata, ScreenRect, User};
use super::handlers::images::{RankingSchedule, RankingSweep};
use super::http::{ApiError, ForbiddenError, ForbiddenReason};
use super::{Settings, SharedState, State, router};
use crate::auth::{AuthLink, AuthLinkType, hash_bytes_v1};
use crate::shared::signed_fetch::{
    AUTH_CHAIN_HEADER_PREFIX, AUTH_METADATA_HEADER, AUTH_TIMESTAMP_HEADER, build_legacy_payload,
    build_payload_v6,
};
use crate::storage::Storage;
use tokio::time::{Duration, Instant};

mod captures;
mod signed_fetch;

const API_URL: &str = "http://127.0.0.1:5149/camera-reel-service";
const PREFIX: &str = "/camera-reel-service";

async fn parts(err: ApiError) -> (StatusCode, String, Value) {
    let resp = err.into_response();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, content_type, value)
}

fn assert_json_content_type(content_type: &str) {
    assert!(
        content_type.starts_with("application/json"),
        "content-type was {content_type}"
    );
}

fn key_set(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("body is a json object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn max_limit_reached_matches_upstream_forbidden_error() {
    let message = "you have reached the limit of 500 max images";
    let (status, content_type, value) = parts(ApiError::MaxLimitReached(message.to_string())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_json_content_type(&content_type);
    assert_eq!(
        value,
        json!({ "reason": "maxLimitReached", "message": message })
    );
    assert_eq!(key_set(&value), vec!["message", "reason"]);
}

#[tokio::test]
async fn max_limit_reached_roundtrips_through_typed_struct() {
    let message = "you have reached the limit of 500 max images";
    let (_status, _content_type, value) =
        parts(ApiError::MaxLimitReached(message.to_string())).await;
    let typed: ForbiddenError = serde_json::from_value(value).unwrap();
    assert_eq!(typed.reason, ForbiddenReason::MaxLimitReached);
    assert_eq!(typed.message, message);
}

#[tokio::test]
async fn bad_request_matches_upstream_response_error() {
    let (status, content_type, value) =
        parts(ApiError::BadRequest("invalid metadata".to_string())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_json_content_type(&content_type);
    assert_eq!(value, json!({ "message": "invalid metadata" }));
    assert_eq!(key_set(&value), vec!["message"]);
}

#[tokio::test]
async fn unauthorized_matches_bare_message_envelope() {
    let (status, content_type, value) = parts(ApiError::Unauthorized).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_json_content_type(&content_type);
    assert_eq!(value, json!({ "message": "Unauthorized" }));
    assert_eq!(key_set(&value), vec!["message"]);
}

#[tokio::test]
async fn forbidden_matches_upstream_response_error() {
    let (status, content_type, value) = parts(ApiError::Forbidden("forbidden".to_string())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_json_content_type(&content_type);
    assert_eq!(value, json!({ "message": "forbidden" }));
    assert_eq!(key_set(&value), vec!["message"]);
}

#[tokio::test]
async fn not_found_matches_upstream_response_error() {
    let (status, content_type, value) =
        parts(ApiError::NotFound("image not found".to_string())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_json_content_type(&content_type);
    assert_eq!(value, json!({ "message": "image not found" }));
    assert_eq!(key_set(&value), vec!["message"]);
}

#[tokio::test]
async fn bad_gateway_matches_upstream_response_error() {
    let (status, content_type, value) = parts(ApiError::BadGateway(
        "failed to resolve world name".to_string(),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_json_content_type(&content_type);
    assert_eq!(value, json!({ "message": "failed to resolve world name" }));
    assert_eq!(key_set(&value), vec!["message"]);
}

#[tokio::test]
async fn internal_matches_upstream_response_error() {
    let (status, content_type, value) =
        parts(ApiError::Internal("database error".to_string())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_json_content_type(&content_type);
    assert_eq!(value, json!({ "message": "database error" }));
    assert_eq!(key_set(&value), vec!["message"]);
}

struct Scratch {
    state: SharedState,
    owners: Vec<String>,
    /// Hashes a test may have withheld (`camera_reel.withheld_blobs` rows
    /// outlive their image row by design); `done` deletes them.
    hashes: Vec<String>,
    dir: std::path::PathBuf,
}

async fn scratch(settings: Settings) -> Option<Scratch> {
    let pool = catalyst_db::scratch::pool(4).await?;
    let dir =
        std::env::temp_dir().join(format!("catalyst-camera-reel-{}", Uuid::new_v4().simple()));
    let storage = Arc::new(
        Storage::from_config(&dir)
            .await
            .expect("a scratch blob store"),
    );
    let state = Arc::new(State::new(pool, storage, settings, None));
    Some(Scratch {
        state,
        owners: Vec::new(),
        hashes: Vec::new(),
        dir,
    })
}

impl Scratch {
    fn db(&self) -> &Database {
        &self.state.db
    }

    fn app(&self) -> Router {
        Router::new().nest(PREFIX, router(self.state.clone()))
    }

    fn owner(&mut self, address: &str) -> String {
        self.owners.push(address.to_lowercase());
        address.to_string()
    }

    fn hash(&mut self, hash: &str) -> String {
        self.hashes.push(hash.to_string());
        hash.to_string()
    }

    async fn withheld(&self, hash: &str) -> Option<String> {
        sqlx::query_scalar("SELECT reason FROM camera_reel.withheld_blobs WHERE hash = $1")
            .bind(hash)
            .fetch_optional(self.db().pool())
            .await
            .unwrap()
    }

    async fn is_public(&self, id: Uuid) -> Option<bool> {
        sqlx::query_scalar("SELECT is_public FROM camera_reel.images WHERE id = $1")
            .bind(id)
            .fetch_optional(self.db().pool())
            .await
            .unwrap()
    }

    async fn count(&self, owner: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM camera_reel.images WHERE user_address = $1")
            .bind(owner.to_lowercase())
            .fetch_one(self.db().pool())
            .await
            .unwrap()
    }

    async fn done(self) {
        sqlx::query("DELETE FROM camera_reel.images WHERE user_address = ANY($1)")
            .bind(&self.owners)
            .execute(self.db().pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM camera_reel.photo_events WHERE user_address = ANY($1)")
            .bind(&self.owners)
            .execute(self.db().pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM camera_reel.withheld_blobs WHERE hash = ANY($1)")
            .bind(&self.hashes)
            .execute(self.db().pool())
            .await
            .unwrap();
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }
}

fn random_address() -> String {
    format!("{:#x}", PrivateKeySigner::random().address())
}

fn tag() -> String {
    Uuid::new_v4().simple().to_string()
}

fn image(owner: &str, hash: &str, thumb: &str, is_public: bool, place_id: &str) -> NewImage {
    NewImage {
        id: Uuid::new_v4(),
        image_hash: hash.to_string(),
        thumbnail_hash: thumb.to_string(),
        is_public,
        metadata: Metadata {
            user_name: "owner".to_string(),
            user_address: owner.to_string(),
            date_time: "1700000000".to_string(),
            place_id: place_id.to_string(),
            ..Metadata::default()
        },
    }
}

struct Wallet {
    signer: PrivateKeySigner,
    address: String,
}

fn wallet() -> Wallet {
    let signer = PrivateKeySigner::random();
    let address = format!("{:#x}", signer.address());
    Wallet { signer, address }
}

async fn signed_headers(w: &Wallet, method: &str, path: &str, v6: bool) -> Vec<(String, String)> {
    let timestamp = chrono::Utc::now().timestamp_millis().to_string();
    let payload = if v6 {
        build_payload_v6(method, path, &timestamp, "{}")
    } else {
        build_legacy_payload(method, path, &timestamp, "{}")
    };
    let signature = w
        .signer
        .sign_message(payload.as_bytes())
        .await
        .unwrap()
        .to_string();
    let chain = [
        AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: format!("{:#x}", w.signer.address()),
            signature: Some(String::new()),
        },
        AuthLink {
            link_type: AuthLinkType::EcdsaSignedEntity,
            payload,
            signature: Some(signature),
        },
    ];
    let mut out = vec![
        (AUTH_TIMESTAMP_HEADER.to_string(), timestamp),
        (AUTH_METADATA_HEADER.to_string(), "{}".to_string()),
    ];
    for (i, link) in chain.iter().enumerate() {
        out.push((
            format!("{AUTH_CHAIN_HEADER_PREFIX}{i}"),
            serde_json::to_string(link).unwrap(),
        ));
    }
    out
}

async fn send(
    app: Router,
    method: &str,
    path: &str,
    headers: Vec<(String, String)>,
    body: Option<(Vec<u8>, &str)>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(path);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let req = match body {
        Some((bytes, content_type)) => req
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(bytes))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 32 << 20).await.unwrap();
    (status, headers, bytes.to_vec())
}

fn json_of(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(bytes).unwrap()
    }
}

fn png(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x as u8).wrapping_mul(seed), y as u8, seed, 255])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn multipart(
    image: &[u8],
    content_type: &str,
    metadata: &Value,
    is_public: Option<bool>,
) -> (Vec<u8>, String) {
    let boundary = format!("----catalyst{}", tag());
    let mut body = Vec::new();
    let mut part = |name: &str, filename: Option<&str>, ct: Option<&str>, data: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n")
                    .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            ),
        }
        if let Some(ct) = ct {
            body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    };
    part("image", Some("shot.png"), Some(content_type), image);
    part(
        "metadata",
        None,
        Some("application/json"),
        metadata.to_string().as_bytes(),
    );
    if let Some(public) = is_public {
        part("is_public", None, None, public.to_string().as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

fn metadata_json(address: &str, place_id: &str) -> Value {
    json!({
        "userName": "tester",
        "userAddress": address,
        "dateTime": "1700000000",
        "realm": "main",
        "scene": { "name": "Genesis Plaza", "location": { "x": "0", "y": "0" } },
        "visiblePeople": [],
        "placeId": place_id
    })
}

#[tokio::test]
async fn photo_ranking_finalization_is_atomic_repairable_and_never_counts_incomplete_blobs() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let community = Uuid::new_v4();
    let place = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Photo ranking',$2)",
    )
    .bind(community)
    .bind(&owner)
    .execute(s.db().pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO social.community_places(community_id,place_id,added_by) VALUES($1,$2,$3)",
    )
    .bind(community)
    .bind(&place)
    .bind(&owner)
    .execute(s.db().pool())
    .await
    .unwrap();
    let bytes = bytes::Bytes::from_static(b"complete image");
    let thumb = bytes::Bytes::from_static(b"complete thumbnail");
    let mut photo = image(
        &owner,
        &hash_bytes_v1(&bytes),
        &hash_bytes_v1(&thumb),
        true,
        &place,
    );
    // Own the final keyset range: other parallel tests deliberately insert
    // invalid placeholder hashes and do not share this test's blob store.
    photo.id = Uuid::from_u128(u128::MAX - 1);
    s.db()
        .insert_image_within_limit(&photo, 500)
        .await
        .unwrap()
        .unwrap();
    s.state
        .storage
        .store(&photo.image_hash, bytes)
        .await
        .unwrap();
    let mut sweep = RankingSweep::new(Instant::now());
    sweep.after = Uuid::from_u128(u128::MAX - 2);
    s.state
        .repair_ranking_publications(&mut sweep, false, Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    let pending: bool =
        sqlx::query_scalar("SELECT NOT ranking_published FROM camera_reel.images WHERE id=$1")
            .bind(photo.id)
            .fetch_one(s.db().pool())
            .await
            .unwrap();
    assert!(pending, "a missing thumbnail does not publish an upload");
    assert_eq!(
        sweep.after, photo.id,
        "an incomplete upload advances the cursor"
    );
    s.state
        .repair_ranking_publications(&mut sweep, false, Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(
        sweep.after,
        Uuid::nil(),
        "the empty final page wraps the cursor"
    );
    s.state
        .storage
        .store(&photo.thumbnail_hash, thumb)
        .await
        .unwrap();
    let function = format!("ranking_failure_{}", Uuid::new_v4().simple());
    let trigger = format!("ranking_failure_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION social.{function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id='{community}'::uuid THEN RAISE EXCEPTION 'injected ranking failure'; END IF; RETURN NEW; END $$")))
        .execute(s.db().pool()).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER {trigger} BEFORE UPDATE ON social.communities FOR EACH ROW EXECUTE FUNCTION social.{function}()")))
        .execute(s.db().pool()).await.unwrap();
    assert!(s.db().publish_ranking(photo.id).await.is_err());
    let row:(bool,i32,i64)=sqlx::query_as("SELECT NOT i.ranking_published,c.ranking_photos_count,(SELECT count(*) FROM social.community_ranking_events r WHERE r.community_id=c.id) FROM camera_reel.images i CROSS JOIN social.communities c WHERE i.id=$1 AND c.id=$2")
        .bind(photo.id).bind(community).fetch_one(s.db().pool()).await.unwrap();
    assert_eq!(
        row,
        (true, 0, 0),
        "the receipt, counter and publication marker roll back together"
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {trigger} ON social.communities"
    )))
    .execute(s.db().pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP FUNCTION social.{function}()"
    )))
    .execute(s.db().pool())
    .await
    .unwrap();
    sweep.after = Uuid::from_u128(u128::MAX - 2);
    let (repair, direct) = tokio::join!(
        s.state.repair_ranking_publications(
            &mut sweep,
            false,
            Instant::now() + Duration::from_secs(3)
        ),
        s.db().publish_ranking(photo.id),
    );
    repair.unwrap();
    direct.unwrap();
    let count: i32 =
        sqlx::query_scalar("SELECT ranking_photos_count FROM social.communities WHERE id=$1")
            .bind(community)
            .fetch_one(s.db().pool())
            .await
            .unwrap();
    assert_eq!(count, 1, "repair and a replay publish once");
    s.db().delete_image(photo.id).await.unwrap();
    let count: i32 =
        sqlx::query_scalar("SELECT ranking_photos_count FROM social.communities WHERE id=$1")
            .bind(community)
            .fetch_one(s.db().pool())
            .await
            .unwrap();
    assert_eq!(count, 1, "deletion does not decrement a lifetime fact");
    sqlx::query("DELETE FROM social.communities WHERE id=$1")
        .bind(community)
        .execute(s.db().pool())
        .await
        .unwrap();
    s.done().await;
}

// The reserved, contiguous high UUID ranges keep each first page isolated from
// other camera tests, which use random IDs and separate real blob stores.
async fn repair_photo(s: &Scratch, owner: &str, id: Uuid, hash: &str, thumb: &str) {
    let mut photo = image(owner, hash, thumb, true, "");
    photo.id = id;
    s.db()
        .insert_image_within_limit(&photo, 500)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn ranking_repair_continues_after_first_middle_and_last_row_faults() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let start = u128::MAX - 10_000;
    let bytes = bytes::Bytes::from(format!("repair complete {}", tag()));
    let hash = hash_bytes_v1(&bytes);
    s.state.storage.store(&hash, bytes).await.unwrap();
    let broken_hash = hash_bytes_v1(tag().as_bytes());
    let broken_path =
        crate::storage::resolve_file_path(s.state.storage.root(), &broken_hash).unwrap();
    // A directory at a canonical blob path is a real stat fault, not a miss.
    tokio::fs::create_dir_all(&broken_path).await.unwrap();
    assert!(s.state.storage.exist(&broken_hash).await.is_err());
    for index in 1..=101 {
        let image = if index == 1 || index == 50 || index == 100 {
            &broken_hash
        } else {
            &hash
        };
        repair_photo(&s, &owner, Uuid::from_u128(start + index), image, &hash).await;
    }
    let failed = Uuid::from_u128(start + 75);
    let community = Uuid::new_v4();
    let place = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Repair row failure',$2)",
    )
    .bind(community)
    .bind(&owner)
    .execute(s.db().pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO social.community_places(community_id,place_id,added_by) VALUES($1,$2,$3)",
    )
    .bind(community)
    .bind(&place)
    .bind(&owner)
    .execute(s.db().pool())
    .await
    .unwrap();
    sqlx::query("UPDATE camera_reel.images SET metadata=jsonb_set(metadata,'{placeId}',to_jsonb($1::text)) WHERE id=$2")
        .bind(&place).bind(failed).execute(s.db().pool()).await.unwrap();
    let function = format!("repair_failure_{}", tag());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION social.{function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id='{community}'::uuid THEN RAISE EXCEPTION 'injected repair publication failure'; END IF; RETURN NEW; END $$")))
        .execute(s.db().pool()).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER {function} BEFORE UPDATE ON social.communities FOR EACH ROW EXECUTE FUNCTION social.{function}()")))
        .execute(s.db().pool()).await.unwrap();
    let mut sweep = RankingSweep::new(Instant::now());
    sweep.after = Uuid::from_u128(start);
    let progress = s
        .state
        .repair_ranking_publications(&mut sweep, false, Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(
        (
            progress.fetched,
            progress.inspected,
            progress.errors,
            progress.published
        ),
        (100, 100, 4, 96)
    );
    assert_eq!(progress.probes, 197);
    assert!(!progress.timed_out);
    assert_eq!(sweep.after, Uuid::from_u128(start + 100));
    let pending: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM camera_reel.images WHERE user_address=$1 AND NOT ranking_published ORDER BY id")
        .bind(&owner).fetch_all(s.db().pool()).await.unwrap();
    assert_eq!(
        pending,
        vec![
            Uuid::from_u128(start + 1),
            Uuid::from_u128(start + 50),
            failed,
            Uuid::from_u128(start + 100),
            Uuid::from_u128(start + 101)
        ]
    );
    let progress = s
        .state
        .repair_ranking_publications(&mut sweep, false, Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        progress.published >= 1,
        "the next page is not starved by faulty rows"
    );
    let published: bool =
        sqlx::query_scalar("SELECT ranking_published FROM camera_reel.images WHERE id=$1")
            .bind(Uuid::from_u128(start + 101))
            .fetch_one(s.db().pool())
            .await
            .unwrap();
    assert!(published);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {function} ON social.communities"
    )))
    .execute(s.db().pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP FUNCTION social.{function}()"
    )))
    .execute(s.db().pool())
    .await
    .unwrap();
    let facts: (i32,i64) = sqlx::query_as("SELECT ranking_photos_count,(SELECT count(*) FROM social.community_ranking_events WHERE community_id=$1) FROM social.communities WHERE id=$1")
        .bind(community).fetch_one(s.db().pool()).await.unwrap();
    assert_eq!(
        facts,
        (0, 0),
        "the failed row retained no counter or receipt"
    );
    sqlx::query("DELETE FROM social.communities WHERE id=$1")
        .bind(community)
        .execute(s.db().pool())
        .await
        .unwrap();
    s.done().await;
}

async fn repair_is_blocked(pool: &sqlx::PgPool, blocker: i32) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) AND query LIKE 'UPDATE camera_reel.images SET ranking_published=true%')",
    )
    .bind(blocker)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn wait_for_repair_block(pool: &sqlx::PgPool, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if repair_is_blocked(pool, blocker).await {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("repair reaches the real PostgreSQL row-lock barrier");
}

#[tokio::test]
async fn ranking_repair_slow_row_does_not_starve_healthy_tail_while_lock_stays_held() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let start = u128::MAX - 50_000;
    let bytes = bytes::Bytes::from(format!("repair slow row {}", tag()));
    let hash = hash_bytes_v1(&bytes);
    s.state.storage.store(&hash, bytes).await.unwrap();
    for index in 1..=100 {
        repair_photo(&s, &owner, Uuid::from_u128(start + index), &hash, &hash).await;
    }
    sqlx::query("UPDATE camera_reel.images SET created_at=timezone('UTC',now())-interval '1 day' WHERE user_address=$1 AND id>$2")
        .bind(&owner).bind(Uuid::from_u128(start+3)).execute(s.db().pool()).await.unwrap();
    let mut lock = s.db().pool().begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM camera_reel.images WHERE id=$1 FOR UPDATE")
        .bind(Uuid::from_u128(start + 2))
        .execute(&mut *lock)
        .await
        .unwrap();
    let mut sweep = RankingSweep::new(Instant::now());
    sweep.after = Uuid::from_u128(start);
    let started = std::time::Instant::now();
    let result = {
        let repair = s.state.repair_ranking_publications(
            &mut sweep,
            false,
            Instant::now() + Duration::from_secs(3),
        );
        tokio::pin!(repair);
        tokio::select! {
            result=&mut repair=>panic!("repair finished before the real row-lock barrier: {result:?}"),
            _=wait_for_repair_block(s.db().pool(),blocker)=>{},
        }
        repair.await.unwrap()
    };
    let markers: Vec<bool> = sqlx::query_scalar(
        "SELECT ranking_published FROM camera_reel.images WHERE user_address=$1 ORDER BY id",
    )
    .bind(&owner)
    .fetch_all(s.db().pool())
    .await
    .unwrap();
    assert_eq!(
        markers,
        [vec![true, false, true], vec![false; 97]].concat(),
        "the healthy row after a persistently blocked row must publish before its lock is released"
    );
    assert!(!result.timed_out);
    assert_eq!(
        (result.inspected, result.published, result.errors),
        (3, 2, 1)
    );
    assert_eq!(sweep.after, Uuid::from_u128(start + 100));
    // The real blocked server statement must end while the lock is still held.
    // Dropping its SQLx future without SET LOCAL would leave it waiting here.
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) AND query LIKE 'UPDATE camera_reel.images SET ranking_published=true%')")
                .bind(blocker).fetch_one(s.db().pool()).await.unwrap();
            if !waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("the actual server statement timed out without releasing its blocker");
    eprintln!(
        "slow-row repair completed healthy tail with lock held in {:.3}ms; inspected={}, published={}, errors={}",
        started.elapsed().as_secs_f64() * 1000.0,
        result.inspected,
        result.published,
        result.errors
    );
    lock.rollback().await.unwrap();
    sweep.after = Uuid::from_u128(start);
    s.state
        .repair_ranking_publications(&mut sweep, false, Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM camera_reel.images WHERE user_address=$1 AND NOT ranking_published",
    )
    .bind(&owner)
    .fetch_one(s.db().pool())
    .await
    .unwrap();
    assert_eq!(
        pending, 97,
        "only the deferred cold rows remain after retrying the timed-out row"
    );
    s.done().await;
}

#[tokio::test]
async fn ranking_repair_deadline_cancellation_and_restart_retain_unvisited_tail() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let start = u128::MAX - 20_000;
    let bytes = bytes::Bytes::from(format!("repair deadline {}", tag()));
    let hash = hash_bytes_v1(&bytes);
    s.state.storage.store(&hash, bytes).await.unwrap();
    let missing = hash_bytes_v1(tag().as_bytes());
    for index in 1..=103 {
        let image_hash = if index <= 100 { &missing } else { &hash };
        repair_photo(
            &s,
            &owner,
            Uuid::from_u128(start + index),
            image_hash,
            &hash,
        )
        .await;
    }
    sqlx::query("UPDATE camera_reel.images SET created_at=timezone('UTC',now())-interval '1 day' WHERE user_address=$1 AND id<=$2")
        .bind(&owner).bind(Uuid::from_u128(start + 100)).execute(s.db().pool()).await.unwrap();
    let mut lock = s.db().pool().begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM camera_reel.images WHERE id=$1 FOR UPDATE")
        .bind(Uuid::from_u128(start + 102))
        .execute(&mut *lock)
        .await
        .unwrap();
    let mut schedule = RankingSchedule::new(Instant::now());
    for sweep in &mut schedule.sweeps {
        sweep.after = Uuid::from_u128(start);
    }
    tokio::time::pause();
    let clock = crate::shared::test_wait::hold_paused_clock();
    let pass_budget = Duration::from_millis(500);
    let timer_tick = Duration::from_millis(1);
    let pass_started = Instant::now();
    let pass_deadline = pass_started + pass_budget;
    let progress = {
        // Isolate the virtual global deadline from PostgreSQL's wall-clock timer.
        // The slow-row test separately verifies the real server statement timeout.
        let repair = super::db::DISABLE_RANKING_STATEMENT_TIMEOUT.scope(
            (),
            s.state.repair_ranking_pass(&mut schedule, pass_deadline),
        );
        tokio::pin!(repair);
        let deadline = tokio::time::sleep_until(pass_deadline);
        tokio::pin!(deadline);
        tokio::select! {
            result = &mut repair => panic!("repair finished before the row-lock barrier: {result:?}"),
            _ = wait_for_repair_block(s.db().pool(), blocker) => {},
        }
        tokio::time::advance(pass_budget - timer_tick).await;
        tokio::select! {
            biased;
            result = &mut repair => panic!("repair finished before the global deadline: {result:?}"),
            _ = &mut deadline => panic!("global deadline elapsed before 500ms"),
            _ = std::future::ready(()) => {},
        }
        tokio::time::advance(timer_tick * 2).await;
        deadline.await;
        std::future::poll_fn(|cx| match std::future::Future::poll(repair.as_mut(), cx) {
            std::task::Poll::Ready(result) => std::task::Poll::Ready(result.unwrap()),
            std::task::Poll::Pending => panic!("repair did not stop at the global deadline"),
        })
        .await
    };
    assert!(
        repair_is_blocked(s.db().pool(), blocker).await,
        "the global deadline must finish while the server query remains blocked"
    );
    assert!(progress.timed_out);
    assert_eq!(pass_started.elapsed(), pass_budget + timer_tick);
    assert_eq!(
        (progress.pages, progress.published, progress.inspected),
        (3, 1, 101)
    );
    assert_eq!(schedule.sweeps[0].after, Uuid::from_u128(start + 101));
    // Finish the timed-out query's rollback before installing a new cancellation
    // barrier; the second observation must belong to this new blocked repair.
    lock.rollback().await.unwrap();
    let mut lock = s.db().pool().begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM camera_reel.images WHERE id=$1 FOR UPDATE")
        .bind(Uuid::from_u128(start + 102))
        .execute(&mut *lock)
        .await
        .unwrap();
    // Dropping the same in-flight future models the leased job's cancellation select.
    {
        let repair = super::db::DISABLE_RANKING_STATEMENT_TIMEOUT.scope(
            (),
            s.state
                .repair_ranking_pass(&mut schedule, Instant::now() + Duration::from_secs(30)),
        );
        tokio::pin!(repair);
        tokio::select! {
            result = &mut repair => panic!("repair finished before cancellation: {result:?}"),
            _ = wait_for_repair_block(s.db().pool(), blocker) => {},
        }
    }
    assert_eq!(schedule.sweeps[0].after, Uuid::from_u128(start + 101));
    lock.rollback().await.unwrap();
    tokio::time::resume();
    drop(clock);
    let markers: Vec<bool> = sqlx::query_scalar(
        "SELECT ranking_published FROM camera_reel.images WHERE user_address=$1 ORDER BY id",
    )
    .bind(&owner)
    .fetch_all(s.db().pool())
    .await
    .unwrap();
    assert_eq!(
        markers,
        [vec![false; 100], vec![true], vec![false; 2]].concat()
    );
    // A process restart discards scalar cursors, but never the durable marker.
    let restarted = State::new(
        s.db().pool().clone(),
        Arc::new(Storage::from_config(&s.dir).await.unwrap()),
        Settings::new(API_URL),
        None,
    );
    let mut fresh = RankingSchedule::new(Instant::now());
    for sweep in &mut fresh.sweeps {
        sweep.after = Uuid::from_u128(start);
    }
    let result = restarted
        .repair_ranking_pass(&mut fresh, Instant::now() + Duration::from_secs(30))
        .await
        .unwrap();
    assert!(result.published >= 2);
    let markers: Vec<bool> = sqlx::query_scalar(
        "SELECT ranking_published FROM camera_reel.images WHERE user_address=$1 ORDER BY id",
    )
    .bind(&owner)
    .fetch_all(s.db().pool())
    .await
    .unwrap();
    assert_eq!(markers, [vec![false; 100], vec![true; 3]].concat());
    s.done().await;
}

#[tokio::test]
async fn ranking_repair_pass_caps_pages_and_shares_progress_between_due_cohorts() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owners: Vec<String> = (0..7).map(|_| s.owner(&random_address())).collect();
    let start = u128::MAX - 40_000;
    let hash = hash_bytes_v1(tag().as_bytes());
    // A real pending backlog spread across owners, each below the upload limit.
    let ids: Vec<Uuid> = (1..=3300)
        .map(|offset| Uuid::from_u128(start + offset))
        .collect();
    sqlx::query("INSERT INTO camera_reel.images(id,user_address,image_hash,thumbnail_hash,metadata,ranking_published,created_at) SELECT id,($2::text[])[((ordinality-1)/500+1)::int],$3,$3,jsonb_build_object('userAddress',($2::text[])[((ordinality-1)/500+1)::int]),false,timezone('UTC',now())-interval '1 day' FROM unnest($1::uuid[]) WITH ORDINALITY AS pending(id,ordinality)")
        .bind(&ids).bind(&owners).bind(&hash).execute(s.db().pool()).await.unwrap();
    let mut schedule = RankingSchedule::new(Instant::now());
    for sweep in &mut schedule.sweeps {
        sweep.after = Uuid::from_u128(start);
    }
    let progress = s
        .state
        .repair_ranking_pass(&mut schedule, Instant::now() + Duration::from_secs(30))
        .await
        .unwrap();
    assert!(!progress.timed_out);
    assert_eq!(
        (
            progress.pages,
            progress.fetched,
            progress.deferred,
            progress.inspected,
            progress.probes
        ),
        (32, 3200, 1600, 1600, 1600)
    );
    assert_eq!((progress.published, progress.errors), (0, 0));
    assert_eq!(schedule.sweeps[0].after, Uuid::from_u128(start + 1600));
    assert_eq!(schedule.sweeps[1].after, Uuid::from_u128(start + 1600));
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM camera_reel.images WHERE user_address=ANY($1) AND NOT ranking_published")
        .bind(&owners).fetch_one(s.db().pool()).await.unwrap();
    assert_eq!(pending, 3300);
    s.done().await;
}

#[tokio::test]
async fn ranking_repair_bounds_keys_before_age_filter_and_retries_late_cold_blobs() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let start = u128::MAX - 30_000;
    let bytes = bytes::Bytes::from(format!("repair late cold {}", tag()));
    let hash = hash_bytes_v1(&bytes);
    let missing = hash_bytes_v1(tag().as_bytes());
    let late_bytes = bytes::Bytes::from(format!("late completed {}", tag()));
    let late_hash = hash_bytes_v1(&late_bytes);
    s.state.storage.store(&hash, bytes).await.unwrap();
    for index in 1..=101 {
        repair_photo(
            &s,
            &owner,
            Uuid::from_u128(start + index),
            &hash,
            if index == 101 {
                &hash
            } else if index == 1 {
                &late_hash
            } else {
                &missing
            },
        )
        .await;
    }
    sqlx::query("UPDATE camera_reel.images SET created_at=timezone('UTC',now())-interval '11 minutes' WHERE user_address=$1 AND id<$2")
        .bind(&owner).bind(Uuid::from_u128(start + 101)).execute(s.db().pool()).await.unwrap();
    let mut schedule = RankingSchedule::new(Instant::now());
    for sweep in &mut schedule.sweeps {
        sweep.after = Uuid::from_u128(start);
    }
    schedule.sweeps[0].cutoff = Some(
        chrono::Utc::now().naive_utc()
            - chrono::TimeDelta::minutes(10)
            - chrono::TimeDelta::seconds(1),
    );
    assert_eq!(schedule.due(Instant::now()), Some(0));
    let recent = s
        .state
        .repair_ranking_publications(
            &mut schedule.sweeps[0],
            false,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert_eq!(
        (recent.fetched, recent.deferred, recent.probes),
        (100, 100, 0)
    );
    assert_eq!(schedule.sweeps[0].after, Uuid::from_u128(start + 100));
    let frozen = schedule.sweeps[0].cutoff;
    let boundary_row_time = frozen.unwrap() + chrono::TimeDelta::microseconds(1);
    sqlx::query("UPDATE camera_reel.images SET created_at=$1 WHERE id=$2")
        .bind(boundary_row_time)
        .bind(Uuid::from_u128(start + 101))
        .execute(s.db().pool())
        .await
        .unwrap();
    assert!(boundary_row_time < chrono::Utc::now().naive_utc() - chrono::TimeDelta::minutes(10));
    let recent = s
        .state
        .repair_ranking_publications(
            &mut schedule.sweeps[0],
            false,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert!(
        recent.published >= 1,
        "the recent row after 100 cold keys is reached on the next page"
    );
    assert_eq!(
        schedule.sweeps[0].cutoff, frozen,
        "a sweep freezes its age boundary across pages"
    );
    assert_eq!(
        schedule.due(Instant::now()),
        Some(1),
        "due cohorts alternate"
    );
    let cold = s
        .state
        .repair_ranking_publications(
            &mut schedule.sweeps[1],
            true,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert_eq!(
        (cold.fetched, cold.inspected, cold.probes, cold.published),
        (100, 100, 200, 0)
    );
    assert!(
        schedule.sweeps[1].next_due <= Instant::now(),
        "a nonempty cold page has no hour delay"
    );
    // Supply the previously missing thumbnail after the cold cohort passed it.
    s.state.storage.store(&late_hash, late_bytes).await.unwrap();
    // Skip unrelated tests' key ranges to exercise the real empty-page completion.
    schedule.sweeps[1].after = Uuid::from_u128(u128::MAX);
    let before_end = Instant::now();
    s.state
        .repair_ranking_publications(
            &mut schedule.sweeps[1],
            true,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert_eq!(schedule.sweeps[1].after, Uuid::nil());
    assert!(schedule.sweeps[1].cutoff.is_none());
    let cold_due = schedule.sweeps[1].next_due;
    assert!(cold_due >= before_end + Duration::from_secs(3600));
    schedule.sweeps[0].next_due = cold_due + Duration::from_secs(60);
    assert_eq!(schedule.due(cold_due - Duration::from_millis(1)), None);
    assert_eq!(schedule.due(cold_due), Some(1));
    schedule.sweeps[1].after = Uuid::from_u128(start);
    let cold = s
        .state
        .repair_ranking_publications(
            &mut schedule.sweeps[1],
            true,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert_eq!(
        cold.published, 1,
        "a late cold upload is repaired on the next sweep"
    );
    let retained: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM camera_reel.images WHERE user_address=$1 AND NOT ranking_published",
    )
    .bind(&owner)
    .fetch_one(s.db().pool())
    .await
    .unwrap();
    assert_eq!(
        retained, 99,
        "missing uploads remain pending, never deleted or marked published"
    );
    s.done().await;
}

#[tokio::test]
async fn private_image_metadata_is_returned_without_auth() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let t = tag();
    let img = image(
        &owner,
        &format!("deadbeef-{t}"),
        &format!("cafebabe-{t}"),
        false,
        "",
    );
    s.db()
        .insert_image(&img)
        .await
        .expect("insert private image");

    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/images/{}/metadata", img.id),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let returned: Image = serde_json::from_slice(&body).expect("parse Image json");
    assert_eq!(returned.id, img.id.to_string());
    assert!(
        !returned.is_public,
        "returned image must be the private one (is_public stays false)"
    );
    assert_eq!(returned.metadata.user_address, owner);
    assert_eq!(returned.url, format!("{API_URL}/api/images/deadbeef-{t}"));
    assert_eq!(
        returned.thumbnail_url,
        format!("{API_URL}/api/images/cafebabe-{t}")
    );
    s.done().await;
}

#[tokio::test]
async fn missing_image_metadata_is_not_found_not_unauthorized() {
    let Some(s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/images/{}/metadata", Uuid::new_v4()),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_of(&body), json!({ "message": "image not found" }));
    let (status, _, _) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/images/not-a-uuid/metadata"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    s.done().await;
}

#[tokio::test]
async fn the_insert_itself_enforces_the_per_user_limit() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let a = s.owner(&random_address());
    let b = s.owner(&random_address());
    let t = tag();
    assert_eq!(
        s.db()
            .insert_image_within_limit(
                &image(&a, &format!("h1-{t}"), &format!("t1-{t}"), true, ""),
                2
            )
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        s.db()
            .insert_image_within_limit(
                &image(&a, &format!("h2-{t}"), &format!("t2-{t}"), true, ""),
                2
            )
            .await
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        s.db()
            .insert_image_within_limit(
                &image(&a, &format!("h3-{t}"), &format!("t3-{t}"), true, ""),
                2
            )
            .await
            .unwrap(),
        None,
        "the third row must be refused by the statement"
    );
    assert_eq!(s.count(&a).await, 2);
    assert_eq!(
        s.db()
            .insert_image_within_limit(
                &image(&b, &format!("h3-{t}"), &format!("t3-{t}"), true, ""),
                2
            )
            .await
            .unwrap(),
        Some(1),
        "limits are per user"
    );
    s.done().await;
}

#[tokio::test]
async fn owner_delete_reports_ownership_and_remaining() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let a = s.owner(&random_address());
    let b = s.owner(&random_address());
    let t = tag();
    let (h1, h3, t1, t2) = (
        s.hash(&format!("h1-{t}")),
        s.hash(&format!("h3-{t}")),
        s.hash(&format!("t1-{t}")),
        s.hash(&format!("t2-{t}")),
    );
    let a1 = image(&a, &h1, &t1, true, "");
    let a2 = image(&a, &h1, &t2, true, "");
    let b1 = image(&b, &h3, &t1, true, "");
    for img in [&a1, &a2, &b1] {
        s.db().insert_image(img).await.unwrap();
    }
    let now = chrono::Utc::now().naive_utc();

    assert!(
        s.db()
            .delete_image_owned(&Uuid::new_v4().to_string(), Some(&a), now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        s.db()
            .delete_image_owned("not-a-uuid", Some(&a), now)
            .await
            .unwrap()
            .is_none()
    );

    let refused = s
        .db()
        .delete_image_owned(&a2.id.to_string(), Some(&b), now)
        .await
        .unwrap()
        .expect("row exists");
    assert!(
        !refused.deleted,
        "another user's image is reported, not deleted"
    );
    assert!(refused.withheld.is_empty());
    assert_eq!(s.count(&a).await, 2);

    let d = s
        .db()
        .delete_image_owned(&a1.id.to_string(), Some(&a.to_uppercase()), now)
        .await
        .unwrap()
        .unwrap();
    assert!(d.deleted);
    assert_eq!(d.user_address, a.to_lowercase());
    assert_eq!(
        (
            d.image_hash.as_str(),
            d.thumbnail_hash.as_str(),
            d.remaining
        ),
        (h1.as_str(), t1.as_str(), 1)
    );
    assert!(
        d.withheld.is_empty(),
        "a2 still names h1 and b1 still names t1: nothing withheld"
    );
    assert_eq!(s.withheld(&h1).await, None);

    let d = s
        .db()
        .delete_image_owned(&a2.id.to_string(), Some(&a), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((d.deleted, d.remaining), (true, 0));
    assert_eq!(
        d.withheld,
        vec![h1.clone(), t2.clone()],
        "the last row naming h1 and t2 is gone"
    );
    assert_eq!(s.withheld(&h1).await.as_deref(), Some(WITHHELD_DELETED));
    assert_eq!(s.withheld(&t1).await, None, "b1 still names t1");

    let d = s
        .db()
        .delete_image_owned(&b1.id.to_string(), None, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((d.deleted, d.remaining), (true, 0));
    assert_eq!(d.withheld, vec![h3.clone(), t1.clone()]);
    assert_eq!(s.count(&a).await + s.count(&b).await, 0);
    s.done().await;
}

#[tokio::test]
async fn visibility_update_keeps_404_403_and_noop_apart() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let a = s.owner(&random_address());
    let b = s.owner(&random_address());
    let t = tag();
    let a1 = image(&a, &format!("h1-{t}"), &format!("t1-{t}"), false, "");
    s.db().insert_image(&a1).await.unwrap();
    let id = a1.id.to_string();

    assert_eq!(
        s.db()
            .set_image_visibility(&Uuid::new_v4().to_string(), &a, true)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        s.db().set_image_visibility("nope", &a, true).await.unwrap(),
        None
    );
    assert_eq!(
        s.db().set_image_visibility(&id, &b, true).await.unwrap(),
        Some(false)
    );
    assert_eq!(
        s.is_public(a1.id).await,
        Some(false),
        "a stranger's update changes nothing"
    );
    assert_eq!(
        s.db()
            .set_image_visibility(&id, &a.to_uppercase(), true)
            .await
            .unwrap(),
        Some(true)
    );
    assert_eq!(s.is_public(a1.id).await, Some(true));
    assert_eq!(
        s.db().set_image_visibility(&id, &a, true).await.unwrap(),
        Some(true),
        "no-op is still owned"
    );
    assert_eq!(s.is_public(a1.id).await, Some(true));
    s.done().await;
}

#[tokio::test]
async fn pages_carry_the_total_and_fall_back_past_the_end() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let a = s.owner(&random_address());
    let b = s.owner(&random_address());
    let t = tag();
    let place = Uuid::new_v4().to_string();
    for (i, public) in [true, true, true, false].into_iter().enumerate() {
        let place_id = if i < 2 { place.as_str() } else { "" };
        s.db()
            .insert_image(&image(
                &a,
                &format!("h{i}-{t}"),
                &format!("t{i}-{t}"),
                public,
                place_id,
            ))
            .await
            .unwrap();
    }
    let db = s.db();

    let p = db
        .get_user_images_page(&a, &Page::Offset(0), 2, true)
        .await
        .unwrap();
    assert_eq!((p.images.len(), p.total, p.next.is_none()), (2, 3, true));
    let p = db
        .get_user_images_page(&a.to_uppercase(), &Page::Offset(0), 2, false)
        .await
        .unwrap();
    assert_eq!((p.images.len(), p.total), (2, 4));
    let p = db
        .get_user_images_page(&a, &Page::Offset(10), 2, true)
        .await
        .unwrap();
    assert_eq!(
        (p.images.len(), p.total),
        (0, 3),
        "past the end still reports the total"
    );
    let p = db
        .get_user_images_page(&a, &Page::Offset(0), 0, false)
        .await
        .unwrap();
    assert_eq!((p.images.len(), p.total), (0, 4));
    let p = db
        .get_user_images_page(&b, &Page::Offset(0), 5, false)
        .await
        .unwrap();
    assert_eq!((p.images.len(), p.total), (0, 0));

    let p = db
        .get_place_images_page(&place, &Page::Offset(0), 10)
        .await
        .unwrap();
    assert_eq!((p.images.len(), p.total), (2, 2));
    let p = db
        .get_place_images_page(&place, &Page::Offset(1), 10)
        .await
        .unwrap();
    assert_eq!((p.images.len(), p.total), (1, 2));
    let p = db
        .get_multiple_places_images_page(std::slice::from_ref(&place), &Page::Offset(0), 1)
        .await
        .unwrap();
    assert_eq!((p.images.len(), p.total), (1, 2));
    assert!(
        db.get_place_images_page("not-a-uuid", &Page::Offset(0), 10)
            .await
            .is_err()
    );
    s.done().await;
}

#[tokio::test]
async fn keyset_pages_walk_the_offset_order_and_end_with_none() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let a = s.owner(&random_address());
    let t = tag();
    for i in 0..5 {
        s.db()
            .insert_image(&image(
                &a,
                &format!("h{i}-{t}"),
                &format!("t{i}-{t}"),
                true,
                "",
            ))
            .await
            .unwrap();
    }
    let db = s.db();
    let by_offset: Vec<Uuid> = db
        .get_user_images_page(&a, &Page::Offset(0), 10, true)
        .await
        .unwrap()
        .images
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(by_offset.len(), 5);

    let mut walked = Vec::new();
    let mut page = Page::After(None);
    let mut pages = 0;
    loop {
        let p = db.get_user_images_page(&a, &page, 2, true).await.unwrap();
        assert_eq!(
            p.total, 5,
            "every keyset page carries the whole set's total"
        );
        walked.extend(p.images.iter().map(|i| i.id));
        pages += 1;
        match p
            .next
            .expect("a keyset page always says whether there is a next")
        {
            Some(cursor) => page = Page::After(Some(cursor)),
            None => break,
        }
    }
    assert_eq!(walked, by_offset, "the keyset walk is the offset order");
    assert_eq!(pages, 3, "2 + 2 + 1: the short last page ends the walk");
    let p = db
        .get_user_images_page(&a, &Page::After(None), 0, true)
        .await
        .unwrap();
    assert_eq!(
        (p.images.len(), p.next),
        (0, Some(None)),
        "limit 0 is an empty last page"
    );
    s.done().await;
}

#[tokio::test]
async fn servable_memo_is_dropped_by_review_and_delete() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let a = s.owner(&random_address());
    let t = tag();
    let (hx, tx) = (s.hash(&format!("hx-{t}")), s.hash(&format!("tx-{t}")));
    let a1 = image(&a, &hx, &tx, true, "");
    s.db().insert_image(&a1).await.unwrap();
    let db = s.db();
    let id = a1.id.to_string();
    let now = chrono::Utc::now().naive_utc();
    let both = || vec![hx.clone(), tx.clone()];

    assert!(db.hash_is_servable(&hx).await.unwrap());
    assert!(db.hash_is_servable(&tx).await.unwrap());
    let verdict = db
        .update_image_review_status(&id, "rejected", now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        verdict,
        BlobVerdict {
            withheld: both(),
            released: Vec::new()
        }
    );
    assert!(
        !db.hash_is_servable(&hx).await.unwrap(),
        "rejection must not be masked by the memo"
    );
    assert!(!db.hash_is_servable(&tx).await.unwrap());
    assert_eq!(s.withheld(&hx).await.as_deref(), Some(WITHHELD_REJECTED));
    let verdict = db
        .update_image_review_status(&id, "ok", now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        verdict,
        BlobVerdict {
            withheld: Vec::new(),
            released: both()
        }
    );
    assert!(db.hash_is_servable(&hx).await.unwrap());
    assert_eq!(s.withheld(&hx).await, None);
    assert!(
        db.update_image_review_status(&Uuid::new_v4().to_string(), "ok", now)
            .await
            .unwrap()
            .is_none()
    );
    let flagged = db
        .update_image_review_status(&id, "flagged", now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        flagged,
        BlobVerdict::default(),
        "flagged is served; nothing to release"
    );

    let verdict = db
        .update_image_review_status(&id, "rejected", now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(verdict.withheld, both());
    assert!(!db.hash_is_servable(&hx).await.unwrap());
    let d = db
        .delete_image_owned(&id, None, now)
        .await
        .unwrap()
        .unwrap();
    assert!(d.deleted);
    assert!(
        d.withheld.is_empty(),
        "already withheld by the rejection; the row stays"
    );
    assert_eq!(s.withheld(&hx).await.as_deref(), Some(WITHHELD_REJECTED));
    assert!(
        !db.hash_is_servable(&hx).await.unwrap(),
        "no referencing row: NOT servable (the old orphan rule is gone over the shared store)"
    );
    assert!(!db.hash_is_servable(&format!("never-{t}")).await.unwrap());
    s.done().await;
}

#[tokio::test]
async fn explorer_boot_corpus_shapes_answer_for_an_unknown_wallet_and_place() {
    let Some(s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let address = random_address();
    // row 34: GET /camera-reel-service/api/users/{address}
    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/users/{address}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_of(&body),
        json!({ "currentImages": 0, "maxImages": 500 })
    );
    // row 2: GET /camera-reel-service/api/places/{uuid}/images
    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/places/{}/images", Uuid::new_v4()),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body), json!({ "images": [], "maxImages": 0 }));
    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/places/not-a-uuid/images"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::NOT_FOUND,
            json!({ "message": "place not found" })
        )
    );
    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/users/{address}/images?compact=true"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_of(&body),
        json!({ "images": [], "currentImages": 0, "maxImages": 500 }),
        "no nextCursor on an offset page"
    );
    let (status, _, body) = send(s.app(), "GET", &format!("{PREFIX}/health"), vec![], None).await;
    assert_eq!((status, json_of(&body)), (StatusCode::OK, json!("alive")));
    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/docs/openapi.json"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["info"]["title"], "Camera Reel Service");
    s.done().await;
}

#[tokio::test]
async fn writes_disabled_gates_uploads_and_moderation_but_not_the_batch_gallery() {
    let Some(s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let w = wallet();
    let path = format!("{PREFIX}/api/images");
    let (body, ct) = multipart(
        &png(8, 8, 1),
        "image/png",
        &metadata_json(&w.address, ""),
        None,
    );
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, resp_headers, body) =
        send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp_headers.get(header::RETRY_AFTER).unwrap(), "3600");
    assert_eq!(key_set(&json_of(&body)), vec!["message"]);
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &format!("{PREFIX}/admin/images/{}/review", Uuid::new_v4()),
        vec![],
        Some((b"{}".to_vec(), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let batch = json!({ "placesIds": [Uuid::new_v4().to_string()] })
        .to_string()
        .into_bytes();
    let (status, _, body) = send(
        s.app(),
        "POST",
        &format!("{PREFIX}/api/places/images"),
        vec![],
        Some((batch, "application/json")),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (StatusCode::OK, json!({ "images": [], "maxImages": 0 })),
        "the batch gallery is a read"
    );
    s.done().await;
}

#[tokio::test]
async fn upload_over_signed_fetch_is_served_back_and_owned() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let stranger = wallet();
    let place = Uuid::new_v4().to_string();
    let bytes = png(1280, 720, 7);
    let upload_path = format!("{PREFIX}/api/images");

    // unsigned: 401; signed for another wallet's metadata: 400; both before any row.
    let (body, ct) = multipart(
        &bytes,
        "image/png",
        &metadata_json(&owner, &place),
        Some(false),
    );
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &upload_path,
        vec![],
        Some((body.clone(), &ct)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp, b"Unathorized");
    let (other, other_ct) = multipart(
        &bytes,
        "image/png",
        &metadata_json(&stranger.address, &place),
        Some(false),
    );
    let headers = signed_headers(&w, "post", &upload_path, true).await;
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &upload_path,
        headers,
        Some((other, &other_ct)),
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)),
        (
            StatusCode::BAD_REQUEST,
            json!({ "message": "invalid user address" })
        )
    );
    assert_eq!(s.count(&owner).await, 0);

    // the upload, signed over the request path (the gateway shape)
    let headers = signed_headers(&w, "post", &upload_path, true).await;
    let (status, _, resp) = send(s.app(), "POST", &upload_path, headers, Some((body, &ct))).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
    let uploaded = json_of(&resp);
    assert_eq!(uploaded["currentImages"], 1);
    assert_eq!(uploaded["maxImages"], 500);
    let image_hash = hash_bytes_v1(&bytes);
    let image_id = uploaded["image"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        uploaded["image"]["url"],
        format!("{API_URL}/api/images/{image_id}-shot.png")
    );
    assert_eq!(uploaded["image"]["isPublic"], false);
    assert_eq!(uploaded["image"]["metadata"]["placeId"], place);
    let thumbnail_url = uploaded["image"]["thumbnailUrl"].as_str().unwrap();
    assert_eq!(
        thumbnail_url,
        format!("{API_URL}/api/images/{image_id}-thumbnail-shot.png")
    );
    let thumbnail_hash = s.db().get_image(&image_id).await.unwrap().thumbnail_hash;

    // the blobs, from the ONE store, typed by their bytes
    let (status, headers, served) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/files/{image_hash}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/png");
    assert_eq!(served, bytes);
    let (status, _, thumb) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/files/{thumbnail_hash}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let decoded = image::load_from_memory(&thumb).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (640, 360));
    assert!(s.state.storage.exist(&image_hash).await.unwrap());

    // private: the owner sees it (signed over the request path, or the vhost's local path behind the proxy header), nobody else does
    let gallery = format!("{PREFIX}/api/users/{owner}/images");
    let (status, _, resp) = send(s.app(), "GET", &gallery, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_of(&resp)["currentImages"],
        0,
        "anonymous reads the public set"
    );
    let headers = signed_headers(&w, "get", &gallery, true).await;
    let (_, _, resp) = send(s.app(), "GET", &gallery, headers, None).await;
    assert_eq!(json_of(&resp)["images"][0]["id"], image_id);
    let headers = signed_headers(&w, "get", &gallery, false).await;
    let (_, _, resp) = send(s.app(), "GET", &gallery, headers, None).await;
    assert_eq!(
        json_of(&resp)["currentImages"],
        1,
        "the legacy payload over the request path verifies too"
    );
    let local = format!("/api/users/{owner}/images");
    let headers = signed_headers(&w, "get", &local, true).await;
    let (_, _, resp) = send(s.app(), "GET", &gallery, headers.clone(), None).await;
    assert_eq!(
        json_of(&resp)["currentImages"],
        0,
        "the route-local path alone reads as anonymous"
    );
    let mut forwarded = headers;
    forwarded.push(("x-original-path".to_string(), local));
    let (_, _, resp) = send(s.app(), "GET", &gallery, forwarded, None).await;
    assert_eq!(
        json_of(&resp)["currentImages"],
        1,
        "the vhost proxy's x-original-path says the client asked for the local path"
    );
    let headers = signed_headers(&stranger, "get", &gallery, true).await;
    let (_, _, resp) = send(s.app(), "GET", &gallery, headers, None).await;
    assert_eq!(
        json_of(&resp)["currentImages"],
        0,
        "another wallet reads the public set"
    );
    let (status, _, resp) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/places/{place}/images"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)["maxImages"].as_u64()),
        (StatusCode::OK, Some(0)),
        "a private photo is not in the place gallery"
    );

    // visibility: the owner's only
    let vis = format!("{PREFIX}/api/images/{image_id}/visibility");
    let flip = json!({ "is_public": true }).to_string().into_bytes();
    let headers = signed_headers(&stranger, "patch", &vis, true).await;
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &vis,
        headers,
        Some((flip.clone(), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let headers = signed_headers(&w, "patch", &vis, true).await;
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &vis,
        headers,
        Some((flip, "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, resp) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/places/{place}/images?cursor="),
        vec![],
        None,
    )
    .await;
    let page = json_of(&resp);
    assert_eq!(page["maxImages"], 1);
    assert_eq!(page["images"][0]["id"], image_id);
    assert_eq!(
        page["nextCursor"],
        Value::Null,
        "a keyset page says it is the last one"
    );
    let batch = json!({ "placesIds": [place, "nobody.dcl.eth"] })
        .to_string()
        .into_bytes();
    let (_, _, resp) = send(
        s.app(),
        "POST",
        &format!("{PREFIX}/api/places/images"),
        vec![],
        Some((batch, "application/json")),
    )
    .await;
    assert_eq!(json_of(&resp)["images"][0]["placeId"], place);

    // delete: the owner's only; the row goes, the blob stays but is no longer servable
    let del = format!("{PREFIX}/api/images/{image_id}");
    let headers = signed_headers(&stranger, "delete", &del, true).await;
    let (status, _, _) = send(s.app(), "DELETE", &del, headers, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let headers = signed_headers(&w, "delete", &del, true).await;
    let (status, _, resp) = send(s.app(), "DELETE", &del, headers, None).await;
    assert_eq!(
        (status, json_of(&resp)),
        (
            StatusCode::OK,
            json!({ "currentImages": 0, "maxImages": 500 })
        )
    );
    let (status, _, _) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/files/{image_hash}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unreferenced hash is not servable"
    );
    assert!(
        s.state.storage.exist(&image_hash).await.unwrap(),
        "the shared store is never unlinked"
    );
    let headers = signed_headers(&w, "delete", &del, true).await;
    let (status, _, _) = send(s.app(), "DELETE", &del, headers, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    s.done().await;
}

#[tokio::test]
async fn json_upload_and_the_cap_answer_the_upstream_envelopes() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.max_images_per_user = 1;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let path = format!("{PREFIX}/api/images-json");
    let upload = |seed: u8| {
        use base64::Engine as _;
        json!({
            "image": base64::engine::general_purpose::STANDARD.encode(png(16, 9, seed)),
            "content_type": "image/png",
            "metadata": metadata_json(&owner, ""),
            "is_public": true
        })
        .to_string()
        .into_bytes()
    };
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &path,
        headers,
        Some((upload(3), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
    assert_eq!(json_of(&resp)["currentImages"], 1);
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &path,
        headers,
        Some((upload(4), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        json_of(&resp),
        json!({ "reason": "maxLimitReached", "message": "you have reached the limit of 1 max images" })
    );
    assert_eq!(s.count(&owner).await, 1, "a refused upload leaves no row");
    let headers = signed_headers(&w, "post", &path, true).await;
    let bad = json!({ "image": "not base64!", "content_type": "image/png", "metadata": metadata_json(&owner, "") }).to_string().into_bytes();
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &path,
        headers,
        Some((bad, "application/json")),
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)),
        (
            StatusCode::BAD_REQUEST,
            json!({ "message": "invalid base64 image" })
        )
    );
    s.done().await;
}

#[tokio::test]
async fn the_moderator_hides_a_rejected_row_and_its_blobs() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.admin_token = Some("moderator-secret".to_string());
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let bytes = png(32, 32, 9);
    let hash = s.hash(&hash_bytes_v1(&bytes));
    s.state
        .storage
        .store(&hash, bytes.clone().into())
        .await
        .unwrap();
    let img = image(&owner, &hash, &hash, true, "");
    s.db().insert_image(&img).await.unwrap();
    let review = format!("{PREFIX}/admin/images/{}/review", img.id);
    let reject = json!({ "reviewStatus": "rejected" })
        .to_string()
        .into_bytes();
    let bearer = |token: &str| vec![("authorization".to_string(), format!("Bearer {token}"))];

    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &review,
        vec![],
        Some((reject.clone(), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &review,
        bearer("wrong"),
        Some((reject.clone(), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, served) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/files/{hash}"),
        vec![],
        None,
    )
    .await;
    assert_eq!((status, served == bytes), (StatusCode::OK, true));

    let bad = json!({ "reviewStatus": "meh" }).to_string().into_bytes();
    let (status, _, resp) = send(
        s.app(),
        "PATCH",
        &review,
        bearer("moderator-secret"),
        Some((bad, "application/json")),
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)),
        (
            StatusCode::BAD_REQUEST,
            json!({ "message": "reviewStatus must be one of: ok, flagged, rejected" })
        )
    );
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &review,
        bearer("moderator-secret"),
        Some((reject.clone(), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/files/{hash}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a rejected row's blob is withheld"
    );
    let (status, _, _) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/images/{}/metadata", img.id),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, resp) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/users/{owner}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)["currentImages"].as_u64()),
        (StatusCode::OK, Some(0))
    );
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &review,
        bearer("moderator-secret"),
        Some((
            json!({ "reviewStatus": "ok" }).to_string().into_bytes(),
            "application/json",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/files/{hash}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the memo was dropped by the review write"
    );

    let (status, _, resp) = send(
        s.app(),
        "DELETE",
        &format!("{PREFIX}/admin/images/{}", img.id),
        bearer("moderator-secret"),
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)),
        (
            StatusCode::OK,
            json!({ "currentImages": 0, "maxImages": 500 })
        )
    );
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &review,
        bearer("moderator-secret"),
        Some((reject, "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    s.done().await;
}

/// The node as `serve` composes it around the ONE store: the content
/// server's `/contents/{hash}`, the builder's under `/builder-api` and the
/// reel under its prefix, the builder and reel states holding the content
/// state's denylist (`main.rs`).
async fn node_around(s: &Scratch) -> Router {
    let pool = s.db().pool().clone();
    let storage = s.state.storage.clone();
    let mut content = crate::content::ContentState::new(
        pool.clone(),
        crate::content::Settings::for_tests(),
        storage.clone(),
    );
    content.denylist = s.state.denylist.clone();
    let builder = Arc::new(
        crate::builder::State::new(
            pool,
            storage,
            s.state.denylist.clone(),
            dcl_http::Outbound::for_tests(),
            None,
            None,
            crate::builder::Settings::default(),
        )
        .unwrap(),
    );
    Router::new()
        .merge(crate::content::build_router(Arc::new(content)))
        .nest("/builder-api", crate::builder::router(builder))
        .nest(PREFIX, router(s.state.clone()))
}

/// The three routes that can hand out a photo's bytes, as `(status, body)`.
async fn bytes_everywhere(app: &Router, hash: &str) -> [(StatusCode, bool); 3] {
    let mut out = [(StatusCode::IM_A_TEAPOT, false); 3];
    for (i, path) in [
        format!("{PREFIX}/api/files/{hash}"),
        format!("/contents/{hash}"),
        format!("/builder-api/contents/{hash}"),
    ]
    .iter()
    .enumerate()
    {
        let (status, _, body) = send(app.clone(), "GET", path, vec![], None).await;
        out[i] = (status, !body.is_empty() && status == StatusCode::OK);
    }
    out
}

#[tokio::test]
async fn a_rejected_or_deleted_photo_is_withheld_on_every_contents_route() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.admin_token = Some("moderator-secret".to_string());
    let Some(mut s) = scratch(settings.clone()).await else {
        return;
    };
    let app = node_around(&s).await;
    let w = wallet();
    let owner = s.owner(&w.address);
    let bytes = png(48, 48, 11);
    let upload_path = format!("{PREFIX}/api/images");
    let (body, ct) = multipart(&bytes, "image/png", &metadata_json(&owner, ""), Some(false));
    let headers = signed_headers(&w, "post", &upload_path, true).await;
    let (status, _, resp) = send(
        app.clone(),
        "POST",
        &upload_path,
        headers,
        Some((body, &ct)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
    let uploaded = json_of(&resp);
    let id = uploaded["image"]["id"].as_str().unwrap().to_string();
    let image_hash = s.hash(&hash_bytes_v1(&bytes));
    let stored_thumbnail = s.db().get_image(&id).await.unwrap().thumbnail_hash;
    let thumb_hash = s.hash(&stored_thumbnail);
    assert_ne!(image_hash, thumb_hash);
    let served = [(StatusCode::OK, true); 3];
    let withheld = [(StatusCode::NOT_FOUND, false); 3];

    // private and not rejected: hash-as-capability on every route, as the old reel route
    assert_eq!(bytes_everywhere(&app, &image_hash).await, served);
    assert_eq!(bytes_everywhere(&app, &thumb_hash).await, served);
    let (_, _, root) = send(
        app.clone(),
        "GET",
        &format!("/contents/{image_hash}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(root, bytes, "the content server serves the photo's bytes");

    // rejected: 404 on all three, image and thumbnail, and the record is durable
    let review = format!("{PREFIX}/admin/images/{id}/review");
    let bearer = vec![(
        "authorization".to_string(),
        "Bearer moderator-secret".to_string(),
    )];
    let verdict = |status: &str| {
        Some((
            json!({ "reviewStatus": status }).to_string().into_bytes(),
            "application/json",
        ))
    };
    let (status, _, _) = send(
        app.clone(),
        "PATCH",
        &review,
        bearer.clone(),
        verdict("rejected"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes_everywhere(&app, &image_hash).await, withheld);
    assert_eq!(bytes_everywhere(&app, &thumb_hash).await, withheld);
    assert_eq!(
        s.withheld(&image_hash).await.as_deref(),
        Some(WITHHELD_REJECTED)
    );
    assert_eq!(
        s.withheld(&thumb_hash).await.as_deref(),
        Some(WITHHELD_REJECTED)
    );
    // a restart is a new process: a fresh store over the same root, its projection empty until the load
    let restarted_store = Arc::new(
        Storage::from_config(&s.dir)
            .await
            .expect("the same blob root"),
    );
    let restarted = State::new(s.db().pool().clone(), restarted_store, settings, None);
    assert!(!restarted.denylist.contains(&image_hash));
    assert!(
        restarted.load_withheld().await.unwrap() >= 2,
        "a restart reloads the withheld set"
    );
    assert!(restarted.denylist.contains(&image_hash) && restarted.denylist.contains(&thumb_hash));

    // set back to ok: served again everywhere, the record released
    let (status, _, _) = send(app.clone(), "PATCH", &review, bearer.clone(), verdict("ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes_everywhere(&app, &image_hash).await, served);
    assert_eq!(bytes_everywhere(&app, &thumb_hash).await, served);
    assert_eq!(s.withheld(&image_hash).await, None);

    // the same bytes accepted as a builder upload: a rejection withholds only the thumbnail
    sqlx::query(
        "INSERT INTO builder.uploaded_contents (hash, size, uploaded_by) VALUES ($1, $2, $3)",
    )
    .bind(&image_hash)
    .bind(bytes.len() as i64)
    .bind(owner.to_lowercase())
    .execute(s.db().pool())
    .await
    .unwrap();
    let (status, _, _) = send(
        app.clone(),
        "PATCH",
        &review,
        bearer.clone(),
        verdict("rejected"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        bytes_everywhere(&app, &image_hash).await,
        [
            (StatusCode::NOT_FOUND, false),
            (StatusCode::OK, true),
            (StatusCode::OK, true)
        ],
        "a deployed file of the same bytes stays served by the content routes"
    );
    assert_eq!(bytes_everywhere(&app, &thumb_hash).await, withheld);
    sqlx::query("DELETE FROM builder.uploaded_contents WHERE hash = $1")
        .bind(&image_hash)
        .execute(s.db().pool())
        .await
        .unwrap();
    let (status, _, _) = send(app.clone(), "PATCH", &review, bearer, verdict("ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes_everywhere(&app, &image_hash).await, served);

    // the owner's delete: the row is gone and the bytes are withheld everywhere
    let del_path = format!("{PREFIX}/api/images/{id}");
    let headers = signed_headers(&w, "delete", &del_path, true).await;
    let (status, _, _) = send(app.clone(), "DELETE", &del_path, headers, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes_everywhere(&app, &image_hash).await, withheld);
    assert_eq!(bytes_everywhere(&app, &thumb_hash).await, withheld);
    assert_eq!(
        s.withheld(&image_hash).await.as_deref(),
        Some(WITHHELD_DELETED)
    );
    assert!(
        s.state.storage.exist(&image_hash).await.unwrap(),
        "the blob itself is never unlinked"
    );
    s.done().await;
}

#[tokio::test]
async fn admin_routes_fail_closed_without_a_configured_token() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(s) = scratch(settings).await else {
        return;
    };
    let (status, _, resp) = send(
        s.app(),
        "DELETE",
        &format!("{PREFIX}/admin/images/{}", Uuid::new_v4()),
        vec![("authorization".to_string(), "Bearer anything".to_string())],
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)),
        (
            StatusCode::FORBIDDEN,
            json!({ "message": "admin token not configured" })
        )
    );
    s.done().await;
}

#[tokio::test]
async fn a_world_name_gallery_reads_the_places_catalogue() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let t = tag();
    let world = format!("cam{}.dcl.eth", &t[..8]);
    let place = Uuid::new_v4().to_string();
    let hidden = Uuid::new_v4().to_string();
    let pool = s.db().pool().clone();
    // The world's catalogue row (`id = lower(world_name)`) is not one of its places.
    let catalogue = world.clone();
    for (id, disabled) in [(&place, false), (&hidden, true), (&catalogue, false)] {
        sqlx::query("INSERT INTO explore.place (id, base_position, disabled, raw) VALUES ($1, '0,0', $2, $3)")
            .bind(id)
            .bind(disabled)
            .bind(json!({ "world": true, "world_name": world.to_uppercase() }))
            .execute(&pool)
            .await
            .unwrap();
    }
    s.db()
        .insert_image(&image(
            &owner,
            &format!("w1-{t}"),
            &format!("wt1-{t}"),
            true,
            &place,
        ))
        .await
        .unwrap();
    s.db()
        .insert_image(&image(
            &owner,
            &format!("w2-{t}"),
            &format!("wt2-{t}"),
            true,
            &hidden,
        ))
        .await
        .unwrap();
    s.db()
        .insert_image(&image(
            &owner,
            &format!("w3-{t}"),
            &format!("wt3-{t}"),
            true,
            &catalogue,
        ))
        .await
        .unwrap();

    let (status, _, resp) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/places/{world}/images"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page = json_of(&resp);
    assert_eq!(
        page["maxImages"], 1,
        "neither the disabled place's photo nor one named by the catalogue row is in the world gallery"
    );
    assert_eq!(
        page["images"][0]["url"],
        format!("{API_URL}/api/images/w1-{t}")
    );
    let batch = json!({ "placesIds": [world, hidden] })
        .to_string()
        .into_bytes();
    let (_, _, resp) = send(
        s.app(),
        "POST",
        &format!("{PREFIX}/api/places/images"),
        vec![],
        Some((batch, "application/json")),
    )
    .await;
    assert_eq!(
        json_of(&resp)["maxImages"],
        2,
        "a place id in the batch is taken as given"
    );
    let (status, _, resp) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/places/nobody-{}.dcl.eth/images", &t[..8]),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)),
        (StatusCode::OK, json!({ "images": [], "maxImages": 0 }))
    );

    sqlx::query("DELETE FROM explore.place WHERE id = ANY($1)")
        .bind(vec![place, hidden, catalogue])
        .execute(&pool)
        .await
        .unwrap();
    s.done().await;
}

fn worn(owner: &str, hash: &str, is_public: bool, people: &[&[String]]) -> NewImage {
    let mut new = image(owner, hash, &format!("{hash}-thumb"), is_public, "");
    new.metadata.visible_people = people
        .iter()
        .map(|wearables| User {
            wearables: wearables.to_vec(),
            ..User::default()
        })
        .collect();
    new
}

#[tokio::test]
async fn the_wearable_items_of_a_photo_are_its_collection_items_without_token_ids() {
    let Some(s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let items = |doc: Value| {
        let pool = s.db().pool().clone();
        async move {
            let mut got: Vec<String> =
                sqlx::query_scalar("SELECT camera_reel.image_wearable_items($1)")
                    .bind(doc)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            got.sort();
            got
        }
    };
    let contract = "0x0bf152a83a6fc55066c2b664b164ca2916ad38f5";
    let doc = json!({ "visiblePeople": [
        { "wearables": [
            "urn:decentraland:off-chain:base-avatars:eyes_00",
            format!("urn:decentraland:matic:collections-v2:{}:2:7", contract.to_uppercase()),
            5
        ] },
        { "wearables": [
            format!("urn:decentraland:matic:collections-v2:{contract}:2:105312291668557186697918027683670432318895095400549111254310977559"),
            format!("urn:decentraland:ethereum:collections-v2:{contract}:3")
        ] }
    ] });
    assert_eq!(
        items(doc).await,
        vec![format!("{contract}-2"), format!("{contract}-3")],
        "one entry per item, lowercase"
    );
    for empty in [
        json!({}),
        json!({ "visiblePeople": null }),
        json!({ "visiblePeople": "someone" }),
        json!({ "visiblePeople": [1, "s", { "wearables": "none" }, { "wearables": null }, {}] }),
    ] {
        assert_eq!(items(empty.clone()).await, Vec::<String>::new(), "{empty}");
    }
    s.done().await;
}

#[tokio::test]
async fn the_wearable_gallery_is_the_public_photos_of_an_item_whoever_asks() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let t = tag();
    let contract = random_address();
    let urn = |item: &str, token: &str| {
        format!("urn:decentraland:matic:collections-v2:{contract}:{item}:{token}")
    };

    let long_token = worn(
        &owner,
        &format!("wa-{t}"),
        true,
        &[&[urn(
            "2",
            "105312291668557186697918027683670432318895095400549111254310977559",
        )]],
    );
    let upper_contract = format!("0x{}", contract[2..].to_uppercase());
    let upper = worn(
        &owner,
        &format!("wb-{t}"),
        true,
        &[&[format!(
            "urn:decentraland:matic:collections-v2:{upper_contract}:2:7"
        )]],
    );
    let two_wearers = worn(
        &owner,
        &format!("wc-{t}"),
        true,
        &[&[urn("2", "8")], &[urn("2", "9"), urn("3", "1")]],
    );
    let other_item = worn(&owner, &format!("wd-{t}"), true, &[&[urn("3", "7")]]);
    let private = worn(&owner, &format!("we-{t}"), false, &[&[urn("2", "10")]]);
    let rejected = worn(
        &owner,
        &s.hash(&format!("wf-{t}")),
        true,
        &[&[urn("2", "11")]],
    );
    s.hash(&format!("wf-{t}-thumb"));
    for new in [
        &long_token,
        &upper,
        &two_wearers,
        &other_item,
        &private,
        &rejected,
    ] {
        s.db().insert_image(new).await.unwrap();
    }
    s.db()
        .update_image_review_status(
            &rejected.id.to_string(),
            "rejected",
            chrono::Utc::now().naive_utc(),
        )
        .await
        .unwrap();

    let expected: std::collections::BTreeSet<String> = [&long_token, &upper, &two_wearers]
        .iter()
        .map(|n| n.id.to_string())
        .collect();
    let ids = |page: &Value| -> Vec<String> {
        page["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    };

    let path = format!("{PREFIX}/api/wearables/{upper_contract}-2/images");
    let (status, _, body) = send(s.app(), "GET", &path, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    let page = json_of(&body);
    assert_eq!(
        key_set(&page),
        vec!["images", "maxImages"],
        "offset-only: no nextCursor, no currentImages"
    );
    assert_eq!(
        page["maxImages"], 3,
        "every token of the item, never the private, rejected or neighbouring one"
    );
    assert_eq!(
        ids(&page)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    let first = &page["images"][0];
    assert_eq!(
        key_set(first),
        vec!["id", "isPublic", "metadata", "thumbnailUrl", "url"],
        "full images, not gallery rows"
    );
    assert!(first["metadata"]["visiblePeople"].is_array());

    let (_, _, body) = send(s.app(), "GET", &format!("{path}?limit=2"), vec![], None).await;
    let head = json_of(&body);
    let (_, _, body) = send(
        s.app(),
        "GET",
        &format!("{path}?limit=2&offset=2"),
        vec![],
        None,
    )
    .await;
    let tail = json_of(&body);
    assert_eq!(
        (head["maxImages"].clone(), tail["maxImages"].clone()),
        (json!(3), json!(3))
    );
    let walked: Vec<String> = ids(&head).into_iter().chain(ids(&tail)).collect();
    assert_eq!(
        walked,
        ids(&page),
        "offset pages walk the newest-first order"
    );

    let signed = signed_headers(&w, "get", &path, true).await;
    let (status, _, body) = send(s.app(), "GET", &path, signed, None).await;
    assert_eq!(
        (status, json_of(&body)["maxImages"].clone()),
        (StatusCode::OK, json!(3)),
        "the owner's own chain does not lift the public filter"
    );

    let (status, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/wearables/{contract}-4/images"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (StatusCode::OK, json!({ "images": [], "maxImages": 0 }))
    );

    let message = json!({ "message": "the item must be an item id, as `<contract>-<itemId>`" });
    for bad in [
        "not-an-item".to_string(),
        urn("2", "7"),
        contract.clone(),
        format!("{contract}-2-7"),
    ] {
        let (status, _, body) = send(
            s.app(),
            "GET",
            &format!("{PREFIX}/api/wearables/{bad}/images"),
            vec![],
            None,
        )
        .await;
        assert_eq!(
            (status, json_of(&body)),
            (StatusCode::BAD_REQUEST, message.clone()),
            "{bad}"
        );
    }

    let (_, _, body) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/docs/openapi.json"),
        vec![],
        None,
    )
    .await;
    let spec = json_of(&body);
    assert_eq!(
        spec["paths"]["/api/wearables/{item}/images"]["get"]["operationId"],
        "get_wearable_images"
    );
    assert_eq!(
        spec["components"]["schemas"]["GetWearableImagesResponse"]["required"],
        json!(["images", "maxImages"])
    );
    s.done().await;
}

#[path = "storage_parity_tests.rs"]
mod storage_parity_tests;
// ---- parity with camera-reel-service 3e8bd0b: screen rects, CORS, extractor
// rejections, routing fallbacks, upload checks ----

/// The router over a pool that never connects: every answer these tests
/// read is decided before a query (or is the lookup failure itself).
async fn offline_app(settings: Settings) -> (Router, std::path::PathBuf) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_lazy("postgres://catalyst@127.0.0.1:1/none")
        .unwrap();
    let dir =
        std::env::temp_dir().join(format!("catalyst-camera-reel-{}", Uuid::new_v4().simple()));
    let storage = Arc::new(Storage::from_config(&dir).await.unwrap());
    let state = Arc::new(State::new(pool, storage, settings, None));
    (Router::new().nest(PREFIX, router(state)), dir)
}

fn capture(name: &str) -> Value {
    let path = format!(
        "{}/services/catalyst/tests/fixtures/conformance/social/camera-reel/{name}",
        env!("DCL_ONE_ROOT")
    );
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

#[test]
fn a_visible_persons_screen_rect_round_trips_and_is_omitted_when_absent() {
    let with = json!({"userName":"a","userAddress":"0xa","wearables":[],"isGuest":false,"isEmoting":null,
                      "screenRect":{"x":0.25,"y":0.5,"width":0.25,"height":0.5}});
    let user: User = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(serde_json::to_value(&user).unwrap(), with);
    let without =
        json!({"userName":"a","userAddress":"0xa","wearables":[],"isGuest":false,"isEmoting":null});
    let user: User = serde_json::from_value(without.clone()).unwrap();
    assert_eq!(serde_json::to_value(&user).unwrap(), without);
}

#[test]
fn screen_rect_validity_is_upstreams() {
    let rect = |x, y, width, height| ScreenRect {
        x,
        y,
        width,
        height,
    };
    assert!(rect(0.25, 0.1, 0.2, 0.6).is_valid());
    assert!(rect(0.0, 0.0, 1.0, 1.0).is_valid());
    assert!(
        rect(0.5, 0.5, 0.50005, 0.50005).is_valid(),
        "within the edge tolerance"
    );
    assert!(!rect(0.9, 0.1, 0.2, 0.6).is_valid(), "past the right edge");
    assert!(!rect(0.1, 0.9, 0.2, 0.2).is_valid(), "past the bottom edge");
    assert!(!rect(-0.1, 0.0, 0.2, 0.2).is_valid());
    assert!(!rect(f32::INFINITY, 0.0, 0.2, 0.2).is_valid());
    assert!(!rect(f32::NAN, 0.0, 0.2, 0.2).is_valid());
    let too_big: User =
        serde_json::from_value(json!({"userName":"a","userAddress":"0xa","wearables":[],
        "screenRect":{"x":1e39,"y":0.1,"width":0.2,"height":0.6}}))
        .unwrap();
    assert!(
        !too_big.screen_rect.unwrap().is_valid(),
        "a number past f32 parses as infinity"
    );
}

#[tokio::test]
async fn the_production_preflight_replays_byte_for_byte() {
    let (app, dir) = offline_app(Settings::new(API_URL)).await;
    let captured = capture("preflight-user.json");
    let request = &captured["request"];
    let headers = request["headers"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    let path = format!("{PREFIX}{}", request["path"].as_str().unwrap());
    let (status, got, body) = send(
        app,
        request["method"].as_str().unwrap(),
        &path,
        headers,
        None,
    )
    .await;
    assert_eq!(
        status.as_u16() as u64,
        captured["response"]["status"].as_u64().unwrap()
    );
    assert!(body.is_empty());
    for (name, value) in captured["response"]["headers"].as_object().unwrap() {
        if name == "content-length" {
            continue;
        }
        assert_eq!(got.get(name).unwrap(), value.as_str().unwrap(), "{name}");
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn cors_wraps_api_writes_refusals_and_health_as_upstream() {
    let (app, dir) = offline_app(Settings::new(API_URL)).await;
    let origin = ("origin".to_string(), "https://decentraland.org".to_string());
    let vary = "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";
    let preflight = |method: &str, requested: &str| {
        vec![
            origin.clone(),
            (
                "access-control-request-method".to_string(),
                method.to_string(),
            ),
            (
                "access-control-request-headers".to_string(),
                requested.to_string(),
            ),
        ]
    };

    // A signed upload's preflight: never 405, never the writes 503.
    let (status, h, body) = send(
        app.clone(),
        "OPTIONS",
        &format!("{PREFIX}/api/images"),
        preflight("POST", "x-identity-auth-chain-0, x-identity-timestamp"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(h["access-control-allow-origin"], "https://decentraland.org");
    assert_eq!(
        h["access-control-allow-methods"],
        "DELETE, GET, POST, PATCH"
    );
    assert_eq!(
        h["access-control-allow-headers"],
        "x-identity-auth-chain-0, x-identity-timestamp"
    );
    assert_eq!(h["access-control-max-age"], "300");
    assert_eq!(h["vary"], vary);
    let (status, _, body) = send(
        app.clone(),
        "OPTIONS",
        &format!("{PREFIX}/api/images"),
        preflight("PUT", "content-type"),
        None,
    )
    .await;
    assert_eq!(
        (status, body.as_slice()),
        (
            StatusCode::BAD_REQUEST,
            &b"Requested method is not allowed"[..]
        )
    );

    // The writes 503 carries the headers, so a browser can read it.
    let (status, h, _) = send(
        app.clone(),
        "DELETE",
        &format!("{PREFIX}/api/images/{}", Uuid::new_v4()),
        vec![origin.clone()],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h["access-control-allow-origin"], "https://decentraland.org");
    assert_eq!(h["vary"], vary);
    let mut exposed: Vec<&str> = h["access-control-expose-headers"]
        .to_str()
        .unwrap()
        .split(", ")
        .collect();
    exposed.sort();
    assert_eq!(
        exposed,
        vec!["access-control-allow-origin", "content-type", "retry-after"],
        "actix-cors lists the answer's headers after adding the allow-origin"
    );

    // Without an Origin: Vary and the expose list, no allow-origin (the capture's GET).
    let (status, h, _) = send(
        app.clone(),
        "GET",
        &format!("{PREFIX}/api/wearables/not-an-item/images"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(h["vary"], vary);
    assert_eq!(h["access-control-expose-headers"], "content-type");
    assert!(h.get("access-control-allow-origin").is_none());

    // /health: GET and OPTIONS, max-age 3600, nothing exposed.
    let (status, h, _) = send(
        app.clone(),
        "OPTIONS",
        &format!("{PREFIX}/health/live"),
        preflight("GET", "content-type"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h["access-control-allow-methods"], "GET, OPTIONS");
    assert_eq!(h["access-control-max-age"], "3600");
    assert_eq!(h["access-control-allow-origin"], "https://decentraland.org");
    let (status, h, body) = send(
        app.clone(),
        "GET",
        &format!("{PREFIX}/health/live"),
        vec![origin.clone()],
        None,
    )
    .await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, &b"\"alive\""[..])
    );
    assert_eq!(h["vary"], vary);
    assert_eq!(h["access-control-allow-origin"], "https://decentraland.org");
    assert!(h.get("access-control-expose-headers").is_none());

    // The docs carry none, as upstream registers them outside the /api scope.
    let (status, h, _) = send(
        app,
        "GET",
        &format!("{PREFIX}/api/docs/openapi.json"),
        vec![origin],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(h.get("vary").is_none());
    assert!(h.get("access-control-allow-origin").is_none());
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn unknown_paths_and_methods_are_an_empty_404_inside_any_host_app() {
    let (inner, dir) = offline_app(Settings::new(API_URL)).await;
    let app = inner.fallback(|| async { (StatusCode::IM_A_TEAPOT, "outer") });
    for (method, path) in [
        ("GET", "/api/does-not-exist"),
        ("GET", "/api/images"),
        ("GET", "/api/places/images"),
        ("POST", "/health/live"),
        ("PUT", "/api/users/0xabc"),
        ("DELETE", "/api/users/0xabc/images"),
        ("PATCH", "/api/images/x"),
        ("OPTIONS", "/api/users/0xabc"),
    ] {
        let (status, h, body) = send(
            app.clone(),
            method,
            &format!("{PREFIX}{path}"),
            vec![],
            None,
        )
        .await;
        assert_eq!(
            (status, body.len()),
            (StatusCode::NOT_FOUND, 0),
            "{method} {path}: neither 405 nor the writes 503 ({status})"
        );
        assert!(h.get(header::ALLOW).is_none(), "{method} {path}");
    }
    let (_, h, _) = send(
        app.clone(),
        "GET",
        &format!("{PREFIX}/api/does-not-exist"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        h["vary"], "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        "the capture's 404 carries the /api Vary"
    );
    let (status, h, _) = send(app, "GET", &format!("{PREFIX}/api/docs/ui/"), vec![], None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the Swagger page upstream documents"
    );
    assert!(
        h[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn query_and_body_rejections_are_upstreams_plain_text_400() {
    let (app, dir) = offline_app(Settings::new(API_URL)).await;
    let captured = capture("user-images-bad-limit.raw.json");
    let url = captured["url"].as_str().unwrap();
    let path = &url[url.find("/api/").unwrap()..];
    let expected = std::fs::read(format!(
        "{}/services/catalyst/tests/fixtures/conformance/social/camera-reel/user-images-bad-limit.raw.body",
        env!("DCL_ONE_ROOT")
    ))
    .unwrap();
    let (status, h, body) =
        send(app.clone(), "GET", &format!("{PREFIX}{path}"), vec![], None).await;
    assert_eq!(status.as_u16() as u64, captured["status"].as_u64().unwrap());
    assert_eq!(h[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(body, expected);

    for path in [
        "/api/users/0xabc?limit=abc",
        "/api/users/0xabc?compact=1",
        "/api/places/x/images?offset=-1",
        "/api/wearables/0x0bf152a83a6fc55066c2b664b164ca2916ad38f5-2/images?limit=abc",
    ] {
        let (status, h, body) =
            send(app.clone(), "GET", &format!("{PREFIX}{path}"), vec![], None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(h[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert!(
            String::from_utf8(body)
                .unwrap()
                .starts_with("Query deserialize error: "),
            "{path}"
        );
    }

    let batch = format!("{PREFIX}/api/places/images");
    let (status, _, body) = send(
        app.clone(),
        "POST",
        &batch,
        vec![],
        Some((b"{}".to_vec(), "application/json")),
    )
    .await;
    assert_eq!(
        (status, String::from_utf8(body).unwrap()),
        (
            StatusCode::BAD_REQUEST,
            "Json deserialize error: missing field `placesIds` at line 1 column 2".to_string()
        )
    );
    let ids = json!({ "placesIds": ["x"] }).to_string().into_bytes();
    let (status, _, body) = send(
        app.clone(),
        "POST",
        &batch,
        vec![],
        Some((ids, "text/plain")),
    )
    .await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::BAD_REQUEST, &b"Content type error"[..])
    );
    let big = vec![b' '; (2 * 1024 * 1024) + 1];
    let (status, _, _) = send(
        app.clone(),
        "POST",
        &batch,
        vec![],
        Some((big, "application/json")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "actix's 2 MiB JSON cap"
    );

    // Visibility: the signature decides before the body; then the body is a 400.
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let (app, dir2) = offline_app(settings).await;
    let vis = format!("{PREFIX}/api/images/{}/visibility", Uuid::new_v4());
    let camel = json!({ "isPublic": true }).to_string().into_bytes();
    let (status, _, _) = send(
        app.clone(),
        "PATCH",
        &vis,
        vec![],
        Some((camel.clone(), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let w = wallet();
    let headers = signed_headers(&w, "patch", &vis, true).await;
    let (status, h, body) = send(
        app.clone(),
        "PATCH",
        &vis,
        headers,
        Some((camel, "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(h[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(
        String::from_utf8(body).unwrap(),
        "Json deserialize error: missing field `is_public` at line 1 column 17"
    );
    let headers = signed_headers(&w, "patch", &vis, true).await;
    let flip = json!({ "is_public": true }).to_string().into_bytes();
    let (status, _, body) = send(app, "PATCH", &vis, headers, Some((flip, "text/plain"))).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::BAD_REQUEST, &b"Content type error"[..])
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
    let _ = tokio::fs::remove_dir_all(&dir2).await;
}

#[tokio::test]
async fn a_failed_world_lookup_is_a_502_without_the_database_error() {
    let (app, dir) = offline_app(Settings::new(API_URL)).await;
    let (status, _, body) = send(
        app.clone(),
        "GET",
        &format!("{PREFIX}/api/places/somebody.dcl.eth/images"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_GATEWAY,
            json!({ "message": "failed to resolve world name: request failed: place lookup unavailable" })
        )
    );
    let batch = json!({ "placesIds": ["somebody.dcl.eth"] })
        .to_string()
        .into_bytes();
    let (status, _, body) = send(
        app,
        "POST",
        &format!("{PREFIX}/api/places/images"),
        vec![],
        Some((batch, "application/json")),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_GATEWAY,
            json!({ "message": "failed to resolve world name: request failed: place lookup unavailable" })
        )
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

fn metadata_with_rect(address: &str, rect: Value) -> Value {
    let mut metadata = metadata_json(address, "");
    metadata["visiblePeople"] = json!([{
        "userName": "someone",
        "userAddress": "0x7949f9f239d1a0816ce5eb364a1f588ae9cc1bf5",
        "wearables": [],
        "isGuest": false,
        "isEmoting": false,
        "screenRect": rect
    }]);
    metadata
}

/// A multipart upload with the image part named `file_name` and a raw
/// `is_public` part.
fn multipart_raw(
    image: &[u8],
    file_name: &str,
    metadata: &Value,
    is_public: &str,
) -> (Vec<u8>, String) {
    let boundary = format!("----catalyst{}", tag());
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{file_name}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(image);
    body.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\n{metadata}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"is_public\"\r\n\r\n{is_public}\r\n--{boundary}--\r\n"
        )
        .as_bytes(),
    );
    (body, format!("multipart/form-data; boundary={boundary}"))
}

#[tokio::test]
async fn an_uploaded_screen_rect_is_validated_and_read_back_everywhere() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let path = format!("{PREFIX}/api/images");
    for rect in [
        json!({ "x": 0.9, "y": 0.1, "width": 1000.0, "height": 0.6 }),
        json!({ "x": 1e39, "y": 0.1, "width": 0.2, "height": 0.6 }),
        json!({ "x": 0.9, "y": 0.0, "width": 0.2, "height": 0.5 }),
    ] {
        let (body, ct) = multipart(
            &png(8, 8, 2),
            "image/png",
            &metadata_with_rect(&owner, rect.clone()),
            Some(true),
        );
        let headers = signed_headers(&w, "post", &path, true).await;
        let (status, _, resp) = send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
        assert_eq!(
            (status, json_of(&resp)),
            (
                StatusCode::BAD_REQUEST,
                json!({ "message": "invalid metadata" })
            ),
            "{rect}"
        );
    }
    assert_eq!(s.count(&owner).await, 0);

    let rect = json!({ "x": 0.25, "y": 0.5, "width": 0.25, "height": 0.5 });
    let (body, ct) = multipart(
        &png(8, 8, 3),
        "image/png",
        &metadata_with_rect(&owner, rect.clone()),
        Some(true),
    );
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
    let uploaded = json_of(&resp);
    assert_eq!(
        uploaded["image"]["metadata"]["visiblePeople"][0]["screenRect"],
        rect
    );
    let id = uploaded["image"]["id"].as_str().unwrap().to_string();
    let (_, _, resp) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/images/{id}/metadata"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        json_of(&resp)["metadata"]["visiblePeople"][0]["screenRect"],
        rect
    );
    let (_, _, resp) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/users/{owner}/images"),
        vec![],
        None,
    )
    .await;
    assert_eq!(
        json_of(&resp)["images"][0]["metadata"]["visiblePeople"][0]["screenRect"],
        rect
    );
    s.done().await;
}

#[tokio::test]
async fn upload_checks_form_types_then_cap_then_metadata_as_upstream() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.max_images_per_user = 1;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let path = format!("{PREFIX}/api/images");
    let image = png(8, 8, 4);
    let metadata = metadata_json(&owner, "");
    let upload =
        |file_name: &str, is_public: &str| multipart_raw(&image, file_name, &metadata, is_public);

    for (file_name, is_public, message) in [
        ("any/image.png", "true", "invalid file name"),
        ("any\\image.png", "true", "invalid file name"),
        ("image.png", "yes", "invalid is_public field"),
        ("image.png", " true", "invalid is_public field"),
    ] {
        let (body, ct) = upload(file_name, is_public);
        let headers = signed_headers(&w, "post", &path, true).await;
        let (status, headers, resp) =
            send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{file_name:?} {is_public:?}"
        );
        if message == "invalid is_public field" {
            assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
            assert_eq!(
                String::from_utf8(resp).unwrap(),
                "An error occurred processing field `is_public`: Plain text deserialize error: cannot parse boolean: provided string was not `true` or `false`"
            );
        } else {
            assert_eq!(json_of(&resp), json!({ "message": message }));
        }
    }
    assert_eq!(s.count(&owner).await, 0);

    let (body, ct) = upload("image.png", "false");
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
    assert_eq!(json_of(&resp)["image"]["isPublic"], false);

    // MultipartForm<Upload> parses Text<bool> before the source handler runs,
    // so a malformed boolean still fails before the quota check at the cap.
    let (body, ct) = multipart_raw(b"not an image", "a/b.png", &json!("not metadata"), "maybe");
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, headers, resp) = send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&resp)
    );
    assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(
        String::from_utf8(resp).unwrap(),
        "An error occurred processing field `is_public`: Plain text deserialize error: cannot parse boolean: provided string was not `true` or `false`"
    );

    // Once form types parse, the cap precedes metadata, image and filename checks.
    let (body, ct) = multipart_raw(b"not an image", "a/b.png", &json!("not metadata"), "true");
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "{}",
        String::from_utf8_lossy(&resp)
    );
    assert_eq!(
        json_of(&resp),
        json!({ "reason": "maxLimitReached", "message": "you have reached the limit of 1 max images" })
    );
    assert_eq!(s.count(&owner).await, 1);
    s.done().await;
}

async fn events_of(s: &Scratch, owner: &str) -> Vec<crate::events::PhotoEvent> {
    crate::events::read_after(s.db().pool(), 0, i64::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.user_address == owner.to_lowercase())
        .collect()
}

#[tokio::test]
async fn uploads_and_visibility_changes_record_the_upstream_photo_events() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let place = Uuid::new_v4().to_string();
    let path = format!("{PREFIX}/api/images");

    // A refused upload records nothing.
    let (body, ct) = multipart(
        &png(8, 8, 5),
        "image/png",
        &metadata_with_rect(
            &owner,
            json!({ "x": 2.0, "y": 0.0, "width": 0.1, "height": 0.1 }),
        ),
        Some(false),
    );
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, _) = send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(events_of(&s, &owner).await.is_empty());

    // A private upload: photo-taken, as upstream publishes it for any photo.
    let mut metadata = metadata_with_rect(
        &owner,
        json!({ "x": 0.25, "y": 0.5, "width": 0.25, "height": 0.5 }),
    );
    metadata["placeId"] = json!(place);
    metadata["visiblePeople"][0]["isEmoting"] = Value::Null;
    let (body, ct) = multipart(&png(8, 8, 6), "image/png", &metadata, Some(false));
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(s.app(), "POST", &path, headers, Some((body, &ct))).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
    let id = json_of(&resp)["image"]["id"].as_str().unwrap().to_string();
    let events = events_of(&s, &owner).await;
    assert_eq!(events.len(), 1);
    let taken = &events[0];
    assert_eq!(taken.kind, "photo-taken");
    assert_eq!(taken.photo_id.to_string(), id);
    let timestamp = taken.payload["timestamp"].as_i64().unwrap();
    assert!((chrono::Utc::now().timestamp() - timestamp).abs() < 60);
    assert_eq!(
        taken.payload,
        json!({
            "type": "camera",
            "subType": "photo-taken",
            "key": id,
            "timestamp": timestamp,
            "metadata": {
                "sceneId": "Genesis Plaza",
                "realm": "main",
                "userAddress": owner.to_lowercase(),
                "isPublic": false,
                "photoId": id,
                "placeId": place,
                "users": [{ "address": "0x7949f9f239d1a0816ce5eb364a1f588ae9cc1bf5", "isEmoting": false }]
            }
        })
    );

    // Visibility: a change records photo-privacy-changed; a no-op and a stranger's request do not.
    let vis = format!("{PREFIX}/api/images/{id}/visibility");
    let flip = |public: bool| json!({ "is_public": public }).to_string().into_bytes();
    for (who, public, expected) in [
        (&w, true, StatusCode::OK),
        (&w, true, StatusCode::OK),
        (&wallet(), false, StatusCode::FORBIDDEN),
    ] {
        let headers = signed_headers(who, "patch", &vis, true).await;
        let (status, _, _) = send(
            s.app(),
            "PATCH",
            &vis,
            headers,
            Some((flip(public), "application/json")),
        )
        .await;
        assert_eq!(status, expected);
    }
    let events = events_of(&s, &owner).await;
    assert_eq!(events.len(), 2, "{events:?}");
    let changed = &events[1];
    assert!(changed.id > taken.id);
    assert_eq!(changed.kind, "photo-privacy-changed");
    let timestamp = changed.payload["timestamp"].as_i64().unwrap();
    assert_eq!(
        changed.payload,
        json!({
            "type": "camera",
            "subType": "photo-privacy-changed",
            "key": id,
            "timestamp": timestamp,
            "metadata": { "photoId": id, "userAddress": owner.to_lowercase(), "isPublic": true }
        })
    );
    let after = crate::events::read_after(s.db().pool(), taken.id, 1)
        .await
        .unwrap();
    assert_eq!(after.len(), 1, "the limit holds");
    assert!(after[0].id > taken.id, "strictly after the cursor");
    s.done().await;
}

#[tokio::test]
async fn retention_deletes_only_events_past_thirty_days() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let owner = s.owner(&random_address());
    let payload = json!({ "type": "camera" });
    let mut ids = Vec::new();
    for days in [45, 31, 29, 0] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO camera_reel.photo_events (kind, photo_id, user_address, payload, created_at) \
             VALUES ('photo-taken', $1, $2, $3, now() - make_interval(days => $4)) RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(owner.to_lowercase())
        .bind(&payload)
        .bind(days)
        .fetch_one(s.db().pool())
        .await
        .unwrap();
        ids.push((days, id));
    }
    let deleted =
        crate::events::delete_expired(s.db().pool(), chrono::Utc::now() - crate::events::RETENTION)
            .await
            .unwrap();
    assert!(deleted >= 2);
    let left: Vec<i64> = events_of(&s, &owner)
        .await
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(
        left,
        ids.iter()
            .filter(|(days, _)| *days < 30)
            .map(|(_, id)| *id)
            .collect::<Vec<_>>()
    );
    s.done().await;
}

#[test]
fn thumbnails_decode_only_the_declared_format_within_the_size_limits() {
    use crate::handlers::images::{MAX_IMAGE_DIMENSION, thumbnail_of};
    let ok = png(32, 18, 1);
    assert!(thumbnail_of(&ok, image::ImageFormat::Png).is_ok());
    let err = thumbnail_of(&ok, image::ImageFormat::Jpeg).unwrap_err();
    assert!(
        matches!(err, ApiError::BadRequest(_)),
        "a PNG declared as JPEG"
    );
    let wide = png(MAX_IMAGE_DIMENSION + 1, 1, 1);
    let err = thumbnail_of(&wide, image::ImageFormat::Png).unwrap_err();
    assert!(
        matches!(err, ApiError::BadRequest(_)),
        "over the dimension limit"
    );
    assert!(thumbnail_of(&png(MAX_IMAGE_DIMENSION, 1, 1), image::ImageFormat::Png).is_ok());
}

#[tokio::test]
async fn a_full_account_is_refused_before_the_image_is_decoded() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.max_images_per_user = 1;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let path = format!("{PREFIX}/api/images-json");
    let upload = |image: Vec<u8>| {
        use base64::Engine as _;
        json!({
            "image": base64::engine::general_purpose::STANDARD.encode(image),
            "content_type": "image/png",
            "metadata": metadata_json(&owner, ""),
        })
        .to_string()
        .into_bytes()
    };
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &path,
        headers,
        Some((upload(b"not an image".to_vec()), "application/json")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&resp)
    );
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, _) = send(
        s.app(),
        "POST",
        &path,
        headers,
        Some((upload(png(16, 9, 5)), "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let headers = signed_headers(&w, "post", &path, true).await;
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &path,
        headers,
        Some((upload(b"not an image".to_vec()), "application/json")),
    )
    .await;
    assert_eq!(
        (status, json_of(&resp)["reason"].clone()),
        (StatusCode::FORBIDDEN, json!("maxLimitReached")),
        "the quota answers before the decoder runs"
    );
    s.done().await;
}

#[tokio::test]
async fn a_dropped_upload_keeps_its_decode_permit_until_the_decode_ends() {
    static PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let task = tokio::spawn(crate::handlers::images::decode_with(&PERMITS, move || {
        let _ = wait.recv();
    }));
    let deadline = Instant::now() + Duration::from_secs(5);
    while PERMITS.available_permits() != 0 {
        assert!(Instant::now() < deadline, "the decode never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        PERMITS.available_permits(),
        0,
        "the running decode still holds its permit"
    );
    release.send(()).unwrap();
    while PERMITS.available_permits() != 1 {
        assert!(Instant::now() < deadline, "the permit came back");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
