//! Signed enrollment, seasonal MANA progress and the separate paid USD balance.

use axum::Json;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::HeaderMap;
use serde::Serialize;

use super::signer_from;
use crate::credits::SharedState;
use crate::credits::dto::{
    CreditsProgramProgressResponse, CreditsTotals, SeasonsData, UsdCredits, UserCreditsResponse,
};
use crate::credits::errors::ApiError;
use crate::credits::history::HeldOut;
use crate::types::UsdCents;

pub async fn enroll(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let signer = signer_from(&state, &headers, "post", "/users", &original).await?;
    crate::credits::goals::enroll(&state, signer.as_str()).await?;
    Ok(Json(serde_json::json!({"ok":true})))
}

pub async fn progress(
    State(state): State<SharedState>,
    Path(wallet_id): Path<String>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<CreditsProgramProgressResponse>, ApiError> {
    let wallet = wallet_id.to_lowercase();
    let path = format!("/users/{}/progress", wallet_id);
    let signer = signer_from(&state, &headers, "get", &path, &original).await?;

    if signer.as_str() != wallet {
        return Err(ApiError::forbidden("walletId does not match signer"));
    }

    Ok(Json(
        crate::credits::goals::progress(&state, &wallet).await?,
    ))
}

#[derive(serde::Deserialize)]
pub struct CreditsQuery {
    pub status: Option<String>,
}

pub async fn user_credits(
    Query(query): Query<CreditsQuery>,
    State(state): State<SharedState>,
    Path(wallet_id): Path<String>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<UserCreditsResponse>, ApiError> {
    let wallet = wallet_id.to_lowercase();
    let path = format!("/users/{}/credits", wallet_id);
    let signer = signer_from(&state, &headers, "get", &path, &original).await?;

    if signer.as_str() != wallet {
        return Err(ApiError::forbidden("walletId does not match signer"));
    }

    let spendable = state.credits.spendable(&wallet).await?;
    let held = state.credits.held(&wallet).await?;
    let mut response = user_credits_response(spendable.spendable_cents(), held);
    let (credits, total, _) =
        crate::credits::goals::user_credits(&state, &wallet, query.status.as_deref()).await?;
    response.credits = credits;
    response.total_credits = total;
    response.totals.expiring = total;
    let mut conn = state.credits.pool.acquire().await?;
    if let Some((at, reason)) = crate::credits::goals::flag_reason(&mut conn, &wallet).await? {
        response.credits.clear();
        response.total_credits = 0.0;
        response.totals.expiring = 0.0;
        response.message = Some(format!(
            "User {wallet} was flagged for multi-accounting at {at} UTC"
        ));
        response.reason = Some(reason);
        response.is_blocked_for_claiming = Some(true);
    }
    Ok(Json(response))
}

fn user_credits_response(spendable_cents: i64, held: Option<HeldOut>) -> UserCreditsResponse {
    let cents = UsdCents::new(spendable_cents);
    UserCreditsResponse {
        message: None,
        reason: None,
        is_blocked_for_claiming: None,
        credits: Vec::new(),
        total_credits: 0.0,
        totals: CreditsTotals {
            expiring: 0.0,
            non_expiring: 0.0,
        },
        usd: UsdCredits {
            balance_cents: cents.get(),
            credits: cents.credits_floor(),
            held,
        },
    }
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct BalanceOut {
    address: String,
    available: String,
}

pub async fn balance(
    State(state): State<SharedState>,
    Path(wallet_id): Path<String>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<BalanceOut>, ApiError> {
    let wallet = wallet_id.to_lowercase();
    let path = format!("/wallet/{}/balance", wallet_id);
    let signer = signer_from(&state, &headers, "get", &path, &original).await?;

    if signer.as_str() != wallet {
        return Err(ApiError::forbidden("walletId does not match signer"));
    }

    let available = state.credits.balance(&wallet).await?;

    Ok(Json(BalanceOut {
        address: wallet,
        available,
    }))
}

pub async fn seasons(State(state): State<SharedState>) -> Result<Json<SeasonsData>, ApiError> {
    Ok(Json(crate::credits::goals::season_document(&state).await?))
}

pub async fn status(
    State(state): State<SharedState>,
    Path(wallet): Path<String>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let signer = signer_from(
        &state,
        &headers,
        "get",
        &format!("/users/{wallet}/status"),
        &original,
    )
    .await?;
    if !signer.as_str().eq_ignore_ascii_case(&wallet) {
        return Err(ApiError::forbidden("walletId does not match signer"));
    }
    Ok(Json(
        crate::credits::goals::status(&state, signer.as_str()).await?,
    ))
}

pub async fn unregister(
    State(state): State<SharedState>,
    Path(wallet): Path<String>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let signer = signer_from(
        &state,
        &headers,
        "delete",
        &format!("/users/{wallet}"),
        &original,
    )
    .await?;
    if !signer.as_str().eq_ignore_ascii_case(&wallet) {
        return Err(ApiError::forbidden("walletId does not match signer"));
    }
    crate::credits::goals::opt_out(&state, signer.as_str()).await?;
    Ok(Json(serde_json::json!({"ok":true})))
}

pub async fn season(State(state): State<SharedState>) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(crate::credits::goals::season_status(&state).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_credits_wire_shape_matches_upstream_client_dto() {
        let v = serde_json::to_value(user_credits_response(125, None)).unwrap();
        assert_eq!(
            v,
            json!({
                "credits": [],
                "totalCredits": 0,
                "totals": { "expiring": 0, "nonExpiring": 0 },
                "usd": { "balanceCents": 125, "credits": 12 }
            })
        );
        let held = HeldOut {
            cents: 90,
            credits: 9,
            releases_at_seconds: Some(1_800_000_060),
            purchases: Vec::new(),
        };
        let v = serde_json::to_value(user_credits_response(1234, Some(held))).unwrap();
        assert_eq!(v["usd"]["balanceCents"], 1234);
        assert_eq!(v["usd"]["credits"], 123);
        assert_eq!(
            v["usd"]["held"],
            json!({ "cents": 90, "credits": 9, "releasesAtSeconds": 1_800_000_060i64, "purchases": [] })
        );
    }

    #[test]
    fn user_credits_zero_state_for_unknown_wallet() {
        let v = serde_json::to_value(user_credits_response(0, None)).unwrap();
        assert_eq!(v["totalCredits"], 0);
        assert_eq!(v["usd"]["credits"], 0);
        assert_eq!(v["usd"]["balanceCents"], 0);
        assert!(v["usd"].get("held").is_none());
    }

    #[test]
    fn wire_identity_balance() {
        let out = BalanceOut {
            address: "0xabc".into(),
            available: "12.5".into(),
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "address": "0xabc", "available": "12.5" })
        );
    }
}
