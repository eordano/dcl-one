use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tower::ServiceExt;

use crate::http::{JsonBody, Query};
use crate::{Settings, State, router};

async fn app() -> (Router, std::path::PathBuf) {
    app_with_token(None).await
}

async fn app_with_token(token: Option<&str>) -> (Router, std::path::PathBuf) {
    app_at_mount(token, "/camera-reel-service").await
}

async fn app_at_mount(token: Option<&str>, prefix: &str) -> (Router, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("camera-http-{}", uuid::Uuid::new_v4()));
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/camera_http_unused")
        .unwrap();
    let storage = Arc::new(crate::storage::Storage::from_config(&dir).await.unwrap());
    let mut settings = Settings::new("https://camera.example");
    settings.writes_enabled = true;
    settings.metrics_bearer_token = token.map(str::to_owned);
    let state = Arc::new(State::new(pool, storage, settings, None));
    let app = router(state);
    (
        if prefix.is_empty() {
            app
        } else {
            Router::new().nest(prefix, app)
        },
        dir,
    )
}

#[tokio::test]
async fn health_and_api_preflights_keep_their_individual_policy() {
    let (app, dir) = app().await;
    for (path, methods, age) in [
        ("/api/images", "DELETE, GET, POST, PATCH", "300"),
        ("/api/unknown", "DELETE, GET, POST, PATCH", "300"),
        ("/health/live", "GET, OPTIONS", "3600"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri(format!("/camera-reel-service{path}"))
                    .header(header::ORIGIN, "https://play.example")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .header(
                        header::ACCESS_CONTROL_REQUEST_HEADERS,
                        "x-identity-auth-chain-7,content-type",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://play.example"
        );
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
            methods
        );
        assert_eq!(response.headers()[header::ACCESS_CONTROL_MAX_AGE], age);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
            "x-identity-auth-chain-7,content-type"
        );
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );
        assert!(
            to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .is_empty()
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn cors_validates_preflight_method_and_preserves_plain_options() {
    let (app, dir) = app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/camera-reel-service/api/images")
                .header(header::ORIGIN, "https://play.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PUT")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
    assert_eq!(
        to_bytes(response.into_body(), 4096).await.unwrap(),
        "Requested method is not allowed"
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/camera-reel-service/api/images")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn health_and_missing_paths_preserve_status_and_body() {
    let (app, dir) = app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/camera-reel-service/health/live")
                .header(header::ORIGIN, "https://play.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://play.example"
    );
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_EXPOSE_HEADERS)
    );
    assert_eq!(
        to_bytes(response.into_body(), 4096).await.unwrap(),
        "\"alive\""
    );
    for (method, path) in [
        ("GET", "/api/unknown"),
        ("PUT", "/api/images"),
        ("HEAD", "/health/live"),
        ("HEAD", "/api/users/example"),
        ("HEAD", "/api/users/example/images"),
        ("HEAD", "/api/images/example"),
        ("HEAD", "/api/images/example/metadata"),
        ("HEAD", "/api/places/example/images"),
        ("HEAD", "/api/wearables/example/images"),
        ("HEAD", "/api/docs/openapi.json"),
        ("HEAD", "/api/docs/ui/"),
        ("HEAD", "/api/docs/ui/swagger-ui.css"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/camera-reel-service{path}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert!(
            to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .is_empty()
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[derive(Deserialize)]
struct Page {
    limit: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Places {
    places_ids: Vec<String>,
}

fn extraction_app() -> Router {
    Router::new()
        .route(
            "/query",
            get(|Query(page): Query<Page>| async move { Json(page.limit) }),
        )
        .route(
            "/json",
            post(|JsonBody(places): JsonBody<Places>| async move { Json(places.places_ids) }),
        )
}

#[tokio::test]
async fn query_rejections_match_the_form_parser_messages() {
    for (query, expected) in [
        (
            "limit=nope",
            "Query deserialize error: invalid digit found in string",
        ),
        (
            "limit=-1",
            "Query deserialize error: invalid digit found in string",
        ),
        (
            "limit=",
            "Query deserialize error: cannot parse integer from empty string",
        ),
        (
            "limit=1&limit=2",
            "Query deserialize error: unsupported value",
        ),
    ] {
        let response = extraction_app()
            .oneshot(
                Request::builder()
                    .uri(format!("/query?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap(),
            expected,
            "{query}"
        );
    }
}

#[tokio::test]
async fn json_rejections_keep_plain_text_and_the_original_size_limit() {
    for (content_type, body, expected) in [
        ("text/plain", "{}", "Content type error"),
        (
            "application/json",
            "{}",
            "Json deserialize error: missing field `placesIds` at line 1 column 2",
        ),
    ] {
        let response = extraction_app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/json")
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap(),
            expected
        );
    }
    let response = extraction_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/json")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, "2097153")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        to_bytes(response.into_body(), 4096).await.unwrap(),
        "JSON payload (2097153 bytes) is larger than allowed (limit: 2097152 bytes)."
    );
}

#[tokio::test]
async fn openapi_describes_visible_person_screen_rect() {
    let response = crate::docs::openapi_json().await;
    let spec: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(
        spec["components"]["schemas"]["ScreenRect"]["required"],
        json!(["x", "y", "width", "height"])
    );
    assert_eq!(
        spec["components"]["schemas"]["User"]["properties"]["screenRect"]["allOf"][0]["$ref"],
        "#/components/schemas/ScreenRect"
    );
    assert_eq!(spec["paths"].as_object().unwrap().len(), 9);
    assert!(
        spec["components"]["schemas"]["User"]["properties"]["screenRect"]
            .get("description")
            .is_none()
    );
}

#[tokio::test]
async fn metrics_enforces_bearer_before_recording_and_preserves_service_series() {
    let (unconfigured, dir) = app().await;
    let response = unconfigured
        .oneshot(
            Request::builder()
                .uri("/camera-reel-service/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        to_bytes(response.into_body(), 4096)
            .await
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(dir).unwrap();

    let (app, dir) = app_with_token(Some("camera-token")).await;
    for (token, message) in [
        (None, "Token not present"),
        (Some("Basic wrong"), "Wrong schema"),
        (Some("Bearer wrong"), "Invalid token"),
    ] {
        let mut request = Request::builder().uri("/camera-reel-service/metrics");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, token);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), message);
    }
    let scrape = || {
        Request::builder()
            .uri("/camera-reel-service/metrics")
            .header(header::AUTHORIZATION, "Bearer camera-token")
            .body(Body::empty())
            .unwrap()
    };
    let response = app.clone().oneshot(scrape()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert!(
        to_bytes(response.into_body(), 4096)
            .await
            .unwrap()
            .is_empty(),
        "denied scrapes are not observations"
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/camera-reel-service/health/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/camera-reel-service/api/missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app.clone().oneshot(scrape()).await.unwrap();
    let body = String::from_utf8(
        to_bytes(response.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        body.contains(
            "http_requests_total{code=\"200\",handler=\"/health/live\",method=\"GET\"} 1"
        ),
        "{body}"
    );
    assert!(
        body.contains("http_requests_total{code=\"404\",handler=\"*\",method=\"GET\"} 1"),
        "{body}"
    );
    assert!(
        body.contains("http_requests_total{code=\"200\",handler=\"*\",method=\"GET\"} 1"),
        "previous successful scrape counted"
    );
    assert!(!body.contains("401"));
    for bucket in [
        "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10", "20", "30",
        "60", "+Inf",
    ] {
        assert!(
            body.contains(&format!("le=\"{bucket}\"")),
            "missing {bucket}"
        );
    }
    for method in ["POST", "OPTIONS", "HEAD"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/camera-reel-service/metrics")
                    .header(header::AUTHORIZATION, "Bearer camera-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn swagger_serves_local_assets_and_uses_the_mounted_document_url() {
    let (app, dir) = app().await;
    for (file, mime, needle) in [
        ("", "text/html", "swagger-ui-bundle.js"),
        ("swagger-ui.css", "text/css", ".swagger-ui"),
        (
            "swagger-ui-bundle.js",
            "application/javascript",
            "SwaggerUIBundle",
        ),
        (
            "swagger-initializer.js",
            "application/javascript",
            "/camera-reel-service/api/docs/openapi.json",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/camera-reel-service/api/docs/ui/{file}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{file}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
        let bytes = to_bytes(response.into_body(), 3_000_000).await.unwrap();
        assert!(
            std::str::from_utf8(&bytes).unwrap().contains(needle),
            "{file}"
        );
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/camera-reel-service/api/docs/ui/not-an-asset.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/camera-reel-service/api/docs/ui")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn openapi_keeps_all_declared_paths_and_existing_schema_contracts() {
    let baseline: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/services/catalyst/tests/fixtures/conformance/social/camera-reel/docs-openapi.raw.body"
    )))
    .unwrap();
    let response = crate::docs::openapi_json().await;
    let spec: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(spec["paths"], baseline["paths"]);
    for (name, schema) in baseline["components"]["schemas"].as_object().unwrap() {
        if name == "User" {
            let mut user = spec["components"]["schemas"][name].clone();
            user["properties"]
                .as_object_mut()
                .unwrap()
                .remove("screenRect");
            assert_eq!(&user, schema);
        } else {
            assert_eq!(&spec["components"]["schemas"][name], schema, "{name}");
        }
    }
}

#[tokio::test]
async fn json_content_types_use_mime_syntax_before_reading_the_body() {
    for (content_type, expected) in [
        ("application/json", StatusCode::OK),
        ("Application/JSON", StatusCode::OK),
        ("text/json", StatusCode::OK),
        ("application/problem+json", StatusCode::OK),
        ("application/json; charset=iso-8859-1", StatusCode::OK),
        ("application/json; charset=\"utf-8\"", StatusCode::OK),
        ("/json", StatusCode::BAD_REQUEST),
        (" /json", StatusCode::BAD_REQUEST),
        ("application/a b+json", StatusCode::BAD_REQUEST),
        ("application/json; nonsense", StatusCode::BAD_REQUEST),
        ("application/json; charset", StatusCode::BAD_REQUEST),
        ("application/json;charset=\"", StatusCode::BAD_REQUEST),
        ("application/json ;charset=utf-8", StatusCode::BAD_REQUEST),
    ] {
        let response = extraction_app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/json")
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(r#"{"placesIds":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{content_type}");
        if expected == StatusCode::BAD_REQUEST {
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
            assert_eq!(
                to_bytes(response.into_body(), 4096).await.unwrap(),
                "Content type error"
            );
        }
    }
}

#[tokio::test]
async fn documentation_fallbacks_and_other_methods_keep_api_cors() {
    let (app, dir) = app().await;
    for (method, path) in [
        ("GET", "/api/docs/missing"),
        ("GET", "/api/docs/ui"),
        ("HEAD", "/api/docs/openapi.json"),
        ("POST", "/api/docs/openapi.json"),
        ("HEAD", "/api/docs/ui/swagger-ui.css"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/camera-reel-service{path}"))
                    .header(header::ORIGIN, "https://play.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://play.example"
        );
        assert!(response.headers().contains_key(header::VARY));
    }
    for path in [
        "/api/docs/missing",
        "/api/docs/openapi.json",
        "/api/docs/ui/swagger-ui.css",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri(format!("/camera-reel-service{path}"))
                    .header(header::ORIGIN, "https://play.example")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "OPTIONS {path}");
        assert_eq!(response.headers()[header::ACCESS_CONTROL_MAX_AGE], "300");
    }
    for path in ["/api/docs/openapi.json", "/api/docs/ui/swagger-ui.css"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/camera-reel-service{path}"))
                    .header(header::ORIGIN, "https://play.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn encoded_static_routes_work_through_the_mounted_camera_router() {
    let (app, dir) = app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/camera-reel-service/%61pi/%75sers/alice/%69mages?limit=nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        to_bytes(response.into_body(), 4096).await.unwrap(),
        "Query deserialize error: invalid digit found in string"
    );
    for (path, mime, needle) in [
        (
            "/%61pi/docs/%6fpenapi.json",
            "application/json",
            "Camera Reel Service",
        ),
        (
            "/api/%64ocs/%75i/%73wagger-ui.css",
            "text/css",
            ".swagger-ui",
        ),
        (
            "/%61pi/docs/ui/swagger-initializer.js",
            "application/javascript",
            "/camera-reel-service/api/docs/openapi.json",
        ),
        ("/%68ealth/%6cive", "application/json", "alive"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/camera-reel-service{path}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
        let body = to_bytes(response.into_body(), 3_000_000).await.unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains(needle));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn static_route_decoding_preserves_raw_signed_paths_and_decodes_parameters_once() {
    use axum::extract::{MatchedPath, OriginalUri, Path};
    let routes = Router::new().route("/api/users/{address}/images", get(|
        Path(address): Path<String>, OriginalUri(original): OriginalUri, matched: MatchedPath,
    | async move { Json(json!({ "address": address, "original": original.to_string(), "matched": matched.as_str() })) }));
    let routes = Router::new().fallback_service(
        tower::ServiceBuilder::new()
            .map_request(crate::http::routing_request)
            .service(routes),
    );
    for prefix in ["", "/camera-reel-service"] {
        let app = if prefix.is_empty() {
            routes.clone()
        } else {
            Router::new().nest(prefix, routes.clone())
        };
        for (raw, address) in [
            ("a%252Fb", "a%2Fb"),
            ("a%2Fb", "a/b"),
            ("a+b", "a+b"),
            ("%E2%82%AC", "€"),
            ("%FF", "�"),
            ("%C0%AF", "��"),
            ("%E2%82", "�"),
            ("a%FF%2F%252F%25%2B", "a�/%2F%+"),
            ("%FF%3F%23", "�?#"),
        ] {
            let original = format!("{prefix}/%61pi/%75sers/{raw}/%69mages?limit=%31");
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&original)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{original}");
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            // Request metrics use the same service-local route label at every mount.
            assert_eq!(
                body,
                json!({ "address": address, "original": original, "matched": "/api/users/{address}/images" })
            );
        }
    }
}

#[tokio::test]
async fn swagger_document_url_keeps_api_nesting_for_canonical_and_encoded_routes() {
    for prefix in ["", "/camera-reel-service", "/custom/camera"] {
        let (app, dir) = app_at_mount(None, prefix).await;
        for path in [
            "/api/docs/ui/swagger-initializer.js",
            "/%61pi/%64ocs/%75i/%73wagger-initializer.js",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("{prefix}{path}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{prefix}{path}");
            let body =
                String::from_utf8(to_bytes(response.into_body(), 4096).await.unwrap().to_vec())
                    .unwrap();
            assert!(
                body.contains(&format!("\"url\": \"{prefix}/api/docs/openapi.json\"")),
                "{body}"
            );
            assert!(!body.contains("/api/api/"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn lossy_path_decoding_keeps_authorization_bound_to_the_raw_request() {
    for prefix in ["", "/camera-reel-service"] {
        let (app, dir) = app_at_mount(None, prefix).await;
        let original = format!("{prefix}/%61pi/images/%FF");
        let mut headers = crate::shared::signed_fetch::test_support::signed_headers(
            "DELETE", &original, "{}", true,
        )
        .await;
        let mut signer: serde_json::Value =
            serde_json::from_str(headers["x-identity-auth-chain-0"].to_str().unwrap()).unwrap();
        signer["signature"] = json!("");
        headers.insert(
            "x-identity-auth-chain-0",
            signer.to_string().parse().unwrap(),
        );
        for (path, expected) in [
            (original.clone(), StatusCode::NOT_FOUND),
            (
                format!("{prefix}/%61pi/images/%FE"),
                StatusCode::UNAUTHORIZED,
            ),
        ] {
            let mut request = Request::builder()
                .method("DELETE")
                .uri(&path)
                .body(Body::empty())
                .unwrap();
            *request.headers_mut() = headers.clone();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected, "{path}");
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            if expected == StatusCode::NOT_FOUND {
                assert_eq!(body, r#"{"message":"image not found"}"#);
            } else {
                assert_eq!(body, "Unathorized");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod main_extractor_contracts {
    use crate::http::{JsonBody as Json, Query};
    use axum::extract::Request;
    use axum::http::{StatusCode, header};
    const MAX_JSON_BODY_BYTES: usize = 2 * 1024 * 1024;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::routing::{get, post};
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        #[allow(dead_code)]
        limit: u64,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Ids {
        places_ids: Vec<String>,
    }

    async fn call(app: Router, req: Request) -> (StatusCode, String, String) {
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = to_bytes(res.into_body(), 1 << 16).await.unwrap();
        (status, ct, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_bad_query_answers_the_upstream_plain_text() {
        let app = Router::new().route("/x", get(|_: Query<Page>| async { "ok" }));
        let req = Request::builder()
            .uri("/x?limit=abc")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            call(app.clone(), req).await,
            (
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8".to_string(),
                "Query deserialize error: invalid digit found in string".to_string()
            )
        );
        let req = Request::builder()
            .uri("/x?limit=3&other=1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(app, req).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn json_rejections_are_actix_s_400_and_413() {
        let app = Router::new().route(
            "/b",
            post(|Json(b): Json<Ids>| async move { b.places_ids.len().to_string() }),
        );
        let send = |body: Vec<u8>, ct: Option<&str>| {
            let mut req = Request::builder().method("POST").uri("/b");
            if let Some(ct) = ct {
                req = req.header(header::CONTENT_TYPE, ct);
            }
            req.body(Body::from(body)).unwrap()
        };
        for (body, ct, status, text) in [
            (
                "{}",
                Some("application/json"),
                StatusCode::BAD_REQUEST,
                "Json deserialize error: missing field `placesIds` at line 1 column 2",
            ),
            (
                r#"{"placesIds":"x"}"#,
                Some("application/json"),
                StatusCode::BAD_REQUEST,
                "Json deserialize error: invalid type: string \"x\", expected a sequence at line 1 column 16",
            ),
            (
                r#"{"placesIds":["a"]}"#,
                None,
                StatusCode::BAD_REQUEST,
                "Content type error",
            ),
            (
                r#"{"placesIds":["a"]}"#,
                Some("text/plain"),
                StatusCode::BAD_REQUEST,
                "Content type error",
            ),
            (
                r#"{"placesIds":["a"]}"#,
                Some("application/json; charset=utf-8"),
                StatusCode::OK,
                "1",
            ),
            (
                r#"{"placesIds":["a","b"]}"#,
                Some("application/vnd.x+json"),
                StatusCode::OK,
                "2",
            ),
        ] {
            let (got, _, got_text) = call(app.clone(), send(body.as_bytes().to_vec(), ct)).await;
            assert_eq!((got, got_text.as_str()), (status, text), "{body} {ct:?}");
        }
        let big = vec![b' '; MAX_JSON_BODY_BYTES + 1];
        let (status, ct, _) = call(app, send(big, Some("application/json"))).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(ct, "text/plain; charset=utf-8");
    }
}

#[tokio::test]
async fn camera_requires_a_string_signature_field_on_the_signer_link() {
    let (app, dir) = app().await;
    let path = "/camera-reel-service/api/images/not-a-uuid";
    let headers =
        crate::shared::signed_fetch::test_support::signed_headers("DELETE", path, "{}", true).await;
    for (signature, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(json!(null)), StatusCode::UNAUTHORIZED),
        (Some(json!("")), StatusCode::NOT_FOUND),
        (Some(json!("unused signer field")), StatusCode::NOT_FOUND),
    ] {
        let mut request = Request::builder()
            .method("DELETE")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        *request.headers_mut() = headers.clone();
        let mut signer: serde_json::Value =
            serde_json::from_str(headers["x-identity-auth-chain-0"].to_str().unwrap()).unwrap();
        signer.as_object_mut().unwrap().remove("signature");
        if let Some(signature) = signature {
            signer["signature"] = signature;
        }
        request.headers_mut().insert(
            "x-identity-auth-chain-0",
            signer.to_string().parse().unwrap(),
        );
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        if expected == StatusCode::UNAUTHORIZED {
            assert_eq!(body, "Unathorized");
        } else {
            assert_eq!(body, r#"{"message":"image not found"}"#);
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn preflight_responses_do_not_leak_method_router_allow_headers() {
    for prefix in ["", "/camera-reel-service"] {
        let (app, dir) = app_at_mount(None, prefix).await;
        for method in ["GET", "POST", "PUT", "get"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri(format!("{prefix}/api/users/someone/images"))
                        .header(header::ACCESS_CONTROL_REQUEST_METHOD, method)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if matches!(method, "GET" | "POST") {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                }
            );
            assert!(!response.headers().contains_key(header::ALLOW));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
