// from: library/shop/model/{purchase,settlement,wire}.ts (PurchaseOffer, the serde forms the UI decodes), shop@94584a5c:app/src/lib/payment-options.ts (what a presentation needs: a quote per offered method, the outage per withheld one)
//! The wire of the MANA routes: the quote request (cart lines named by
//! their authoritative listing, never by a price), the `PurchaseOffer` the
//! UI's `presentPayment` renders, its MANA execution block (network,
//! venue, required balance, allowance, calldata, gas: the web2 guidance
//! data), the seller/owner actions and the tracked executions. Money is
//! exact: cents as integers, MANA as decimal wei strings (`ManaWei`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::policy::{Eligibility, MissingFact, Quote};
use crate::types::{
    AssetId, AssetKind, ChainId, EvmAddress, ManaWei, PaymentMethod, SaleKind, SettlementGroupKey,
};

/// The most lines one quote request carries (the cart's own bound).
pub const MAX_LINES: usize = 50;

/// What identifies the listing a line buys, beyond the asset: never a price.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ListingRef {
    pub trade_id: Option<String>,
    pub order_id: Option<String>,
    pub rental_id: Option<String>,
    /// A NAME registration's label.
    pub name: Option<String>,
    /// Upstream `ListingAcquisition`: `trade` | `store`.
    pub acquisition: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineRequest {
    /// Absent only for a NAME registration (`listing.name` names it).
    #[serde(default)]
    pub asset: Option<AssetId>,
    #[serde(default)]
    pub listing: ListingRef,
    #[serde(default = "one")]
    pub quantity: u32,
    /// A rental: the period chosen among the listing's, and the days inside it.
    #[serde(default)]
    pub period_index: Option<usize>,
    #[serde(default)]
    pub rental_days: Option<u64>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OffersRequest {
    pub lines: Vec<LineRequest>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Unavailable {
    pub method: PaymentMethod,
    pub missing: MissingFact,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SettlementFor {
    pub method: PaymentMethod,
    pub key: SettlementGroupKey,
}

/// One verification the server ran before quoting: `ok` is `None` when the
/// source that would answer is not available (an index catching up, a chain
/// read that failed), which the UI shows as freshness, never as a pass.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: &'static str,
    pub ok: Option<bool>,
    pub detail: String,
}

impl Check {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: Some(true),
            detail: detail.into(),
        }
    }

    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: Some(false),
            detail: detail.into(),
        }
    }

    pub fn unknown(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: None,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AllowanceNeed {
    pub token: EvmAddress,
    pub spender: EvmAddress,
    pub amount: ManaWei,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalNeed {
    pub contract: EvmAddress,
    pub token_id: String,
    pub operator: EvmAddress,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CallDto {
    pub to: EvmAddress,
    /// `0x` hex calldata.
    pub data: String,
}

/// The gas a purchase needs: an RPC estimate from the buyer's own address
/// when the call would succeed as it stands, else the static budget of the
/// call's shape (`source: "static"`), each with the current gas price and
/// the native fee it implies.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GasEstimate {
    pub estimate: Option<u64>,
    pub source: &'static str,
    pub gas_price_wei: Option<String>,
    pub fee_wei: Option<String>,
    pub detail: Option<String>,
}

/// The buyer's position on the settlement chain, read when a wallet is
/// known and the chain answers.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Balances {
    pub wallet: EvmAddress,
    pub mana: ManaWei,
    pub native: String,
    pub allowance: ManaWei,
    pub mana_shortfall: ManaWei,
    pub native_shortfall: String,
}

/// What the indexes said about the listing, and whether they were
/// published when they said it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Freshness {
    pub source: &'static str,
    pub ready: bool,
    pub verdict: Option<String>,
}

/// The MANA execution of one line: everything a wallet needs to pay it, and
/// everything a web2 buyer is told (SHOP.md: the network, the required
/// balance, the gas requirement, the wallet to set up).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManaExecution {
    pub chain_id: ChainId,
    pub venue: EvmAddress,
    pub token: EvmAddress,
    pub amount: ManaWei,
    pub native_gas_required: bool,
    pub allowance: Option<AllowanceNeed>,
    pub approval: Option<ApprovalNeed>,
    pub call: Option<CallDto>,
    pub gas: GasEstimate,
    pub balances: Option<Balances>,
    pub checks: Vec<Check>,
    /// Every check that could run passed.
    pub executable: bool,
    pub freshness: Freshness,
    /// The Estate registry fingerprint an estate purchase binds to.
    pub fingerprint: Option<String>,
}

/// The server's answer for one line (`library/shop/model/purchase.ts`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseOffer {
    pub asset: AssetId,
    pub kind: AssetKind,
    pub sale: SaleKind,
    pub bundle: bool,
    pub eligibility: Eligibility,
    pub quotes: Vec<Quote>,
    pub unavailable: Vec<Unavailable>,
    pub settlement: Vec<SettlementFor>,
    pub available: Option<i64>,
    pub quoted_at: i64,
    /// The creator coupon the quotes are discounted by, when the sale has
    /// one: the buyer's settlement must use this coupon, or none of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coupon: Option<OfferCoupon>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mana: Option<ManaExecution>,
}

/// The identity and economics of the coupon an offer was priced with.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfferCoupon {
    pub id: String,
    pub coupon_address: String,
    pub discount_ppm: u32,
}

/// A line that could not be quoted at all: the rule's refusal or the
/// listing's absence, in place of its offer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LineError {
    pub index: usize,
    pub error: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum OfferAnswer {
    Offer(Box<PurchaseOffer>),
    Error(LineError),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OffersResponse {
    pub quoted_at: i64,
    pub offers: Vec<OfferAnswer>,
}

/// The `error` code of a rule refusal (the same the guards spell).
pub const PAYMENT_RULE_REFUSED: &str = "payment_rule_refused";

pub(crate) use crate::guards::base_refusal_json as refusal_json;

/// The source label an incomplete policy refusal names.
pub fn missing_source(missing: MissingFact) -> &'static str {
    match missing {
        MissingFact::ContractRegistry => "shop-contract-registry",
        MissingFact::CollectionItem => "shop-collection-items",
        MissingFact::NameRegistry => "shop-name-registry",
        MissingFact::ManaUsdRate => "mana-usd-rate",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionKind {
    /// The seller accepts a bid (a trade whose signer pays MANA for the seller's token).
    AcceptBid,
    /// The signer revokes a listing or a bid (`cancelSignature`).
    CancelTrade,
    /// The seller cancels a legacy order.
    CancelOrder,
    /// The owner sends a token to another address.
    Transfer,
    /// The lessor takes a LAND back after its rent.
    ClaimRental,
    /// The lessor removes an open rental listing (`bumpAssetIndex`).
    CancelRental,
    /// A MANA allowance to a venue.
    ApproveMana,
}

impl ActionKind {
    pub const ALL: [ActionKind; 7] = [
        Self::AcceptBid,
        Self::CancelTrade,
        Self::CancelOrder,
        Self::Transfer,
        Self::ClaimRental,
        Self::CancelRental,
        Self::ApproveMana,
    ];

    pub fn execution_kind(self) -> &'static str {
        match self {
            Self::AcceptBid => "accept_trade",
            Self::CancelTrade => "cancel_trade",
            Self::CancelOrder => "cancel_order",
            Self::Transfer => "transfer",
            Self::ClaimRental => "claim_rental",
            Self::CancelRental => "cancel_rental",
            Self::ApproveMana => "approve_mana",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequest {
    pub kind: ActionKind,
    #[serde(default)]
    pub asset: Option<AssetId>,
    #[serde(default)]
    pub trade_id: Option<String>,
    #[serde(default)]
    pub rental_id: Option<String>,
    /// A transfer's recipient.
    #[serde(default)]
    pub to: Option<EvmAddress>,
    /// An allowance's venue and amount.
    #[serde(default)]
    pub spender: Option<EvmAddress>,
    #[serde(default)]
    pub amount: Option<ManaWei>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActionResponse {
    pub kind: ActionKind,
    pub execution_kind: &'static str,
    pub chain_id: ChainId,
    pub call: CallDto,
    pub allowance: Option<AllowanceNeed>,
    pub approval: Option<ApprovalNeed>,
    pub gas: GasEstimate,
    pub checks: Vec<Check>,
    pub executable: bool,
    /// What to report with the transaction hash (`POST /v1/executions`).
    pub execution: Value,
}

/// A submitted transaction the Shop tracks to settlement.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRequest {
    pub chain_id: ChainId,
    pub tx_hash: String,
    pub kind: String,
    #[serde(default)]
    pub asset: Option<AssetId>,
    #[serde(default)]
    pub listing: ListingRef,
    /// A transfer's recipient, a rental's operator: the address the asset
    /// or right should end with.
    #[serde(default)]
    pub to: Option<EvmAddress>,
    #[serde(default)]
    pub period_index: Option<usize>,
    #[serde(default)]
    pub rental_days: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionDto {
    pub id: String,
    pub chain_id: ChainId,
    pub tx_hash: String,
    pub kind: String,
    pub actor: String,
    pub venue: String,
    pub contract_address: Option<String>,
    pub token_id: Option<String>,
    pub reference: Option<String>,
    pub expected_price: Option<String>,
    pub expected_seller: Option<String>,
    pub expected_owner: Option<String>,
    pub status: String,
    pub block_number: Option<i64>,
    pub confirmed_by: Option<String>,
    pub settlement: Option<Value>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Refusal;
    use serde_json::json;

    #[test]
    fn a_line_request_reads_the_uis_listing_ref_and_defaults_the_quantity() {
        let raw = json!({ "asset": { "chainId": 137, "contract": "0x00000000000000000000000000000000000000cc", "token": { "kind": "token", "tokenId": "7" } }, "listing": { "tradeId": "abc" } });
        let line: LineRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(
            (
                line.quantity,
                line.listing.trade_id.as_deref(),
                line.period_index
            ),
            (1, Some("abc"), None)
        );
        let name: LineRequest =
            serde_json::from_value(json!({ "listing": { "name": "pixelgolf" } })).unwrap();
        assert!(name.asset.is_none());
    }

    #[test]
    fn the_answer_is_an_offer_or_an_error_per_line() {
        let err = OfferAnswer::Error(LineError {
            index: 2,
            error: PAYMENT_RULE_REFUSED,
            message: "no".into(),
            refusal: Some(refusal_json(&Refusal::MixedGroup)),
            source: None,
        });
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["index"], 2);
        assert_eq!(v["refusal"]["code"], "mixedGroup");
        assert!(v.get("source").is_none());
        assert_eq!(missing_source(MissingFact::ManaUsdRate), "mana-usd-rate");
        assert_eq!(
            serde_json::to_value(ActionKind::AcceptBid).unwrap(),
            "acceptBid"
        );
        assert_eq!(ActionKind::ClaimRental.execution_kind(), "claim_rental");
    }
}
