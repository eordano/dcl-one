// from: notifications-workers@e2af2ee inbox/src/controllers/handlers/common-email-handlers.ts, @dcl/http-commons bearerTokenMiddleware (Apache-2.0)
//! `POST /notifications/email`: a service sends one ad-hoc email through the
//! generic template, gated by `NOTIFICATION_SERVICE_TOKEN`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;

use super::email::{SendFailure, common_email_data};
use super::errors::{ApiError, NotificationsError};
use crate::auth::admin_bearer::{Refusal, check};

type AppState = Arc<super::State>;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EmailBody {
    pub email: Option<String>,
    pub subject: Option<String>,
    pub content: Option<String>,
    pub title: Option<String>,
    #[serde(rename = "titleHighlight")]
    pub title_highlight: Option<String>,
}

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, NotificationsError> {
    value
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::bad_request(format!("Missing {name}")).into())
}

/// The bearer is checked before the body is read, as upstream's middleware.
pub async fn post_email(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, NotificationsError> {
    match check(
        state.settings.notification_service_token.as_deref(),
        &headers,
    ) {
        Ok(()) => {}
        Err(Refusal::NoBearer) if !headers.contains_key("authorization") => {
            return Err(ApiError::unauthorized("Authorization header is missing").into());
        }
        Err(refusal) => {
            tracing::warn!(?refusal, "notification service bearer refused");
            return Err(ApiError::unauthorized("Invalid authorization header").into());
        }
    }
    let body: EmailBody =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("Invalid body"))?;
    let email = required(&body.email, "email")?;
    let content = required(&body.content, "content")?;
    let subject = required(&body.subject, "subject")?;
    let title = body
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or("New");
    let highlight = body
        .title_highlight
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or("Notification");
    let data = common_email_data(subject, content, title, highlight);
    match state
        .email
        .send_generic(
            email,
            data,
            serde_json::json!({ "email_type": "notification" }),
        )
        .await
    {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(SendFailure::NotConfigured | SendFailure::NoSigningKey) => {
            Err(ApiError::Internal("email delivery is not configured".into()).into())
        }
        Err(SendFailure::Transient(e) | SendFailure::Permanent(e)) => {
            Err(ApiError::Internal(e).into())
        }
    }
}
