use axum::Json;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::dto::{
    GalleryImage, GetGalleryImagesResponse, GetImagesResponse, Image, UserDataResponse,
};
use crate::handlers::{PageQuery, default_compact, default_limit, default_offset, next_cursor};
use crate::http::{ApiError, Query};
use crate::{SharedState, SignedPath, signed_path};

#[derive(Deserialize, Debug)]
pub struct GetImagesQuery {
    #[serde(default = "default_offset")]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
    #[serde(default = "default_compact")]
    pub compact: bool,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Private rows are listed only to their owner: anyone else, a missing chain
/// or one that does not verify reads the public, non-rejected set.
async fn only_public_for(
    state: &SharedState,
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    user_address: &str,
) -> bool {
    !matches!(state.optional_auth(headers, method, path).await, Some(signer) if signer.as_str().eq_ignore_ascii_case(user_address))
}

pub async fn get_user_data(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(user_address): Path<String>,
    Query(_query): Query<GetImagesQuery>,
) -> Result<Response, ApiError> {
    let path = signed_path(&uri, format!("/api/users/{user_address}"));
    let only_public = only_public_for(&state, &headers, "get", &path, &user_address).await;

    let images_count = state
        .db
        .get_user_images_count(&user_address, only_public)
        .await
        .map_err(|_| ApiError::NotFound("user not found".to_string()))?;

    Ok((
        StatusCode::OK,
        Json(UserDataResponse {
            current_images: images_count,
            max_images: state.settings.max_images_per_user,
        }),
    )
        .into_response())
}

pub async fn get_user_images(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(user_address): Path<String>,
    Query(q): Query<GetImagesQuery>,
) -> Result<Response, ApiError> {
    let path = signed_path(&uri, format!("/api/users/{user_address}/images"));
    let only_public = only_public_for(&state, &headers, "get", &path, &user_address).await;

    let page = PageQuery {
        offset: q.offset,
        limit: q.limit,
        cursor: q.cursor,
    };
    let limit = page.capped_limit();
    let page = page.page()?;
    let found = state
        .db
        .get_user_images_page(&user_address, &page, limit, only_public)
        .await
        .map_err(|_| ApiError::NotFound("user not found".to_string()))?;

    let user_data = UserDataResponse {
        current_images: found.total,
        max_images: state.settings.max_images_per_user,
    };
    let next_cursor = next_cursor(found.next);

    if q.compact {
        let images = found.images.into_iter().map(GalleryImage::from).collect();
        Ok((
            StatusCode::OK,
            Json(GetGalleryImagesResponse {
                images,
                user_data,
                next_cursor,
            }),
        )
            .into_response())
    } else {
        let images = found.images.into_iter().map(Image::from).collect();
        Ok((
            StatusCode::OK,
            Json(GetImagesResponse {
                images,
                user_data,
                next_cursor,
            }),
        )
            .into_response())
    }
}

#[cfg(test)]
#[path = "../read_parity_tests.rs"]
mod read_parity_tests;
