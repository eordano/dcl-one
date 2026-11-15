//! Registry DTOs over the same local entity lookup and profile processing as
//! Catalyst Lambdas. There is no Registry upstream or separate profile cache.
//!
//! Both answers keep the Registry's key order: `{timestamp, avatars}` with a
//! numeric `timestamp` for `/profiles` (`captures/abr/AB-2-one-known.json`),
//! `{pointer, hasClaimedName, name, nameColor, thumbnailUrl}` for
//! `/profiles/metadata` (`captures/abr/AB-1-one-known.json`).
//!
//! As the Registry, the number of ids is not capped (the body limit bounds
//! it, `captures/abr/AB-3-1001-ids.json`) and a failed lookup layer answers
//! `200` with what was found (`profile-retriever.ts:50-53`, `:69-72`): only ids
//! that can name a profile are looked up, 1000 per entity read, and a failed
//! read or ownership pass leaves out its chunk only, counted in
//! `profiles_lookup_failures_total`.

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::value::RawValue;
use serde_json::{Value, json};

use super::{
    MarketplaceState, PROFILE_LOOKUP_CHUNK, SharedState, active_entities, count_lookup_failure,
    is_profile_pointer, process_profiles_positional, profile_metadata,
};

/// One `/profiles` element in the Registry's key order.
#[derive(serde::Serialize)]
struct FullProfile<'a> {
    timestamp: Value,
    avatars: &'a Value,
}

const INVALID_IDS: &str = "The ids property must be an array of non-empty strings";
/// Bounds address lookups by body size; default pointers have a finite namespace.
const BODY_LIMIT: usize = 256 * 1024;
/// Entities per ownership pass; a failed pass leaves out this many at most.
const OWNERSHIP_CHUNK: usize = 128;

/// Mount under `/asset-bundle-registry`, using the existing market state,
/// with the Registry's CORS like the gateway half of that prefix.
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/profiles", post(profiles))
        .route("/profiles/metadata", post(metadata))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(axum::middleware::from_fn(
            crate::explore::api::registry::cors,
        ))
        .with_state(state)
}

fn parse_ids(body: &[u8]) -> Result<Vec<String>, Box<Response>> {
    // The pinned Registry captures define these error envelopes. In contrast,
    // Lambdas rejects an empty ids array; Registry accepts it as an empty read.
    let body: Value = serde_json::from_slice(body).map_err(|_| Box::new(internal_error()))?;
    let ids = body
        .get("ids")
        .and_then(Value::as_array)
        .ok_or_else(invalid_ids)?;
    let mut seen = std::collections::HashSet::new();
    let mut pointers = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(invalid_ids)?;
        let id = id.to_lowercase();
        if seen.insert(id.clone()) {
            pointers.push(id);
        }
    }
    Ok(pointers)
}

/// The Registry's unhandled-error envelope (the captured malformed-JSON answer).
fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"Internal Server Error"})),
    )
        .into_response()
}

fn invalid_ids() -> Box<Response> {
    Box::new(
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Bad request", "message": INVALID_IDS})),
        )
            .into_response(),
    )
}

async fn profiles(
    State(state): State<SharedState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    respond(state, body, false).await
}

async fn metadata(
    State(state): State<SharedState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    respond(state, body, true).await
}

/// A body the extractor refused (over `BODY_LIMIT`, or a broken stream) in
/// the same JSON envelope as the other errors, not axum's plain text.
fn body_rejection(rejection: BytesRejection) -> Response {
    let status = rejection.status();
    let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
        format!("The request body exceeds {BODY_LIMIT} bytes")
    } else {
        rejection.body_text()
    };
    let error = status.canonical_reason().unwrap_or("Bad request");
    (status, Json(json!({"error": error, "message": message}))).into_response()
}

async fn respond(
    state: SharedState,
    body: Result<Bytes, BytesRejection>,
    compact: bool,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    let pointers = match parse_ids(&body) {
        Ok(pointers) => pointers,
        Err(response) => return *response,
    };
    // Only an id that can name a profile is looked up, a chunk at a time, so
    // the body limit bounds the lookups and one upstream read stays small.
    let pointers: Vec<String> = pointers
        .into_iter()
        .filter(|pointer| is_profile_pointer(pointer))
        .collect();
    let mut entities = Vec::new();
    for chunk in pointers.chunks(PROFILE_LOOKUP_CHUNK) {
        match active_entities(&state, chunk).await {
            Ok(found) => entities.extend(found),
            Err(()) => {
                tracing::warn!(
                    left_out = chunk.len(),
                    "Registry profile lookup failed; answering what was found"
                );
                count_lookup_failure("registry_profiles", "entities");
            }
        }
    }
    // Content pointers may resolve scenes or incomplete profile documents.
    // Registry's profile store only admits profiles with usable avatars.
    let mut entities: Vec<_> = entities
        .into_iter()
        .filter(|entity| {
            entity.get("type").and_then(Value::as_str) == Some("profile")
                && entity
                    .get("metadata")
                    .and_then(|metadata| metadata.get("avatars"))
                    .and_then(Value::as_array)
                    .and_then(|avatars| avatars.first())
                    .is_some_and(Value::is_object)
        })
        .collect();
    sort_by_request(&mut entities, &pointers);
    Json(project_profiles(&state, &entities, compact).await).into_response()
}

/// Request order: an entity sits at its first requested (lowercased) pointer.
fn sort_by_request(entities: &mut [Value], pointers: &[String]) {
    let position: std::collections::HashMap<&str, usize> = pointers
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();
    entities.sort_by_cached_key(|entity| {
        entity
            .get("pointers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|p| position.get(p.to_lowercase().as_str()).copied())
            .min()
            .unwrap_or(usize::MAX)
    });
}

/// The processed profiles in `entities` order, each serialized in the
/// Registry's key order. A malformed entity (no id, pointer, avatar or
/// timestamp) is left out with a warning. The ownership pass runs per
/// [`OWNERSHIP_CHUNK`] entities; a chunk whose pass fails is left out with a
/// warning, never served unvalidated.
async fn project_profiles(
    state: &MarketplaceState,
    entities: &[Value],
    compact: bool,
) -> Vec<Box<RawValue>> {
    let mut out = Vec::with_capacity(entities.len());
    for chunk in entities.chunks(OWNERSHIP_CHUNK) {
        let processed = match process_profiles_positional(state, chunk).await {
            Ok(processed) => processed,
            Err(error) => {
                tracing::warn!(
                    %error,
                    left_out = chunk.len(),
                    "Registry ownership reader failed; answering what was found"
                );
                count_lookup_failure("registry_profiles", "ownership");
                continue;
            }
        };
        out.extend(
            chunk
                .iter()
                .zip(processed)
                .filter_map(|(entity, profile)| profile.map(|profile| (entity, profile)))
                .filter_map(|(entity, profile)| {
                    let projected = project(state, entity, profile, compact);
                    if projected.is_none() {
                        let id = entity.get("id").and_then(Value::as_str).unwrap_or("");
                        tracing::warn!(
                            entity = id,
                            "Registry profile entity is malformed; left out"
                        );
                    }
                    projected
                }),
        );
    }
    out
}

fn project(
    state: &MarketplaceState,
    entity: &Value,
    mut profile: Value,
    compact: bool,
) -> Option<Box<RawValue>> {
    let id = entity.get("id").and_then(Value::as_str)?;
    let images = state
        .settings
        .profile_images_public_url
        .trim_end_matches('/');
    // The Registry's sanitizer replaces every avatar's `snapshots` with
    // exactly `face256` and `body` on this node's profile-images routes,
    // dropping any other deployed key (Lambdas keeps those).
    let face = format!("{images}/entities/{id}/face.png");
    let snapshots = json!({
        "face256": face,
        "body": format!("{images}/entities/{id}/body.png"),
    });
    for avatar in profile
        .get_mut("avatars")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(avatar) = avatar.get_mut("avatar").and_then(Value::as_object_mut) {
            avatar.insert("snapshots".into(), snapshots.clone());
        }
    }
    let projected = if compact {
        let pointer = entity
            .get("pointers")
            .and_then(Value::as_array)
            .and_then(|pointers| pointers.first())
            .and_then(Value::as_str)?;
        serde_json::value::to_raw_value(&profile_metadata(pointer, &profile, Some(&face))?)
    } else {
        // A number, as the Registry answers from its cache and its Catalyst
        // fallback (`captures/abr/AB-2-one-known.json`).
        let timestamp = match entity.get("timestamp") {
            Some(Value::Number(value)) => Value::Number(value.clone()),
            Some(Value::String(value)) => Value::from(value.parse::<u64>().ok()?),
            _ => return None,
        };
        serde_json::value::to_raw_value(&FullProfile {
            timestamp,
            avatars: &profile["avatars"],
        })
    };
    projected.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::market::tests::support::{state_with_empty_readers, state_without_db};

    fn values(raw: Vec<Box<RawValue>>) -> Vec<Value> {
        raw.iter()
            .map(|raw| serde_json::from_str(raw.get()).unwrap())
            .collect()
    }

    async fn call(path: &str, body: &str) -> (StatusCode, Value) {
        let response = router(state_without_db())
            .oneshot(
                Request::post(path)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["content-type"], "application/json");
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn registry_empty_ids_does_not_require_a_database_or_upstream() {
        for path in ["/profiles", "/profiles/metadata"] {
            assert_eq!(
                call(path, r#"{"ids":[]}"#).await,
                (StatusCode::OK, json!([]))
            );
        }
    }

    #[tokio::test]
    async fn registry_oversized_body_is_a_json_413() {
        let body = json!({"ids": ["a".repeat(BODY_LIMIT)]}).to_string();
        for path in ["/profiles", "/profiles/metadata"] {
            assert_eq!(
                call(path, &body).await,
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({
                        "error": "Payload Too Large",
                        "message": "The request body exceeds 262144 bytes",
                    })
                )
            );
        }
    }

    /// The Registry answers `200` with what it found when a lookup layer
    /// fails (`profile-retriever.ts:50-53`); with no database nothing is found.
    #[tokio::test]
    async fn registry_failed_lookup_answers_what_was_found() {
        let recorder = crate::comms::metrics::prometheus_builder()
            .unwrap()
            .build_recorder();
        let _recording = metrics::set_default_local_recorder(&recorder);
        for path in ["/profiles", "/profiles/metadata"] {
            assert_eq!(
                call(
                    path,
                    r#"{"ids":["0x00000000000000000000000000000000000000aa"]}"#
                )
                .await,
                (StatusCode::OK, json!([]))
            );
        }
        let response = super::super::profiles_metadata(
            State(state_without_db()),
            Bytes::from_static(br#"{"ids":["0x00000000000000000000000000000000000000aa"]}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let rendered = recorder.handle().render();
        for (route, count) in [("registry_profiles", 2), ("lambdas_profiles_metadata", 1)] {
            assert!(
                rendered
                    .lines()
                    .any(|line| line.starts_with("profiles_lookup_failures_total{")
                        && line.contains(&format!("route=\"{route}\""))
                        && line.contains("layer=\"entities\"")
                        && line.ends_with(&format!(" {count}"))),
                "{rendered}"
            );
        }
    }

    /// No cap on the ids, as the Registry (`captures/abr/AB-3-1001-ids.json`:
    /// 1001 unknown ids answer `200 []`); only the body limit bounds them.
    #[tokio::test]
    async fn registry_ids_are_not_capped() {
        let ids: Vec<String> = (0..1001).map(|i| format!("0x{i:040x}")).collect();
        for path in ["/profiles", "/profiles/metadata"] {
            assert_eq!(
                call(path, &json!({"ids": ids}).to_string()).await,
                (StatusCode::OK, json!([]))
            );
        }
    }

    #[tokio::test]
    async fn registry_short_ids_are_empty_reads_but_invalid_tail_is_rejected() {
        let ids: Vec<Value> = (0..30_000).map(|i| json!(format!("a{i}"))).collect();
        let body = json!({"ids": ids}).to_string();
        assert!(body.len() < BODY_LIMIT);
        for path in ["/profiles", "/profiles/metadata"] {
            assert_eq!(call(path, &body).await, (StatusCode::OK, json!([])));
            for invalid in [json!(""), json!(1), Value::Null] {
                let mut invalid_ids = ids.clone();
                invalid_ids.push(invalid);
                assert_eq!(
                    call(path, &json!({"ids": invalid_ids}).to_string()).await.0,
                    StatusCode::BAD_REQUEST
                );
            }
        }
        let pointers = parse_ids(br#"{"ids":["DEFAULT1","default1","0xABC","0xabc"]}"#).unwrap();
        assert_eq!(pointers, ["default1", "0xabc"]);
    }

    #[test]
    fn registry_profiles_follow_request_order() {
        let mut entities = vec![
            json!({"id":"a", "pointers":["0xaa"]}),
            json!({"id":"b", "pointers":["0xBB"]}),
            json!({"id":"c", "pointers":["0xcc"]}),
        ];
        sort_by_request(
            &mut entities,
            &["0xcc".into(), "0xbb".into(), "0xaa".into()],
        );
        let ids: Vec<_> = entities.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["c", "b", "a"]);
    }

    #[tokio::test]
    async fn registry_malformed_entity_is_left_out_not_the_batch() {
        let state = state_with_empty_readers(None).await;
        let mut broken = entity();
        broken.as_object_mut().unwrap().remove("timestamp");
        let full = values(project_profiles(&state, &[broken, entity()], false).await);
        assert_eq!(full.len(), 1);
        assert_eq!(full[0]["timestamp"], 1700000000000_u64);
    }

    /// The Registry's CORS (`captures/abr/AB-12-preflight.json`): the
    /// captured preflight headers exactly, and origin `*` on a read only when
    /// the request names an origin.
    #[tokio::test]
    async fn registry_profile_preflight_and_empty_reads_allow_browser_clients() {
        let capture: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/content-lambdas/captures/abr/AB-12-preflight.json"
        )))
        .unwrap();
        let app = router(state_without_db());
        for path in ["/profiles", "/profiles/metadata"] {
            let mut request = Request::builder().method("OPTIONS").uri(path);
            for (name, value) in capture["request"]["headers"].as_object().unwrap() {
                request = request.header(name.as_str(), value.as_str().unwrap());
            }
            let preflight = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                u64::from(preflight.status().as_u16()),
                capture["response"]["status"].as_u64().unwrap()
            );
            let headers: serde_json::Map<String, Value> = preflight
                .headers()
                .iter()
                // The capture keeps the CORS headers; axum also adds framing
                // (`content-length`) and `Allow` for a route without an
                // OPTIONS handler.
                .filter(|(name, _)| name.as_str().starts_with("access-control-") || *name == "vary")
                .map(|(name, value)| (name.to_string(), json!(value.to_str().unwrap())))
                .collect();
            assert_eq!(
                Value::Object(headers),
                capture["response"]["headers"],
                "{path}"
            );
            for origin in [Some("https://explorer.example"), None] {
                let mut request = Request::post(path).header("content-type", "application/json");
                if let Some(origin) = origin {
                    request = request.header("origin", origin);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::from(r#"{"ids":[]}"#)).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response
                        .headers()
                        .get("access-control-allow-origin")
                        .map(|v| v.to_str().unwrap()),
                    origin.map(|_| "*")
                );
                assert!(
                    response
                        .headers()
                        .get("access-control-expose-headers")
                        .is_none()
                );
                assert_eq!(
                    to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                    b"[]"
                );
            }
        }
    }

    #[tokio::test]
    async fn registry_invalid_ids_preserve_captured_error_envelope() {
        let capture: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/content-lambdas/captures/abr/AB-3-missing-ids.json"
        ))).unwrap();
        let expected = capture["response"]["body"].clone();
        for path in ["/profiles", "/profiles/metadata"] {
            for body in [
                "{}",
                r#"{"ids":null}"#,
                r#"{"ids":"x"}"#,
                r#"{"ids":[""]}"#,
                r#"{"ids":[1]}"#,
            ] {
                assert_eq!(
                    call(path, body).await,
                    (StatusCode::BAD_REQUEST, expected.clone())
                );
            }
        }
    }

    #[tokio::test]
    async fn registry_invalid_json_matches_current_capture() {
        let capture: Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/content-lambdas/captures/abr/AB-3-not-json.json"
        ))).unwrap();
        for path in ["/profiles", "/profiles/metadata"] {
            for body in ["", "{"] {
                assert_eq!(call(path, body).await, (
                    StatusCode::from_u16(capture["response"]["status"].as_u64().unwrap() as u16).unwrap(),
                    capture["response"]["body"].clone(),
                ));
            }
        }
    }

    fn entity() -> Value {
        json!({
            "id":"bafkprofile", "type":"profile", "pointers":["0xabc"], "timestamp":1700000000000_i64,
            "metadata": {"unrelated":"omit", "timestamp":1, "avatars":[{
                "name":"Alice", "hasClaimedName":true, "nameColor":null,
                "avatar": {"wearables":[
                    "urn:decentraland:off-chain:base-avatars:eyes_00",
                    "urn:decentraland:matic:collections-v2:0xabc:1"
                ], "snapshots":{"face256":"https://external.example/face","body":"old"}}
            }]}
        })
    }

    #[tokio::test]
    async fn registry_failed_ownership_chunk_preserves_successful_neighbours() {
        let recorder = crate::comms::metrics::prometheus_builder()
            .unwrap()
            .build_recorder();
        let _recording = metrics::set_default_local_recorder(&recorder);
        let failed = format!("0x{:040x}", OWNERSHIP_CHUNK);
        let graph = Router::new().route(
            "/",
            post(move |Json(body): Json<Value>| {
                let failed = failed.clone();
                async move {
                    if body["variables"]["owner"] == failed {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error":"reader unavailable"})),
                        )
                    } else {
                        (StatusCode::OK, Json(json!({"data": {}})))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, graph).await.unwrap() });
        let state =
            crate::market::tests::support::state_without_db_settings(crate::market::Settings {
                marketplace_subgraph_url: Some(format!("http://{address}/")),
                ..crate::market::Settings::for_tests()
            });
        state
            .test_allow_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let entities: Vec<_> = (0..(2 * OWNERSHIP_CHUNK + 1))
            .map(|i| {
                let mut entity = entity();
                entity["id"] = json!(format!("bafkprofile{i}"));
                entity["pointers"] = json!([format!("0x{i:040x}")]);
                entity["timestamp"] = json!(i);
                entity
            })
            .collect();
        for compact in [false, true] {
            let projected = values(project_profiles(&state, &entities, compact).await);
            assert_eq!(projected.len(), OWNERSHIP_CHUNK + 1);
            let actual: Vec<_> = projected
                .iter()
                .map(|profile| {
                    if compact {
                        profile["pointer"].as_str().unwrap().to_string()
                    } else {
                        profile["avatars"][0]["userId"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    }
                })
                .collect();
            let expected: Vec<_> = (0..OWNERSHIP_CHUNK)
                .chain([2 * OWNERSHIP_CHUNK])
                .map(|i| format!("0x{i:040x}"))
                .collect();
            assert_eq!(actual, expected);
            for profile in &projected {
                let claimed = if compact {
                    &profile["hasClaimedName"]
                } else {
                    &profile["avatars"][0]["hasClaimedName"]
                };
                assert_eq!(claimed, &json!(false));
            }
        }
        let rendered = recorder.handle().render();
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with("profiles_lookup_failures_total{")
                    && line.contains("route=\"registry_profiles\"")
                    && line.contains("layer=\"ownership\"")
                    && line.ends_with(" 2")),
            "{rendered}"
        );
        assert!(state.profile_cache.is_empty());
        let pointers: Vec<_> = entities
            .iter()
            .map(|entity| entity["pointers"][0].as_str().unwrap().to_string())
            .collect();
        assert!(
            super::super::process_and_cache_profiles(&state, &pointers, &entities)
                .await
                .is_err()
        );
        assert!(
            state.profile_cache.is_empty(),
            "a failed later chunk must not cache any partial result"
        );
        server.abort();
    }

    #[tokio::test]
    async fn registry_projection_uses_shared_ownership_and_entity_snapshot_identity() {
        let state = state_with_empty_readers(None).await;
        let mut entity = entity();
        entity["metadata"]["avatars"][0]["avatar"]["snapshots"] = json!({
            "face256": "https://external.example/face", "body": "old",
            "face128": "bafkold", "face": "bafkold2",
        });
        let entities = [entity];
        let full_raw = project_profiles(&state, &entities, false).await;
        let compact_raw = project_profiles(&state, &entities, true).await;
        // The Registry's key order (AB-2 and AB-1 captures).
        assert!(
            full_raw[0]
                .get()
                .starts_with(r#"{"timestamp":1700000000000,"avatars":["#)
        );
        assert!(compact_raw[0].get().starts_with(r#"{"pointer":"0xabc","hasClaimedName":false,"name":"Alice","nameColor":null,"thumbnailUrl":"#));
        let (full, compact) = (values(full_raw), values(compact_raw));
        assert_eq!(full[0].as_object().unwrap().len(), 2);
        assert_eq!(full[0]["timestamp"], 1700000000000_u64);
        let avatar = &full[0]["avatars"][0];
        assert_eq!(avatar["hasClaimedName"], false);
        assert_eq!(
            avatar["avatar"]["wearables"],
            json!(["urn:decentraland:off-chain:base-avatars:eyes_00"])
        );
        // The Registry sanitizer's snapshots: exactly these two keys.
        assert_eq!(
            avatar["avatar"]["snapshots"],
            json!({
                "face256": "http://localhost/profile-images/entities/bafkprofile/face.png",
                "body": "http://localhost/profile-images/entities/bafkprofile/body.png",
            })
        );
        assert_eq!(
            compact,
            vec![json!({
                "pointer":"0xabc", "name":"Alice", "nameColor":null, "hasClaimedName":false,
                "thumbnailUrl":"http://localhost/profile-images/entities/bafkprofile/face.png"
            })]
        );
    }

    /// Like the Registry sanitizer, an avatar without an `avatar` object
    /// gets no snapshots.
    #[tokio::test]
    async fn registry_projection_leaves_an_avatar_without_avatar_object_unchanged() {
        let avatars = json!([{"name": "Bob", "userId": "0xabc"}]);
        let projected = project(
            &state_without_db(),
            &entity(),
            json!({"avatars": avatars.clone()}),
            false,
        )
        .unwrap();
        let projected: Value = serde_json::from_str(projected.get()).unwrap();
        assert_eq!(projected["avatars"], avatars);
    }

    #[tokio::test]
    async fn registry_projection_preserves_omitted_name_color() {
        let state = state_without_db();
        let mut entity = entity();
        entity["metadata"]["avatars"][0]
            .as_object_mut()
            .unwrap()
            .remove("nameColor");
        let compact = values(project_profiles(&state, &[entity], true).await);
        assert!(compact[0].get("nameColor").is_none());
    }

    #[tokio::test]
    async fn registry_routes_read_local_profiles_and_observe_changes_without_a_registry_cache() {
        let Some(state) =
            crate::market::tests::support::state_with_db(crate::market::Settings::for_tests())
                .await
        else {
            return;
        };
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let pointer = format!("0x{tag}00000000");
        let entity_id = format!("bafyprofile{tag}");
        let mut metadata = entity()["metadata"].clone();
        sqlx::query(
            "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, \
                 entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
             VALUES ($1, 'v3', 'profile', $2, $3, to_timestamp(1700000000), ARRAY[$1], timezone('UTC', now()), '[]'::json)",
        )
        .bind(&pointer)
        .bind(&entity_id)
        .bind(json!({"v": metadata}))
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'profile')")
            .bind(&pointer)
            .bind(&entity_id)
            .execute(&state.pool)
            .await
            .unwrap();
        let scene_pointer = format!("{},0", u64::from_str_radix(&tag[..12], 16).unwrap());
        let malformed_pointer = format!("0xmissingavatars{tag}");
        let other_entities = [
            (
                format!("bafyscene{tag}"),
                scene_pointer.clone(),
                "scene",
                json!({"scene":{"base":scene_pointer}}),
            ),
            (
                format!("bafyempty{tag}"),
                malformed_pointer.clone(),
                "profile",
                json!({"avatars":[]}),
            ),
        ];
        for (id, pointer, kind, metadata) in &other_entities {
            sqlx::query(
                "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, \
                 entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
                 VALUES ($1, 'v3', $2, $3, $4, to_timestamp(1700000000), ARRAY[$1], timezone('UTC', now()), '[]'::json)",
            ).bind(pointer).bind(kind).bind(id).bind(json!({"v":metadata}))
                .execute(&state.pool).await.unwrap();
            sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, $3)")
                .bind(pointer).bind(id).bind(kind).execute(&state.pool).await.unwrap();
        }
        let app = router(state.clone());
        for (path, name) in [("/profiles", "Alice"), ("/profiles/metadata", "Updated")] {
            if name == "Updated" {
                metadata["avatars"][0]["name"] = json!(name);
                sqlx::query(
                    "UPDATE content.deployments SET entity_metadata = $1 WHERE entity_id = $2",
                )
                .bind(json!({"v": metadata}))
                .bind(&entity_id)
                .execute(&state.pool)
                .await
                .unwrap();
            }
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .body(Body::from(
                            json!({"ids":[scene_pointer, pointer.to_uppercase(), malformed_pointer, pointer, "0xunknown"]})
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body.as_array().unwrap().len(), 1);
            if path == "/profiles" {
                assert_eq!(body[0]["timestamp"], 1700000000000_u64);
                assert_eq!(body[0]["avatars"][0]["name"], name);
                assert_eq!(body[0]["avatars"][0]["userId"], pointer);
            } else {
                assert_eq!(body[0]["name"], name);
                assert_eq!(body[0]["pointer"], pointer);
                assert_eq!(
                    body[0]["thumbnailUrl"],
                    format!("http://localhost/profile-images/entities/{entity_id}/face.png")
                );
            }
        }
        let bounded_app = crate::market::router(state.clone())
            .nest("/asset-bundle-registry", router(state.clone()));
        for path in [
            "/asset-bundle-registry/profiles",
            "/asset-bundle-registry/profiles/metadata",
            "/lambdas/profiles/metadata",
        ] {
            for before_known in [999, 1000] {
                let mut ids: Vec<String> = (0..before_known)
                    .flat_map(|i| {
                        [
                            format!("0x{tag}{:08x}", i + 1),
                            format!("0X{tag}{:08X}", i + 1),
                        ]
                    })
                    .collect();
                ids.push(pointer.clone());
                let response = bounded_app
                    .clone()
                    .oneshot(
                        Request::post(path)
                            .header("content-type", "application/json")
                            .body(Body::from(json!({"ids": ids}).to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body: Vec<Value> = serde_json::from_slice(
                    &to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
                )
                .unwrap();
                assert_eq!(
                    body.len(),
                    1,
                    "{path}: valid pointer beyond first chunk is retained"
                );
                let actual = if path.ends_with("/metadata") {
                    &body[0]["pointer"]
                } else {
                    &body[0]["avatars"][0]["userId"]
                };
                assert_eq!(actual, &json!(pointer));
            }
        }
        let unavailable = std::sync::Arc::new(MarketplaceState::new(
            state.pool.clone(),
            crate::http::Outbound::for_tests(),
            crate::market::Settings {
                marketplace_subgraph_url: Some("http://127.0.0.1:1/".into()),
                ..crate::market::Settings::for_tests()
            },
        ));
        unavailable
            .test_allow_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let app = router(unavailable);
        for path in ["/profiles", "/profiles/metadata"] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .body(Body::from(json!({"ids":[pointer]}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            // The Registry's 200 with what was found: the chunk whose
            // ownership pass failed is left out, never served unvalidated.
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                b"[]",
                "unvalidated metadata must never be returned when an ownership reader fails"
            );
        }
        for (id, pointer, _, _) in &other_entities {
            sqlx::query("DELETE FROM content.active_pointers WHERE pointer = $1")
                .bind(pointer)
                .execute(&state.pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM content.deployments WHERE entity_id = $1")
                .bind(id)
                .execute(&state.pool)
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM content.active_pointers WHERE pointer = $1")
            .bind(&pointer)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM content.deployments WHERE entity_id = $1")
            .bind(&entity_id)
            .execute(&state.pool)
            .await
            .unwrap();
    }
}
