use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::SharedState;
use crate::dto::{
    GalleryImage, GalleryImageWithPlace, GetMultiplePlacesImagesBody,
    GetMultiplePlacesImagesResponse, GetPlaceImagesResponse, PlaceDataResponse,
};
use crate::handlers::{PageQuery, next_cursor};
use crate::http::{ApiError, JsonBody, Query};

pub const MAX_PLACES_IDS: usize = 100;

pub fn validate_places_ids(places_ids: &[String]) -> Result<(), ApiError> {
    if places_ids.is_empty() {
        return Err(ApiError::BadRequest("no place IDs provided".to_string()));
    }
    if places_ids.len() > MAX_PLACES_IDS {
        return Err(ApiError::BadRequest(format!(
            "too many place IDs provided, maximum is {MAX_PLACES_IDS}"
        )));
    }
    Ok(())
}

/// Preserve the Places client's acquisition/decoding distinction without
/// exposing database diagnostics. The transport-specific detail is local: no
/// HTTP status is invented for a lookup that never made an HTTP request.
fn world_place_lookup_error(error: &sqlx::Error) -> ApiError {
    let diagnostic = match error {
        sqlx::Error::ColumnDecode { .. }
        | sqlx::Error::Decode(_)
        | sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnIndexOutOfBounds { .. } => {
            "failed to parse response: invalid place lookup result"
        }
        _ => "request failed: place lookup unavailable",
    };
    ApiError::BadGateway(format!("failed to resolve world name: {diagnostic}"))
}

async fn world_place_ids(state: &SharedState, name: &str) -> Result<Vec<String>, ApiError> {
    state.db.world_place_ids(name).await.map_err(|error| {
        tracing::error!(world_name = name, error = %error, "failed to resolve world name");
        world_place_lookup_error(&error)
    })
}

pub async fn get_place_images(
    State(state): State<SharedState>,
    Path(place_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    let limit = q.capped_limit();
    let page = q.page()?;
    if place_id.ends_with(".eth") {
        let place_ids = world_place_ids(&state, &place_id).await?;

        if place_ids.is_empty() {
            return Ok((
                StatusCode::OK,
                Json(GetPlaceImagesResponse {
                    images: vec![],
                    place_data: PlaceDataResponse { max_images: 0 },
                    next_cursor: None,
                }),
            )
                .into_response());
        }

        let found = state
            .db
            .get_multiple_places_images_page(&place_ids, &page, limit)
            .await
            .map_err(|_| ApiError::NotFound("place not found".to_string()))?;

        let next_cursor = next_cursor(found.next);
        let images = found.images.into_iter().map(GalleryImage::from).collect();
        return Ok((
            StatusCode::OK,
            Json(GetPlaceImagesResponse {
                images,
                place_data: PlaceDataResponse {
                    max_images: found.total,
                },
                next_cursor,
            }),
        )
            .into_response());
    }

    let found = state
        .db
        .get_place_images_page(&place_id, &page, limit)
        .await
        .map_err(|_| ApiError::NotFound("place not found".to_string()))?;

    let next_cursor = next_cursor(found.next);
    let images = found.images.into_iter().map(GalleryImage::from).collect();
    Ok((
        StatusCode::OK,
        Json(GetPlaceImagesResponse {
            images,
            place_data: PlaceDataResponse {
                max_images: found.total,
            },
            next_cursor,
        }),
    )
        .into_response())
}

/// A read behind a `POST` (the id list is the body); the write-route gate keeps
/// it open while creator writes are disabled.
pub async fn get_multiple_places_images(
    State(state): State<SharedState>,
    Query(q): Query<PageQuery>,
    JsonBody(body): JsonBody<GetMultiplePlacesImagesBody>,
) -> Result<Response, ApiError> {
    validate_places_ids(&body.places_ids)?;

    let limit = q.capped_limit();
    let page = q.page()?;
    let resolved_ids = resolve_places_ids(&state, body.places_ids).await?;

    if resolved_ids.is_empty() {
        return Ok((
            StatusCode::OK,
            Json(GetMultiplePlacesImagesResponse {
                images: vec![],
                place_data: PlaceDataResponse { max_images: 0 },
                next_cursor: None,
            }),
        )
            .into_response());
    }

    let found = state
        .db
        .get_multiple_places_images_page(&resolved_ids, &page, limit)
        .await
        .map_err(|_| ApiError::NotFound("places not found".to_string()))?;

    let next_cursor = next_cursor(found.next);
    let images = found
        .images
        .into_iter()
        .map(GalleryImageWithPlace::from)
        .collect();
    Ok((
        StatusCode::OK,
        Json(GetMultiplePlacesImagesResponse {
            images,
            place_data: PlaceDataResponse {
                max_images: found.total,
            },
            next_cursor,
        }),
    )
        .into_response())
}

/// World names resolve to their place ids, in input order (a local read now,
/// so the old bounded concurrency is gone).
async fn resolve_places_ids(
    state: &SharedState,
    places_ids: Vec<String>,
) -> Result<Vec<String>, ApiError> {
    let mut resolved = Vec::with_capacity(places_ids.len());
    for id in places_ids {
        if id.ends_with(".eth") {
            resolved.extend(world_place_ids(state, &id).await?);
        } else {
            resolved.push(id);
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::{MAX_PLACES_IDS, validate_places_ids, world_place_lookup_error};
    use crate::handlers::{MAX_LIMIT, capped_limit};
    use crate::http::ApiError;

    #[test]
    fn capped_limit_passes_values_at_or_below_max() {
        assert_eq!(capped_limit(0), 0);
        assert_eq!(capped_limit(20), 20);
        assert_eq!(capped_limit(MAX_LIMIT), MAX_LIMIT as i64);
    }

    #[test]
    fn capped_limit_clamps_values_above_max() {
        assert_eq!(capped_limit(MAX_LIMIT + 1), MAX_LIMIT as i64);
        assert_eq!(capped_limit(1_000), MAX_LIMIT as i64);
        assert_eq!(capped_limit(u64::MAX), MAX_LIMIT as i64);
    }

    #[test]
    fn validate_places_ids_rejects_empty() {
        let err = validate_places_ids(&[]).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn validate_places_ids_accepts_up_to_max() {
        let ids: Vec<String> = (0..MAX_PLACES_IDS).map(|i| i.to_string()).collect();
        assert_eq!(ids.len(), MAX_PLACES_IDS);
        assert!(validate_places_ids(&ids).is_ok());
    }

    #[test]
    fn validate_places_ids_rejects_over_max() {
        let ids: Vec<String> = (0..=MAX_PLACES_IDS).map(|i| i.to_string()).collect();
        assert_eq!(ids.len(), MAX_PLACES_IDS + 1);
        let err = validate_places_ids(&ids).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn lookup_errors_keep_source_classes_without_database_diagnostics() {
        use axum::body::to_bytes;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use serde_json::{Value, json};

        let private_detail = "private database connection or column detail";
        let cases = [
            (
                sqlx::Error::PoolClosed,
                "request failed: place lookup unavailable",
            ),
            (
                sqlx::Error::PoolTimedOut,
                "request failed: place lookup unavailable",
            ),
            (
                sqlx::Error::Io(std::io::Error::other(private_detail)),
                "request failed: place lookup unavailable",
            ),
            (
                sqlx::Error::Protocol(private_detail.to_owned()),
                "request failed: place lookup unavailable",
            ),
            (
                sqlx::Error::ColumnDecode {
                    index: private_detail.to_owned(),
                    source: std::io::Error::other(private_detail).into(),
                },
                "failed to parse response: invalid place lookup result",
            ),
            (
                sqlx::Error::Decode(std::io::Error::other(private_detail).into()),
                "failed to parse response: invalid place lookup result",
            ),
            (
                sqlx::Error::ColumnNotFound(private_detail.to_owned()),
                "failed to parse response: invalid place lookup result",
            ),
            (
                sqlx::Error::ColumnIndexOutOfBounds { index: 3, len: 1 },
                "failed to parse response: invalid place lookup result",
            ),
        ];
        for (error, diagnostic) in cases {
            let response = world_place_lookup_error(&error).into_response();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(response.headers()["content-type"], "application/json");
            let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"message": format!("failed to resolve world name: {diagnostic}")}),
            );
            assert!(!String::from_utf8_lossy(&bytes).contains(private_detail));
        }
    }
}
