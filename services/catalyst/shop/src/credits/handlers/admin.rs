//! The operator routes behind [`RequireAdmin`]: grants, revokes, the claim
//! block, the multi-accounting flags (notified over `wallet_flags.rs` or
//! posted by an operator), the pack
//! catalogue, the purchase/checkout/ledger pages, the manual
//! refund and force-fulfil of a checkout, the escrow reclaim/release of a
//! lease (through the economy broker) and the reconciler. Every mutation
//! audits into `ops.admin_audit` with the server-chosen actor.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::credits::SharedState;
use crate::credits::admin::{
    FlagBody, RequireAdmin, normalize_address, paginate, validate_currency, validate_escrow_ref,
    validate_idempotency_key, validate_positive_amount, validate_price_cents, validated_reason,
};
use crate::credits::admin_store::{GrantRequest, PackAdminRow, PackSpec};
use crate::credits::errors::ApiError;
use crate::credits::packs::validate_sku;
use crate::credits::reconcile::ReconcileReport;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/admin/credits/grant", post(grant_credits))
        .route("/admin/credits/revoke", post(revoke_credits))
        .route("/admin/users/{address}/block", post(block_user))
        .route("/admin/flags", get(list_flags).post(flag_wallets))
        .route("/admin/flags/{address}", delete(unflag_wallet))
        .route("/admin/packs", get(list_packs).post(create_pack))
        .route("/admin/packs/{sku}", put(update_pack).delete(delete_pack))
        .route("/admin/purchases", get(list_purchases))
        .route("/admin/checkouts", get(list_checkouts))
        .route("/admin/ledger", get(list_ledger))
        .route("/admin/checkouts/{id}/refund", post(refund_checkout))
        .route(
            "/admin/checkouts/{id}/force-fulfill",
            post(force_fulfill_checkout),
        )
        .route("/admin/grants/{escrow_ref}/reclaim", post(reclaim_grant))
        .route("/admin/grants/{escrow_ref}/release", post(release_grant))
        .route("/admin/reconcile", get(reconcile))
}

/// The canonical `/shop-api/admin/*` paths of the same handlers: the ledger
/// and the reconcile report sit under `/admin/credits/`.
pub fn shop_router() -> Router<SharedState> {
    Router::new()
        .route("/admin/credits/grant", post(grant_credits))
        .route("/admin/credits/revoke", post(revoke_credits))
        .route("/admin/users/{address}/block", post(block_user))
        .route("/admin/flags", get(list_flags).post(flag_wallets))
        .route("/admin/flags/{address}", delete(unflag_wallet))
        .route("/admin/packs", get(list_packs).post(create_pack))
        .route("/admin/packs/{sku}", put(update_pack).delete(delete_pack))
        .route("/admin/purchases", get(list_purchases))
        .route("/admin/checkouts", get(list_checkouts))
        .route("/admin/credits/ledger", get(list_ledger))
        .route("/admin/checkouts/{id}/refund", post(refund_checkout))
        .route(
            "/admin/checkouts/{id}/force-fulfill",
            post(force_fulfill_checkout),
        )
        .route("/admin/grants/{escrow_ref}/reclaim", post(reclaim_grant))
        .route("/admin/grants/{escrow_ref}/release", post(release_grant))
        .route("/admin/credits/reconcile", get(reconcile))
}

#[derive(Debug, Deserialize)]
pub struct GrantBody {
    address: String,
    amount: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(rename = "idempotencyKey", default)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GrantOut {
    address: String,
    applied: String,
    available: String,
    replayed: bool,
}

async fn grant_credits(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    body: Option<Json<GrantBody>>,
) -> Result<Json<GrantOut>, ApiError> {
    let Json(b) = body.ok_or_else(|| ApiError::bad_request("missing JSON body"))?;
    let address = normalize_address(&b.address)?;
    let amount = validate_positive_amount(&b.amount)?;
    let reason = validated_reason(&b.reason)?;
    let mut idempotency_key = validate_idempotency_key(&b.idempotency_key)?;
    // A recovered MANA top-up takes the claim's own key and is paid for, like the claim.
    let topup = match idempotency_key
        .as_deref()
        .and_then(super::topup::recovered_topup_key)
    {
        Some(key) => {
            idempotency_key = Some(key?);
            true
        }
        None => false,
    };
    let actor = admin.audit_actor_description();
    let detail = json!({ "address": address, "amount": amount, "reason": reason, "idempotencyKey": idempotency_key });
    let outcome = state
        .credits
        .admin_grant_credits(GrantRequest {
            address: &address,
            amount: &amount,
            kind: if topup { "purchase" } else { "grant" },
            reason: reason.as_deref(),
            actor: Some(&actor),
            idempotency_key: idempotency_key.as_deref(),
            detail: &detail,
        })
        .await?;
    Ok(Json(GrantOut {
        address,
        applied: outcome.applied,
        available: outcome.available,
        replayed: outcome.replayed,
    }))
}

async fn revoke_credits(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    body: Option<Json<GrantBody>>,
) -> Result<Json<GrantOut>, ApiError> {
    let Json(b) = body.ok_or_else(|| ApiError::bad_request("missing JSON body"))?;
    let address = normalize_address(&b.address)?;
    let amount = validate_positive_amount(&b.amount)?;
    let reason = validated_reason(&b.reason)?;
    let actor = admin.audit_actor_description();
    let detail = json!({ "address": address, "amount": amount, "reason": reason });
    let outcome = state
        .credits
        .admin_revoke_credits(&address, &amount, reason.as_deref(), Some(&actor), &detail)
        .await?;
    Ok(Json(GrantOut {
        address,
        applied: outcome.applied,
        available: outcome.available,
        replayed: outcome.replayed,
    }))
}

#[derive(Debug, Deserialize)]
pub struct BlockBody {
    blocked: bool,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BlockOut {
    address: String,
    blocked: bool,
}

async fn block_user(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(address): Path<String>,
    body: Option<Json<BlockBody>>,
) -> Result<Json<BlockOut>, ApiError> {
    let address = normalize_address(&address)?;
    let Json(b) = body.ok_or_else(|| ApiError::bad_request("missing JSON body { blocked }"))?;
    let reason = validated_reason(&b.reason)?;
    let actor = admin.audit_actor_description();
    let detail = json!({ "address": address, "blocked": b.blocked, "reason": reason });
    let blocked = state
        .credits
        .admin_set_blocked(
            &address,
            b.blocked,
            reason.as_deref(),
            Some(&actor),
            &detail,
        )
        .await?;
    Ok(Json(BlockOut { address, blocked }))
}

#[derive(Debug, Serialize)]
pub struct FlagOut {
    flagged: Vec<String>,
}

/// `POST /admin/flags`: an operator's flag by hand, the body the
/// `credits.wallet_flags` notification carries (`FlagBody`). Every address
/// of the body is flagged, the body being each row's linked set.
async fn flag_wallets(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    body: Option<Json<FlagBody>>,
) -> Result<Json<FlagOut>, ApiError> {
    let Json(body) =
        body.ok_or_else(|| ApiError::bad_request("missing JSON body { addresses, source, ip? }"))?;
    let flag = body.validate()?;
    let actor = admin.audit_actor_description();
    state
        .credits
        .admin_flag_wallets(
            &flag.addresses,
            &flag.source,
            flag.ip.as_deref(),
            &actor,
            &flag.detail(),
        )
        .await?;
    Ok(Json(FlagOut {
        flagged: flag.addresses,
    }))
}

#[derive(Debug, Serialize)]
pub struct UnflagOut {
    address: String,
    deleted: bool,
}

async fn unflag_wallet(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(address): Path<String>,
) -> Result<Json<UnflagOut>, ApiError> {
    let address = normalize_address(&address)?;
    let actor = admin.audit_actor_description();
    let detail = json!({ "address": address });
    let deleted = state
        .credits
        .admin_unflag_wallet(&address, &actor, &detail)
        .await?;
    Ok(Json(UnflagOut { address, deleted }))
}

#[derive(Debug, Deserialize)]
pub struct FlagListQuery {
    address: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct FlaggedOut {
    address: String,
    source: String,
    ip: Option<String>,
    #[serde(rename = "linkedAddresses")]
    linked_addresses: Vec<String>,
    #[serde(rename = "flaggedAt")]
    flagged_at: String,
}

async fn list_flags(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Query(q): Query<FlagListQuery>,
) -> Result<Json<Vec<FlaggedOut>>, ApiError> {
    let address = q.address.as_deref().map(normalize_address).transpose()?;
    let (limit, offset) = paginate(q.limit, q.offset);
    let rows = state
        .credits
        .admin_list_flags(address.as_deref(), limit, offset)
        .await?;
    let out = rows
        .into_iter()
        .map(|f| FlaggedOut {
            address: f.address,
            source: f.source,
            ip: f.ip,
            linked_addresses: f.linked_addresses,
            flagged_at: rfc3339(f.flagged_at),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct PurchaseListQuery {
    status: Option<String>,
    address: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PurchaseOut {
    id: i64,
    address: String,
    sku: String,
    credits: String,
    #[serde(rename = "amountCents")]
    amount_cents: i64,
    currency: String,
    #[serde(rename = "stripePaymentIntent")]
    stripe_payment_intent: Option<String>,
    method: String,
    status: String,
    #[serde(rename = "createdAt")]
    created_at: String,
}

fn rfc3339(t: chrono::NaiveDateTime) -> String {
    t.and_utc().to_rfc3339()
}

async fn list_purchases(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Query(q): Query<PurchaseListQuery>,
) -> Result<Json<Vec<PurchaseOut>>, ApiError> {
    let address = match q.address.as_deref() {
        Some(a) => Some(normalize_address(a)?),
        None => None,
    };
    let (limit, offset) = paginate(q.limit, q.offset);
    let rows = state
        .credits
        .admin_list_purchases(q.status.as_deref(), address.as_deref(), limit, offset)
        .await?;
    let out = rows
        .into_iter()
        .map(|p| PurchaseOut {
            id: p.id,
            address: p.address,
            sku: p.sku,
            credits: p.credits,
            amount_cents: p.amount_cents,
            currency: p.currency,
            stripe_payment_intent: p.stripe_payment_intent,
            method: p.method,
            status: p.status,
            created_at: rfc3339(p.created_at),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct CheckoutListQuery {
    address: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
struct OutboxLineOut {
    id: i64,
    #[serde(rename = "itemId")]
    item_id: String,
    urn: String,
    #[serde(rename = "tokenId")]
    token_id: Option<String>,
    #[serde(rename = "unitPriceCredits")]
    unit_price_credits: String,
    mode: String,
    status: String,
    attempts: i32,
    #[serde(rename = "lastError")]
    last_error: Option<String>,
    #[serde(rename = "externalRef")]
    external_ref: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckoutOut {
    id: i64,
    address: String,
    #[serde(rename = "totalCredits")]
    total_credits: String,
    status: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    lines: Vec<OutboxLineOut>,
}

async fn list_checkouts(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Query(q): Query<CheckoutListQuery>,
) -> Result<Json<Vec<CheckoutOut>>, ApiError> {
    let address = match q.address.as_deref() {
        Some(a) => Some(normalize_address(a)?),
        None => None,
    };
    let (limit, offset) = paginate(q.limit, q.offset);
    let rows = state
        .credits
        .admin_list_checkouts(address.as_deref(), q.status.as_deref(), limit, offset)
        .await?;
    let out = rows
        .into_iter()
        .map(|c| CheckoutOut {
            id: c.id,
            address: c.address,
            total_credits: c.total_credits,
            status: c.status,
            created_at: rfc3339(c.created_at),
            lines: c
                .lines
                .into_iter()
                .map(|l| OutboxLineOut {
                    id: l.id,
                    item_id: l.item_id,
                    urn: l.urn,
                    token_id: l.token_id,
                    unit_price_credits: l.unit_price_credits,
                    mode: l.mode,
                    status: l.status,
                    attempts: l.attempts,
                    last_error: l.last_error,
                    external_ref: l.external_ref,
                })
                .collect(),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct LedgerListQuery {
    address: String,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct LedgerOut {
    id: i64,
    address: String,
    kind: String,
    amount: String,
    #[serde(rename = "txRef")]
    tx_ref: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: String,
}

async fn list_ledger(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Query(q): Query<LedgerListQuery>,
) -> Result<Json<Vec<LedgerOut>>, ApiError> {
    let address = normalize_address(&q.address)?;
    let (limit, offset) = paginate(q.limit, q.offset);
    let rows = state
        .credits
        .admin_list_ledger(&address, limit, offset)
        .await?;
    let out = rows
        .into_iter()
        .map(|e| LedgerOut {
            id: e.id,
            address: e.address,
            kind: e.kind,
            amount: e.amount,
            tx_ref: e.tx_ref,
            created_at: rfc3339(e.created_at),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize, Default)]
pub struct ManualOpBody {
    #[serde(default)]
    reason: Option<String>,
}

async fn refund_checkout(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    body: Option<Json<ManualOpBody>>,
) -> Result<Json<JsonValue>, ApiError> {
    let reason = validated_reason(&body.map(|b| b.0.reason).unwrap_or(None))?;
    let actor = admin.audit_actor_description();

    let checkout = state
        .credits
        .get_checkout(id)
        .await?
        .ok_or_else(|| ApiError::not_found("checkout not found"))?;

    if checkout.status != "fulfilling" && checkout.status != "fulfilled" {
        return Err(ApiError::conflict(
            "checkout is not refundable (only a debited, not-yet-reversed checkout in 'fulfilling'/'fulfilled' can be manually refunded)",
        ));
    }

    tracing::info!(
        action = "checkout.refund",
        checkout_id = id,
        "admin manual refund"
    );
    let (outcome, _closed) = state
        .credits
        .refund_checkout_manual_audited(
            id,
            &checkout.address,
            &checkout.total_credits,
            &actor,
            |outcome| {
                json!({
                    "checkoutId": id, "address": checkout.address,
                    "requested": checkout.total_credits, "applied": outcome.applied,
                    "replayed": outcome.replayed, "reason": reason,
                })
            },
        )
        .await?;

    Ok(Json(json!({
        "checkoutId": id,
        "address": checkout.address,
        "refunded": outcome.applied,
        "available": outcome.available,
        "replayed": outcome.replayed,
    })))
}

async fn force_fulfill_checkout(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    body: Option<Json<ManualOpBody>>,
) -> Result<Json<JsonValue>, ApiError> {
    let reason = validated_reason(&body.map(|b| b.0.reason).unwrap_or(None))?;
    let actor = admin.audit_actor_description();

    tracing::info!(
        action = "checkout.force_fulfill",
        checkout_id = id,
        "admin force-fulfill"
    );
    let rearmed = state.credits.admin_force_fulfill(id).await?;

    let detail = json!({ "checkoutId": id, "rearmedLines": rearmed, "reason": reason });
    state
        .credits
        .admin_audit_op(
            "checkout.force_fulfill",
            Some(&id.to_string()),
            &actor,
            &detail,
        )
        .await?;

    Ok(Json(json!({ "checkoutId": id, "rearmedLines": rearmed })))
}

async fn reclaim_grant(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(escrow_ref): Path<String>,
    body: Option<Json<ManualOpBody>>,
) -> Result<Json<JsonValue>, ApiError> {
    let escrow_ref = validate_escrow_ref(&escrow_ref)?;
    let reason = validated_reason(&body.map(|b| b.0.reason).unwrap_or(None))?;
    let actor = admin.audit_actor_description();

    let grant = state
        .credits
        .fetch_usage_grant(&escrow_ref)
        .await?
        .ok_or_else(|| ApiError::not_found("usage_grant not found"))?;

    if grant.status != "active" && grant.status != "revoked" {
        return Err(ApiError::conflict(
            "usage_grant is not reclaimable (must be active or a resumable revoked)",
        ));
    }
    let (Some(collection), Some(token_id)) =
        (grant.collection.as_deref(), grant.token_id.as_deref())
    else {
        return Err(ApiError::bad_request(
            "grant has no on-chain token_id/collection; cannot reclaim (primary mint pending)",
        ));
    };

    let idem = format!("admin:reclaim:{}", escrow_ref);
    tracing::info!(action = "grant.reclaim", escrow_ref = %escrow_ref, "admin reclaim");
    let tx_hash = state.broker.reclaim(collection, token_id, &idem).await?;

    state.credits.revoke_usage_grant(&escrow_ref).await?;

    let refunded = match state
        .credits
        .find_confirmed_line_by_ref(&escrow_ref)
        .await?
    {
        Some((checkout_id, address, amount)) => {
            let tx_ref = format!("checkout:{}", checkout_id);
            let outcome = state
                .credits
                .refund(&address, &amount, &tx_ref, Some(&idem))
                .await?;
            Some((address, amount, outcome.applied))
        }
        None => None,
    };

    let detail = json!({
        "escrowRef": escrow_ref, "grantee": grant.grantee_address, "urn": grant.urn,
        "txHash": tx_hash,
        "refundRequested": refunded.as_ref().map(|(_, req, _)| req.clone()),
        "refunded": refunded.as_ref().map(|(_, _, app)| app.clone()),
        "reason": reason,
    });
    state
        .credits
        .admin_audit_op("grant.reclaim", Some(&escrow_ref), &actor, &detail)
        .await?;

    Ok(Json(json!({
        "escrowRef": escrow_ref,
        "txHash": tx_hash,
        "refunded": refunded.map(|(addr, _, app)| json!({ "address": addr, "amount": app })),
    })))
}

async fn release_grant(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(escrow_ref): Path<String>,
    body: Option<Json<ManualOpBody>>,
) -> Result<Json<JsonValue>, ApiError> {
    let escrow_ref = validate_escrow_ref(&escrow_ref)?;
    let reason = validated_reason(&body.map(|b| b.0.reason).unwrap_or(None))?;
    let actor = admin.audit_actor_description();

    let grant = state
        .credits
        .fetch_usage_grant(&escrow_ref)
        .await?
        .ok_or_else(|| ApiError::not_found("usage_grant not found"))?;

    if grant.status != "active" {
        return Err(ApiError::conflict(
            "usage_grant is not releasable (must be active)",
        ));
    }
    if grant.unlock_at > crate::credits::store::now() {
        return Err(ApiError::conflict(format!(
            "usage_grant is still in the return window; release is not allowed until unlock_at ({}). Use reclaim to return during the window.",
            rfc3339(grant.unlock_at)
        )));
    }
    let (Some(collection), Some(token_id)) =
        (grant.collection.as_deref(), grant.token_id.as_deref())
    else {
        return Err(ApiError::bad_request(
            "grant has no on-chain token_id/collection; cannot release (primary mint pending)",
        ));
    };

    let idem = format!("admin:release:{}", escrow_ref);
    tracing::info!(action = "grant.release", escrow_ref = %escrow_ref, "admin release");
    let tx_hash = state
        .broker
        .release(collection, token_id, &grant.grantee_address, &idem)
        .await?;
    state.credits.mark_usage_grant_released(&escrow_ref).await?;

    let detail = json!({ "escrowRef": escrow_ref, "grantee": grant.grantee_address, "urn": grant.urn, "txHash": tx_hash, "reason": reason });
    state
        .credits
        .admin_audit_op("grant.release", Some(&escrow_ref), &actor, &detail)
        .await?;

    Ok(Json(json!({ "escrowRef": escrow_ref, "txHash": tx_hash })))
}

async fn reconcile(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
) -> Result<Json<ReconcileReport>, ApiError> {
    tracing::info!(action = "reconcile", "admin reconciliation run");
    let report = state.reconcile_report().await?;
    Ok(Json(report))
}

#[derive(Debug, Serialize)]
pub struct PackOut {
    sku: String,
    title: String,
    credits: String,
    #[serde(rename = "priceCents")]
    price_cents: i64,
    currency: String,
    active: bool,
    #[serde(rename = "sortOrder")]
    sort_order: i32,
}

impl From<PackAdminRow> for PackOut {
    fn from(p: PackAdminRow) -> Self {
        PackOut {
            sku: p.sku,
            title: p.title,
            credits: p.credits,
            price_cents: p.price_cents,
            currency: p.currency,
            active: p.active,
            sort_order: p.sort_order,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PackCreateBody {
    sku: String,
    title: String,
    credits: String,
    #[serde(rename = "priceCents")]
    price_cents: i64,
    currency: String,
    #[serde(default = "default_true")]
    active: bool,
    #[serde(rename = "sortOrder", default)]
    sort_order: i32,
}

#[derive(Debug, Deserialize)]
pub struct PackUpdateBody {
    title: String,
    credits: String,
    #[serde(rename = "priceCents")]
    price_cents: i64,
    currency: String,
    #[serde(default = "default_true")]
    active: bool,
    #[serde(rename = "sortOrder", default)]
    sort_order: i32,
}

fn default_true() -> bool {
    true
}

fn validate_pack_title(raw: &str) -> Result<(), ApiError> {
    if raw.trim().is_empty() || raw.len() > 200 {
        return Err(ApiError::bad_request("title must be 1..200 chars"));
    }
    Ok(())
}

async fn list_packs(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
) -> Result<Json<Vec<PackOut>>, ApiError> {
    let rows = state.credits.admin_list_packs().await?;
    Ok(Json(rows.into_iter().map(PackOut::from).collect()))
}

async fn create_pack(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    body: Option<Json<PackCreateBody>>,
) -> Result<(StatusCode, Json<PackOut>), ApiError> {
    let Json(b) = body.ok_or_else(|| ApiError::bad_request("missing JSON body"))?;
    let sku = validate_sku(&b.sku)?;
    validate_pack_title(&b.title)?;
    let credits = validate_positive_amount(&b.credits)?;
    let price_cents = validate_price_cents(b.price_cents)?;
    let currency = validate_currency(&b.currency)?;
    let detail = json!({ "sku": sku, "credits": credits, "priceCents": price_cents, "currency": currency, "active": b.active, "sortOrder": b.sort_order });
    tracing::info!(action = "pack.create", sku = %sku, "admin pack create");
    let pack = state
        .credits
        .admin_create_pack(
            PackSpec {
                sku: &sku,
                title: &b.title,
                credits: &credits,
                price_cents,
                currency: &currency,
                active: b.active,
                sort_order: b.sort_order,
            },
            &admin.audit_actor_description(),
            &detail,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(pack.into())))
}

async fn update_pack(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(sku): Path<String>,
    body: Option<Json<PackUpdateBody>>,
) -> Result<Json<PackOut>, ApiError> {
    let sku = validate_sku(&sku)?;
    let Json(b) = body.ok_or_else(|| ApiError::bad_request("missing JSON body"))?;
    validate_pack_title(&b.title)?;
    let credits = validate_positive_amount(&b.credits)?;
    let price_cents = validate_price_cents(b.price_cents)?;
    let currency = validate_currency(&b.currency)?;
    let detail = json!({ "sku": sku, "credits": credits, "priceCents": price_cents, "currency": currency, "active": b.active, "sortOrder": b.sort_order });
    tracing::info!(action = "pack.update", sku = %sku, "admin pack update");
    let pack = state
        .credits
        .admin_update_pack(
            PackSpec {
                sku: &sku,
                title: &b.title,
                credits: &credits,
                price_cents,
                currency: &currency,
                active: b.active,
                sort_order: b.sort_order,
            },
            &admin.audit_actor_description(),
            &detail,
        )
        .await?;
    Ok(Json(pack.into()))
}

async fn delete_pack(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(sku): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sku = validate_sku(&sku)?;
    let detail = json!({ "sku": sku });
    tracing::info!(action = "pack.delete", sku = %sku, "admin pack delete");
    state
        .credits
        .admin_delete_pack(&sku, &admin.audit_actor_description(), &detail)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
