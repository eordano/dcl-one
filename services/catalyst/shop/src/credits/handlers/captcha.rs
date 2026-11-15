//! Signed seasonal claims use the shared SQL slider challenge and failure limit.
//! The optional native hCaptcha provider runs after slider verification, outside
//! both the challenge transaction and the atomic MANA claim transaction.
use super::signer_from;
use crate::credits::{
    SharedState,
    captcha::{self, CLAIM_TOLERANCE, Challenge, render_png},
    dto::{ClaimCreditsBody, ClaimCreditsResponse},
    errors::ApiError,
    store::now,
};
use axum::Json;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

pub async fn generate(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let signer = signer_from(&state, &headers, "get", "/captcha", &original).await?;
    if let Some(refusal) = crate::shared::writes::ops_refusal(state.settings.writes_enabled) {
        return Ok(crate::credits::writes::refused_response(&refusal));
    }
    if let Err(error) = crate::credits::goals::claim_preflight(&state, signer.as_str()).await {
        if matches!(&error,ApiError::BadRequest(message) if message=="No active season found") {
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
                ],
                Json(serde_json::json!({"ok":false,"error":"Error retrieving captcha"})),
            )
                .into_response());
        }
        if matches!(&error,ApiError::BadRequest(message) if message=="No unclaimed credits found") {
            return Ok((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"ok":false,"message":"No unclaimed credits found"})),
            )
                .into_response());
        }
        return Err(error);
    }
    match captcha::challenge(&state.credits.pool,signer.as_str(),now()).await? {
        Challenge::Ready(answer)=>Ok((StatusCode::OK,[(header::CONTENT_TYPE,"image/png"),(header::CACHE_CONTROL,"no-store, no-cache, must-revalidate")],render_png(answer)).into_response()),
        Challenge::Blocked(seconds)=>Ok((StatusCode::OK,[(header::CACHE_CONTROL,"no-store, no-cache, must-revalidate")],Json(serde_json::json!({"ok":false,"message":format!("Max attempts reached, please try again in {seconds} seconds"),"isBlockedForClaiming":true}))).into_response()),
    }
}

pub fn slider_solved(answer: f64, submitted: f64) -> bool {
    (answer - submitted).abs() < CLAIM_TOLERANCE
}
fn rejected(blocked: bool) -> Response {
    Json(ClaimCreditsResponse {
        ok: false,
        message: "Captcha verification failed. Please try again.".into(),
        credits_granted: 0.0,
        is_blocked_for_claiming: blocked,
    })
    .into_response()
}
pub async fn claim(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
    body: Option<Json<ClaimCreditsBody>>,
) -> Result<Response, ApiError> {
    let signer = signer_from(&state, &headers, "post", "/captcha", &original).await?;
    let Json(claim) = body.ok_or_else(|| ApiError::bad_request("missing JSON body { x }"))?;
    crate::credits::goals::claim_preflight(&state, signer.as_str()).await?;
    let (success, blocked) =
        captcha::solve(&state.credits.pool, signer.as_str(), claim.x, now()).await?;
    if !success {
        return Ok(rejected(blocked));
    }
    if let Some(provider) = &state.captcha {
        let Some(token) = claim.token.as_deref().filter(|t| !t.is_empty()) else {
            return Ok(rejected(false));
        };
        if !provider.verify(token, None).await? {
            return Ok(rejected(false));
        }
    }
    let granted = crate::credits::goals::claim(&state, signer.as_str()).await?;
    Ok(Json(ClaimCreditsResponse {
        ok: true,
        message: "Credit successfully claimed".into(),
        credits_granted: granted,
        is_blocked_for_claiming: false,
    })
    .into_response())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slider_requires_strict_source_tolerance() {
        assert!(slider_solved(50.0, 50.0));
        assert!(slider_solved(50.0, 52.99));
        assert!(!slider_solved(50.0, 53.0));
        assert!(!slider_solved(50.0, 47.0));
        assert!(!slider_solved(50.0, f64::NAN));
    }
}
