//! The cart (`GET /cart`, `POST /cart/items`, `DELETE
//! /cart/items/{collection}/{item}`; the server price is captured at add
//! time and re-computed at checkout, the client's price is never read),
//! `POST /checkout` (the Idempotency-Key replay, the signed EIP-712 purchase
//! intent required by default, the server-side reprice, the saga in
//! `checkout.rs`) and `GET /checkout/{id}`.

use axum::Json;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use super::prices::QuoteCache;
use super::signer_from;
use crate::credits::SharedState;
use crate::credits::batch::{OWN_SALE, pays_buyer, trade_payees};
use crate::credits::checkout::{CartItemRow, CartLine, CartView, CheckoutIdemRow, RepricedLine};
use crate::credits::errors::ApiError;
use crate::credits::pricing::{
    BasisKind, ChargeBasis, ORDER_SCAN_MAX_PAGES, TradeLookup, ensure_charge_covers_payment,
    payment_is_positive,
};
use crate::credits::purchase_intent::{
    PurchaseIntentIn, verify_intent_matches_order, verify_purchase_intent,
};
use crate::guards;
use crate::types::{EvmAddress, Session};

/// The rule's session for a signed-fetch wallet.
pub(crate) fn session_of(signer: &str) -> Result<Session, ApiError> {
    Ok(guards::signed_session(
        EvmAddress::parse(&signer.to_ascii_lowercase())
            .map_err(|e| ApiError::bad_request(format!("signer: {e}")))?,
    ))
}

/// The rule's view of a priced basis: the mirror's item and the basis'
/// source (the store, the order book row, the trade book row).
pub(crate) async fn rule_line(
    state: &SharedState,
    session: Session,
    basis: &ChargeBasis,
) -> Result<guards::credits::Line, ApiError> {
    let kind = match &basis.kind {
        BasisKind::Primary => guards::credits::Kind::Mint,
        BasisKind::Secondary { token_id } => guards::credits::Kind::LegacyListing {
            token_id: token_id.clone(),
        },
        BasisKind::Trade { trade_id } => match state.pricing.fetch_trade(trade_id).await? {
            TradeLookup::Open(trade) => {
                // Selection skips the buyer's listings; this backs it on the signed trade.
                if trade_payees(&trade)
                    .into_iter()
                    .any(|p| pays_buyer(session, p))
                {
                    return Err(ApiError::forbidden(OWN_SALE));
                }
                guards::credits::Kind::Trade { trade }
            }
            TradeLookup::NotFound => {
                return Err(ApiError::conflict(
                    "the listing this line was priced from is gone",
                ));
            }
            TradeLookup::Closed(why) => {
                return Err(ApiError::conflict(format!(
                    "the listing this line was priced from is closed ({why})"
                )));
            }
        },
    };
    guards::credits::line(session, &basis.info, &basis.basis_wei, kind)
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CartLineOut {
    #[serde(rename = "itemId")]
    item_id: String,
    collection: String,
    urn: String,
    category: String,
    qty: i32,
    #[serde(rename = "unitPriceCredits")]
    unit_price_credits: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CartOut {
    address: String,
    items: Vec<CartLineOut>,
    #[serde(rename = "totalCredits")]
    total_credits: String,
}

fn cart_out(address: &str, cart: &CartView) -> CartOut {
    CartOut {
        address: address.to_string(),
        items: cart
            .items
            .iter()
            .map(|i| CartLineOut {
                item_id: i.item_id.clone(),
                collection: i.collection.clone(),
                urn: i.urn.clone(),
                category: i.category.clone(),
                qty: i.qty,
                unit_price_credits: i.unit_price_credits.clone(),
            })
            .collect(),
        total_credits: cart.total_credits.clone(),
    }
}

pub async fn get_cart(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<CartOut>, ApiError> {
    let signer = signer_from(&state, &headers, "get", "/cart", &original).await?;
    let cart = state.credits.get_cart(signer.as_str()).await?;
    Ok(Json(cart_out(signer.as_str(), &cart)))
}

#[derive(Debug, Deserialize)]
pub struct AddItemBody {
    #[serde(rename = "itemId")]
    item_id: String,
    collection: String,
    #[serde(default)]
    qty: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct CheckoutScopeItem {
    #[serde(rename = "itemId")]
    item_id: String,
    collection: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct CheckoutBody {
    #[serde(default)]
    items: Option<Vec<CheckoutScopeItem>>,
    #[serde(default)]
    intent: Option<PurchaseIntentIn>,
    #[serde(rename = "intentSignature", default)]
    intent_signature: Option<String>,
}

pub(crate) fn ensure_intent_present(
    has_signed_intent: bool,
    require_purchase_intent: bool,
) -> Result<(), ApiError> {
    if !has_signed_intent && require_purchase_intent {
        return Err(ApiError::unauthorized(
            "purchase requires a signed intent: sign the EIP-712 purchase sheet and send it as `intent` + `intentSignature`",
        ));
    }
    Ok(())
}

pub(crate) fn ensure_qty_fillable(kind: &BasisKind, qty: i32) -> Result<(), ApiError> {
    if kind.is_single_listing() && qty > 1 {
        return Err(ApiError::conflict(
            "this item is stocked from individual marketplace listings, so it's one per checkout \
             while supplies are listed individually \u{2014} please keep the quantity at 1",
        ));
    }
    Ok(())
}

pub(crate) fn validate_item_id(raw: &str) -> Result<String, ApiError> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 200 {
        return Err(ApiError::bad_request("invalid itemId"));
    }
    if !s.chars().all(|c| c.is_ascii_graphic() && c != '\\') {
        return Err(ApiError::bad_request("invalid itemId"));
    }
    Ok(s.to_string())
}

pub(crate) fn validate_collection(raw: &str) -> Result<String, ApiError> {
    let s = raw.trim().to_ascii_lowercase();
    if !crate::types::is_hex_address(&s) {
        return Err(ApiError::bad_request(
            "invalid collection (expected 0x + 40 hex)",
        ));
    }
    Ok(s)
}

pub async fn add_item(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
    Json(body): Json<AddItemBody>,
) -> Result<Json<CartOut>, ApiError> {
    let signer = signer_from(&state, &headers, "post", "/cart/items", &original).await?;
    let item_id = validate_item_id(&body.item_id)?;
    let collection = validate_collection(&body.collection)?;
    let qty = body.qty.unwrap_or(1);
    if qty <= 0 {
        return Err(ApiError::bad_request("qty must be > 0"));
    }

    let mode = state.settings.checkout_fulfillment_mode.as_str();
    let basis = state
        .pricing
        .fetch_charge_bases_batch(
            &[(collection.clone(), item_id.clone())],
            mode,
            ORDER_SCAN_MAX_PAGES,
            Some(signer.as_str()),
        )
        .await?
        .pop()
        .unwrap_or_else(|| Err(ApiError::Internal("empty charge basis batch".into())))?;

    ensure_qty_fillable(&basis.kind, qty)?;
    rule_line(&state, session_of(signer.as_str())?, &basis)
        .await?
        .guard_quote()?;

    let mana_usd = state.pricing.fetch_mana_usd().await?;
    let credit_price = state
        .credits
        .add_item_priced(
            CartLine {
                address: signer.as_str(),
                item_id: &item_id,
                collection: &collection,
                urn: &basis.info.urn,
                category: &basis.info.category,
                qty,
            },
            &basis.basis_wei,
            &mana_usd,
            state.pricing.markup_bps(),
            payment_is_positive(&basis.basis_wei),
        )
        .await?;
    ensure_charge_covers_payment(&basis.basis_wei, &credit_price)?;

    let cart = state.credits.get_cart(signer.as_str()).await?;
    Ok(Json(cart_out(signer.as_str(), &cart)))
}

pub async fn remove_item(
    State(state): State<SharedState>,
    Path((collection, item_id)): Path<(String, String)>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<CartOut>, ApiError> {
    let path = format!("/cart/items/{}/{}", collection, item_id);
    let signer = signer_from(&state, &headers, "delete", &path, &original).await?;
    let collection = validate_collection(&collection)?;
    let item_id = validate_item_id(&item_id)?;
    let cart = state
        .credits
        .remove_item(signer.as_str(), &collection, &item_id)
        .await?;
    Ok(Json(cart_out(signer.as_str(), &cart)))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing Idempotency-Key header"))?;
    if raw.len() > 200 {
        return Err(ApiError::bad_request("Idempotency-Key too long"));
    }
    Ok(raw.to_string())
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CheckoutStartOut {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    id: i64,
    status: String,
    replayed: bool,
}

/// The signer's own earlier checkout under the key (the lookup is scoped to it).
pub(crate) fn replay_of(prior: Option<CheckoutIdemRow>) -> Option<CheckoutStartOut> {
    prior.map(|row| CheckoutStartOut {
        id: row.id,
        status: row.status,
        replayed: true,
    })
}

type CheckoutScopeAndIntent = (
    Option<Vec<(String, String)>>,
    Option<(PurchaseIntentIn, String)>,
);

pub(crate) fn seed_quote_cache_after_drift(cache: &QuoteCache, repriced: &[RepricedLine]) {
    for line in repriced {
        cache.put(
            &line.collection.to_ascii_lowercase(),
            &line.item_id,
            Some(line.unit_price_credits.clone()),
        );
    }
}

pub async fn checkout(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
    body: Result<Json<CheckoutBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<CheckoutStartOut>, ApiError> {
    // The cart's checkout spends ledger credits through the economy broker, outside the
    // CreditsManager and the trial's buyers: a trial spends credits on chain only.
    if state.settings.trial.is_some() {
        return Err(ApiError::forbidden(
            "the cart checkout is disabled in this test (CREDITS_TRIAL_MODE): credits are spent through the CreditsManager only",
        ));
    }
    let signer = signer_from(&state, &headers, "post", "/checkout", &original).await?;
    let idem = idempotency_key(&headers)?;

    if let Some(out) = replay_of(
        state
            .credits
            .find_checkout_by_idempotency_key(signer.as_str(), &idem)
            .await?,
    ) {
        return Ok(Json(out));
    }

    let (scope, signed_intent): CheckoutScopeAndIntent = match body {
        Ok(Json(b)) => {
            let scope = match b.items {
                Some(items) if !items.is_empty() => {
                    let mut v = Vec::with_capacity(items.len());
                    for it in &items {
                        v.push((
                            validate_collection(&it.collection)?,
                            validate_item_id(&it.item_id)?,
                        ));
                    }
                    Some(v)
                }
                _ => None,
            };
            let signed = match (b.intent, b.intent_signature) {
                (Some(i), Some(s)) => Some((i, s)),
                (None, None) => None,
                _ => {
                    return Err(ApiError::bad_request(
                        "intent and intentSignature must be provided together",
                    ));
                }
            };
            (scope, signed)
        }
        Err(_) => (None, None),
    };

    match &signed_intent {
        Some((intent, sig)) => {
            let now = chrono::Utc::now().timestamp().max(0) as u64;
            verify_purchase_intent(intent, sig, signer.as_str(), &idem, now)?;
        }
        None => ensure_intent_present(false, state.settings.require_purchase_intent)?,
    }

    if !state.settings.mock_fulfillment && !state.broker.configured() {
        return Err(ApiError::not_implemented(
            "checkout is disabled: the broker/escrow fulfilment path is not configured \
             (CATALYRST_ECONOMY_ADMIN_TOKEN / LANDILER_ESCROW_ADDRESS / DATA_WRITES_ENABLED). Refusing to debit \
             Credits for an order that cannot be fulfilled.",
        ));
    }

    let cart = state.credits.get_cart(signer.as_str()).await?;
    if cart.items.is_empty() {
        return Err(ApiError::bad_request("cart is empty"));
    }

    if let Some(scope) = &scope {
        for (c, i) in scope {
            let found = cart
                .items
                .iter()
                .any(|l| l.collection.eq_ignore_ascii_case(c) && &l.item_id == i);
            if !found {
                return Err(ApiError::bad_request(format!(
                    "scoped item {c}:{i} is not in the cart"
                )));
            }
        }
    }

    let lines: Vec<&CartItemRow> = cart
        .items
        .iter()
        .filter(|line| match &scope {
            Some(scope) => scope
                .iter()
                .any(|(c, i)| line.collection.eq_ignore_ascii_case(c) && &line.item_id == i),
            None => true,
        })
        .collect();
    let pairs: Vec<(String, String)> = lines
        .iter()
        .map(|l| (l.collection.clone(), l.item_id.clone()))
        .collect();
    let priced_lines = state
        .pricing
        .price_items_for_mode(
            &state.credits.pool,
            &pairs,
            &state.settings.checkout_fulfillment_mode,
            ORDER_SCAN_MAX_PAGES,
            Some(signer.as_str()),
        )
        .await?;

    let session = session_of(signer.as_str())?;
    let mut repriced: Vec<RepricedLine> = Vec::with_capacity(lines.len());
    for (line, priced) in lines.into_iter().zip(priced_lines) {
        let priced = priced?;
        // A mint pays the creator; a listing pays its seller, never the buyer (selection skips those).
        if matches!(priced.basis.kind, BasisKind::Primary)
            && priced
                .basis
                .info
                .creator
                .eq_ignore_ascii_case(signer.as_str())
        {
            return Err(ApiError::forbidden(format!(
                "{}:{}: {OWN_SALE}",
                line.collection, line.item_id
            )));
        }
        ensure_qty_fillable(&priced.basis.kind, line.qty)?;
        rule_line(&state, session, &priced.basis)
            .await?
            .guard_spend(
                guards::charge_cents(&priced.credit_price).unwrap_or(guards::broker::CREDIT_FUNDED),
            )?;
        let (mode, token_id, trade_id) = match &priced.basis.kind {
            BasisKind::Primary => ("primary", None, None),
            BasisKind::Secondary { token_id } => ("secondary", Some(token_id.clone()), None),
            BasisKind::Trade { trade_id } => ("trade", None, Some(trade_id.clone())),
        };
        repriced.push(RepricedLine {
            item_id: line.item_id.clone(),
            collection: line.collection.clone(),
            urn: priced.basis.info.urn,
            category: priced.basis.info.category,
            qty: line.qty,
            unit_price_credits: priced.credit_price,
            token_id,
            trade_id,
            basis_wei: Some(priced.basis.basis_wei),
            mode: mode.to_string(),
        });
    }

    if repriced.is_empty() {
        return Err(ApiError::bad_request("no items to checkout"));
    }

    if let Some((intent, _)) = &signed_intent
        && let Err(err) = verify_intent_matches_order(intent, &repriced)
    {
        if matches!(err, ApiError::Conflict(_)) {
            seed_quote_cache_after_drift(&state.quote_cache, &repriced);
        }
        return Err(err);
    }

    let outcome = state
        .credits
        .run_checkout(signer.as_str(), &idem, &repriced)
        .await?;

    Ok(Json(CheckoutStartOut {
        id: outcome.id,
        status: outcome.status,
        replayed: outcome.replayed,
    }))
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CheckoutOut {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    id: i64,
    address: String,
    #[serde(rename = "totalCredits")]
    total_credits: String,
    status: String,
}

pub async fn get_checkout(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    original: OriginalUri,
    headers: HeaderMap,
) -> Result<Json<CheckoutOut>, ApiError> {
    let path = format!("/checkout/{}", id);
    let signer = signer_from(&state, &headers, "get", &path, &original).await?;

    let row = state
        .credits
        .get_checkout(id)
        .await?
        .ok_or_else(|| ApiError::not_found("checkout not found"))?;
    if row.address.to_lowercase() != signer.as_str() {
        return Err(ApiError::forbidden("checkout does not belong to signer"));
    }

    Ok(Json(CheckoutOut {
        id: row.id,
        address: row.address,
        total_credits: row.total_credits,
        status: row.status,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_identity_cart_populated() {
        let cart = CartView {
            items: vec![
                CartItemRow {
                    item_id: "12".into(),
                    collection: "0x59a90bad9570ecd08895f132daf7b79696337f61".into(),
                    urn: "urn:decentraland:matic:collections-v2:0x59a9:12".into(),
                    category: "wearable".into(),
                    qty: 2,
                    unit_price_credits: "1.5".into(),
                },
                CartItemRow {
                    item_id: "3".into(),
                    collection: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    urn: "urn:decentraland:matic:collections-v2:0xaaaa:3".into(),
                    category: "emote".into(),
                    qty: 1,
                    unit_price_credits: "0".into(),
                },
            ],
            total_credits: "3".into(),
        };
        assert_eq!(
            serde_json::to_value(cart_out("0xabc", &cart)).unwrap(),
            json!({
                "address": "0xabc",
                "items": [
                    { "itemId": "12", "collection": "0x59a90bad9570ecd08895f132daf7b79696337f61", "urn": "urn:decentraland:matic:collections-v2:0x59a9:12", "category": "wearable", "qty": 2, "unitPriceCredits": "1.5" },
                    { "itemId": "3", "collection": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "urn": "urn:decentraland:matic:collections-v2:0xaaaa:3", "category": "emote", "qty": 1, "unitPriceCredits": "0" },
                ],
                "totalCredits": "3",
            })
        );
    }

    #[test]
    fn wire_identity_cart_empty() {
        let cart = CartView {
            items: vec![],
            total_credits: "0".into(),
        };
        assert_eq!(
            serde_json::to_value(cart_out("0xabc", &cart)).unwrap(),
            json!({ "address": "0xabc", "items": [], "totalCredits": "0" })
        );
    }

    #[test]
    fn wire_identity_checkout_start_and_get() {
        let fresh = CheckoutStartOut {
            id: 41,
            status: "fulfilling".into(),
            replayed: false,
        };
        assert_eq!(
            serde_json::to_value(&fresh).unwrap(),
            json!({ "id": 41, "status": "fulfilling", "replayed": false })
        );
        let out = CheckoutOut {
            id: 41,
            address: "0xabc".into(),
            total_credits: "1.5".into(),
            status: "fulfilled".into(),
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "id": 41, "address": "0xabc", "totalCredits": "1.5", "status": "fulfilled" })
        );
    }

    #[test]
    fn no_intent_checkout_is_rejected_under_default_config() {
        const {
            assert!(
                crate::credits::DEFAULT_REQUIRE_PURCHASE_INTENT,
                "intent enforcement must be the default"
            )
        };
        let err = ensure_intent_present(false, true).unwrap_err();
        match &err {
            ApiError::Unauthorized(m) => {
                assert!(m.contains("purchase requires a signed intent"), "{m}")
            }
            other => panic!("expected 401 Unauthorized, got {other:?}"),
        }
        ensure_intent_present(true, true).unwrap();
        ensure_intent_present(true, false).unwrap();
        ensure_intent_present(false, false).unwrap();
    }

    #[test]
    fn single_listing_bases_are_one_per_checkout() {
        let secondary = BasisKind::Secondary {
            token_id: "2901".into(),
        };
        let trade = BasisKind::Trade {
            trade_id: "1bbe7d78-dd71-4cbe-9085-70d679d3ad11".into(),
        };
        let err = ensure_qty_fillable(&secondary, 2).unwrap_err();
        assert!(
            matches!(&err, ApiError::Conflict(m) if m.contains("one per checkout")),
            "{err:?}"
        );
        assert!(ensure_qty_fillable(&secondary, 100).is_err());
        assert!(ensure_qty_fillable(&trade, 2).is_err());
        ensure_qty_fillable(&trade, 1).unwrap();
        ensure_qty_fillable(&BasisKind::Primary, 100).unwrap();
        ensure_qty_fillable(&secondary, 1).unwrap();
    }

    #[test]
    fn replay_decisions() {
        let out = replay_of(Some(CheckoutIdemRow {
            id: 41,
            address: "0xAbC0000000000000000000000000000000000001".into(),
            status: "fulfilling".into(),
        }))
        .expect("must replay");
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "id": 41, "status": "fulfilling", "replayed": true })
        );
        assert!(replay_of(None).is_none());
    }

    #[test]
    fn post_drift_reseed_makes_the_next_quote_serve_checkout_prices() {
        use std::time::Duration;
        let collection = "0x59a90bad9570ecd08895f132daf7b79696337f61";
        let item_id = "12";
        let cache = QuoteCache::new(Duration::from_secs(60), 100);
        cache.put(collection, item_id, Some("1.5".into()));
        let repriced = vec![RepricedLine {
            item_id: item_id.into(),
            collection: collection.to_ascii_uppercase().replace("0X", "0x"),
            urn: "urn:decentraland:matic:collections-v2:0x59a9:12".into(),
            category: "wearable".into(),
            qty: 1,
            unit_price_credits: "2".into(),
            token_id: Some("2901".into()),
            trade_id: None,
            basis_wei: Some("20000000000000000".into()),
            mode: "secondary".into(),
        }];
        let intent = PurchaseIntentIn {
            buyer: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".into(),
            items: crate::credits::purchase_intent::canonical_items(&repriced),
            total_credits: "1.5".into(),
            currency: "CREDITS".into(),
            nonce: "idem-1".into(),
            expires_at: 4_000_000_000,
        };
        let err = verify_intent_matches_order(&intent, &repriced).unwrap_err();
        assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
        seed_quote_cache_after_drift(&cache, &repriced);
        assert_eq!(cache.get(collection, item_id), Some(Some("2".into())));
    }

    #[test]
    fn validates_item_id_and_collection() {
        assert_eq!(validate_item_id(" 0xabc-1 ").unwrap(), "0xabc-1");
        assert!(validate_item_id("").is_err());
        assert!(validate_item_id("   ").is_err());
        assert!(validate_item_id("a\\b").is_err());
        assert!(validate_item_id(&"x".repeat(201)).is_err());
        assert_eq!(
            validate_collection(" 0x59A90BAD9570ECD08895F132DAF7B79696337F61 ").unwrap(),
            "0x59a90bad9570ecd08895f132daf7b79696337f61"
        );
        assert!(validate_collection("").is_err());
        assert!(validate_collection("59a90bad9570ecd08895f132daf7b79696337f61").is_err());
        assert!(validate_collection("0x123").is_err());
        assert!(validate_collection(&format!("0x{}", "z".repeat(40))).is_err());
        assert!(validate_collection(&format!("0x{}", "a".repeat(41))).is_err());
    }
}
