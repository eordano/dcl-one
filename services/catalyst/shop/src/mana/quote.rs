// from: shop@94584a5c:app/src/lib/{payment-options,mana,buy}.ts (what a purchase presentation reads: the quote per method, the wallet's MANA and allowance, the gas), SHOP.md "For a web2 buyer on a MANA-only feature"
//! `POST /v1/purchase-offers`: the quote of every cart line, authoritative
//! end to end. Each line is resolved to its record ([`facts`]), judged by
//! THE rule ([`policy::eligibility`]), priced per offered method
//! ([`policy::quote`], credits and MANA, at the node's MANA/USD spot), and,
//! where MANA is offered, given its execution block: the chain and venue,
//! the exact MANA required, the allowance and the calldata, a gas estimate
//! and, for a known wallet, its balances and shortfalls; a web2 buyer's
//! guidance comes from the rule (`WalletGuidance`) and this block together.
//! A method the rule offers but the node cannot price is reported in
//! `unavailable` with the missing fact, never dropped (the UI keeps the
//! option on screen, unavailable, with a retry).

use std::sync::Arc;

use alloy_primitives::{Address, U256};
use axum::Json;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde_json::{Value, json};

use super::chain::{ReadError, Reader, calldata_hex};
use super::facts::{self, Listing, Resolved};
use super::venues::Chain;
use super::wire::{
    AllowanceNeed, ApprovalNeed, Balances, CallDto, Check, Freshness, GasEstimate, LineError,
    MAX_LINES, ManaExecution, OfferAnswer, OffersRequest, OffersResponse, PAYMENT_RULE_REFUSED,
    PurchaseOffer, SettlementFor, Unavailable, missing_source, refusal_json,
};
use super::{SharedState, calls, verify};
use crate::policy::{Authoritative, CartLine, Quote, Refusal, eligibility, partition, quote};
use crate::types::{
    AssetKind, EvmAddress, ManaUsdRate, ManaWei, PaymentMethod, SaleKind, Session,
    SettlementGroupKey,
};

pub const ROUTE: &str = "/v1/purchase-offers";

/// The static gas budgets of the calls, used when the RPC estimate cannot
/// run (no wallet known, or the call would revert as it stands: an
/// allowance not yet given): generous upper bounds of the observed costs.
pub fn static_gas(listing: &Listing) -> u64 {
    match listing {
        Listing::Store { .. } => 300_000,
        Listing::Trade { .. } => 350_000,
        Listing::LegacyOrder { .. } => 250_000,
        Listing::Rental { .. } => 400_000,
        Listing::Name { .. } => 300_000,
    }
}

/// The rates a purchase offer prices its lines at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuoteRates {
    /// For showing a price: the credits quote of a line not priced in USD.
    pub display: Option<ManaUsdRate>,
    /// For the MANA quote of a line priced in USD, which the marketplace
    /// converts at its feed in the block that settles it.
    pub purchase: Option<ManaUsdRate>,
}

impl QuoteRates {
    /// The rate `line` is quoted at: a line priced in USD is converted to MANA
    /// at the purchase rate (its credits quote needs none); any other line
    /// needs a rate only for its credits quote, which the display rate
    /// gives, so the purchase cut never takes a fixed-MANA line's credits
    /// price away.
    pub fn for_line(&self, line: &Authoritative) -> Option<&ManaUsdRate> {
        if crate::policy::prices_in_usd(line) {
            self.purchase.as_ref()
        } else {
            self.display.as_ref()
        }
    }
}

/// The Shop's MANA/USD as the rule's rates, exact, never a float: the
/// marketplace's feed where it is wired (`feed_rate::current` to show a
/// price, `feed_rate::current_for_purchase` for a line priced in USD; no
/// current answer is no rate, and that line says its price is unavailable),
/// else the newest price snapshot within its freshness budget for both.
pub async fn mana_usd_rates(pool: &sqlx::PgPool) -> Result<QuoteRates, sqlx::Error> {
    if let Some((display, purchase)) = super::feed_rate::current_rates() {
        return Ok(QuoteRates {
            display: display.ok().map(|shop| shop.rate),
            purchase: purchase.ok().map(|shop| shop.rate),
        });
    }
    let spot = mana_usd_spot(pool).await?;
    Ok(QuoteRates {
        display: spot,
        purchase: spot,
    })
}

/// The newest price snapshot's exact MANA/USD text (trimmed) and when it
/// was quoted in unix seconds: the provider's quote time, or the poll's when
/// that is earlier (`PriceSnapshot::age_at`). No freshness budget is
/// applied here; readers that need a live spot ask [`spot_is_fresh`].
/// `price_snapshots_newest_idx`.
pub(crate) async fn newest_spot(pool: &sqlx::PgPool) -> Result<Option<(String, i64)>, sqlx::Error> {
    let row: Option<(String, chrono::NaiveDateTime)> = sqlx::query_as(
        "SELECT mana_usd::text, LEAST(taken_at, COALESCE(source_updated_at, taken_at)) FROM economy.price_snapshots ORDER BY taken_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(rate, quoted_at)| (rate.trim().to_string(), quoted_at.and_utc().timestamp())))
}

/// Whether a spot quoted at `quoted_at` is still a price at `now` (both
/// unix seconds): within `price::store::SNAPSHOT_MAX_AGE`, the budget
/// `PriceSnapshot::is_fresh_at` applies.
pub(crate) fn spot_is_fresh(quoted_at: i64, now: i64) -> bool {
    now - quoted_at <= crate::price::store::SNAPSHOT_MAX_AGE.as_secs() as i64
}

/// The newest price snapshot within its freshness budget.
async fn mana_usd_spot(pool: &sqlx::PgPool) -> Result<Option<ManaUsdRate>, sqlx::Error> {
    Ok(newest_spot(pool).await?.and_then(|(rate, quoted_at)| {
        spot_is_fresh(quoted_at, Utc::now().timestamp())
            .then(|| ManaUsdRate::parse_usd_per_mana(&rate))
            .flatten()
    }))
}

/// The session the rule reads: a signed-fetch identity is an account; it
/// is a wallet only when the signed metadata says so (`walletKind:
/// "web3"`, the UI's own session fact, signed by the identity that made
/// it). Nothing else in a request can widen the choices.
pub fn session_of(signer: Option<&str>, metadata: Option<&str>) -> Session {
    let Some(signer) = signer.and_then(|s| EvmAddress::parse(&s.to_ascii_lowercase()).ok()) else {
        return Session::SignedOut;
    };
    let web3 = metadata
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .and_then(|m| {
            m.get("walletKind")
                .and_then(|k| k.as_str())
                .map(|k| k == "web3")
        })
        .unwrap_or(false);
    if web3 {
        Session::Web3 { wallet: signer }
    } else {
        Session::Web2 { account: signer }
    }
}

fn line_error(index: usize, e: facts::ResolveError) -> LineError {
    match e {
        facts::ResolveError::IndexNotReady { index: unpublished } => LineError {
            index,
            error: "Service Unavailable",
            message: e.to_string(),
            refusal: None,
            source: Some(unpublished),
        },
        facts::ResolveError::Db(_) => LineError {
            index,
            error: "Internal Server Error",
            message: "the listing could not be read".into(),
            refusal: None,
            source: None,
        },
        other => LineError {
            index,
            error: "Bad Request",
            message: other.to_string(),
            refusal: None,
            source: None,
        },
    }
}

fn refused(index: usize, refusal: Refusal) -> LineError {
    let source = match &refusal {
        Refusal::Incomplete { missing } => Some(missing_source(*missing)),
        _ => None,
    };
    LineError {
        index,
        error: if source.is_some() {
            "Service Unavailable"
        } else {
            PAYMENT_RULE_REFUSED
        },
        message: refusal.to_string(),
        refusal: Some(refusal_json(&refusal)),
        source,
    }
}

/// The settlement key a line joins when paid with `method`.
fn settlement_key(
    line: &Authoritative,
    method: PaymentMethod,
    rate: Option<&ManaUsdRate>,
) -> Option<SettlementGroupKey> {
    partition(
        &[CartLine {
            line: line.clone(),
            method,
        }],
        rate,
    )
    .ok()
    .and_then(|groups| groups.first().map(|g| g.key))
}

/// The MANA execution block of a line the rule offers MANA for.
async fn mana_execution(
    resolved: &Resolved,
    amount: ManaWei,
    wallet: Option<Address>,
    reader: Option<&Reader>,
    mut checks: Vec<Check>,
    fingerprint: Option<alloy_primitives::B256>,
) -> ManaExecution {
    let chain: &Chain = &resolved.chain;
    let caller = wallet.unwrap_or(Address::ZERO);
    // A USD-pegged trade settles at the feed's answer in the block that takes it,
    // which can move after this quote: its approval carries upstream's +2%.
    let usd_pegged = crate::policy::priced_in_usd(&resolved.offer);
    let approve = if usd_pegged {
        super::feed_rate::usd_pegged_allowance(amount.get())
    } else {
        amount.get()
    };
    let call = match &resolved.listing {
        Listing::Store { venue, price } => match (resolved.asset.token, wallet) {
            (crate::types::TokenRef::Item { item_id }, Some(buyer)) => {
                let call = crate::checkout::calls::build_store_buy(*venue, resolved.asset.contract.address(), item_id, price.get(), buyer);
                Ok(calls::Call { to: call.to, data: call.data, allowance: Some((chain.mana, *venue, price.get())), approval: None })
            }
            _ => Err("a mint names its beneficiary: sign in for the unit calldata".to_string()),
        },
        Listing::Trade { trade, venue, coupon: Some(coupon), .. } => {
            calls::accept_with_coupon(chain, *venue, trade, coupon, caller, Some(approve)).map_err(|e| e.to_string())
        }
        Listing::Trade { trade, venue, coupon: None, .. } => calls::accept(chain, *venue, trade, caller, Some(approve)).map_err(|e| e.to_string()),
        Listing::LegacyOrder { order, .. } => match resolved.asset.token {
            crate::types::TokenRef::Token { token_id } => calls::execute_order(chain, resolved.asset.contract.address(), token_id, order.price, fingerprint).map_err(|e| e.to_string()),
            _ => Err("a legacy order sells a token".to_string()),
        },
        Listing::Rental { listing, terms, rentals, .. } => match wallet {
            Some(tenant) => crate::rentals::calls::accept_listing_calldata(listing, tenant, terms, fingerprint.unwrap_or_default())
                .map(|data| calls::Call { to: *rentals, data, allowance: Some((chain.mana, *rentals, amount.get())), approval: None })
                .map_err(|e| e.to_string()),
            None => Err("a rent names its tenant as the operator: sign in with the renting wallet for the calldata".to_string()),
        },
        Listing::Name { label, .. } => match wallet {
            Some(beneficiary) => calls::register_name(chain, label, beneficiary, amount.get()).map_err(|e| e.to_string()),
            None => Err("a registration names its beneficiary: sign in with the registering wallet for the calldata".to_string()),
        },
    };
    let (call, venue) = match call {
        Ok(call) => {
            let venue = call.to;
            (Some(call), venue)
        }
        Err(e) => {
            checks.push(Check::fail("calldata", e));
            (None, resolved.offer_venue().unwrap_or(Address::ZERO))
        }
    };
    let allowance = call
        .as_ref()
        .and_then(|c| c.allowance)
        .map(|(token, spender, amount)| AllowanceNeed {
            token: EvmAddress::new(token),
            spender: EvmAddress::new(spender),
            amount: ManaWei::new(amount),
        });
    let approval = call
        .as_ref()
        .and_then(|c| c.approval)
        .map(|(contract, token_id)| ApprovalNeed {
            contract: EvmAddress::new(contract),
            token_id: token_id.to_string(),
            operator: EvmAddress::new(venue),
        });
    let mut gas = GasEstimate {
        estimate: Some(static_gas(&resolved.listing)),
        source: "static",
        gas_price_wei: None,
        fee_wei: None,
        detail: None,
    };
    let mut balances = None;
    if let Some(reader) = reader {
        match reader.gas_price().await {
            Ok(price) => {
                gas.gas_price_wei = Some(price.to_string());
            }
            Err(e) => gas.detail = Some(format!("gas price: {e}")),
        }
        if let (Some(wallet), Some(call)) = (wallet, call.as_ref()) {
            match reader.estimate_gas(wallet, call.to, &call.data).await {
                Ok(estimate) => {
                    gas.estimate = Some(estimate);
                    gas.source = "rpc";
                }
                Err(ReadError::Revert(why)) => {
                    gas.detail = Some(format!(
                        "the call would revert as it stands (an allowance or balance short?): {why}"
                    ))
                }
                Err(e) => gas.detail = Some(e.to_string()),
            }
            let mana = reader.erc20_balance(chain.mana, wallet).await;
            let native = reader.native_balance(wallet).await;
            let allowed = reader.erc20_allowance(chain.mana, wallet, venue).await;
            if let (Ok(mana), Ok(native), Ok(allowed)) = (mana, native, allowed) {
                let fee = gas
                    .gas_price_wei
                    .as_deref()
                    .and_then(|p| U256::from_str_radix(p, 10).ok())
                    .and_then(|p| p.checked_mul(U256::from(gas.estimate.unwrap_or(0))))
                    .unwrap_or(U256::ZERO);
                balances = Some(Balances {
                    wallet: EvmAddress::new(wallet),
                    mana: ManaWei::new(mana),
                    native: native.to_string(),
                    allowance: ManaWei::new(allowed),
                    mana_shortfall: ManaWei::new(amount.get().saturating_sub(mana)),
                    native_shortfall: if chain.chain_id.native_gas_required() {
                        fee.saturating_sub(native).to_string()
                    } else {
                        "0".to_string()
                    },
                });
            } else {
                gas.detail
                    .get_or_insert_with(|| "the wallet's balances could not be read".to_string());
            }
        }
        if let Some(price) = gas
            .gas_price_wei
            .as_deref()
            .and_then(|p| U256::from_str_radix(p, 10).ok())
        {
            gas.fee_wei = gas.estimate.map(|g| (price * U256::from(g)).to_string());
        }
    } else {
        gas.detail = Some("no JSON-RPC endpoint is configured for the chain".to_string());
    }
    let executable = verify::executable(&checks);
    ManaExecution {
        chain_id: chain.chain_id,
        venue: EvmAddress::new(venue),
        token: EvmAddress::new(chain.mana),
        amount,
        native_gas_required: chain.chain_id.native_gas_required(),
        allowance,
        approval,
        call: call.map(|c| CallDto {
            to: EvmAddress::new(c.to),
            data: calldata_hex(&c.data),
        }),
        gas,
        balances,
        checks,
        executable,
        freshness: Freshness {
            source: resolved.listing.source(),
            ready: resolved.listing.index_ready(),
            verdict: resolved.listing.verdict(),
        },
        fingerprint: fingerprint.map(|f| format!("{f:#x}")),
    }
}

impl Resolved {
    fn offer_venue(&self) -> Option<Address> {
        match &self.listing {
            Listing::Store { venue, .. } => Some(*venue),
            Listing::Trade { venue, .. } => Some(*venue),
            Listing::LegacyOrder { order, .. } => Some(order.marketplace),
            Listing::Rental { rentals, .. } => Some(*rentals),
            Listing::Name { controller, .. } => Some(*controller),
        }
    }
}

/// Whether a line may be quoted under the Marketplace rollout: a primary sale (a mint, a NAME registration) always,
/// a resale or a rental only while it is on.
pub(crate) fn marketplace_offered(sale: SaleKind, kind: AssetKind, marketplace_on: bool) -> bool {
    marketplace_on || (sale != SaleKind::Secondary && kind != AssetKind::Rental)
}

/// One line's offer, or why there is none.
pub async fn quote_line(
    state: &SharedState,
    session: Session,
    index: usize,
    line: &super::wire::LineRequest,
    rates: &QuoteRates,
    now_ms: i64,
) -> OfferAnswer {
    let resolved =
        match facts::resolve(&state.pool, &state.rentals.db, &state.venues, session, line).await {
            Ok(r) => r,
            Err(e) => return OfferAnswer::Error(line_error(index, e)),
        };
    let reader = match state.reader(resolved.chain.chain_id).await {
        Ok(reader) => reader,
        Err(refusal) => {
            tracing::warn!(error = %refusal, "refusing to quote through a mismatched chain endpoint");
            return OfferAnswer::Error(LineError {
                index,
                error: "Service Unavailable",
                message: refusal.to_string(),
                refusal: None,
                source: None,
            });
        }
    };
    let (checks, fingerprint, name_reads) = verify::checks(&resolved, reader.as_ref()).await;
    let resolved = match name_reads {
        Some((price, owner)) => facts::with_name_reads(resolved, price, owner),
        None => resolved,
    };
    let authoritative = match Authoritative::derive(resolved.facts.clone()) {
        Ok(a) => a,
        Err(refusal) => return OfferAnswer::Error(refused(index, refusal)),
    };
    // A resale (an existing token, LAND and NAMEs included) and a rental are the Marketplace's: while its rollout
    // is off they are not offered at all (`market::features`), whatever the session.
    if !marketplace_offered(
        authoritative.sale(),
        authoritative.kind(),
        state.market.marketplace_features(),
    ) {
        return OfferAnswer::Error(LineError {
            index,
            error: "Forbidden",
            message: crate::market::features::refusal(),
            refusal: None,
            source: None,
        });
    }
    let rate = rates.for_line(&authoritative);
    let elig = eligibility(&authoritative);
    let mut quotes = Vec::new();
    let mut unavailable = Vec::new();
    let mut settlement = Vec::new();
    let mut mana_amount = None;
    for method in &elig.allowed {
        match quote(&authoritative, *method, rate) {
            Ok(q) => {
                if let Quote::Mana { amount } = q {
                    mana_amount = Some(amount);
                }
                quotes.push(q);
                if let Some(key) = settlement_key(&authoritative, *method, rate) {
                    settlement.push(SettlementFor {
                        method: *method,
                        key,
                    });
                }
            }
            Err(Refusal::Incomplete { missing }) => unavailable.push(Unavailable {
                method: *method,
                missing,
            }),
            Err(refusal) => return OfferAnswer::Error(refused(index, refusal)),
        }
    }
    let wallet = match session {
        Session::Web3 { wallet } => Some(wallet.address()),
        Session::Web2 { account } => Some(account.address()),
        Session::SignedOut => None,
    };
    let mana = match mana_amount {
        Some(amount) if elig.allows(PaymentMethod::Mana) => Some(
            mana_execution(
                &resolved,
                amount,
                wallet,
                reader.as_ref(),
                checks,
                fingerprint,
            )
            .await,
        ),
        _ => None,
    };
    OfferAnswer::Offer(Box::new(PurchaseOffer {
        asset: authoritative.asset(),
        kind: authoritative.kind(),
        sale: authoritative.sale(),
        bundle: authoritative.is_bundle(),
        eligibility: elig,
        quotes,
        unavailable,
        settlement,
        available: resolved.available,
        quoted_at: now_ms,
        coupon: match &resolved.listing {
            Listing::Trade {
                coupon: Some(c), ..
            } => Some(super::wire::OfferCoupon {
                id: c.id.clone(),
                coupon_address: format!("{:#x}", c.coupon_address),
                discount_ppm: c.discount_ppm,
            }),
            _ => None,
        },
        mana,
    }))
}

pub async fn post_purchase_offers(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let request: OffersRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return super::error(
                axum::http::StatusCode::BAD_REQUEST,
                "Bad Request",
                &format!("body: {e}"),
            );
        }
    };
    if request.lines.is_empty() || request.lines.len() > MAX_LINES {
        return super::error(
            axum::http::StatusCode::BAD_REQUEST,
            "Bad Request",
            &format!("lines: between 1 and {MAX_LINES}"),
        );
    }
    let signer = match state
        .optional_signer(&headers, "post", &original, ROUTE)
        .await
    {
        Ok(s) => s,
        Err(r) => return r,
    };
    let session = session_of(
        signer.as_deref(),
        crate::shared::signed_fetch::header_str(
            &headers,
            crate::shared::signed_fetch::AUTH_METADATA_HEADER,
        ),
    );
    let rates = match mana_usd_rates(&state.pool).await {
        Ok(rates) => rates,
        Err(e) => {
            tracing::error!(error = %e, "price_snapshots read failed");
            return super::error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                "the MANA spot could not be read",
            );
        }
    };
    let now_ms = Utc::now().timestamp_millis();
    let mut offers = Vec::with_capacity(request.lines.len());
    for (index, line) in request.lines.iter().enumerate() {
        offers.push(quote_line(&state, session, index, line, &rates, now_ms).await);
    }
    Json(json!(OffersResponse {
        quoted_at: now_ms,
        offers
    }))
    .into_response()
}

/// The states a route needs, shared with the actions and executions.
pub type Shared = Arc<super::State>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spot_is_fresh_through_the_snapshot_budget() {
        let budget = crate::price::store::SNAPSHOT_MAX_AGE.as_secs() as i64;
        assert!(spot_is_fresh(1_000, 1_000 + budget));
        assert!(!spot_is_fresh(1_000, 1_001 + budget));
        assert!(spot_is_fresh(1_000, 900), "a quote stamped ahead of now");
    }

    #[test]
    fn the_marketplace_rollout_gates_resales_and_rentals_only() {
        use crate::types::{AssetKind, SaleKind};
        assert!(marketplace_offered(
            SaleKind::PrimaryMint,
            AssetKind::Wearable,
            false
        ));
        assert!(marketplace_offered(
            SaleKind::PrimaryNameRegistration,
            AssetKind::Name,
            false
        ));
        assert!(!marketplace_offered(
            SaleKind::Secondary,
            AssetKind::Wearable,
            false
        ));
        assert!(!marketplace_offered(
            SaleKind::Secondary,
            AssetKind::Parcel,
            false
        ));
        assert!(!marketplace_offered(
            SaleKind::PrimaryMint,
            AssetKind::Rental,
            false
        ));
        assert!(marketplace_offered(
            SaleKind::Secondary,
            AssetKind::Parcel,
            true
        ));
        assert!(marketplace_offered(
            SaleKind::PrimaryMint,
            AssetKind::Rental,
            true
        ));
    }

    #[test]
    fn a_signed_identity_is_an_account_unless_its_metadata_names_a_wallet() {
        let a = "0x00000000000000000000000000000000000000a1";
        assert_eq!(session_of(None, None), Session::SignedOut);
        assert_eq!(
            session_of(Some(a), None),
            Session::Web2 {
                account: EvmAddress::parse(a).unwrap()
            }
        );
        assert_eq!(
            session_of(Some(a), Some(r#"{"walletKind":"web2"}"#)),
            Session::Web2 {
                account: EvmAddress::parse(a).unwrap()
            }
        );
        assert_eq!(
            session_of(Some(a), Some(r#"{"walletKind":"web3"}"#)),
            Session::Web3 {
                wallet: EvmAddress::parse(a).unwrap()
            }
        );
        assert_eq!(
            session_of(Some("not an address"), Some(r#"{"walletKind":"web3"}"#)),
            Session::SignedOut
        );
    }

    #[test]
    fn the_static_budgets_cover_the_call_shapes() {
        assert!(
            static_gas(&Listing::Name {
                label: crate::types::NameLabel::parse("ab").unwrap(),
                registrar: Address::ZERO,
                controller: Address::ZERO
            }) >= 200_000
        );
    }

    /// A creator's primary listing with a live coupon is quoted, keyed and
    /// encoded with that coupon end to end: the resolved offer carries it, the
    /// MANA quote is the contract's discounted amount, the settlement key is
    /// `AcceptWithCoupon`, and the API's calldata is `acceptWithCoupon` with
    /// that coupon. Without the coupon in `resolve_trade`, every part fails.
    #[tokio::test]
    async fn a_primary_listing_with_a_live_coupon_settles_through_accept_with_coupon() {
        use alloy_sol_types::{SolCall, SolValue};
        // Its own database: other tests delete and reset the mirrors'
        // completeness rows on the shared one while this runs.
        let Some(db) = crate::test_db::OwnDatabase::create("coupon_quote").await else {
            return;
        };
        let pool = db.pool.clone();
        let fresh = || -> Address {
            format!(
                "0x{}{}",
                &uuid::Uuid::new_v4().simple().to_string()[..32],
                "00000000"
            )
            .parse()
            .unwrap()
        };
        let creator = fresh();
        let buyer = fresh();
        let collection = fresh();
        let book = super::super::venues::Book::production(|_| None);
        let chain = book.chain(crate::types::ChainId::POLYGON).unwrap().clone();
        let venue = chain.latest_marketplace().unwrap().address;
        let price = U256::from(10u64).pow(U256::from(18u64));
        let trade = crate::market::listings::test_support::parse(&format!(
            r#"{{"signer":"{creator:#x}","signature":"0x{sig}","type":"public_item_order","network":"MATIC","chainId":137,
              "checks":{{"uses":10,"expiration":4102444800000,"effective":0,"salt":"0x{salt}","contractSignatureIndex":0,"signerSignatureIndex":0,"allowedRoot":"0x","externalChecks":[]}},
              "sent":[{{"assetType":4,"contractAddress":"{collection:#x}","itemId":"0","extra":"0x"}}],
              "received":[{{"assetType":2,"contractAddress":"{mana:#x}","amount":"{price}","extra":"0x","beneficiary":"{creator:#x}"}}]}}"#,
            sig = "11".repeat(65),
            salt = uuid::Uuid::new_v4().simple(),
            mana = chain.mana,
        ));
        // The collection and its item as the mirrors hold them, after a
        // complete pass, so the facts derive a primary mint.
        sqlx::query("INSERT INTO shop.mirror_state (mirror, network, source_url, last_complete_pass_at) VALUES ('items','matic','fixture',now()), ('collections','matic','fixture',now()) ON CONFLICT (mirror,network) DO UPDATE SET last_complete_pass_at = now()")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO shop.collections (network,contract_address,chain_id,name,creator,owner,urn,is_completed,is_approved,is_editable,created_at,updated_at,reviewed_at,mirrored_at) VALUES ('matic',$1,137,'Coupon fixture',$2,$2,'urn:coupon-quote',true,true,false,0,0,0,now())")
            .bind(format!("{collection:#x}")).bind(format!("{creator:#x}")).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO shop.items (network,contract_address,item_id,chain_id,item_type,urn,creator,rarity,total_supply,max_supply,available,price,name,category,search_is_store_minter,search_is_collection_approved,created_at,updated_at,reviewed_at,mirrored_at) VALUES ('matic',$1,0,137,'wearable_v2','urn:coupon-quote:0',$2,'common',0,10,10,$3::numeric,'Hat','hat',true,true,0,0,0,now())")
            .bind(format!("{collection:#x}")).bind(format!("{creator:#x}")).bind(price.to_string()).execute(&pool).await.unwrap();
        let trade_id =
            crate::market::listings::insert_trade(&pool, &trade, "MATIC", &format!("{venue:#x}"))
                .await
                .unwrap();
        let coupon_id: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "INSERT INTO shop.coupons (network, chain_id, signer, signature, hashed_signature, state_key, coupon_manager, coupon_address, checks, \
                discount_type, discount_ppm, root, collections, effective_since, expires_at) \
             VALUES ('MATIC', 137, $1, $2, $2, $2, '0x655fdfa91d69ea49f4ce1a8f7f7e2622c8630813', '0xc914507fe297b2dddd1232ac3a8903f1c125e794', \
                '{{\"uses\": 10, \"expiration\": 4102444800000, \"effective\": 0, \"salt\": \"0x02\", \"contractSignatureIndex\": 0, \"signerSignatureIndex\": 0, \"allowedRoot\": \"0x\", \"externalChecks\": []}}'::jsonb, \
                1, 250000, '0x{root}', ARRAY[$3], timezone('UTC', now()) - interval '1 day', timezone('UTC', now()) + interval '1 day') RETURNING id::text",
            root = "00".repeat(32)
        )))
        .bind(format!("{creator:#x}"))
        .bind(format!("0x{}{}", "22".repeat(64), &uuid::Uuid::new_v4().simple().to_string()[..2]))
        .bind(format!("{collection:#x}"))
        .fetch_one(&pool)
        .await
        .unwrap();

        let session = Session::Web3 {
            wallet: EvmAddress::new(buyer),
        };
        let asset = crate::types::AssetId::item(
            crate::types::ChainId::POLYGON,
            EvmAddress::new(collection),
            U256::ZERO,
        );
        let resolved = facts::resolve_trade(&pool, chain.clone(), session, asset, &trade_id)
            .await
            .unwrap();
        match &resolved.listing {
            Listing::Trade {
                coupon: Some(c), ..
            } => assert_eq!(
                (c.id.as_str(), c.discount_ppm),
                (coupon_id.as_str(), 250_000)
            ),
            other => panic!("the listing carries no coupon: {other:?}"),
        }
        let line = Authoritative::derive(resolved.facts.clone()).unwrap();
        // $1 less 25% is $0.75, then converted at $0.50 a MANA: 1.5 MANA.
        let rate = crate::types::ManaUsdRate::parse_usd_per_mana("0.5").unwrap();
        let discounted = ManaWei::new(U256::from(1_500_000_000_000_000_000u128));
        assert_eq!(
            quote(&line, PaymentMethod::Mana, Some(&rate)).unwrap(),
            Quote::Mana { amount: discounted }
        );
        assert_eq!(
            settlement_key(&line, PaymentMethod::Mana, Some(&rate)).map(|k| k.call),
            Some(crate::types::SettlementCall::AcceptWithCoupon)
        );
        let execution =
            mana_execution(&resolved, discounted, Some(buyer), None, Vec::new(), None).await;
        let data = execution.call.expect("the API's calldata").data;
        let bytes = alloy_primitives::hex::decode(data.trim_start_matches("0x")).unwrap();
        let call = crate::evm::abi::offchain::acceptWithCouponCall::abi_decode(&bytes)
            .expect("acceptWithCoupon calldata");
        let on_chain = &call._coupons[0];
        assert_eq!(
            format!("{:#x}", on_chain.couponAddress),
            "0xc914507fe297b2dddd1232ac3a8903f1c125e794"
        );
        let (kind, discount, root) =
            <(U256, U256, alloy_primitives::B256)>::abi_decode(&on_chain.data).unwrap();
        assert_eq!(
            (kind, discount, root),
            (
                U256::from(1u8),
                U256::from(250_000u32),
                alloy_primitives::B256::ZERO
            )
        );
        assert_eq!(call._trades[0].signer, creator);
        assert_eq!(
            execution.allowance.as_ref().map(|a| a.amount.get()),
            Some(super::super::feed_rate::usd_pegged_allowance(
                discounted.get()
            )),
            "the buyer approves the discounted amount, plus upstream's 2% for a USD-pegged trade"
        );
        assert_eq!(
            execution.allowance.as_ref().map(|a| a.amount.get()),
            Some(U256::from(1_530_000_000_000_000_000u128))
        );
        db.drop().await;
    }
}
