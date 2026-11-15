//! `/admin/api/credits/*` and `/admin/api/price/*`.
//!
//! Credits: the ledger operations take their actor as an argument, so the
//! console calls them with the session wallet and the grant, revoke or block
//! records that wallet in its own `ops.admin_audit` row, in the same
//! transaction as the ledger write (`credits::admin_store`); the console's
//! row records the call. The input rules are the credits admin routes'
//! (`credits::admin` validators). As in the old console, every call needs
//! `CATALYRST_CREDITS_ADMIN_TOKEN` configured (403
//! `credits-admin-token-not-configured` otherwise, like every other console
//! family): the credits operator credential is what enables the console to
//! mint, not the console session alone. Then `OPS_WRITES_ENABLED`, the
//! credits gate's own predicate and 503.
//!
//! Price: the override console's `PUT`/`DELETE` handlers with the price
//! bearer and the wallet as `x-catalyrst-admin` (the override's recorded
//! operator), behind `DATA_WRITES_ENABLED` (`marketplace::DataWrites`).

#![allow(clippy::result_large_err)]

use axum::Json;
use axum::extract::{Path, State};
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::SharedState;
use super::auth::AdminSession;
use super::console::{
    Call, address_field, body_as, bool_field, configured, finish, not_configured, refused,
    segment_field, service_headers, str_field, token_missing,
};
use crate::shared::writes::DataWrites;
use crate::shop::credits::admin::{
    normalize_address, validate_idempotency_key, validate_positive_amount, validated_reason,
    validated_studio_name,
};
use crate::shop::credits::admin_store::GrantRequest;
use crate::shop::credits::errors::ApiError as CreditsError;

/// The credits state behind the credits admin credential and the credits
/// gate's own predicate, or the answer.
async fn credits_gate(
    state: &SharedState,
    path: &str,
) -> Result<crate::shop::credits::SharedState, Response> {
    let Some(cs) = state.domains.credits.clone() else {
        return Err(not_configured("credits"));
    };
    if cs
        .settings
        .admin_token
        .as_deref()
        .is_none_or(|t| t.trim().is_empty())
    {
        return Err(token_missing("credits"));
    }
    if crate::shop::credits::writes::is_gated(&Method::POST, path)
        && let Some(r) = crate::shared::writes::ops_refusal(cs.settings.writes_enabled)
    {
        return Err(refused(crate::shop::credits::writes::refused_response(&r)).await);
    }
    Ok(cs)
}

fn opt_string(body: &Value, key: &str) -> Option<String> {
    str_field(body, key).filter(|s| !s.trim().is_empty())
}

/// `POST /admin/api/credits/grant` `{address, amount, reason?, idempotencyKey?, studioName?}`.
pub async fn credits_grant(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(body): Json<Value>,
) -> Response {
    let cs = match credits_gate(&state, "/admin/credits/grant").await {
        Ok(cs) => cs,
        Err(resp) => return resp,
    };
    let actor = session.address().to_string();
    let answer = async {
        let address = normalize_address(&str_field(&body, "address").unwrap_or_default())?;
        let amount = validate_positive_amount(&str_field(&body, "amount").unwrap_or_default())?;
        let reason = validated_reason(&opt_string(&body, "reason"))?;
        let idempotency_key = validate_idempotency_key(&opt_string(&body, "idempotencyKey"))?;
        let studio_name = validated_studio_name(&opt_string(&body, "studioName"))?;
        let mut detail = json!({ "address": address, "amount": amount, "reason": reason, "idempotencyKey": idempotency_key, "via": "console" });
        if let Some(studio) = &studio_name {
            detail["studioName"] = json!(studio);
        }
        let outcome = cs
            .credits
            .admin_grant_credits(GrantRequest {
                address: &address,
                amount: &amount,
                kind: "grant",
                reason: reason.as_deref(),
                actor: Some(&actor),
                idempotency_key: idempotency_key.as_deref(),
                detail: &detail,
                studio_name: studio_name.as_deref(),
            })
            .await?;
        Ok::<_, CreditsError>(Json(json!({ "address": address, "applied": outcome.applied, "available": outcome.available, "replayed": outcome.replayed })))
    }
    .await
    .into_response();
    let target = str_field(&body, "address")
        .unwrap_or_default()
        .to_lowercase();
    finish(
        &state,
        Call::new(&session, "credits", "credits.grant")
            .target(target)
            .detail(body),
        answer,
    )
    .await
}

/// `POST /admin/api/credits/revoke` `{address, amount, reason?}`.
pub async fn credits_revoke(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(body): Json<Value>,
) -> Response {
    let cs = match credits_gate(&state, "/admin/credits/revoke").await {
        Ok(cs) => cs,
        Err(resp) => return resp,
    };
    let actor = session.address().to_string();
    let answer = async {
        let address = normalize_address(&str_field(&body, "address").unwrap_or_default())?;
        let amount = validate_positive_amount(&str_field(&body, "amount").unwrap_or_default())?;
        let reason = validated_reason(&opt_string(&body, "reason"))?;
        let detail = json!({ "address": address, "amount": amount, "reason": reason, "via": "console" });
        let outcome = cs.credits.admin_revoke_credits(&address, &amount, reason.as_deref(), Some(&actor), &detail).await?;
        Ok::<_, CreditsError>(Json(json!({ "address": address, "applied": outcome.applied, "available": outcome.available, "replayed": outcome.replayed })))
    }
    .await
    .into_response();
    let target = str_field(&body, "address")
        .unwrap_or_default()
        .to_lowercase();
    finish(
        &state,
        Call::new(&session, "credits", "credits.revoke")
            .target(target)
            .detail(body),
        answer,
    )
    .await
}

/// `POST /admin/api/credits/user-block` `{address, blocked, reason?}`: the
/// claim block (`/admin/users/{address}/block`).
pub async fn credits_user_block(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(body): Json<Value>,
) -> Response {
    let address = match address_field(&body, "address", "address") {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let cs = match credits_gate(&state, &format!("/admin/users/{address}/block")).await {
        Ok(cs) => cs,
        Err(resp) => return resp,
    };
    let actor = session.address().to_string();
    let answer = async {
        let address = normalize_address(&address)?;
        let blocked = bool_field(&body, "blocked")
            .ok_or_else(|| CreditsError::bad_request("missing JSON body { blocked }"))?;
        let reason = validated_reason(&opt_string(&body, "reason"))?;
        let detail =
            json!({ "address": address, "blocked": blocked, "reason": reason, "via": "console" });
        let blocked = cs
            .credits
            .admin_set_blocked(&address, blocked, reason.as_deref(), Some(&actor), &detail)
            .await?;
        Ok::<_, CreditsError>(Json(json!({ "address": address, "blocked": blocked })))
    }
    .await
    .into_response();
    finish(
        &state,
        Call::new(&session, "credits", "credits.user.block")
            .target(address)
            .detail(body),
        answer,
    )
    .await
}

/// The price state behind the data gate and its bearer, or the answer.
async fn price_for(
    state: &SharedState,
    session: &AdminSession,
    method: Method,
    path: &str,
) -> Result<(crate::price::SharedState, axum::http::HeaderMap), Response> {
    let Some(ps) = state.domains.price.clone() else {
        return Err(not_configured("price"));
    };
    if DataWrites::is_gated(&method, path)
        && let Some(r) = crate::shared::writes::refusal(ps.settings.writes_enabled)
    {
        return Err(refused(DataWrites::refused_response(&r)).await);
    }
    let Some(token) = configured(ps.settings.admin_token.as_deref()) else {
        return Err(token_missing("price"));
    };
    let headers = service_headers(Some(token), session.address());
    Ok((ps, headers))
}

fn token_vs(body: &Value) -> Result<(String, String), Response> {
    Ok((
        segment_field(body, "token", "token")?,
        segment_field(body, "vs", "vs")?,
    ))
}

/// `POST /admin/api/price/override-set` `{token, vs, value, note?}`.
pub async fn price_override_set(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(body): Json<Value>,
) -> Response {
    let (token, vs) = match token_vs(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let (ps, headers) = match price_for(
        &state,
        &session,
        Method::PUT,
        &format!("/admin/api/price/overrides/{token}/{vs}"),
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let rest = super::console::body_without(&body, &["token", "vs"]).unwrap_or_else(|| json!({}));
    let typed = match body_as::<crate::price::handlers::SetOverrideBody>(rest.clone()) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let answer = crate::price::handlers::set_override(
        State(ps),
        headers,
        Path((token.clone(), vs.clone())),
        Json(typed),
    )
    .await;
    finish(
        &state,
        Call::new(&session, "economy", "price.override.set")
            .target(format!("{token}/{vs}"))
            .detail(rest),
        answer,
    )
    .await
}

/// `POST /admin/api/price/override-delete` `{token, vs}`.
pub async fn price_override_delete(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(body): Json<Value>,
) -> Response {
    let (token, vs) = match token_vs(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let (ps, headers) = match price_for(
        &state,
        &session,
        Method::DELETE,
        &format!("/admin/api/price/overrides/{token}/{vs}"),
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let answer = crate::price::handlers::clear_override(
        State(ps),
        headers,
        Path((token.clone(), vs.clone())),
    )
    .await;
    finish(
        &state,
        Call::new(&session, "economy", "price.override.delete")
            .target(format!("{token}/{vs}"))
            .detail(json!({ "token": token, "vs": vs })),
        answer,
    )
    .await
}
