//! The payment rule at every entry (SHOP.md, backend item 3): each lane
//! derives its [`Authoritative`] line(s) through [`policy_entry`](super::policy_entry)
//! and asks [`policy`](super::policy) once, before any ledger row, signature
//! or broadcast. There is no client `primary` flag anywhere: the sale kind
//! comes from the CollectionStore listing, the decoded signed trade, the
//! legacy order or the registrar, and the asset kind from the registry and
//! the mirrors.
//!
//! [`super::paths::CREDIT_PATHS`] names the guard of every public path that
//! prices or spends credits.
//!
//! Every refusal is one wire shape, [`Refused`]: HTTP 422 with
//! `{"ok":false,"error":"payment_rule_refused","message":..,"refusal":{"code":..}}`,
//! or 503 with the unavailable envelope (`source`, `Retry-After`) when a
//! fact source cannot answer yet, or 401 for a credit spend without a
//! session. A refusal never re-routes a line to another payment method.

use alloy_primitives::{Address, U256};
use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::policy::{
    Authoritative, Facts, MissingFact, Refusal, eligibility, guard_group_credit_spend, guard_method,
};
use super::policy_entry::{self, ItemFacts, item_facts_from_mirror, manager_routes};
use super::types::{
    AssetId, ChainId, ContractRef, ContractRole, EvmAddress, ExternalCall, ItemRecord, Lookup,
    ManaWei, NameLabel, Offer, PaymentMethod, Session, SettlementOperation, SignedTrade, StoreItem,
    TokenRecord, TradeAssetType, TradeCoupon, UsdCents,
};

/// The `error` code of every rule refusal.
pub const ERROR_CODE: &str = "payment_rule_refused";

/// A rule refusal on its way to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub Refusal);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Refused {}

fn camel(fact: MissingFact) -> &'static str {
    match fact {
        MissingFact::ContractRegistry => "shop-contract-registry",
        MissingFact::CollectionItem => "shop-collection-items",
        MissingFact::NameRegistry => "shop-name-registry",
        MissingFact::ManaUsdRate => "mana-usd-rate",
    }
}

impl Refused {
    pub fn status(&self) -> StatusCode {
        match &self.0 {
            Refusal::Incomplete { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Refusal::NotSignedIn => StatusCode::UNAUTHORIZED,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }

    /// The source a 503 names, `None` for a refusal proper.
    pub fn source(&self) -> Option<&'static str> {
        match &self.0 {
            Refusal::Incomplete { missing } => Some(camel(*missing)),
            _ => None,
        }
    }

    /// The `refusal` object: the variant as a camelCase `code` and its fields.
    pub fn detail(&self) -> Value {
        refusal_json(&self.0)
    }

    pub fn body(&self) -> Value {
        let message = self.to_string();
        match self.source() {
            Some(source) => {
                json!({ "ok": false, "error": "Service Unavailable", "message": message, "source": source, "refusal": self.detail() })
            }
            None => {
                json!({ "ok": false, "error": ERROR_CODE, "message": message, "refusal": self.detail() })
            }
        }
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = self.body();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            (status, [(header::RETRY_AFTER, "60")], Json(body)).into_response()
        } else {
            (status, Json(body)).into_response()
        }
    }
}

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        Self(refusal)
    }
}

/// Every [`Refusal`] variant as JSON: `code` names the variant, the rest are
/// its fields under their names.
pub fn refusal_json(refusal: &Refusal) -> Value {
    let mut detail = base_refusal_json(refusal);
    if matches!(
        refusal,
        Refusal::CreditsNotAllowed { .. } | Refusal::SecondaryCreditSpend { .. }
    ) {
        detail["allowed"] = json!([PaymentMethod::Mana]);
    }
    detail
}

pub(crate) fn base_refusal_json(refusal: &Refusal) -> Value {
    match refusal {
        Refusal::Incomplete { missing } => json!({ "code": "incomplete", "missing": missing }),
        Refusal::UnknownItem { asset } => json!({ "code": "unknownItem", "asset": asset }),
        Refusal::RecordMismatch { asset } => json!({ "code": "recordMismatch", "asset": asset }),
        Refusal::NotMintable { asset } => json!({ "code": "notMintable", "asset": asset }),
        Refusal::UnsupportedTrade { detail } => {
            json!({ "code": "unsupportedTrade", "detail": detail })
        }
        Refusal::NameTaken { asset } => json!({ "code": "nameTaken", "asset": asset }),
        Refusal::NotARegistrar { asset } => json!({ "code": "notARegistrar", "asset": asset }),
        Refusal::MethodNotAllowed {
            asset,
            method,
            allowed,
        } => {
            json!({ "code": "methodNotAllowed", "asset": asset, "method": method, "allowed": allowed })
        }
        Refusal::NonPositiveQuote { asset } => {
            json!({ "code": "nonPositiveQuote", "asset": asset })
        }
        Refusal::AmountOverflow { asset } => json!({ "code": "amountOverflow", "asset": asset }),
        Refusal::NotSignedIn => json!({ "code": "notSignedIn" }),
        Refusal::CreditsNotAllowed { asset, sale, kind } => {
            json!({ "code": "creditsNotAllowed", "asset": asset, "sale": sale, "kind": kind })
        }
        Refusal::SecondaryCreditSpend { asset } => {
            json!({ "code": "secondaryCreditSpend", "asset": asset })
        }
        Refusal::UndecodedCustomCall { target, selector } => {
            json!({ "code": "undecodedCustomCall", "target": target, "selector": format!("0x{}", hex::encode(selector)) })
        }
        Refusal::ForwardedCustomCall { target, selector } => {
            json!({ "code": "forwardedCustomCall", "target": target, "selector": format!("0x{}", hex::encode(selector)) })
        }
        Refusal::ManaRemainderNotAllowed { asset } => {
            json!({ "code": "manaRemainderNotAllowed", "asset": asset })
        }
        Refusal::AssetNotInGroup { asset } => json!({ "code": "assetNotInGroup", "asset": asset }),
        Refusal::LineNotSettled { asset } => json!({ "code": "lineNotSettled", "asset": asset }),
        Refusal::WrongCall { asset } => json!({ "code": "wrongCall", "asset": asset }),
        Refusal::MixedGroup => json!({ "code": "mixedGroup" }),
        Refusal::CreditsChainUnsupported { chain_id } => {
            json!({ "code": "creditsChainUnsupported", "chainId": chain_id })
        }
        Refusal::WrongChain { expected, got } => {
            json!({ "code": "wrongChain", "expected": expected, "got": got })
        }
        Refusal::GroupTooLarge { lines, max } => {
            json!({ "code": "groupTooLarge", "lines": lines, "max": max })
        }
        Refusal::EmptyGroup => json!({ "code": "emptyGroup" }),
        Refusal::NegativeDebit { debit } => json!({ "code": "negativeDebit", "debit": debit }),
        Refusal::CreditValueTooLarge { value, cap } => {
            json!({ "code": "creditValueTooLarge", "value": value, "cap": cap })
        }
        Refusal::SettlementNeedAboveReference { need, reference } => {
            json!({ "code": "settlementNeedAboveReference", "need": need, "reference": reference })
        }
        Refusal::UnlistedMarketplace { venue } => {
            json!({ "code": "unlistedMarketplace", "chainId": venue.chain_id, "contract": venue.contract })
        }
    }
}

/// A signed-fetch identity as the rule's session: the signature proves an
/// account, not a connected web3 wallet, so it is a web2 session (credits
/// eligible, no MANA remainder). A meta-transaction signature is a wallet
/// capability ([`Session::Web3`]).
pub fn signed_session(account: EvmAddress) -> Session {
    Session::Web2 { account }
}

/// The exact cents a whole-credit charge debits (`"13"` credits are 130
/// cents); a fractional or malformed charge is not a debit the rule reads.
pub fn charge_cents(unit_price_credits: &str) -> Option<UsdCents> {
    let text = unit_price_credits.trim();
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b == b'0')
    {
        return None;
    }
    UsdCents::from_credits(whole.parse::<i64>().ok()?)
}

fn item_record(facts: &ItemFacts) -> Lookup<ItemRecord> {
    facts.item.clone()
}

fn mint_facts(session: Session, offer: Offer, facts: &ItemFacts) -> Facts {
    Facts {
        session,
        offer,
        contract: facts.contract.clone(),
        item: item_record(facts),
        token: Lookup::Absent,
    }
}

fn existing_facts(session: Session, offer: Offer, contract: Lookup<ContractRole>) -> Facts {
    Facts {
        session,
        offer,
        contract,
        item: Lookup::Unknown,
        token: Lookup::Unknown,
    }
}

/// The first sent asset of a trade names what the line sells.
fn trade_subject(trade: &SignedTrade) -> Option<(TradeAssetType, EvmAddress, U256)> {
    trade
        .sent
        .first()
        .map(|a| (a.asset_type, a.contract, a.value))
}

pub mod credits {
    //! The credits ledger lanes: the cart, the checkout, the fulfilment
    //! outbox and the quote, over the items mirror's `ItemInfo`.

    use super::*;
    use crate::checkout::trade::TradeIn;
    use crate::credits::errors::ApiError;
    use crate::credits::pricing::ItemInfo;

    /// How a charge basis would be fulfilled.
    #[derive(Debug, Clone)]
    pub enum Kind {
        /// `CollectionStore.buy` at the store's price.
        Mint,
        /// A MarketplaceV2 order on an existing token.
        LegacyListing { token_id: String },
        /// A signed off-chain trade, as the trades read serializes it.
        Trade { trade: Value },
    }

    /// One credits line: what the rule judged and the call the broker makes.
    #[derive(Debug, Clone)]
    pub struct Line {
        pub line: Authoritative,
        pub call: ExternalCall,
    }

    fn parse_item(info: &ItemInfo) -> Result<(EvmAddress, U256, EvmAddress), ApiError> {
        let collection = EvmAddress::parse(&info.contract_address.to_ascii_lowercase())
            .map_err(|e| ApiError::bad_request(format!("invalid collection: {e}")))?;
        let item_id = super::super::types::parse_decimal(info.item_id.trim())
            .map_err(|e| ApiError::bad_request(format!("invalid itemId: {e}")))?;
        let creator = EvmAddress::parse(&info.creator.to_ascii_lowercase()).map_err(|e| {
            ApiError::Internal(format!(
                "the items mirror holds a creator that is not an address: {e}"
            ))
        })?;
        Ok((collection, item_id, creator))
    }

    fn wei(label: &str, text: &str) -> Result<ManaWei, ApiError> {
        super::super::types::parse_decimal(text.trim())
            .map(ManaWei::new)
            .map_err(|e| ApiError::bad_request(format!("invalid {label}: {e}")))
    }

    /// The line a charge basis is, from the mirror's item and the basis'
    /// source; the trade's venue is the one it names (the broker accepts
    /// the Polygon off-chain marketplace only, and says so itself).
    pub fn line(
        session: Session,
        info: &ItemInfo,
        basis_wei: &str,
        kind: Kind,
    ) -> Result<Line, ApiError> {
        let chain_id = ChainId(info.chain_id);
        let routes = manager_routes(chain_id).ok_or_else(|| {
            ApiError::unprocessable(format!("credits do not settle on chain {chain_id}"))
        })?;
        let (collection, item_id, creator) = parse_item(info)?;
        let facts = item_facts_from_mirror(chain_id, collection, item_id, &info.category, creator);
        let price = wei("basis", basis_wei)?;
        let (facts, call) = match kind {
            Kind::Mint => {
                let venue = ContractRef {
                    chain_id,
                    contract: EvmAddress::new(routes.collection_store),
                };
                let offer = Offer::StoreMint {
                    venue,
                    collection,
                    item_id,
                    price,
                };
                let call = ExternalCall::CollectionStoreBuy {
                    venue,
                    items: vec![StoreItem {
                        collection,
                        item_id,
                        price,
                    }],
                };
                (mint_facts(session, offer, &facts), call)
            }
            Kind::LegacyListing { token_id } => {
                let venue = ContractRef {
                    chain_id,
                    contract: EvmAddress::new(routes.legacy_marketplace),
                };
                let token_id = super::super::types::parse_decimal(token_id.trim())
                    .map_err(|e| ApiError::bad_request(format!("invalid tokenId: {e}")))?;
                let offer = Offer::LegacyOrder {
                    venue,
                    nft: collection,
                    token_id,
                    price,
                };
                let call = ExternalCall::LegacyExecuteOrder {
                    venue,
                    nft: collection,
                    token_id,
                    price,
                };
                (existing_facts(session, offer, facts.contract.clone()), call)
            }
            Kind::Trade { trade } => {
                let trade: TradeIn = serde_json::from_value(trade).map_err(|e| {
                    ApiError::Internal(format!("the trade book row does not parse: {e}"))
                })?;
                let venue = match trade.contract.as_deref() {
                    Some(c) => EvmAddress::parse(&c.to_ascii_lowercase()).map_err(|e| {
                        ApiError::Internal(format!("the trade's venue is not an address: {e}"))
                    })?,
                    None => EvmAddress::new(crate::checkout::trade::OFFCHAIN_MARKETPLACE_POLYGON),
                };
                let venue = ContractRef {
                    chain_id,
                    contract: venue,
                };
                let signed = policy_entry::signed_trade_from_trade_in(&trade, venue)
                    .map_err(|e| ApiError::unprocessable(e.to_string()))?;
                let call = ExternalCall::MarketplaceAccept {
                    venue,
                    trades: vec![signed.clone()],
                    coupons: Vec::new(),
                };
                let offer = Offer::Trade {
                    trade: signed.clone(),
                    coupon: None,
                };
                let facts = match trade_subject(&signed) {
                    Some((TradeAssetType::CollectionItem, _, _)) => {
                        mint_facts(session, offer, &facts)
                    }
                    _ => existing_facts(session, offer, facts.contract),
                };
                (facts, call)
            }
        };
        let line = Authoritative::derive(facts).map_err(Refused)?;
        Ok(Line { line, call })
    }

    impl Line {
        /// A quote: the line offers credits at all.
        pub fn guard_quote(&self) -> Result<(), Refused> {
            guard_method(&self.line, PaymentMethod::Credits).map_err(Refused)
        }

        /// A debit of `debit` cents funding this line's broker buy.
        pub fn guard_spend(&self, debit: UsdCents) -> Result<(), Refused> {
            guard_group_credit_spend(
                std::slice::from_ref(&self.line),
                &SettlementOperation::LedgerBroker {
                    debit,
                    call: self.call.clone(),
                },
            )
            .map_err(Refused)
        }
    }
}

pub mod broker {
    //! The legacy broker: a buy the relayer settles with treasury MANA that a
    //! credits debit paid for, so every buy is a credit spend.

    use super::*;
    use crate::checkout::calls::PurchaseMode;
    use crate::checkout::trade::ValidatedTrade;
    use crate::evm::errors::ApiError;

    /// A buy without its exact charge is still credit-funded: the guard
    /// reads only the debit's sign, so one credit stands for "some".
    pub const CREDIT_FUNDED: UsdCents = UsdCents::new(UsdCents::CENTS_PER_CREDIT);

    pub fn debit(charge_cents: Option<i64>) -> Result<UsdCents, ApiError> {
        match charge_cents {
            None => Ok(CREDIT_FUNDED),
            Some(c) if c > 0 => Ok(UsdCents::new(c)),
            Some(c) => Err(ApiError::InvalidTransaction(format!(
                "chargeCents must be a positive integer, got {c}"
            ))),
        }
    }

    pub struct BuyFacts<'a> {
        pub chain_id: u64,
        pub buyer: Address,
        pub collection: Address,
        pub mode: PurchaseMode,
        pub item_id: Option<U256>,
        pub token_id: Option<U256>,
        pub price_wei: U256,
        pub trade: Option<&'a ValidatedTrade>,
        pub charge_cents: Option<i64>,
    }

    /// `POST /v1/broker/buy`: the line from the mode's own source (the
    /// store, the order book, the validated trade) and the registry.
    pub async fn guard_buy(pool: &sqlx::PgPool, facts: BuyFacts<'_>) -> Result<(), ApiError> {
        let chain_id = ChainId(facts.chain_id);
        let routes = manager_routes(chain_id).ok_or_else(|| {
            ApiError::InvalidTransaction(format!("credits do not settle on chain {chain_id}"))
        })?;
        let session = signed_session(EvmAddress::new(facts.buyer));
        let collection = EvmAddress::new(facts.collection);
        let debit = debit(facts.charge_cents)?;
        let (line_facts, call) = match facts.mode {
            PurchaseMode::Primary => {
                let item_id = facts.item_id.ok_or_else(|| {
                    ApiError::InvalidTransaction("primary buy requires `itemId`".into())
                })?;
                let registry =
                    policy_entry::item_facts(pool, chain_id, collection, item_id).await?;
                let venue = ContractRef {
                    chain_id,
                    contract: EvmAddress::new(routes.collection_store),
                };
                let price = ManaWei::new(facts.price_wei);
                let offer = Offer::StoreMint {
                    venue,
                    collection,
                    item_id,
                    price,
                };
                let call = ExternalCall::CollectionStoreBuy {
                    venue,
                    items: vec![StoreItem {
                        collection,
                        item_id,
                        price,
                    }],
                };
                (mint_facts(session, offer, &registry), call)
            }
            PurchaseMode::Secondary => {
                let token_id = facts.token_id.ok_or_else(|| {
                    ApiError::InvalidTransaction("secondary buy requires `tokenId`".into())
                })?;
                let contract = policy_entry::contract_role(pool, chain_id, collection).await?;
                let venue = ContractRef {
                    chain_id,
                    contract: EvmAddress::new(routes.legacy_marketplace),
                };
                let price = ManaWei::new(facts.price_wei);
                let offer = Offer::LegacyOrder {
                    venue,
                    nft: collection,
                    token_id,
                    price,
                };
                let call = ExternalCall::LegacyExecuteOrder {
                    venue,
                    nft: collection,
                    token_id,
                    price,
                };
                (existing_facts(session, offer, contract), call)
            }
            PurchaseMode::Trade => {
                let validated = facts.trade.ok_or_else(|| {
                    ApiError::Internal("a trade buy without its validated trade".into())
                })?;
                let venue = ContractRef {
                    chain_id,
                    contract: EvmAddress::new(crate::checkout::trade::OFFCHAIN_MARKETPLACE_POLYGON),
                };
                let signed = policy_entry::signed_trade_from_sol(&validated.onchain, venue);
                let call = ExternalCall::MarketplaceAccept {
                    venue,
                    trades: vec![signed.clone()],
                    coupons: Vec::new(),
                };
                let offer = Offer::Trade {
                    trade: signed.clone(),
                    coupon: None,
                };
                let line_facts = match trade_subject(&signed) {
                    Some((TradeAssetType::CollectionItem, contract, item_id)) => mint_facts(
                        session,
                        offer,
                        &policy_entry::item_facts(pool, chain_id, contract, item_id).await?,
                    ),
                    Some((_, contract, _)) => existing_facts(
                        session,
                        offer,
                        policy_entry::contract_role(pool, chain_id, contract).await?,
                    ),
                    None => existing_facts(session, offer, Lookup::Unknown),
                };
                (line_facts, call)
            }
            PurchaseMode::NameMint | PurchaseMode::NameSecondary => {
                return Err(ApiError::InvalidTransaction(
                    "a NAME buy goes through /v1/broker/names/buy".into(),
                ));
            }
        };
        let line = Authoritative::derive(line_facts).map_err(Refused)?;
        guard_group_credit_spend(
            std::slice::from_ref(&line),
            &SettlementOperation::LedgerBroker { debit, call },
        )
        .map_err(Refused)?;
        Ok(())
    }

    mod registrar_abi {
        alloy_sol_types::sol! {
            function ownerOf(uint256 tokenId) external view returns (address);
        }
    }

    /// `DCLRegistrar.ownerOf(tokenId)` through the NAMEs chain signer's
    /// reader: a registered NAME, an available one (the call reverts), or
    /// an unreadable registry.
    pub async fn name_registration(
        signer: &crate::evm::signer::DirectSigner,
        registrar: Address,
        chain_id: u64,
        token_id: U256,
    ) -> Lookup<TokenRecord> {
        use alloy_sol_types::SolCall;
        let data = registrar_abi::ownerOfCall { tokenId: token_id }.abi_encode();
        let answer = match signer.chain().call_view(registrar, data).await {
            Ok(Some(bytes)) => match registrar_abi::ownerOfCall::abi_decode_returns(&bytes) {
                Ok(owner) => Ok(Some(owner)),
                Err(_) => Err(()),
            },
            Ok(None) => Ok(None),
            Err(_) => Err(()),
        };
        policy_entry::name_token_from_owner_call(
            ContractRef {
                chain_id: ChainId(chain_id),
                contract: EvmAddress::new(registrar),
            },
            token_id,
            answer,
        )
    }

    /// The Polygon executor a credits NAME registration would route
    /// through; the broker never uses it (it pays MANA on Ethereum), the
    /// offer names it because a NAME offer has two venues.
    fn credits_name_venue() -> ContractRef {
        ContractRef {
            chain_id: ChainId::POLYGON,
            contract: EvmAddress::new(policy_entry::passthrough_executors(ChainId::POLYGON)[0]),
        }
    }

    pub struct NameBuyFacts<'a> {
        pub chain_id: u64,
        pub buyer: Address,
        pub mode: PurchaseMode,
        pub registrar: Address,
        pub controller: Address,
        pub legacy_marketplace: Address,
        pub label: Option<&'a str>,
        pub token_id: Option<U256>,
        pub price_wei: U256,
        /// The registrar's answer for the label (mint) or the token (resale).
        pub registered: Lookup<TokenRecord>,
        pub charge_cents: Option<i64>,
    }

    /// `POST /v1/broker/names/buy`: a mint is a primary NAME registration
    /// (credits eligible), a marketplace buy of a registered NAME a resale.
    pub fn guard_name_buy(facts: NameBuyFacts<'_>) -> Result<(), ApiError> {
        let chain_id = ChainId(facts.chain_id);
        let session = signed_session(EvmAddress::new(facts.buyer));
        let registrar = ContractRef {
            chain_id,
            contract: EvmAddress::new(facts.registrar),
        };
        let contract = match policy_entry::static_role(chain_id, facts.registrar) {
            Some(role) => Lookup::Found(role),
            None => Lookup::Absent,
        };
        let debit = debit(facts.charge_cents)?;
        let price = ManaWei::new(facts.price_wei);
        let (line_facts, call) = match facts.mode {
            PurchaseMode::NameMint => {
                let raw = facts
                    .label
                    .ok_or_else(|| ApiError::InvalidTransaction("mint requires `name`".into()))?;
                let label = NameLabel::parse(raw).map_err(|e| {
                    ApiError::InvalidTransaction(format!("invalid NAME {raw:?}: {e}"))
                })?;
                let mana_venue = ContractRef {
                    chain_id,
                    contract: EvmAddress::new(facts.controller),
                };
                let offer = Offer::NameRegistration {
                    label: label.clone(),
                    registrar,
                    mana_venue,
                    credits_venue: credits_name_venue(),
                    price,
                };
                let call = ExternalCall::NameRegister {
                    venue: mana_venue,
                    registrar,
                    label,
                };
                (
                    Facts {
                        session,
                        offer,
                        contract,
                        item: Lookup::Absent,
                        token: facts.registered,
                    },
                    call,
                )
            }
            PurchaseMode::NameSecondary => {
                let token_id = facts.token_id.ok_or_else(|| {
                    ApiError::InvalidTransaction("secondary name buy requires `tokenId`".into())
                })?;
                let venue = ContractRef {
                    chain_id,
                    contract: EvmAddress::new(facts.legacy_marketplace),
                };
                let offer = Offer::LegacyOrder {
                    venue,
                    nft: registrar.contract,
                    token_id,
                    price,
                };
                let call = ExternalCall::LegacyExecuteOrder {
                    venue,
                    nft: registrar.contract,
                    token_id,
                    price,
                };
                (
                    Facts {
                        session,
                        offer,
                        contract,
                        item: Lookup::Absent,
                        token: facts.registered,
                    },
                    call,
                )
            }
            _ => {
                return Err(ApiError::InvalidTransaction(
                    "a wearable buy goes through /v1/broker/buy".into(),
                ));
            }
        };
        let line = Authoritative::derive(line_facts).map_err(Refused)?;
        guard_group_credit_spend(
            std::slice::from_ref(&line),
            &SettlementOperation::LedgerBroker { debit, call },
        )
        .map_err(Refused)?;
        Ok(())
    }
}

pub mod relay {
    //! The meta-transaction relay: a `useCredits` to a CreditsManager is
    //! decoded and judged as the operation it executes; every other target
    //! and every other manager function spends no credit here.

    use super::*;
    use crate::evm::abi::decode_meta_tx_full;
    use crate::evm::errors::ApiError;
    use policy_entry::{USE_CREDITS_SELECTOR, decode_use_credits, is_credits_manager};

    fn hex_bytes(text: &str) -> Option<Vec<u8>> {
        hex::decode(text.trim().strip_prefix("0x")?).ok()
    }

    /// The lines a decoded call settles, from the registry and the mirrors.
    async fn lines_of(
        pool: &sqlx::PgPool,
        chain_id: ChainId,
        session: Session,
        call: &ExternalCall,
    ) -> Result<Vec<Authoritative>, ApiError> {
        let mut lines = Vec::new();
        match call {
            ExternalCall::CollectionStoreBuy { venue, items } => {
                for item in items {
                    let facts =
                        policy_entry::item_facts(pool, chain_id, item.collection, item.item_id)
                            .await?;
                    let offer = Offer::StoreMint {
                        venue: *venue,
                        collection: item.collection,
                        item_id: item.item_id,
                        price: item.price,
                    };
                    lines.push(
                        Authoritative::derive(mint_facts(session, offer, &facts))
                            .map_err(Refused)?,
                    );
                }
            }
            ExternalCall::MarketplaceAccept {
                trades, coupons, ..
            } => {
                for (i, trade) in trades.iter().enumerate() {
                    let coupon: Option<TradeCoupon> = coupons.get(i).copied();
                    let offer = Offer::Trade {
                        trade: trade.clone(),
                        coupon,
                    };
                    let facts = match trade_subject(trade) {
                        Some((TradeAssetType::CollectionItem, contract, item_id)) => mint_facts(
                            session,
                            offer,
                            &policy_entry::item_facts(pool, chain_id, contract, item_id).await?,
                        ),
                        Some((_, contract, _)) => existing_facts(
                            session,
                            offer,
                            policy_entry::contract_role(pool, chain_id, contract).await?,
                        ),
                        None => existing_facts(session, offer, Lookup::Unknown),
                    };
                    lines.push(Authoritative::derive(facts).map_err(Refused)?);
                }
            }
            ExternalCall::LegacyExecuteOrder {
                venue,
                nft,
                token_id,
                price,
            } => {
                let offer = Offer::LegacyOrder {
                    venue: *venue,
                    nft: *nft,
                    token_id: *token_id,
                    price: *price,
                };
                let contract = policy_entry::contract_role(pool, chain_id, *nft).await?;
                lines.push(
                    Authoritative::derive(existing_facts(session, offer, contract))
                        .map_err(Refused)?,
                );
            }
            ExternalCall::NameRegister { .. } => {}
            ExternalCall::Custom {
                venue, selector, ..
            } => {
                return Err(Refused(Refusal::UndecodedCustomCall {
                    target: venue.contract,
                    selector: *selector,
                })
                .into());
            }
        }
        Ok(lines)
    }

    /// `POST /v1/transactions`: `target` and `calldata` are the request's
    /// `params`, `sender` the recovered meta-transaction signer.
    pub async fn guard_meta_transaction(
        pool: &sqlx::PgPool,
        chain_id: u64,
        sender: &str,
        target: &str,
        calldata: &str,
    ) -> Result<(), ApiError> {
        let chain = ChainId(chain_id);
        let Ok(target) = target.trim().parse::<Address>() else {
            return Ok(());
        };
        if !is_credits_manager(chain, target) {
            return Ok(());
        }
        let data = hex_bytes(calldata).unwrap_or_default();
        let Some(inner) = decode_meta_tx_full(&data) else {
            return Ok(());
        };
        let Some(selector) = inner.function_signature.get(..4) else {
            return Ok(());
        };
        if selector != USE_CREDITS_SELECTOR {
            return Ok(());
        }
        let wallet = EvmAddress::parse(&sender.trim().to_ascii_lowercase()).map_err(|e| {
            ApiError::InvalidTransaction(format!(
                "the meta-transaction signer is not an address: {e}"
            ))
        })?;
        let decoded = decode_use_credits(&inner.function_signature, chain).map_err(|e| {
            ApiError::InvalidTransaction(format!(
                "useCredits is relayed only when its external call decodes: {e}"
            ))
        })?;
        let session = Session::Web3 { wallet };
        let lines = lines_of(pool, chain, session, &decoded.call).await?;
        let op = SettlementOperation::UseCredits {
            chain_id: chain,
            max_uncredited: decoded.max_uncredited,
            call: decoded.call,
        };
        guard_group_credit_spend(&lines, &op).map_err(Refused)?;
        Ok(())
    }
}

pub mod catalog {
    //! The feeds: a credit price is a primary line's; a resale row carries
    //! none, whatever its signed price asset.

    /// `Some(price)` for a primary row, `None` for a resale row: the row is
    /// primary when its trade type says so AND it sells no minted token
    /// (`token_id`, the decoded sent asset); a trade whose type and asset
    /// disagree is a resale, whatever it was labelled.
    pub fn credit_price(
        listing_type: &str,
        token_id: Option<&str>,
        price_credits: i64,
    ) -> Option<i64> {
        (listing_type == "primary" && token_id.is_none()).then_some(price_credits)
    }
}

/// The lines of a call as the [`eligibility`] table sees them, for tests and
/// diagnostics: every asset the rule would offer credits for.
pub fn credit_eligible_assets(lines: &[Authoritative]) -> Vec<AssetId> {
    lines
        .iter()
        .filter(|line| eligibility(line).allows(PaymentMethod::Credits))
        .map(|line| line.asset())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout::calls::PurchaseMode;
    use crate::credits::pricing::ItemInfo;
    use alloy_primitives::address;

    const COLLECTION: &str = "0xeede64bfaf8055492aa500846ec7c6e6a9f533d5";
    const BUYER: EvmAddress =
        EvmAddress::new(address!("0x00000000000000000000000000000000000000b1"));

    fn info(category: &str, mintable: bool) -> ItemInfo {
        ItemInfo {
            item_id: "4".into(),
            urn: format!("urn:decentraland:matic:collections-v2:{COLLECTION}:4"),
            category: category.into(),
            price_wei: "2500000000000000000".into(),
            contract_address: COLLECTION.into(),
            store_mintable: mintable,
            creator: "0x00000000000000000000000000000000000000c1".into(),
            chain_id: 137,
        }
    }

    fn trade_json(sent_type: u64, value: &str) -> Value {
        let key = if sent_type == 3 { "tokenId" } else { "itemId" };
        json!({
            "id": "1bbe7d78-dd71-4cbe-9085-70d679d3ad11", "signer": "0x00000000000000000000000000000000000000c1", "signature": "0x00", "type": if sent_type == 3 { "public_nft_order" } else { "public_item_order" },
            "network": "MATIC", "chainId": 137, "contract": "0x540fb08edb56aae562864b390542c97f562825ba",
            "checks": { "uses": 1, "expiration": 4102444800000i64, "effective": 0, "salt": "0x", "contractSignatureIndex": 0, "signerSignatureIndex": 0 },
            "sent": [{ "assetType": sent_type, "contractAddress": COLLECTION, key: value }],
            "received": [{ "assetType": 2, "contractAddress": "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4", "amount": "5000000000000000000", "beneficiary": "0x00000000000000000000000000000000000000c1" }],
        })
    }

    #[test]
    fn a_store_mint_spends_credits_a_legacy_listing_and_an_nft_trade_do_not() {
        let session = signed_session(BUYER);
        let mint = credits::line(
            session,
            &info("wearable", true),
            "2500000000000000000",
            credits::Kind::Mint,
        )
        .unwrap();
        mint.guard_quote().unwrap();
        mint.guard_spend(UsdCents::new(130)).unwrap();

        let listing = credits::line(
            session,
            &info("wearable", false),
            "1000000000000000000",
            credits::Kind::LegacyListing {
                token_id: "42".into(),
            },
        )
        .unwrap();
        let refused = listing.guard_spend(UsdCents::new(50)).unwrap_err();
        assert!(
            matches!(
                refused.0,
                Refusal::CreditsNotAllowed {
                    sale: super::super::types::SaleKind::Secondary,
                    ..
                }
            ),
            "{refused}"
        );
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body()["error"], ERROR_CODE);
        assert_eq!(refused.body()["refusal"]["code"], "creditsNotAllowed");
        assert_eq!(refused.body()["refusal"]["allowed"], json!(["mana"]));
        assert!(matches!(
            listing.guard_quote().unwrap_err().0,
            Refusal::MethodNotAllowed { .. }
        ));

        let resale = credits::line(
            session,
            &info("wearable", false),
            "5000000000000000000",
            credits::Kind::Trade {
                trade: trade_json(3, "42"),
            },
        )
        .unwrap();
        assert!(matches!(
            resale.guard_spend(UsdCents::new(500)).unwrap_err().0,
            Refusal::CreditsNotAllowed { .. }
        ));

        let creator_order = credits::line(
            session,
            &info("emote", false),
            "5000000000000000000",
            credits::Kind::Trade {
                trade: trade_json(4, "4"),
            },
        )
        .unwrap();
        creator_order.guard_spend(UsdCents::new(500)).unwrap();

        let signed_out = credits::line(
            Session::SignedOut,
            &info("wearable", true),
            "2500000000000000000",
            credits::Kind::Mint,
        )
        .unwrap();
        signed_out.guard_quote().unwrap();
        assert_eq!(
            signed_out
                .guard_spend(UsdCents::new(130))
                .unwrap_err()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn charge_cents_reads_whole_credits_only() {
        assert_eq!(charge_cents("13"), Some(UsdCents::new(130)));
        assert_eq!(charge_cents(" 2.0 "), Some(UsdCents::new(20)));
        assert_eq!(charge_cents("1.5"), None);
        assert_eq!(charge_cents(""), None);
        assert_eq!(charge_cents("1e3"), None);
        assert_eq!(broker::debit(None).unwrap(), broker::CREDIT_FUNDED);
        assert!(broker::debit(Some(0)).is_err());
    }

    #[test]
    fn a_name_mint_is_primary_and_a_registered_name_is_a_resale() {
        let registrar = address!("0x2a187453064356c898cae034eaed119e1663acb8");
        let facts = |mode: PurchaseMode,
                     registered: Lookup<TokenRecord>,
                     label: Option<&'static str>,
                     token_id: Option<U256>| broker::NameBuyFacts {
            chain_id: 1,
            buyer: BUYER.address(),
            mode,
            registrar,
            controller: address!("0xbe92b49aee993adea3a002adcda189a2b7dec56c"),
            legacy_marketplace: address!("0x8e5660b4ab70168b5a6feea0e0315cb49c8cd539"),
            label,
            token_id,
            price_wei: U256::from(100u64) * U256::from(1_000_000_000_000_000_000u64),
            registered,
            charge_cents: None,
        };
        broker::guard_name_buy(facts(
            PurchaseMode::NameMint,
            Lookup::Absent,
            Some("nacho"),
            None,
        ))
        .unwrap();
        let taken = broker::guard_name_buy(facts(
            PurchaseMode::NameMint,
            Lookup::Found(TokenRecord {
                asset: AssetId::token(ChainId::ETHEREUM, EvmAddress::new(registrar), U256::from(1)),
                item_id: None,
                owner: BUYER,
            }),
            Some("nacho"),
            None,
        ))
        .unwrap_err();
        assert!(taken.to_string().contains("already registered"), "{taken}");
        let unknown = broker::guard_name_buy(facts(
            PurchaseMode::NameMint,
            Lookup::Unknown,
            Some("nacho"),
            None,
        ))
        .unwrap_err();
        assert_eq!(unknown.status().as_u16(), 503, "{unknown}");
        let resale = broker::guard_name_buy(facts(
            PurchaseMode::NameSecondary,
            Lookup::Found(TokenRecord {
                asset: AssetId::token(ChainId::ETHEREUM, EvmAddress::new(registrar), U256::from(1)),
                item_id: None,
                owner: BUYER,
            }),
            None,
            Some(U256::from(1)),
        ))
        .unwrap_err();
        assert_eq!(resale.status().as_u16(), 422, "{resale}");
        assert!(resale.to_string().contains("MANA only"), "{resale}");
    }

    #[test]
    fn refusal_surfaces_keep_codes_and_guard_only_allowed_methods() {
        use crate::policy::MissingFact;
        use crate::types::{AssetKind, SaleKind};
        let asset = AssetId::item(ChainId::POLYGON, BUYER, U256::from(7));
        let venue = ContractRef {
            chain_id: ChainId::POLYGON,
            contract: BUYER,
        };
        let wei = ManaWei::new(U256::from(1));
        let cases = [
            (
                Refusal::Incomplete {
                    missing: MissingFact::CollectionItem,
                },
                "incomplete",
            ),
            (Refusal::UnknownItem { asset }, "unknownItem"),
            (Refusal::RecordMismatch { asset }, "recordMismatch"),
            (Refusal::NotMintable { asset }, "notMintable"),
            (
                Refusal::UnsupportedTrade { detail: "example" },
                "unsupportedTrade",
            ),
            (Refusal::NameTaken { asset }, "nameTaken"),
            (Refusal::NotARegistrar { asset }, "notARegistrar"),
            (
                Refusal::MethodNotAllowed {
                    asset,
                    method: PaymentMethod::Credits,
                    allowed: vec![PaymentMethod::Mana],
                },
                "methodNotAllowed",
            ),
            (Refusal::NonPositiveQuote { asset }, "nonPositiveQuote"),
            (Refusal::AmountOverflow { asset }, "amountOverflow"),
            (Refusal::NotSignedIn, "notSignedIn"),
            (
                Refusal::CreditsNotAllowed {
                    asset,
                    sale: SaleKind::Secondary,
                    kind: AssetKind::Wearable,
                },
                "creditsNotAllowed",
            ),
            (
                Refusal::SecondaryCreditSpend { asset },
                "secondaryCreditSpend",
            ),
            (
                Refusal::UndecodedCustomCall {
                    target: BUYER,
                    selector: [1, 2, 3, 4],
                },
                "undecodedCustomCall",
            ),
            (
                Refusal::ForwardedCustomCall {
                    target: BUYER,
                    selector: [1, 2, 3, 4],
                },
                "forwardedCustomCall",
            ),
            (
                Refusal::ManaRemainderNotAllowed { asset },
                "manaRemainderNotAllowed",
            ),
            (Refusal::AssetNotInGroup { asset }, "assetNotInGroup"),
            (Refusal::LineNotSettled { asset }, "lineNotSettled"),
            (Refusal::WrongCall { asset }, "wrongCall"),
            (Refusal::MixedGroup, "mixedGroup"),
            (
                Refusal::CreditsChainUnsupported {
                    chain_id: ChainId::ETHEREUM,
                },
                "creditsChainUnsupported",
            ),
            (
                Refusal::WrongChain {
                    expected: ChainId::POLYGON,
                    got: ChainId::ETHEREUM,
                },
                "wrongChain",
            ),
            (
                Refusal::GroupTooLarge { lines: 20, max: 10 },
                "groupTooLarge",
            ),
            (Refusal::EmptyGroup, "emptyGroup"),
            (
                Refusal::NegativeDebit {
                    debit: UsdCents::new(-1),
                },
                "negativeDebit",
            ),
            (
                Refusal::CreditValueTooLarge {
                    value: wei,
                    cap: wei,
                },
                "creditValueTooLarge",
            ),
            (
                Refusal::SettlementNeedAboveReference {
                    need: wei,
                    reference: wei,
                },
                "settlementNeedAboveReference",
            ),
            (
                Refusal::UnlistedMarketplace { venue },
                "unlistedMarketplace",
            ),
        ];
        for (refusal, code) in cases {
            let mana = crate::mana::wire::refusal_json(&refusal);
            let mut guard = refusal_json(&refusal);
            assert_eq!(mana["code"], code);
            if matches!(
                refusal,
                Refusal::CreditsNotAllowed { .. } | Refusal::SecondaryCreditSpend { .. }
            ) {
                assert_eq!(
                    guard.as_object_mut().unwrap().remove("allowed"),
                    Some(json!(["mana"]))
                );
                assert!(mana.get("allowed").is_none());
            }
            assert_eq!(guard, mana, "{code}");
        }
    }

    #[test]
    fn the_refusal_wire_shape() {
        let refused = Refused(Refusal::Incomplete {
            missing: MissingFact::CollectionItem,
        });
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.body()["source"], "shop-collection-items");
        assert_eq!(refused.body()["refusal"]["code"], "incomplete");
        let refused = Refused(Refusal::UndecodedCustomCall {
            target: BUYER,
            selector: policy_entry::EXECUTOR_EXECUTE_SELECTOR,
        });
        assert_eq!(refused.body()["refusal"]["selector"], "0xfd165a73");
        assert_eq!(catalog::credit_price("primary", None, 21), Some(21));
        assert_eq!(catalog::credit_price("secondary", None, 21), None);
        assert_eq!(
            catalog::credit_price("primary", Some("42"), 21),
            None,
            "a minted token under a primary label is a resale"
        );
    }
}
