//! Camera Reel API documentation and locally served Swagger UI assets.

use axum::Extension;
use axum::extract::NestedPath;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub async fn openapi_json() -> Response {
    let spec: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/services/catalyst/camera-reel/src/openapi.json"
    )))
    .expect("pinned Camera Reel OpenAPI document");
    axum::Json(spec).into_response()
}

pub async fn swagger_ui(uri: axum::http::Uri, nested: Option<Extension<NestedPath>>) -> Response {
    let path = uri.path();
    let Some((local_prefix, tail)) = path.split_once("/docs/ui/") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let prefix = format!(
        "{}{local_prefix}",
        nested
            .as_ref()
            .map(|Extension(path)| path.as_str())
            .unwrap_or("")
    );
    let Some(tail) = crate::http::decoded_path_component(tail) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = if tail.is_empty() || tail == "/" {
        "index.html"
    } else {
        tail.as_str()
    };
    if name == "swagger-initializer.js" {
        let config = serde_json::json!({
            "dom_id": "#swagger-ui",
            "url": format!("{prefix}/docs/openapi.json"),
            "deepLinking": true,
            "layout": "StandaloneLayout"
        });
        let config = serde_json::to_string_pretty(&config).expect("Swagger configuration");
        let source = include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/camera-reel/swagger-ui/swagger-initializer.js"
        ));
        let start = source.find("    url:").expect("Swagger URL");
        let end = source.find("    presets:").expect("Swagger presets");
        let source = format!(
            "{}{},\n{}",
            &source[..start],
            &config[2..config.len() - 2],
            &source[end..]
        );
        return (
            [(header::CONTENT_TYPE, "application/javascript")],
            source.replace("    layout: \"StandaloneLayout\"", ""),
        )
            .into_response();
    }
    let (content_type, bytes): (&str, &[u8]) = match name {
        "favicon-16x16.png" => (
            "image/png",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/favicon-16x16.png"
            ))
            .as_slice(),
        ),
        "favicon-32x32.png" => (
            "image/png",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/favicon-32x32.png"
            ))
            .as_slice(),
        ),
        "index.css" => (
            "text/css",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/index.css"
            ))
            .as_slice(),
        ),
        "index.html" => (
            "text/html",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/index.html"
            ))
            .as_slice(),
        ),
        "oauth2-redirect.html" => (
            "text/html",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/oauth2-redirect.html"
            ))
            .as_slice(),
        ),
        "swagger-ui-bundle.js" => (
            "application/javascript",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/swagger-ui-bundle.js"
            ))
            .as_slice(),
        ),
        "swagger-ui-standalone-preset.js" => (
            "application/javascript",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/swagger-ui-standalone-preset.js"
            ))
            .as_slice(),
        ),
        "swagger-ui.css" => (
            "text/css",
            include_bytes!(concat!(
                env!("DCL_ONE_ROOT"),
                "/services/catalyst/camera-reel/swagger-ui/swagger-ui.css"
            ))
            .as_slice(),
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    fn openapi_spec() -> Value {
        serde_json::from_str(include_str!("openapi.json")).unwrap()
    }

    fn ref_to(name: &str) -> Value {
        json!({ "$ref": format!("#/components/schemas/{name}") })
    }

    /// The production document (`docs-openapi`, 2026-09-25) with what
    /// upstream 3e8bd0b added after it (#61: `User.screenRect` and the
    /// `ScreenRect` component). The referenced but undefined
    /// `GalleryImageWithPlace` component stays absent, as in the source.
    #[test]
    fn the_spec_is_the_captured_one_plus_the_later_upstream_screen_rect() {
        let path = format!(
            "{}/services/catalyst/tests/fixtures/conformance/social/camera-reel/docs-openapi.json",
            env!("DCL_ONE_ROOT")
        );
        let captured: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let spec = openapi_spec();
        let ours = &spec["components"]["schemas"];
        let mut expected = captured["response"]["body_json"].clone();
        let schemas = &mut expected["components"]["schemas"];
        assert!(schemas.get("GalleryImageWithPlace").is_none());
        assert!(ours.get("GalleryImageWithPlace").is_none());
        schemas["ScreenRect"] = ours["ScreenRect"].clone();
        schemas["User"]["properties"]["screenRect"] =
            json!({ "allOf": [ref_to("ScreenRect")], "nullable": true });
        assert_eq!(spec, expected);
    }

    #[test]
    fn the_spec_documents_the_wire_and_every_path_parameter() {
        let spec = openapi_spec();
        let schemas = &spec["components"]["schemas"];
        assert_eq!(
            schemas["UpdateVisibility"]["required"],
            json!(["is_public"])
        );
        assert_eq!(
            schemas["Upload"]["required"],
            json!(["image", "metadata", "is_public"])
        );
        assert_eq!(
            schemas["User"]["properties"]["screenRect"],
            json!({ "allOf": [ref_to("ScreenRect")], "nullable": true })
        );
        assert_eq!(
            schemas["ScreenRect"]["required"],
            json!(["x", "y", "width", "height"])
        );
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                let declared: Vec<&str> = op["parameters"]
                    .as_array()
                    .map(|ps| {
                        ps.iter()
                            .filter(|p| p["in"] == "path")
                            .map(|p| p["name"].as_str().unwrap())
                            .collect()
                    })
                    .unwrap_or_default();
                for segment in path.split('/') {
                    if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
                    {
                        assert!(declared.contains(&name), "{method} {path} lacks {name}");
                    }
                }
            }
        }
    }
}
