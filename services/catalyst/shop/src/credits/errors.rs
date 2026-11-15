//! The credits error envelope: every refusal is `{ "ok": false, "error",
//! "message" }` with the old status per variant, an internal error never
//! publishes its detail, and a request without a verifying auth chain answers
//! the upstream 400 ADR-44 body.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

const ADR44_ERROR: &str = "Invalid Auth Chain";
const ADR44_MESSAGE: &str = "This endpoint requires a signed fetch request. See ADR-44.";

#[derive(Debug, Serialize)]
pub struct AuthChainErrorBody {
    pub error: String,
    pub message: String,
}

impl AuthChainErrorBody {
    pub fn adr44() -> Self {
        Self {
            error: ADR44_ERROR.to_string(),
            message: ADR44_MESSAGE.to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("Claim would exceed a mana cap (weekly: {weekly})")]
    ClaimCap { weekly: bool },
    #[error("{message}")]
    ClaimBlocked {
        message: String,
        reason: Option<super::dto::CreditFlagReason>,
    },
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("invalid auth chain: {0}")]
    InvalidAuthChain(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    PaymentRequired(String),
    #[error("{0}")]
    Unprocessable(String),
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error("{0}")]
    ServiceUnavailable(String),
    /// A source this route needs is not ready: the data-plane envelope
    /// (`source`, `configure`, `Retry-After`) inside the credits one.
    #[error("{message}")]
    SourceUnavailable {
        unavailable: &'static str,
        configure: &'static str,
        message: String,
    },
    #[error("{0}")]
    BadGateway(String),
    /// The Shop payment rule refused the line (`shop::guards::Refused`,
    /// its own envelope: 422, or 503 naming the missing source).
    #[error("{0}")]
    PaymentRule(#[from] crate::guards::Refused),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }
    pub fn payment_required(msg: impl Into<String>) -> Self {
        Self::PaymentRequired(msg.into())
    }
    pub fn unprocessable(msg: impl Into<String>) -> Self {
        Self::Unprocessable(msg.into())
    }
    pub fn not_implemented(msg: impl Into<String>) -> Self {
        Self::NotImplemented(msg.into())
    }
    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        Self::ServiceUnavailable(msg.into())
    }
    pub fn source_unavailable(
        unavailable: &'static str,
        configure: &'static str,
        msg: impl Into<String>,
    ) -> Self {
        Self::SourceUnavailable {
            unavailable,
            configure,
            message: msg.into(),
        }
    }
    pub fn bad_gateway(msg: impl Into<String>) -> Self {
        Self::BadGateway(msg.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) | Self::InvalidAuthChain(_) | Self::ClaimCap { .. } => {
                StatusCode::BAD_REQUEST
            }
            Self::Unauthorized(_) | Self::ClaimBlocked { .. } => StatusCode::UNAUTHORIZED,
            Self::PaymentRequired(_) => StatusCode::PAYMENT_REQUIRED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::ServiceUnavailable(_) | Self::SourceUnavailable { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::PaymentRule(refused) => refused.status(),
            Self::Database(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The published message: the variant's text, except for the two
    /// internal arms whose detail (a DSN, a peer address) never leaves the log.
    fn public_message(&self) -> String {
        match self {
            Self::Database(e) => {
                tracing::error!(error = %e, "credits database error");
                "internal error".to_string()
            }
            Self::Internal(m) => {
                tracing::error!(error = %m, "credits internal error");
                "internal error".to_string()
            }
            other => other.to_string(),
        }
    }
}

impl From<crate::shared::signed_fetch::SignedFetchError> for ApiError {
    fn from(e: crate::shared::signed_fetch::SignedFetchError) -> Self {
        ApiError::InvalidAuthChain(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Self::ClaimBlocked { message, reason } = self {
            let mut body = json!({"ok":false,"message":message,"isBlockedForClaiming":true});
            if let Some(reason) = reason {
                body["reason"] = json!(reason);
            }
            return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
        }
        if let Self::ClaimCap { weekly } = self {
            let (message, key) = if weekly {
                ("Claim would exceed the weekly mana cap", "weeklyExceeds")
            } else {
                ("Claim would exceed the season mana cap", "seasonExceeds")
            };
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"message":message,key:true})),
            )
                .into_response();
        }
        if let ApiError::InvalidAuthChain(reason) = &self {
            tracing::debug!(reason = %reason, "signed-fetch auth chain rejected");
            return (StatusCode::BAD_REQUEST, Json(AuthChainErrorBody::adr44())).into_response();
        }
        if let ApiError::PaymentRule(refused) = self {
            return refused.into_response();
        }
        if let ApiError::PaymentRule(refused) = self {
            return refused.into_response();
        }
        if let ApiError::PaymentRule(refused) = self {
            return refused.into_response();
        }
        if let ApiError::PaymentRule(refused) = self {
            return refused.into_response();
        }
        if let ApiError::PaymentRule(refused) = self {
            return refused.into_response();
        }
        if let ApiError::PaymentRule(refused) = self {
            return refused.into_response();
        }
        if let ApiError::SourceUnavailable {
            unavailable,
            configure,
            message,
        } = &self
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "60")],
                Json(json!({ "ok": false, "error": "Service Unavailable", "message": message, "source": unavailable, "configure": configure })),
            )
                .into_response();
        }
        let status = self.status();
        let message = self.public_message();
        (
            status,
            Json(json!({ "ok": false, "error": message, "message": message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn error_envelope_wire_shape() {
        let resp = ApiError::payment_required("insufficient credits").into_response();
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v,
            json!({ "ok": false, "error": "insufficient credits", "message": "insufficient credits" })
        );
    }

    #[test]
    fn every_variant_keeps_its_status() {
        for (err, want) in [
            (ApiError::bad_request("m"), 400u16),
            (ApiError::unauthorized("m"), 401),
            (ApiError::payment_required("m"), 402),
            (ApiError::forbidden("m"), 403),
            (ApiError::not_found("m"), 404),
            (ApiError::conflict("m"), 409),
            (ApiError::unprocessable("m"), 422),
            (ApiError::not_implemented("m"), 501),
            (ApiError::bad_gateway("m"), 502),
            (ApiError::service_unavailable("m"), 503),
            (ApiError::Internal("m".into()), 500),
            (ApiError::Database(sqlx::Error::RowNotFound), 500),
        ] {
            let label = err.to_string();
            assert_eq!(
                err.into_response().status().as_u16(),
                want,
                "wrong status for {label}"
            );
        }
    }

    #[tokio::test]
    async fn internal_detail_is_not_published() {
        let resp =
            ApiError::Internal("connect to 192.0.2.7:5434 refused".to_string()).into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v,
            json!({ "ok": false, "error": "internal error", "message": "internal error" })
        );
    }

    #[tokio::test]
    async fn a_missing_chain_answers_the_adr44_envelope() {
        let resp = ApiError::InvalidAuthChain("no chain".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v, json!({ "error": ADR44_ERROR, "message": ADR44_MESSAGE }));
    }
}
