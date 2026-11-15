//! The pack listings (the marketplace's and the explorer's shapes), the
//! PaymentIntent flow (`POST /packs/{sku}/intent`) and the two development
//! mocks behind `CREDITS_MOCK_CARD` (a pack purchase and a capped top-up
//! granted without a charge).

use axum::Json;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::signer_from;
use crate::credits::SharedState;
use crate::credits::admin::validate_positive_amount;
use crate::credits::admin_store::GrantRequest;
use crate::credits::errors::ApiError;
use crate::credits::packs::{PackRow, validate_sku};
use crate::shared::signed_fetch::AUTH_TIMESTAMP_HEADER;

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct PackOut {
    sku: String,
    title: String,
    credits: String,
    #[serde(rename = "priceCents")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    price_cents: i64,
    currency: String,
    #[serde(rename = "sortOrder")]
    sort_order: i32,
}

impl From<PackRow> for PackOut {
    fn from(p: PackRow) -> Self {
        PackOut {
            sku: p.sku,
            title: p.title,
            credits: p.credits,
            price_cents: p.price_cents,
            currency: p.currency,
            sort_order: p.sort_order,
        }
    }
}

pub async fn list_packs(State(state): State<SharedState>) -> Result<Json<Vec<PackOut>>, ApiError> {
    let rows = state.credits.list_active_packs().await?;
    Ok(Json(rows.into_iter().map(PackOut::from).collect()))
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct UnityCreditPacksResponse {
    pub packs: Vec<UnityCreditPack>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct UnityCreditPack {
    pub id: String,
    pub usd: f64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub credits: i64,
    pub recommended: bool,
    pub order: i32,
}

fn unity_pack(p: &PackRow) -> Option<UnityCreditPack> {
    if !p.currency.eq_ignore_ascii_case("usd") {
        return None;
    }
    let credits = p.credits.parse::<f64>().ok()?.round() as i64;
    Some(UnityCreditPack {
        id: p.sku.clone(),
        usd: p.price_cents as f64 / 100.0,
        credits,
        recommended: false,
        order: p.sort_order,
    })
}

pub async fn unity_list_packs(
    State(state): State<SharedState>,
) -> Result<Json<UnityCreditPacksResponse>, ApiError> {
    let rows = state.credits.list_active_packs().await?;
    Ok(Json(UnityCreditPacksResponse {
        packs: rows.iter().filter_map(unity_pack).collect(),
    }))
}

fn intent_idempotency_key(signer: &str, pack: &PackRow, auth_ts: &str) -> String {
    let mut h = Sha256::new();
    for field in [
        signer,
        pack.sku.as_str(),
        pack.credits.as_str(),
        &pack.price_cents.to_string(),
        pack.currency.as_str(),
        auth_ts,
    ] {
        h.update(field.as_bytes());
        h.update([0u8]);
    }
    format!("landiler-intent-{}", hex::encode(h.finalize()))
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct PackIntentOut {
    #[serde(rename = "clientSecret")]
    client_secret: String,
    #[serde(rename = "paymentIntentId")]
    payment_intent_id: String,
}

pub async fn create_intent(
    State(state): State<SharedState>,
    Path(sku): Path<String>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<PackIntentOut>, ApiError> {
    let sku = validate_sku(&sku)?;
    let path = format!("/packs/{}/intent", sku);
    let signer = signer_from(&state, &headers, "post", &path, &original).await?;
    let auth_ts = headers
        .get(AUTH_TIMESTAMP_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let Some(stripe) = state.stripe.as_ref() else {
        return Err(ApiError::not_implemented(
            "card purchases are disabled (STRIPE_SECRET_KEY unset)",
        ));
    };

    let pack = state
        .credits
        .get_pack(&sku)
        .await?
        .ok_or_else(|| ApiError::not_found("pack not found or inactive"))?;

    let idempotency_key = intent_idempotency_key(signer.as_str(), &pack, auth_ts);

    let pi = stripe
        .create_payment_intent(
            pack.price_cents,
            &pack.currency,
            signer.as_str(),
            &pack.sku,
            &pack.credits,
            &idempotency_key,
        )
        .await?;

    state
        .credits
        .insert_pending_purchase(signer.as_str(), &pack, &pi.id)
        .await?;

    Ok(Json(PackIntentOut {
        client_secret: pi.client_secret,
        payment_intent_id: pi.id,
    }))
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct MockPurchaseOut {
    #[serde(rename = "creditsGranted")]
    credits_granted: String,
    available: String,
    mock: bool,
}

pub async fn mock_purchase(
    State(state): State<SharedState>,
    Path(sku): Path<String>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<MockPurchaseOut>, ApiError> {
    if !state.settings.mock_card {
        return Err(ApiError::not_implemented(
            "mock card purchases are disabled (CREDITS_MOCK_CARD unset)",
        ));
    }
    let sku = validate_sku(&sku)?;
    let path = format!("/packs/{}/mock-purchase", sku);
    let signer = signer_from(&state, &headers, "post", &path, &original).await?;
    let auth_ts = headers
        .get(AUTH_TIMESTAMP_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let pack = state
        .credits
        .get_pack(&sku)
        .await?
        .ok_or_else(|| ApiError::not_found("pack not found or inactive"))?;

    let idem = format!("mock-card:{}:{}:{}", signer.as_str(), pack.sku, auth_ts);
    let detail = serde_json::json!({ "source": "card-mock", "sku": pack.sku, "priceCents": pack.price_cents, "mock": true });
    let outcome = state
        .credits
        .admin_grant_credits(GrantRequest {
            address: signer.as_str(),
            amount: &pack.credits,
            kind: "purchase",
            reason: Some("mock card purchase (no real charge)"),
            actor: Some("card-mock"),
            idempotency_key: Some(&idem),
            detail: &detail,
        })
        .await?;

    Ok(Json(MockPurchaseOut {
        credits_granted: pack.credits.clone(),
        available: outcome.available,
        mock: true,
    }))
}

pub const MOCK_TOPUP_MAX_CREDITS: u32 = 10_000;

fn exceeds_mock_topup_cap(amount: &str) -> bool {
    let (int_part, frac_part) = amount.split_once('.').unwrap_or((amount, ""));
    let int_digits = int_part.trim_start_matches('0');
    let cap = MOCK_TOPUP_MAX_CREDITS.to_string();
    match int_digits.len().cmp(&cap.len()) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => match int_digits.cmp(cap.as_str()) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => frac_part.bytes().any(|b| b != b'0'),
        },
    }
}

#[derive(Debug, Deserialize)]
pub struct MockTopupBody {
    credits: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct MockTopupOut {
    credits_granted: String,
    available: String,
    mock: bool,
}

pub async fn mock_topup(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
    Json(body): Json<MockTopupBody>,
) -> Result<Json<MockTopupOut>, ApiError> {
    if !state.settings.mock_card {
        return Err(ApiError::not_implemented(
            "mock card top-ups are disabled (CREDITS_MOCK_CARD unset)",
        ));
    }
    let signer = signer_from(&state, &headers, "post", "/topup/mock-card", &original).await?;
    let auth_ts = headers
        .get(AUTH_TIMESTAMP_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let credits = validate_positive_amount(&body.credits)?;
    if exceeds_mock_topup_cap(&credits) {
        return Err(ApiError::bad_request(format!(
            "mock top-up is capped at {MOCK_TOPUP_MAX_CREDITS} credits per request"
        )));
    }

    let idem = format!("mock-card-topup:{}:{}", signer.as_str(), auth_ts);
    let detail = serde_json::json!({ "source": "card-mock", "topup": true, "mock": true });
    let outcome = state
        .credits
        .admin_grant_credits(GrantRequest {
            address: signer.as_str(),
            amount: &credits,
            kind: "purchase",
            reason: Some("mock card top-up (no real charge)"),
            actor: Some("card-mock"),
            idempotency_key: Some(&idem),
            detail: &detail,
        })
        .await?;

    Ok(Json(MockTopupOut {
        credits_granted: credits,
        available: outcome.available,
        mock: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pack() -> PackRow {
        PackRow {
            sku: "pack_100".into(),
            title: "100 Credits".into(),
            credits: "100".into(),
            price_cents: 999,
            currency: "usd".into(),
            sort_order: 0,
        }
    }

    #[test]
    fn unity_pack_wire_shape_matches_the_client_dto() {
        let row = PackRow {
            sku: "credits_usd_5".into(),
            title: "Starter".into(),
            credits: "50".into(),
            price_cents: 500,
            currency: "usd".into(),
            sort_order: 1,
        };
        let v = serde_json::to_value(UnityCreditPacksResponse {
            packs: vec![unity_pack(&row).unwrap()],
        })
        .unwrap();
        assert_eq!(
            v,
            json!({ "packs": [{ "id": "credits_usd_5", "usd": 5.0, "credits": 50, "recommended": false, "order": 1 }]})
        );
    }

    #[test]
    fn unity_pack_skips_rows_it_cannot_represent() {
        let mut row = PackRow {
            sku: "s".into(),
            title: "t".into(),
            credits: "not-a-number".into(),
            price_cents: 500,
            currency: "usd".into(),
            sort_order: 1,
        };
        assert!(unity_pack(&row).is_none());
        row.credits = "50".into();
        row.currency = "mana".into();
        assert!(unity_pack(&row).is_none());
    }

    #[test]
    fn wire_identity_pack() {
        let out = PackOut {
            sku: "starter".into(),
            title: "Starter Pack".into(),
            credits: "1000".into(),
            price_cents: 500,
            currency: "usd".into(),
            sort_order: 1,
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "sku": "starter", "title": "Starter Pack", "credits": "1000", "priceCents": 500, "currency": "usd", "sortOrder": 1 })
        );
    }

    #[test]
    fn wire_identity_pack_intent() {
        let out = PackIntentOut {
            client_secret: "pi_3ABC_secret_XYZ".into(),
            payment_intent_id: "pi_3ABC".into(),
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "clientSecret": "pi_3ABC_secret_XYZ", "paymentIntentId": "pi_3ABC" })
        );
    }

    #[test]
    fn wire_identity_mock_topup() {
        let out = MockTopupOut {
            credits_granted: "25".into(),
            available: "125".into(),
            mock: true,
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "creditsGranted": "25", "available": "125", "mock": true })
        );
    }

    #[test]
    fn mock_topup_cap_allows_up_to_the_cap() {
        for ok in ["1", "0.5", "9999.99", "10000", "10000.00", "010000"] {
            assert!(!exceeds_mock_topup_cap(ok), "{ok}");
        }
    }

    #[test]
    fn mock_topup_cap_rejects_above_the_cap() {
        for over in ["10000.01", "10001", "99999", "100000"] {
            assert!(exceeds_mock_topup_cap(over), "{over}");
        }
        assert!(exceeds_mock_topup_cap(&format!("1{}", "0".repeat(70))));
    }

    /// Pure length/lexicographic comparison against the cap, only ever
    /// reached after `validate_positive_amount` rejected exponents, signs,
    /// repeated dots and whitespace.
    #[test]
    fn characterization_exceeds_mock_topup_cap() {
        for small in [
            "1e18", "1E18", " 1.5 ", ".5", "5.", "01.50", "", "-1", "1.2.3",
        ] {
            assert!(!exceeds_mock_topup_cap(small), "{small:?}");
        }
        assert!(exceeds_mock_topup_cap(&"9".repeat(50)));
    }

    #[test]
    fn validates_sku() {
        assert_eq!(validate_sku(" pack_100 ").unwrap(), "pack_100");
        assert_eq!(validate_sku(&"x".repeat(100)).unwrap(), "x".repeat(100));
        assert_eq!(validate_sku("pack_!.-@").unwrap(), "pack_!.-@");
        for invalid in ["bad\tpack", "bad\npack", "é", "a/b", "a\\b"] {
            assert_eq!(
                validate_sku(invalid).unwrap_err().to_string(),
                "invalid sku"
            );
        }

        assert!(validate_sku("").is_err());
        assert!(validate_sku("   ").is_err());
        assert!(validate_sku("a/b").is_err());
        assert!(validate_sku("bad sku").is_err());
        assert!(validate_sku(&"x".repeat(101)).is_err());
    }

    #[test]
    fn idempotency_key_is_deterministic_and_well_formed() {
        let k1 = intent_idempotency_key("0xabc", &pack(), "1690000000000");
        let k2 = intent_idempotency_key("0xabc", &pack(), "1690000000000");
        assert_eq!(k1, k2);
        assert!(k1.starts_with("landiler-intent-"));
        assert_eq!(k1.len(), "landiler-intent-".len() + 64);
    }

    #[test]
    fn idempotency_key_varies_per_distinct_purchase() {
        let base = intent_idempotency_key("0xabc", &pack(), "1690000000000");
        assert_ne!(
            base,
            intent_idempotency_key("0xdef", &pack(), "1690000000000")
        );
        assert_ne!(
            base,
            intent_idempotency_key("0xabc", &pack(), "1690000099999")
        );
        let mut p = pack();
        p.sku = "pack_500".into();
        assert_ne!(base, intent_idempotency_key("0xabc", &p, "1690000000000"));
        let mut p = pack();
        p.price_cents = 1099;
        assert_ne!(base, intent_idempotency_key("0xabc", &p, "1690000000000"));
        let mut p = pack();
        p.credits = "120".into();
        assert_ne!(base, intent_idempotency_key("0xabc", &p, "1690000000000"));
    }
}
