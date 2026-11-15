use super::*;
use crate::dto::{ScreenRect, User};
use std::sync::Arc;

async fn state(pool: sqlx::PgPool) -> (SharedState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("camera-reel-write-contract-{}", Uuid::new_v4()));
    let storage = Arc::new(Storage::from_config(&dir).await.unwrap());
    let state = Arc::new(crate::State::new(
        pool,
        storage,
        crate::Settings::new("https://camera.example"),
        None,
    ));
    (state, dir)
}

fn metadata(address: &str, screen_rect: Option<ScreenRect>) -> Metadata {
    Metadata {
        user_address: address.to_owned(),
        visible_people: vec![User {
            screen_rect,
            ..User::default()
        }],
        ..Metadata::default()
    }
}

fn png() -> Bytes {
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    Bytes::from(output.into_inner())
}

#[tokio::test]
async fn upload_rejects_invalid_rects_before_storage_and_database_access() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    for rect in [
        ScreenRect {
            x: 0.9,
            y: 0.0,
            width: 0.2,
            height: 0.2,
        },
        ScreenRect {
            x: f32::INFINITY,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        },
        ScreenRect {
            x: 0.0,
            y: -0.1,
            width: 0.2,
            height: 0.2,
        },
    ] {
        let response = finalize_upload(
            &state,
            "0xabc",
            png(),
            Some("image/png".into()),
            Some("shot.png"),
            metadata("0xabc", Some(rect)),
            false,
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"message":"invalid metadata"})
        );
    }
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn upload_rejects_path_separators_in_the_original_filename() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    for name in ["any/image.png", "any\\image.png"] {
        let error = finalize_upload(
            &state,
            "0xabc",
            png(),
            Some("image/png".into()),
            Some(name),
            metadata("0xabc", None),
            false,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ApiError::BadRequest(ref message) if message == "invalid file name")
        );
    }
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn upload_requires_metadata_address_to_be_the_signer_in_any_case() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    let error = finalize_upload(
        &state,
        "0xabc",
        png(),
        Some("image/png".into()),
        Some("shot.png"),
        metadata("0xDEF", None),
        false,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, ApiError::BadRequest(ref message) if message == "invalid user address")
    );
    // The same address in another case passes the check and reaches the
    // (closed) database.
    let error = finalize_upload(
        &state,
        "0xabc",
        png(),
        Some("image/png".into()),
        Some("shot.png"),
        metadata("0xABC", None),
        false,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ApiError::Internal(_)), "{error:?}");
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn uploaded_screen_rect_survives_database_roundtrip_and_metadata_response() {
    let Some(pool) = catalyst_db::scratch::pool(4).await else {
        return;
    };
    let (state, dir) = state(pool).await;
    let address = "0x0000000000000000000000000000000000000001";
    let rect = ScreenRect {
        x: 0.25,
        y: 0.125,
        width: 0.5,
        height: 0.75,
    };
    let response = finalize_upload(
        &state,
        address,
        png(),
        Some("image/png".into()),
        Some("shot.png"),
        metadata(address, Some(rect)),
        false,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    let uploaded: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = uploaded["image"]["id"].as_str().unwrap();
    let stored = state.db.get_image(id).await.unwrap();
    assert_eq!(stored.metadata.0.visible_people[0].screen_rect, Some(rect));
    let response = get_metadata(State(state.clone()), Path(id.to_owned()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    let fetched: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        fetched["metadata"]["visiblePeople"][0]["screenRect"],
        uploaded["image"]["metadata"]["visiblePeople"][0]["screenRect"]
    );
    state
        .db
        .delete_image(Uuid::parse_str(id).unwrap())
        .await
        .unwrap();
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

async fn headers(
    metadata: &str,
    timestamp: &str,
    legacy: bool,
    method: &str,
    path: &str,
) -> (HeaderMap, String) {
    use crate::auth::{AuthLink, AuthLinkType};
    use crate::shared::signed_fetch::{build_legacy_payload, build_payload_v6};
    use alloy::signers::{Signer as _, local::PrivateKeySigner};
    let signer = PrivateKeySigner::random();
    let address = format!("{:#x}", signer.address());
    let payload = if legacy {
        build_legacy_payload(method, path, timestamp, metadata)
    } else {
        build_payload_v6(method, path, timestamp, metadata)
    };
    let signature = signer
        .sign_message(payload.as_bytes())
        .await
        .unwrap()
        .to_string();
    let chain = [
        AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: address.clone(),
            signature: Some(String::new()),
        },
        AuthLink {
            link_type: AuthLinkType::EcdsaSignedEntity,
            payload,
            signature: Some(signature),
        },
    ];
    let mut headers = HeaderMap::new();
    for (index, link) in chain.iter().enumerate() {
        headers.insert(
            axum::http::HeaderName::from_bytes(format!("x-identity-auth-chain-{index}").as_bytes())
                .unwrap(),
            serde_json::to_string(link).unwrap().parse().unwrap(),
        );
    }
    headers.insert("x-identity-timestamp", timestamp.parse().unwrap());
    headers.insert("x-identity-metadata", metadata.parse().unwrap());
    (headers, address)
}

#[tokio::test]
async fn camera_auth_accepts_raw_metadata_and_refuses_the_scene_signer_on_writes() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    let path = crate::SignedPath::local("/api/images");
    for metadata in [
        "Not JSON",
        "",
        "null",
        r#"{"signer":"decentraland-kernel-scene","Realm":"Main"}"#,
        r#"{"Signer":"scene","signer":"other"}"#,
    ] {
        for legacy in [false, true] {
            let timestamp = chrono::Utc::now().timestamp_millis().to_string();
            let (headers, address) =
                headers(metadata, &timestamp, legacy, "post", "/api/images").await;
            assert_eq!(
                state.optional_auth(&headers, "post", &path).await,
                Some(address.clone()),
                "a read serves {metadata:?} as its signer"
            );
            let write = state.require_auth(&headers, "post", &path).await;
            if metadata.contains("decentraland-kernel-scene") {
                assert!(
                    matches!(write, Err(ApiError::SignedFetchUnauthorized)),
                    "a scene's write: {metadata:?}"
                );
            } else {
                assert_eq!(write.unwrap(), address);
            }
        }
    }
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn camera_auth_requires_metadata_and_uses_a_one_minute_one_sided_expiry() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    let path = crate::SignedPath::local("/api/images");
    for (offset_ms, accepted) in [(-120_000, false), (30_000, true), (86_400_000, true)] {
        let timestamp = (chrono::Utc::now().timestamp_millis() + offset_ms).to_string();
        let (headers, _) = headers("{}", &timestamp, false, "post", "/api/images").await;
        assert_eq!(
            state.require_auth(&headers, "post", &path).await.is_ok(),
            accepted
        );
    }
    let timestamp = chrono::Utc::now().timestamp_millis().to_string();
    let (mut headers, _) = headers("{}", &timestamp, false, "post", "/api/images").await;
    headers.remove("x-identity-metadata");
    assert!(matches!(
        state.require_auth(&headers, "post", &path).await,
        Err(ApiError::SignedFetchUnauthorized)
    ));
    assert!(state.optional_auth(&headers, "post", &path).await.is_none());
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn camera_auth_binds_current_metadata_bytes_and_requested_path() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    let path = crate::SignedPath::local("/api/images");
    let timestamp = chrono::Utc::now().timestamp_millis().to_string();
    let (mut headers, _) = headers(
        r#"{"realm":"Main"}"#,
        &timestamp,
        false,
        "post",
        "/api/images",
    )
    .await;
    assert!(state.require_auth(&headers, "delete", &path).await.is_err());
    assert!(
        state
            .require_auth(
                &headers,
                "post",
                &crate::SignedPath::local("/api/images-json")
            )
            .await
            .is_err()
    );
    headers.insert(
        "x-identity-metadata",
        r#"{"Realm":"Main"}"#.parse().unwrap(),
    );
    assert!(state.require_auth(&headers, "post", &path).await.is_err());
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

type FormPart<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

fn form(parts: &[FormPart<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, filename, content_type, bytes) in parts {
        body.extend_from_slice(
            format!("--camera-parity\r\nContent-Disposition: form-data; name=\"{name}\"")
                .as_bytes(),
        );
        if let Some(filename) = filename {
            body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        if let Some(content_type) = content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--camera-parity--\r\n");
    body
}

async fn request(
    state: &SharedState,
    method: &str,
    path: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
    signed: bool,
) -> (StatusCode, String, String) {
    use axum::routing::{patch, post};
    use tower::ServiceExt;
    let app = axum::Router::new()
        .route("/api/images", post(upload_image))
        .route(
            "/api/images/{id}/visibility",
            patch(update_image_visibility),
        )
        .layer(axum::extract::DefaultBodyLimit::max(crate::MAX_BODY_BYTES))
        .with_state(state.clone());
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .body(Body::from(body))
        .unwrap();
    if signed {
        let timestamp = chrono::Utc::now().timestamp_millis().to_string();
        let (headers, _) = headers("{}", &timestamp, false, method, path).await;
        *request.headers_mut() = headers;
    }
    if let Some(content_type) = content_type {
        request
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

const FORM_TYPE: &str = "multipart/form-data; boundary=camera-parity";

#[tokio::test]
async fn multipart_invalid_booleans_and_field_types_match_plain_text_errors() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    for value in ["TRUE", " true ", "1", "", "false\n"] {
        let body = form(&[("is_public", None, None, value.as_bytes())]);
        let (status, content_type, body) =
            request(&state, "POST", "/api/images", body, Some(FORM_TYPE), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(
            body,
            "An error occurred processing field `is_public`: Plain text deserialize error: cannot parse boolean: provided string was not `true` or `false`"
        );
    }
    let body = form(&[("is_public", None, Some("application/json"), b"true")]);
    assert_eq!(
        request(&state, "POST", "/api/images", body, Some(FORM_TYPE), true)
            .await
            .2,
        "An error occurred processing field `is_public`: Content type error"
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn multipart_missing_fields_and_boundary_use_plain_errors_and_auth_runs_first() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    let missing_image = request(
        &state,
        "POST",
        "/api/images",
        form(&[]),
        Some(FORM_TYPE),
        true,
    )
    .await;
    assert_eq!(
        missing_image,
        (
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8".into(),
            "Field with name `image` is required".into()
        )
    );
    let missing_metadata = request(
        &state,
        "POST",
        "/api/images",
        form(&[("image", None, Some("image/png"), &png())]),
        Some(FORM_TYPE),
        true,
    )
    .await;
    assert_eq!(missing_metadata.2, "Field with name `metadata` is required");
    let no_content_type = request(&state, "POST", "/api/images", Vec::new(), None, true).await;
    assert_eq!(no_content_type.2, "No Content-Type header found");
    let missing_boundary = request(
        &state,
        "POST",
        "/api/images",
        Vec::new(),
        Some("multipart/form-data"),
        true,
    )
    .await;
    assert_eq!(missing_boundary.2, "Multipart boundary is not found");
    let unsigned = request(&state, "POST", "/api/images", Vec::new(), None, false).await;
    assert_eq!(
        unsigned,
        (
            StatusCode::UNAUTHORIZED,
            "text/plain; charset=utf-8".into(),
            "Unathorized".into()
        )
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn multipart_first_duplicate_wins_and_total_memory_is_bounded() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    let body = form(&[
        ("image", None, Some("image/png"), &png()),
        ("metadata", None, None, b"invalid metadata"),
        ("metadata", None, None, b"{}"),
        ("is_public", None, None, b"true"),
        ("is_public", None, None, b"invalid boolean"),
        ("unknown", None, None, b"ignored"),
    ]);
    let response = request(&state, "POST", "/api/images", body, Some(FORM_TYPE), true).await;
    assert_eq!(
        response,
        (
            StatusCode::BAD_REQUEST,
            "application/json".into(),
            "{\"message\":\"invalid metadata\"}".into()
        )
    );
    let oversized = vec![0; MAX_MULTIPART_MEMORY_BYTES + 1];
    let half = vec![0; MAX_MULTIPART_MEMORY_BYTES / 2];
    for body in [
        form(&[("image", None, Some("image/png"), &oversized)]),
        form(&[("metadata", None, None, &oversized)]),
        form(&[("is_public", None, Some("text/plain"), &oversized)]),
        form(&[
            ("image", None, Some("image/png"), &half),
            ("metadata", None, None, &half),
            ("is_public", None, None, b"true"),
        ]),
    ] {
        let response = request(&state, "POST", "/api/images", body, Some(FORM_TYPE), true).await;
        assert_eq!(
            response,
            (
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8".into(),
                "payload reached size limit".into(),
            )
        );
    }
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn visibility_json_errors_are_400_after_authentication() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    let path = "/api/images/00000000-0000-0000-0000-000000000000/visibility";
    let response = request(
        &state,
        "PATCH",
        path,
        b"{}".to_vec(),
        Some("application/json"),
        true,
    )
    .await;
    assert_eq!(response.0, StatusCode::BAD_REQUEST);
    assert_eq!(response.1, "text/plain; charset=utf-8");
    assert!(response.2.contains("missing field `is_public`"));
    let response = request(
        &state,
        "PATCH",
        path,
        b"{}".to_vec(),
        Some("application/json"),
        false,
    )
    .await;
    assert_eq!(
        response,
        (
            StatusCode::UNAUTHORIZED,
            "text/plain; charset=utf-8".into(),
            "Unathorized".into()
        )
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn multipart_quota_refusal_precedes_invalid_metadata() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (mut state, dir) = state(pool).await;
    Arc::get_mut(&mut state)
        .unwrap()
        .settings
        .max_images_per_user = 0;
    let body = form(&[
        ("image", None, Some("image/png"), b"not an image"),
        ("metadata", None, None, b"not JSON"),
    ]);
    let response = request(&state, "POST", "/api/images", body, Some(FORM_TYPE), true).await;
    assert_eq!(response.0, StatusCode::FORBIDDEN);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response.2).unwrap(),
        serde_json::json!({
            "reason":"maxLimitReached", "message":"you have reached the limit of 0 max images"
        })
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn multipart_requires_named_form_data_and_refuses_nested_parts() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap();
    let (state, dir) = state(pool).await;
    for disposition in [
        "",
        "Content-Disposition: attachment; name=\"image\"\r\n",
        "Content-Disposition: form-data\r\n",
    ] {
        let body = format!("--camera-parity\r\n{disposition}Content-Type: image/png\r\n\r\nbytes\r\n--camera-parity--\r\n").into_bytes();
        let response = request(&state, "POST", "/api/images", body, Some(FORM_TYPE), true).await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST);
        assert_eq!(response.2, "No Content-Disposition `form-data` header");
    }
    let body = form(&[(
        "image",
        None,
        Some("multipart/mixed; boundary=inner"),
        b"data",
    )]);
    let response = request(&state, "POST", "/api/images", body, Some(FORM_TYPE), true).await;
    assert_eq!(response.2, "Nested multipart is not supported");
    tokio::fs::remove_dir_all(dir).await.unwrap();
}
