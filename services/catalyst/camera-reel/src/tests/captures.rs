//! Every production capture of camera-reel-service
//! (`tests/fixtures/conformance/social/camera-reel/`, 2026-09-25) replayed
//! against the router as `serve` mounts it, through the social family's
//! shared replay (`social::test_fixtures`). The rows the captured answers
//! read are seeded from the captured bodies: the photos with their metadata,
//! place ids, public flag and `created_at` from their `dateTime`, plus
//! filler rows that bring the place gallery to its captured `maxImages`
//! (523) and the item gallery to its (12). No blob is stored: no captured
//! answer carries image bytes.

use axum::Router;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{PREFIX, json_of, scratch, send};
use crate::Settings;
use crate::auth::hash_bytes_v1;
use crate::social::test_fixtures;

const SERVICE: &str = "camera-reel";

/// The corpus place of the place captures, and the other place a photo of
/// the item gallery was taken at.
const PLACE: &str = "3ca25728-5b41-48f6-8e24-5cb0e3f2bb5d";
const OTHER_PLACE: &str = "467c89c4-3ca1-4a39-8cb4-851b7c801d49";
const ITEM_URN: &str =
    "urn:decentraland:matic:collections-v2:0x8bac6894f5e7c2a34a237d451f8cb0cf1a824ecc:0";
/// The owner of every row the captures show without its metadata.
const FILLER_OWNER: &str = "0x000000000000000000000000000000000000c0de";
/// The gallery owners of the captured photos.
const CAPTURED_OWNERS: [&str; 2] = [
    "0x0745eeaf0e65e77a65f3fc6aadce51ca3317ca44",
    "0x62c1d7d461837cbf86dd8e99734fe709c4382f43",
];

/// The 21 captures and their dated upstream differences. The shared diff
/// skips every `id` and `url` leaf; [`assert_photo_ids_and_urls`] checks
/// those of the photo answers exactly, including filenames and order.
const CAPTURES: &[(&str, &[&str])] = &[
    (
        "docs-openapi",
        &[
            // Upstream 3e8bd0b (#61), after the capture.
            "components.schemas.ScreenRect extra",
            "components.schemas.User.properties.screenRect extra",
        ],
    ),
    ("health-live", &[]),
    ("image-file-redirect", &[]),
    ("image-metadata-404", &[]),
    ("image-metadata", &[]),
    ("image-redirect", &[]),
    ("place-images-page2", &[]),
    ("place-images-unknown-place", &[]),
    ("place-images", &[]),
    ("places-images-batch-400-empty", &[]),
    ("places-images-batch", &[]),
    ("preflight-user", &[]),
    ("unknown-path-404", &[]),
    ("user-images-bad-limit", &[]),
    ("user-images-compact-page2", &[]),
    ("user-images-limit-over-max", &[]),
    ("user-images", &[]),
    ("user-unknown-address", &[]),
    ("user", &[]),
    ("wearable-images-400-bad-item", &[]),
    ("wearable-images", &[]),
];

/// A photo a capture shows: its id, owner, metadata and `dateTime`.
struct Captured {
    id: Uuid,
    owner: String,
    metadata: Value,
    date_time: i64,
    original_filename: String,
}

fn body(name: &str) -> Value {
    test_fixtures::fixture(SERVICE, name)
        .response
        .body_json
        .unwrap()
}

/// The photos the captures show: the item gallery's with their whole
/// documents, then the place gallery's compact entries the item gallery
/// does not carry (their documents are not in the captures, so they get a
/// filler one at the corpus place).
fn captured_photos() -> Vec<Captured> {
    let mut photos: Vec<Captured> = body("wearable-images")["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|image| Captured {
            id: image["id"].as_str().unwrap().parse().unwrap(),
            owner: image["metadata"]["userAddress"].as_str().unwrap().into(),
            metadata: image["metadata"].clone(),
            original_filename: captured_filename(image),
            date_time: image["metadata"]["dateTime"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        })
        .collect();
    for image in body("place-images")["images"].as_array().unwrap() {
        let id: Uuid = image["id"].as_str().unwrap().parse().unwrap();
        if photos.iter().any(|p| p.id == id) {
            continue;
        }
        let date_time = image["dateTime"].as_str().unwrap();
        photos.push(Captured {
            id,
            owner: FILLER_OWNER.into(),
            metadata: filler_metadata(date_time, PLACE, &[]),
            date_time: date_time.parse().unwrap(),
            original_filename: captured_filename(image),
        });
    }
    photos
}

fn captured_filename(image: &Value) -> String {
    let id = image["id"].as_str().unwrap();
    image["url"]
        .as_str()
        .unwrap()
        .rsplit_once(&format!("/api/images/{id}-"))
        .unwrap()
        .1
        .to_string()
}

fn filler_metadata(date_time: &str, place_id: &str, wearables: &[String]) -> Value {
    json!({
        "userName": "filler",
        "userAddress": FILLER_OWNER,
        "dateTime": date_time,
        "realm": "main",
        "scene": { "name": "filler", "location": { "x": "0", "y": "0" } },
        "visiblePeople": [{
            "userName": "filler",
            "userAddress": FILLER_OWNER,
            "wearables": wearables,
            "isGuest": false,
            "isEmoting": false
        }],
        "placeId": place_id
    })
}

fn image_hash(id: Uuid) -> String {
    hash_bytes_v1(id.as_bytes())
}

fn thumbnail_hash(id: Uuid) -> String {
    hash_bytes_v1(format!("{id}-thumbnail").as_bytes())
}

async fn insert(
    pool: &sqlx::PgPool,
    id: Uuid,
    owner: &str,
    metadata: &Value,
    date_time: i64,
    original_filename: &str,
) {
    sqlx::query(
        "INSERT INTO camera_reel.images \
         (id, user_address, image_hash, thumbnail_hash, is_public, metadata, created_at, original_filename) \
         VALUES ($1, $2, $3, $4, true, $5, to_timestamp($6) AT TIME ZONE 'UTC', $7)",
    )
    .bind(id)
    .bind(owner)
    .bind(image_hash(id))
    .bind(thumbnail_hash(id))
    .bind(metadata)
    .bind(date_time as f64)
    .bind(original_filename)
    .execute(pool)
    .await
    .unwrap();
}

async fn clear(pool: &sqlx::PgPool, ids: &[Uuid]) {
    let owners: Vec<String> = CAPTURED_OWNERS
        .iter()
        .chain([&FILLER_OWNER])
        .map(|s| s.to_string())
        .collect();
    sqlx::query(
        "DELETE FROM camera_reel.images \
         WHERE id = ANY($1) OR user_address = ANY($2) OR place_id = ANY($3)",
    )
    .bind(ids)
    .bind(&owners)
    .bind([PLACE, OTHER_PLACE].map(String::from).to_vec())
    .execute(pool)
    .await
    .unwrap();
}

/// The photos of an answer in order: `(id, url, thumbnailUrl)`.
fn photos_of(answer: &Value) -> Vec<[String; 3]> {
    let images = match answer.get("images") {
        Some(images) => images.as_array().unwrap().clone(),
        None => vec![answer.clone()],
    };
    images
        .iter()
        .map(|i| ["id", "url", "thumbnailUrl"].map(|k| i[k].as_str().unwrap().to_string()))
        .collect()
}

/// Check every photo id and both literal filename URLs, in captured order.
async fn assert_photo_ids_and_urls(app: &Router, name: &str) {
    let fx = test_fixtures::fixture(SERVICE, name);
    let mut uri = format!("{PREFIX}{}", fx.request.path);
    let query: Vec<String> = fx
        .request
        .query
        .iter()
        .flat_map(|(k, vs)| vs.iter().map(move |v| format!("{k}={v}")))
        .collect();
    if !query.is_empty() {
        uri = format!("{uri}?{}", query.join("&"));
    }
    let body = fx
        .request
        .body
        .as_ref()
        .map(|b| (serde_json::to_vec(b).unwrap(), "application/json"));
    let (_, _, bytes) = send(app.clone(), &fx.request.method, &uri, Vec::new(), body).await;
    let got = photos_of(&json_of(&bytes));
    let captured = photos_of(fx.response.body_json.as_ref().unwrap());
    assert_eq!(got, captured, "{name}: photo ids, order and exact URLs");
}

#[tokio::test]
async fn the_camera_reel_host_answers_as_its_captures() {
    let metadata = body("image-metadata");
    let api_url = metadata["url"]
        .as_str()
        .unwrap()
        .split_once("/api/images/")
        .unwrap()
        .0;
    let redirect = test_fixtures::fixture(SERVICE, "image-redirect");
    let bucket_url = redirect.response.headers["location"]
        .rsplit_once('/')
        .unwrap()
        .0
        .to_string();
    let mut settings = Settings::new(api_url);
    settings.bucket_url = Some(bucket_url);
    let Some(s) = scratch(settings).await else {
        return;
    };
    let pool = s.db().pool().clone();
    let photos = captured_photos();
    let ids: Vec<Uuid> = photos.iter().map(|p| p.id).collect();
    clear(&pool, &ids).await;
    for photo in &photos {
        insert(
            &pool,
            photo.id,
            &photo.owner,
            &photo.metadata,
            photo.date_time,
            &photo.original_filename,
        )
        .await;
    }
    let oldest = photos.iter().map(|p| p.date_time).min().unwrap();
    // The place gallery's other public photos, older than every captured one.
    let place_total = body("place-images")["maxImages"].as_u64().unwrap() as usize;
    let at_place = photos
        .iter()
        .filter(|p| p.metadata["placeId"] == PLACE)
        .count();
    for n in 0..place_total - at_place {
        let date_time = oldest - 1 - n as i64;
        let metadata = filler_metadata(&date_time.to_string(), PLACE, &[]);
        insert(
            &pool,
            Uuid::new_v4(),
            FILLER_OWNER,
            &metadata,
            date_time,
            &format!("{date_time}.jpg"),
        )
        .await;
    }
    // The item gallery's other public photos of someone wearing the item,
    // at no captured place and older than every captured one.
    let item_total = body("wearable-images")["maxImages"].as_u64().unwrap() as usize;
    let wearing = photos
        .iter()
        .filter(|p| p.metadata.to_string().contains(ITEM_URN))
        .count();
    for n in 0..item_total - wearing {
        let date_time = oldest - 1_000_000 - n as i64;
        let metadata = filler_metadata(
            &date_time.to_string(),
            "",
            &[format!("{ITEM_URN}:{}", 1000 + n)],
        );
        insert(
            &pool,
            Uuid::new_v4(),
            FILLER_OWNER,
            &metadata,
            date_time,
            &format!("{date_time}.jpg"),
        )
        .await;
    }

    let app = s.app();
    let mut unexpected = Vec::new();
    for (name, expected) in CAPTURES {
        let fx = test_fixtures::fixture(SERVICE, name);
        let notes = test_fixtures::replay(&app, PREFIX, name, &fx).await;
        if notes != *expected {
            unexpected.push(format!("{name}: {notes:#?}"));
        }
    }
    for name in [
        "image-metadata",
        "place-images",
        "place-images-page2",
        "places-images-batch",
        "wearable-images",
    ] {
        assert_photo_ids_and_urls(&app, name).await;
    }
    clear(&pool, &ids).await;
    s.done().await;

    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
    assert_eq!(CAPTURES.len(), 21, "one entry per capture");
}
