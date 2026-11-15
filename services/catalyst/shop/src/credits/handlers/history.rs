// from: credits-server@465e628bc:src/controllers/handlers/{get-user-purchases,get-user-credit-orders}.ts
//! `GET /users/{address}/purchases` (the credits purchase history, SETTLED
//! by default, `?status=all` adds PENDING and EXPIRED; `limit` 1..=200,
//! `offset`) and `GET /users/{address}/credit-orders` (the pack orders,
//! `limit` 1..=500). Signed fetch: the path address must be the signer
//! (403 otherwise, as upstream).

use std::collections::HashMap;

use axum::Json;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::HeaderMap;

use super::signer_from;
use crate::credits::SharedState;
use crate::credits::errors::ApiError;
use crate::credits::history::{CreditOrdersOut, PurchasesOut, clamp_page};

fn owner_of(signer: &str, wallet_id: &str) -> Result<String, ApiError> {
    let wallet = wallet_id.to_ascii_lowercase();
    if !crate::types::is_hex_address(&wallet) {
        return Err(ApiError::bad_request("Invalid address"));
    }
    if signer != wallet {
        return Err(ApiError::forbidden(
            "Address does not match the signed fetch authentication",
        ));
    }
    Ok(wallet)
}

pub async fn purchases(
    State(state): State<SharedState>,
    Path(wallet_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<PurchasesOut>, ApiError> {
    let path = format!("/users/{wallet_id}/purchases");
    let signer = signer_from(&state, &headers, "get", &path, &original).await?;
    let wallet = owner_of(signer.as_str(), &wallet_id)?;
    let all = query.get("status").is_some_and(|s| s == "all");
    let (limit, offset) = clamp_page(
        query.get("limit").map(String::as_str),
        query.get("offset").map(String::as_str),
        200,
    );
    Ok(Json(
        state
            .credits
            .purchases_history(&wallet, all, limit, offset)
            .await?,
    ))
}

pub async fn credit_orders(
    State(state): State<SharedState>,
    Path(wallet_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<CreditOrdersOut>, ApiError> {
    let path = format!("/users/{wallet_id}/credit-orders");
    let signer = signer_from(&state, &headers, "get", &path, &original).await?;
    let wallet = owner_of(signer.as_str(), &wallet_id)?;
    let (limit, offset) = clamp_page(
        query.get("limit").map(String::as_str),
        query.get("offset").map(String::as_str),
        500,
    );
    Ok(Json(
        state
            .credits
            .credit_orders_history(&wallet, limit, offset)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_address_must_be_the_signer() {
        let me = "0x8cb8e16ea85793c0da573615248b6c91c88df2dd";
        assert_eq!(
            owner_of(me, "0x8CB8E16EA85793C0DA573615248B6C91C88DF2DD").unwrap(),
            me
        );
        assert!(matches!(
            owner_of(me, "0x0000000000000000000000000000000000000009"),
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            owner_of(me, "not-an-address"),
            Err(ApiError::BadRequest(_))
        ));
    }
}
