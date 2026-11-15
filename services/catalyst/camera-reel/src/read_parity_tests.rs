use std::sync::Arc;

use alloy::signers::{Signer as _, local::PrivateKeySigner};
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

use crate::auth::{AuthLink, AuthLinkType};
use crate::db::NewImage;
use crate::dto::{Metadata, User};
use crate::shared::signed_fetch::{
    AUTH_CHAIN_HEADER_PREFIX, AUTH_METADATA_HEADER, AUTH_TIMESTAMP_HEADER, build_payload_v6,
};
use crate::{Settings, SharedState, State, router, signed_path};

async fn state(pool: sqlx::PgPool) -> (SharedState, std::path::PathBuf) {
    state_with_settings(pool, Settings::new("https://camera.example")).await
}

async fn state_with_settings(
    pool: sqlx::PgPool,
    settings: Settings,
) -> (SharedState, std::path::PathBuf) {
    let directory = std::env::temp_dir().join(format!("camera-read-{}", Uuid::new_v4()));
    let storage = Arc::new(
        crate::storage::Storage::from_config(&directory)
            .await
            .unwrap(),
    );
    let state = Arc::new(State::new(pool, storage, settings, None));
    (state, directory)
}

async fn closed_state() -> (SharedState, std::path::PathBuf) {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/camera_read_test")
        .unwrap();
    pool.close().await;
    state(pool).await
}

async fn get(state: &SharedState, path: &str) -> (StatusCode, Vec<u8>) {
    let response = router(state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, body.to_vec())
}

async fn signed_headers(signer: &PrivateKeySigner, path: &str) -> HeaderMap {
    let timestamp = chrono::Utc::now().timestamp_millis().to_string();
    let payload = build_payload_v6("get", path, &timestamp, "{}");
    let signature = signer
        .sign_message(payload.as_bytes())
        .await
        .unwrap()
        .to_string();
    let chain = [
        AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: format!("{:#x}", signer.address()),
            signature: Some(String::new()),
        },
        AuthLink {
            link_type: AuthLinkType::EcdsaSignedEntity,
            payload,
            signature: Some(signature),
        },
    ];
    let mut headers = HeaderMap::new();
    headers.insert(AUTH_TIMESTAMP_HEADER, timestamp.parse().unwrap());
    headers.insert(AUTH_METADATA_HEADER, "{}".parse().unwrap());
    for (index, link) in chain.iter().enumerate() {
        let name: axum::http::HeaderName = format!("{AUTH_CHAIN_HEADER_PREFIX}{index}")
            .parse()
            .unwrap();
        headers.insert(name, serde_json::to_string(link).unwrap().parse().unwrap());
    }
    headers
}

async fn json_request(
    state: &SharedState,
    method: &str,
    path: &str,
    signer: Option<&PrivateKeySigner>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(signer) = signer {
        let signed_path = path.split('?').next().unwrap();
        *request.headers_mut().unwrap() = signed_headers(signer, signed_path).await;
    }
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = router(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn user_counts_validate_gallery_query_before_touching_the_database() {
    let (state, directory) = closed_state().await;
    for query in [
        "offset=-1",
        "limit=no",
        "compact=1",
        "limit=18446744073709551616",
    ] {
        let (status, _) = get(&state, &format!("/api/users/anyone?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "query {query}");
    }
    // Valid pagination remains ignored by the count endpoint; reaching the closed
    // database produces its regular user lookup error rather than a query error.
    let (status, body) = get(&state, "/api/users/anyone?offset=3&limit=0&compact=true").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"message":"user not found"})
    );
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn private_gallery_matches_the_owner_address_in_the_path_in_any_case() {
    let (state, directory) = closed_state().await;
    let signer = PrivateKeySigner::random();
    let address = format!("{:#x}", signer.address());
    for (requested_address, public_only) in
        [(address.clone(), false), (address.to_uppercase(), false)]
    {
        let local_path = format!("/api/users/{requested_address}/images");
        let path = signed_path(&local_path.parse().unwrap(), local_path.clone());
        let headers = signed_headers(&signer, &local_path).await;
        assert_eq!(
            super::only_public_for(&state, &headers, "get", &path, &requested_address).await,
            public_only,
        );
        assert!(
            super::only_public_for(&state, &HeaderMap::new(), "get", &path, &requested_address)
                .await
        );
    }
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn world_resolution_failure_is_bad_gateway_for_single_and_batch_reads() {
    let (state, directory) = closed_state().await;
    let expected = json!({
        "message": "failed to resolve world name: request failed: place lookup unavailable"
    });
    let (status, body) = json_request(
        &state,
        "GET",
        "/api/places/unavailable.eth/images",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, expected);
    for ids in [
        json!(["unavailable.eth"]),
        json!(["invalid-place-id", "unavailable.eth"]),
        json!(["unavailable.eth", "another.eth"]),
    ] {
        let (status, body) = json_request(
            &state,
            "POST",
            "/api/places/images",
            None,
            Some(json!({"placesIds": ids})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body, expected);
    }
    // Image-query failures have the source's 404 messages, independent of the
    // 502 used for world-name resolution. The .eth suffix is case-sensitive.
    for id in [Uuid::new_v4().to_string(), "unavailable.ETH".to_owned()] {
        let (status, body) = json_request(
            &state,
            "GET",
            &format!("/api/places/{id}/images"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"message":"place not found"}));
    }
    let (status, body) = json_request(
        &state,
        "POST",
        "/api/places/images",
        None,
        Some(json!({"placesIds":[Uuid::new_v4().to_string()]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"message":"places not found"}));
    let (status, body) = json_request(
        &state,
        "POST",
        "/api/places/images",
        None,
        Some(json!({"placesIds":[]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({"message":"no place IDs provided"}));
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn cached_empty_worlds_skip_image_queries_for_single_and_batch_reads() {
    let Some(pool) = catalyst_db::scratch::pool(2).await else {
        return;
    };
    let (state, directory) = state(pool).await;
    let world = format!("{}.eth", Uuid::new_v4());
    let empty = json!({"images":[],"maxImages":0});
    for _ in 0..2 {
        let (status, body) = json_request(
            &state,
            "GET",
            &format!("/api/places/{world}/images"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, empty);
        // The second pass must use the cached successful empty lookup.
        state.db.pool().close().await;
    }
    // Only the exact world name was cached. A case variant still needs its
    // own lookup even though both queries use the same lowercase catalogue key.
    let variant = format!("{}.eth", world.trim_end_matches(".eth").to_uppercase());
    let (status, body) = json_request(
        &state,
        "GET",
        &format!("/api/places/{variant}/images"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        body,
        json!({
            "message": "failed to resolve world name: request failed: place lookup unavailable"
        })
    );
    let (status, body) = json_request(
        &state,
        "POST",
        "/api/places/images",
        None,
        Some(json!({"placesIds":[world.clone(),world.clone()]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, empty);
    let (status, body) = json_request(
        &state,
        "POST",
        "/api/places/images",
        None,
        Some(json!({"placesIds":[world,Uuid::new_v4().to_string()]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"message":"places not found"}));
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn read_pages_keep_counts_when_empty_and_reject_offsets_outside_sql_range() {
    let Some(pool) = catalyst_db::scratch::pool(2).await else {
        return;
    };
    let (state, directory) = state(pool).await;
    let owner_signer = PrivateKeySigner::random();
    let stranger_signer = PrivateKeySigner::random();
    let owner = format!("{:#x}", owner_signer.address());
    let place = Uuid::new_v4().to_string();
    let mut image_ids = Vec::new();
    for is_public in [true, true, false] {
        let id = Uuid::new_v4();
        state
            .db
            .insert_image(&NewImage {
                id,
                image_hash: format!("image-{id}"),
                thumbnail_hash: format!("thumb-{id}"),
                is_public,
                metadata: Metadata {
                    user_address: owner.clone(),
                    place_id: place.clone(),
                    visible_people: vec![User {
                        wearables: vec![format!(
                            "urn:decentraland:matic:collections-v2:{owner}:0:1"
                        )],
                        ..User::default()
                    }],
                    ..Metadata::default()
                },
            })
            .await
            .unwrap();
        let timestamp = chrono::DateTime::from_timestamp(1_700_000_000 + image_ids.len() as i64, 0)
            .unwrap()
            .naive_utc();
        sqlx::query("UPDATE camera_reel.images SET created_at=$1 WHERE id=$2")
            .bind(timestamp)
            .bind(id)
            .execute(state.db.pool())
            .await
            .unwrap();
        image_ids.push(id.to_string());
    }
    for (signer, count) in [
        (None, 2),
        (Some(&owner_signer), 3),
        (Some(&stranger_signer), 2),
    ] {
        let (status, counts) =
            json_request(&state, "GET", &format!("/api/users/{owner}"), signer, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(counts, json!({"currentImages":count,"maxImages":500}));
        let (status, gallery) = json_request(
            &state,
            "GET",
            &format!("/api/users/{owner}/images?limit=1"),
            signer,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(gallery["currentImages"], count);
        assert_eq!(gallery["images"].as_array().unwrap().len(), 1);
        assert_eq!(gallery["images"][0]["id"], image_ids[count as usize - 1]);
        assert!(gallery["images"][0]["metadata"].is_object());
        assert!(gallery.get("nextCursor").is_none());
        let (status, compact) = json_request(
            &state,
            "GET",
            &format!("/api/users/{owner}/images?compact=true&offset=1&limit=1"),
            signer,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(compact["images"][0]["id"], image_ids[count as usize - 2]);
        assert!(compact["images"][0].get("metadata").is_none());
        assert!(compact["images"][0]["dateTime"].is_string());
        for path in [
            format!("/api/places/{place}/images?limit=1&offset=1"),
            format!("/api/wearables/{owner}-0/images?limit=1&offset=1"),
        ] {
            let (status, gallery) = json_request(&state, "GET", &path, signer, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(gallery["maxImages"], 2);
            assert_eq!(gallery["images"].as_array().unwrap().len(), 1);
            assert_eq!(gallery["images"][0]["id"], image_ids[0]);
        }
    }
    let (status, batch) = json_request(
        &state,
        "POST",
        "/api/places/images?limit=1&offset=1",
        None,
        Some(json!({"placesIds":[place, place]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        batch["maxImages"], 2,
        "duplicate place ids do not duplicate photos or count"
    );
    assert_eq!(batch["images"][0]["id"], image_ids[0]);
    assert_eq!(batch["images"][0]["placeId"], place);
    for query in ["limit=0", "offset=2", "offset=9223372036854775807"] {
        let (status, body) = get(&state, &format!("/api/users/{owner}/images?{query}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"images":[],"currentImages":2,"maxImages":500})
        );
        let (status, body) = get(&state, &format!("/api/places/{place}/images?{query}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"images":[],"maxImages":2})
        );
    }
    for offset in ["9223372036854775808", "18446744073709551615"] {
        for (path, message) in [
            (format!("/api/users/{owner}/images"), "user not found"),
            (format!("/api/places/{place}/images"), "place not found"),
        ] {
            let (status, body) = get(&state, &format!("{path}?offset={offset}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"message":message})
            );
        }
        let (status, body) = get(
            &state,
            &format!("/api/wearables/{owner}-0/images?offset={offset}"),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"message":"failed to fetch the images of the item"})
        );
    }
    sqlx::query("DELETE FROM camera_reel.images WHERE user_address=$1")
        .bind(owner)
        .execute(state.db.pool())
        .await
        .unwrap();
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn unavailable_wearable_count_has_its_own_error_message() {
    let (state, directory) = closed_state().await;
    let item = "0x0000000000000000000000000000000000000001-0";
    let (status, body) = get(&state, &format!("/api/wearables/{item}/images")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"message":"failed to count the images of the item"})
    );
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn zero_world_lookup_ttl_or_capacity_disables_success_cache() {
    for (ttl_seconds, max_size) in [(0, 1000), (300, 0)] {
        let Some(pool) = catalyst_db::scratch::pool(2).await else {
            return;
        };
        let mut settings = Settings::new("https://camera.example");
        settings.places_cache_ttl_seconds = ttl_seconds;
        settings.places_cache_max_size = max_size;
        let (state, directory) = state_with_settings(pool, settings).await;
        let world = format!("{}.eth", Uuid::new_v4());
        let path = format!("/api/places/{world}/images");
        let (status, body) = json_request(&state, "GET", &path, None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"images":[],"maxImages":0}));
        state.db.pool().close().await;
        let expected = json!({
            "message":"failed to resolve world name: request failed: place lookup unavailable"
        });
        let (status, body) = json_request(&state, "GET", &path, None, None).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body, expected);
        let (status, body) = json_request(
            &state,
            "POST",
            "/api/places/images",
            None,
            Some(json!({"placesIds":[world]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body, expected);
        tokio::fs::remove_dir_all(directory).await.unwrap();
    }
}
