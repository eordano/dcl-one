// from: credits-server@465e628bc:src/logic/usd-credit-lines.ts (validateCreditLine, chargeForQuote), src/logic/usd-credit-cap.ts (applyCapHeadroom), src/logic/usd-credit-ttl.ts, src/controllers/handlers/{authorize-usd-credit,authorize-usd-credit-batch}.ts
//! The authorize flow both routes share (`POST /credits/authorize` is a
//! batch of one): validate the lines as upstream does, derive each line's
//! authoritative facts from the mirrors and the decoded signed trade (never
//! a client flag: `shop::policy::Authoritative::derive`), quote it from its
//! offer's own price and refuse a stale client price, size ONE credit for the
//! group from the quoted total (oracle buffer, cap headroom), pass THE rule
//! (`policy::guard_group_credit_spend` with `AuthorizeCredit`), read the
//! manager gate (secondary spending off, no pass-through custom call,
//! our signer holds the role), reserve the charged total under the wallet
//! lock, sign the contract digest (`digest.rs`), answer the upstream wire.
//!
//! Deliberate departures from the pinned handlers (SHOP.md, backend items 3
//! and 4): a line must name what it buys (a trade, a store item or a NAME);
//! a bare `usdPriceCents` is refused, since nothing authoritative can be
//! derived from it; the quote is the server's, a differing `usdPriceCents`
//! answers 409 `Price changed` instead of being charged; a resale or LAND
//! line answers 400 with the rule's reason; NAME registration with credits
//! stays unavailable (503) until an executor that builds the registration on
//! chain exists (docs/shop/CONTRACTS.md, "NAME credits").

use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use serde::Serialize;
use serde_json::{Value, json};

use super::authorize::{GroupLine, NewGroup, ReserveRefusal};
use super::digest::{CreditIdentity, TreasuryWallet, bytes32_hex};
use super::errors::ApiError;
use super::pricing::TradeLookup;
use super::{State, usage};
use crate::policy::{self, Authoritative, Facts, Quote, Refusal};
use crate::types::{
    AssetKind, ChainId, ContractRef, ContractRole, CreditSizing, EvmAddress, ItemKind, ItemRecord,
    Lookup, ManaUsdRate, ManaWei, NameLabel, Offer, PaymentMethod, SaleKind, Session,
    SettlementOperation, SignedTrade, TokenRef, TradeAsset, TradeAssetType, UsdCents,
};

/// Upstream `MAX_LINES_PER_GROUP`.
pub const MAX_LINES_PER_GROUP: usize = policy::MAX_CREDIT_LINES_PER_GROUP;
/// Upstream `DEFAULT_USD_CREDIT_TTL_SECONDS`.
pub const DEFAULT_TTL_SECONDS: u64 = 300;
/// 100 adds no headroom over the buffered quote: with the default buffer a credit signs 1.02x its quote.
pub const DEFAULT_CAP_HEADROOM_PCT: u32 = 100;
/// Upstream `USD_CREDIT_BUFFER_BPS` default.
pub const DEFAULT_ORACLE_BUFFER_BPS: u32 = 200;
/// The unavailable-source name of NAME credits (docs/shop/CONTRACTS.md).
pub const NAME_EXECUTOR_SOURCE: &str = "credits-name-executor";
pub const NAME_EXECUTOR_CONFIGURE: &str = "CREDITS_NAME_EXECUTOR";

/// `CollectionStore` per chain (docs/shop/CONTRACTS.md, "Addresses").
pub fn collection_store(chain_id: ChainId) -> Option<EvmAddress> {
    let hex = match chain_id {
        ChainId::POLYGON => "0x214ffc0f0103735728dc66b61a22e4f163e275ae",
        ChainId::AMOY => "0xe36abc9ec616c83caaa386541380829106149d68",
        _ => return None,
    };
    EvmAddress::parse(hex).ok()
}

/// One requested line after upstream's validation (`validateCreditLine`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLine {
    pub quoted_cents: i64,
    pub charged_cents: i64,
    pub trade_id: Option<String>,
    pub contract_address: Option<String>,
    pub item_id: Option<String>,
    pub registered_name: Option<String>,
}

fn is_eth_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Upstream `validateCreditLine`, message for message.
pub fn validate_line(raw: &Value) -> Result<ValidatedLine, String> {
    let quoted = match raw.get("usdPriceCents").and_then(Value::as_i64) {
        Some(c) if c > 0 => c,
        _ => return Err("A positive integer \"usdPriceCents\" is required".to_string()),
    };
    let contract_address = raw.get("contractAddress").filter(|v| !v.is_null());
    let item_id = raw.get("itemId").filter(|v| !v.is_null());
    if contract_address.is_some() || item_id.is_some() {
        let Some(address) = contract_address
            .and_then(Value::as_str)
            .filter(|s| is_eth_address(s))
        else {
            return Err(
                "\"contractAddress\" must be a valid address when an item is given".to_string(),
            );
        };
        let Some(item) = item_id
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 78 && s.bytes().all(|b| b.is_ascii_digit()))
        else {
            return Err("\"itemId\" must be a numeric id when an item is given".to_string());
        };
        let _ = (address, item);
    }
    let registered_name = match raw.get("name").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v
            .as_str()
            .filter(|s| (2..=15).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            Some(s) => Some(s.to_string()),
            None => return Err("\"name\" must be 2-15 alphanumeric characters".to_string()),
        },
    };
    let trade_id = match raw.get("tradeId").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v.as_str().filter(|s| !s.is_empty() && s.len() <= 100) {
            Some(s) => Some(s.to_string()),
            None => {
                return Err(
                    "\"tradeId\" must be a non-empty string of at most 100 characters".to_string(),
                );
            }
        },
    };
    let charged = UsdCents::new(quoted)
        .charge_for_quote()
        .map(UsdCents::get)
        .ok_or_else(|| "A positive integer \"usdPriceCents\" is required".to_string())?;
    Ok(ValidatedLine {
        quoted_cents: quoted,
        charged_cents: charged,
        trade_id,
        contract_address: contract_address
            .and_then(Value::as_str)
            .map(|s| s.to_ascii_lowercase()),
        item_id: item_id.and_then(Value::as_str).map(str::to_string),
        registered_name,
    })
}

/// Upstream's `source`: `website` or `client`; absent or null is none.
pub fn parse_source(v: Option<&Value>) -> Result<Option<&'static str>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s == "website" => Ok(Some("website")),
        Some(Value::String(s)) if s == "client" => Ok(Some("client")),
        _ => Err("\"source\" must be \"website\" or \"client\" when provided".to_string()),
    }
}

/// The wire of one authorized credit (`credit` in both routes).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct AuthorizedCreditOut {
    pub id: String,
    pub amount: String,
    #[serde(rename = "availableAmount")]
    pub available_amount: String,
    /// Unix seconds.
    #[serde(rename = "expiresAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub expires_at: i64,
    pub signature: String,
    pub contract: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct LineOut {
    #[serde(rename = "usdCents")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub usd_cents: i64,
    #[serde(rename = "tradeId")]
    pub trade_id: Option<String>,
    #[serde(rename = "contractAddress")]
    pub contract_address: Option<String>,
    #[serde(rename = "itemId")]
    pub item_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct AuthorizeCreditOut {
    pub credit: AuthorizedCreditOut,
    #[serde(rename = "maxCreditedValue")]
    pub max_credited_value: String,
    #[serde(rename = "usdCents")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub usd_cents: i64,
    #[serde(rename = "oracleRate")]
    pub oracle_rate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<LineOut>>,
}

/// Every way the flow refuses, with the upstream status and body where one
/// exists and the data-plane envelope where the source is ours.
#[derive(Debug)]
pub enum AuthorizeError {
    /// 503: the lane is off or its signer/manager/index/gate is not ready.
    Unavailable {
        source: &'static str,
        configure: &'static str,
        message: String,
    },
    /// 400 with `items[i]: <reason>` for a batch, `<reason>` for one line.
    Line {
        index: usize,
        message: String,
    },
    /// 400: the rule refused the line (a resale, LAND, an unknown item...).
    Policy {
        index: usize,
        refusal: Refusal,
    },
    /// 409: the server's quote is not what the client displayed.
    PriceChanged {
        lines: Vec<(usize, i64, i64)>,
    },
    /// 402.
    Insufficient {
        balance_cents: i64,
        required_cents: i64,
    },
    /// 409.
    TooManyLive {
        balance_cents: i64,
    },
    /// 409: the request's `operationId` already authorized other lines.
    OperationReused,
    /// 403: a trial (`CREDITS_TRIAL_MODE`) whose buyers don't include the signer.
    NotATrialBuyer,
    Api(ApiError),
}

impl From<ApiError> for AuthorizeError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl AuthorizeError {
    /// `(status, body)`; the body always carries upstream's `error` and the
    /// credits envelope's `ok`/`message`.
    pub fn wire(&self, single: bool) -> (u16, Value) {
        let envelope = |status: u16, error: String, mut extra: serde_json::Map<String, Value>| {
            extra.insert("ok".into(), json!(false));
            extra.insert("error".into(), json!(error));
            extra.insert("message".into(), json!(error));
            (status, Value::Object(extra))
        };
        match self {
            Self::Unavailable {
                source,
                configure,
                message,
            } => {
                let mut extra = serde_json::Map::new();
                extra.insert("source".into(), json!(source));
                extra.insert("configure".into(), json!(configure));
                envelope(503, message.clone(), extra)
            }
            Self::Line { index, message } => envelope(
                400,
                if single {
                    message.clone()
                } else {
                    format!("items[{index}]: {message}")
                },
                Default::default(),
            ),
            Self::Policy { index, refusal } => {
                let mut extra = serde_json::Map::new();
                extra.insert("index".into(), json!(index));
                extra.insert("refusal".into(), json!(refusal_code(refusal)));
                let reason = refusal.to_string();
                envelope(
                    400,
                    if single {
                        reason
                    } else {
                        format!("items[{index}]: {reason}")
                    },
                    extra,
                )
            }
            Self::PriceChanged { lines } => {
                let mut extra = serde_json::Map::new();
                extra.insert(
                    "lines".into(),
                    json!(lines.iter().map(|(i, quoted, requested)| json!({ "index": i, "quotedCents": quoted, "requestedCents": requested })).collect::<Vec<_>>()),
                );
                envelope(409, "Price changed".to_string(), extra)
            }
            Self::Insufficient {
                balance_cents,
                required_cents,
            } => {
                let mut extra = serde_json::Map::new();
                extra.insert("balanceCents".into(), json!(balance_cents));
                extra.insert("requiredCents".into(), json!(required_cents));
                envelope(402, "Insufficient credits".to_string(), extra)
            }
            Self::TooManyLive { balance_cents } => {
                let mut extra = serde_json::Map::new();
                extra.insert("balanceCents".into(), json!(balance_cents));
                envelope(409, "Too many purchases in progress. Complete or cancel them before starting another one.".to_string(), extra)
            }
            Self::OperationReused => {
                let mut extra = serde_json::Map::new();
                extra.insert("code".into(), json!("operation_reused"));
                envelope(
                    409,
                    "This purchase was already authorized for other items. Start a new purchase."
                        .to_string(),
                    extra,
                )
            }
            Self::NotATrialBuyer => {
                let mut extra = serde_json::Map::new();
                extra.insert("code".into(), json!("not_a_trial_buyer"));
                envelope(
                    403,
                    "Credits can't be used by this account in this test.".to_string(),
                    extra,
                )
            }
            Self::Api(e) => {
                let status = e.status().as_u16();
                let message = if status >= 500 {
                    "internal error".to_string()
                } else {
                    e.to_string()
                };
                if status >= 500 {
                    tracing::error!(error = %e, "credits authorize failed");
                }
                envelope(status, message, Default::default())
            }
        }
    }
}

/// A stable code per refusal, for clients that switch on it.
pub fn refusal_code(r: &Refusal) -> &'static str {
    match r {
        Refusal::Incomplete { .. } => "incomplete",
        Refusal::UnknownItem { .. } => "unknown_item",
        Refusal::RecordMismatch { .. } => "record_mismatch",
        Refusal::NotMintable { .. } => "not_mintable",
        Refusal::UnsupportedTrade { .. } => "unsupported_trade",
        Refusal::NameTaken { .. } => "name_taken",
        Refusal::NotARegistrar { .. } => "not_a_registrar",
        Refusal::MethodNotAllowed { .. }
        | Refusal::CreditsNotAllowed { .. }
        | Refusal::SecondaryCreditSpend { .. } => "credits_not_allowed",
        Refusal::NonPositiveQuote { .. } => "non_positive_quote",
        Refusal::AmountOverflow { .. } => "amount_overflow",
        Refusal::NotSignedIn => "not_signed_in",
        Refusal::UndecodedCustomCall { .. } | Refusal::ForwardedCustomCall { .. } => "custom_call",
        Refusal::ManaRemainderNotAllowed { .. } => "mana_remainder_not_allowed",
        Refusal::AssetNotInGroup { .. }
        | Refusal::LineNotSettled { .. }
        | Refusal::WrongCall { .. } => "group_mismatch",
        Refusal::MixedGroup => "mixed_group",
        Refusal::CreditsChainUnsupported { .. } | Refusal::WrongChain { .. } => "chain",
        Refusal::GroupTooLarge { .. } | Refusal::EmptyGroup => "group_size",
        Refusal::NegativeDebit { .. } => "negative_debit",
        Refusal::CreditValueTooLarge { .. } => "credit_too_large",
        Refusal::SettlementNeedAboveReference { .. } => "settlement_need_above_reference",
        Refusal::UnlistedMarketplace { .. } => "unlisted_marketplace",
    }
}

/// A line the rule accepted, with what the store records for it.
pub(super) struct DerivedLine {
    pub(super) line: Authoritative,
    pub(super) quoted: UsdCents,
    charged: UsdCents,
    record: GroupLine,
}

fn u256(text: &str) -> Option<U256> {
    crate::types::parse_decimal(text).ok()
}

/// The signed trade the trades mirror serves (`shop::market::trades::Trade`)
/// as the rule reads it.
pub fn decode_trade(json: &Value) -> Result<SignedTrade, String> {
    let chain_id = json
        .get("chainId")
        .and_then(Value::as_u64)
        .ok_or("trade has no chainId")?;
    let contract = json
        .get("contract")
        .and_then(Value::as_str)
        .ok_or("trade has no contract")?;
    let signer = json
        .get("signer")
        .and_then(Value::as_str)
        .ok_or("trade has no signer")?;
    let venue = ContractRef {
        chain_id: ChainId(chain_id),
        contract: EvmAddress::parse(&contract.to_ascii_lowercase()).map_err(|e| e.to_string())?,
    };
    let signer = EvmAddress::parse(&signer.to_ascii_lowercase()).map_err(|e| e.to_string())?;
    let assets = |key: &str| -> Result<Vec<TradeAsset>, String> {
        json.get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("trade has no {key}"))?
            .iter()
            .map(|a| {
                let code = a
                    .get("assetType")
                    .and_then(Value::as_u64)
                    .ok_or("asset has no assetType")?;
                let asset_type = TradeAssetType::from_code(code);
                let contract = a
                    .get("contractAddress")
                    .and_then(Value::as_str)
                    .ok_or("asset has no contractAddress")?;
                let value_text = match asset_type {
                    TradeAssetType::Erc721 => a.get("tokenId").and_then(Value::as_str),
                    TradeAssetType::CollectionItem => a.get("itemId").and_then(Value::as_str),
                    _ => a.get("amount").and_then(Value::as_str),
                }
                .ok_or("asset has no value")?;
                Ok(TradeAsset {
                    asset_type,
                    contract: EvmAddress::parse(&contract.to_ascii_lowercase())
                        .map_err(|e| e.to_string())?,
                    value: u256(value_text).ok_or("asset value is not a decimal")?,
                })
            })
            .collect()
    };
    Ok(SignedTrade {
        venue,
        signer,
        sent: assets("sent")?,
        received: assets("received")?,
    })
}

fn kind_of(category: &str) -> Option<ItemKind> {
    match category {
        "wearable" => Some(ItemKind::Wearable),
        "emote" => Some(ItemKind::Emote),
        _ => None,
    }
}

/// The record of the item `(collection, item_id)` as the mirror has it, or
/// what its absence means.
async fn item_lookup(
    state: &State,
    chain_id: ChainId,
    collection: EvmAddress,
    item_id: U256,
) -> Result<Lookup<(ItemRecord, super::pricing::ItemInfo)>, ApiError> {
    let pairs = vec![(collection.to_string(), item_id.to_string())];
    let mut answers = match state.pricing.fetch_items_batch(&pairs).await {
        Ok(answers) => answers,
        Err(ApiError::ServiceUnavailable(_)) => return Ok(Lookup::Unknown),
        Err(e) => return Err(e),
    };
    match answers.pop() {
        Some(Ok(info)) => {
            let Some(kind) = kind_of(&info.category) else {
                return Ok(Lookup::Absent);
            };
            let creator =
                EvmAddress::parse(&info.creator.to_ascii_lowercase()).unwrap_or(collection);
            Ok(Lookup::Found((
                ItemRecord {
                    chain_id,
                    collection,
                    item_id,
                    kind,
                    creator,
                },
                info,
            )))
        }
        Some(Err(ApiError::NotFound(_))) | None => Ok(Lookup::Absent),
        Some(Err(ApiError::ServiceUnavailable(_))) => Ok(Lookup::Unknown),
        Some(Err(e)) => Err(e),
    }
}

/// The wallets a trade pays: its `received[].beneficiary` (an unparsable one is skipped;
/// the chain pays the signer for a zero beneficiary, which the signer check covers).
pub(crate) fn trade_payees(json: &Value) -> Vec<EvmAddress> {
    json.get("received")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("beneficiary").and_then(Value::as_str))
        .filter_map(|b| EvmAddress::parse(&b.to_ascii_lowercase()).ok())
        .collect()
}

/// Credits never pay a wallet for its own item: the buyer may not be the
/// item's creator or the trade's signer, who receive the proceeds.
fn refuse_own_sale(
    session: Session,
    index: usize,
    seller: EvmAddress,
) -> Result<(), AuthorizeError> {
    if pays_buyer(session, seller) {
        return Err(AuthorizeError::Api(ApiError::forbidden(format!(
            "items[{index}]: {OWN_SALE}"
        ))));
    }
    Ok(())
}

/// The refusal of a line that would pay the buyer's own wallet.
pub(crate) const OWN_SALE: &str = "credits cannot buy an item from the buyer's own wallet";

/// Whether paying `payee` pays the signed-in buyer.
pub(crate) fn pays_buyer(session: Session, payee: EvmAddress) -> bool {
    matches!(session, Session::Web3 { wallet } | Session::Web2 { account: wallet } if wallet == payee)
}

/// One line's authoritative facts from the mirrors and the decoded order,
/// through the rule; `chain_id` is the credits chain this node serves.
pub(super) async fn derive_line(
    state: &State,
    chain_id: ChainId,
    session: Session,
    rate: ManaUsdRate,
    index: usize,
    v: &ValidatedLine,
) -> Result<DerivedLine, AuthorizeError> {
    let policy_err = |refusal: Refusal| AuthorizeError::Policy { index, refusal };
    let (facts, record_identity): (Facts, (Option<String>, Option<String>)) = if let Some(label) =
        &v.registered_name
    {
        let _ = NameLabel::parse(label).map_err(|_| AuthorizeError::Line {
            index,
            message: "\"name\" must be 2-15 alphanumeric characters".to_string(),
        })?;
        return Err(AuthorizeError::Unavailable {
            source: NAME_EXECUTOR_SOURCE,
            configure: NAME_EXECUTOR_CONFIGURE,
            message: "NAME registration with credits is unavailable: no executor builds the registration on chain yet (docs/shop/CONTRACTS.md); register the NAME with MANA".to_string(),
        });
    } else if let Some(trade_id) = &v.trade_id {
        let (trade, trade_type) = match state.pricing.fetch_trade(trade_id).await? {
            TradeLookup::Open(json) => (
                {
                    for payee in trade_payees(&json) {
                        refuse_own_sale(session, index, payee)?;
                    }
                    decode_trade(&json).map_err(|m| AuthorizeError::Line {
                        index,
                        message: format!("trade {trade_id} cannot be decoded: {m}"),
                    })?
                },
                json.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            TradeLookup::NotFound => {
                return Err(AuthorizeError::Line {
                    index,
                    message: format!("trade {trade_id} is not in the trade book"),
                });
            }
            TradeLookup::Closed(why) => {
                return Err(AuthorizeError::Line {
                    index,
                    message: format!("trade {trade_id} is no longer open ({why})"),
                });
            }
        };
        if trade.venue.chain_id != chain_id {
            return Err(policy_err(Refusal::WrongChain {
                expected: chain_id,
                got: trade.venue.chain_id,
            }));
        }
        refuse_own_sale(session, index, trade.signer)?;
        // The coupon the line's purchase offer was priced with, chosen again
        // here by the same server rule: the authorized price is the offer's,
        // and a coupon that ended or was outbid since is a changed price.
        let coupon = state
            .pricing
            .fetch_trade_coupon(&trade, &trade_type)
            .await?;
        let mut contract = Lookup::Unknown;
        let mut item = Lookup::Unknown;
        let mut identity = (None, None);
        if let Some(sent) = trade.sent.first() {
            if sent.asset_type == TradeAssetType::CollectionItem {
                match item_lookup(state, chain_id, sent.contract, sent.value).await? {
                    Lookup::Found((record, _)) => {
                        refuse_own_sale(session, index, record.creator)?;
                        contract = Lookup::Found(ContractRole::Collection);
                        item = Lookup::Found(record);
                    }
                    Lookup::Absent => {
                        contract = Lookup::Absent;
                        item = Lookup::Absent;
                    }
                    Lookup::Unknown => {}
                }
                identity = (
                    Some(sent.contract.to_string()),
                    Some(sent.value.to_string()),
                );
            } else {
                identity = (Some(sent.contract.to_string()), None);
            }
        }
        let _ = identity;
        (
            Facts {
                session,
                offer: Offer::Trade { trade, coupon },
                contract,
                item,
                token: Lookup::Unknown,
            },
            (None, None),
        )
    } else if let (Some(collection), Some(item_id)) = (&v.contract_address, &v.item_id) {
        let collection = EvmAddress::parse(collection).map_err(|_| AuthorizeError::Line {
            index,
            message: "\"contractAddress\" must be a valid address when an item is given"
                .to_string(),
        })?;
        let item_id = u256(item_id).ok_or_else(|| AuthorizeError::Line {
            index,
            message: "\"itemId\" must be a numeric id when an item is given".to_string(),
        })?;
        let venue = ContractRef {
            chain_id,
            contract: collection_store(chain_id)
                .ok_or_else(|| policy_err(Refusal::CreditsChainUnsupported { chain_id }))?,
        };
        match item_lookup(state, chain_id, collection, item_id).await? {
            Lookup::Found((record, info)) => {
                refuse_own_sale(session, index, record.creator)?;
                if !info.store_mintable {
                    return Err(AuthorizeError::Line {
                        index,
                        message: format!(
                            "item {collection}:{item_id} is not on sale at the CollectionStore"
                        ),
                    });
                }
                let price = ManaWei::new(u256(&info.price_wei).ok_or_else(|| {
                    ApiError::Internal(format!(
                        "item {collection}:{item_id} has a non-decimal price"
                    ))
                })?);
                (
                    Facts {
                        session,
                        offer: Offer::StoreMint {
                            venue,
                            collection,
                            item_id,
                            price,
                        },
                        contract: Lookup::Found(ContractRole::Collection),
                        item: Lookup::Found(record),
                        token: Lookup::Absent,
                    },
                    (Some(collection.to_string()), Some(item_id.to_string())),
                )
            }
            Lookup::Absent => {
                return Err(policy_err(Refusal::UnknownItem {
                    asset: crate::types::AssetId::item(chain_id, collection, item_id),
                }));
            }
            Lookup::Unknown => {
                return Err(policy_err(Refusal::Incomplete {
                    missing: policy::MissingFact::CollectionItem,
                }));
            }
        }
    } else {
        return Err(AuthorizeError::Line { index, message: "a line names what it buys: a trade (\"tradeId\"), a store item (\"contractAddress\" and \"itemId\") or a NAME (\"name\")".to_string() });
    };
    let _ = record_identity;
    let line = Authoritative::derive(facts).map_err(policy_err)?;
    let quoted =
        match policy::quote(&line, PaymentMethod::Credits, Some(&rate)).map_err(policy_err)? {
            Quote::Credits { quoted } => quoted,
            Quote::Mana { .. } => {
                return Err(policy_err(Refusal::MethodNotAllowed {
                    asset: line.asset(),
                    method: PaymentMethod::Credits,
                    allowed: vec![],
                }));
            }
        };
    let charged = quoted.charge_for_quote().ok_or_else(|| {
        policy_err(Refusal::NonPositiveQuote {
            asset: line.asset(),
        })
    })?;
    let asset = line.asset();
    let (asset_item_id, asset_token_id) = match asset.token {
        TokenRef::Item { item_id } => (Some(item_id.to_string()), None),
        TokenRef::Token { token_id } => (None, Some(token_id.to_string())),
    };
    let record = GroupLine {
        quoted_cents: quoted.get(),
        charged_cents: charged.get(),
        sale_kind: match line.sale() {
            SaleKind::PrimaryMint => "primary_mint",
            SaleKind::PrimaryNameRegistration => "primary_name_registration",
            SaleKind::Secondary => "secondary",
        },
        asset_kind: match line.kind() {
            AssetKind::Wearable => "wearable",
            AssetKind::Emote => "emote",
            AssetKind::Name => "name",
            _ => "other",
        },
        asset_chain_id: asset.chain_id.0 as i64,
        asset_contract: asset.contract.to_string(),
        asset_item_id,
        asset_token_id,
        trade_id: v.trade_id.clone(),
        contract_address: v.contract_address.clone(),
        item_id: v.item_id.clone(),
        registered_name: v.registered_name.clone(),
    };
    Ok(DerivedLine {
        line,
        quoted,
        charged,
        record,
    })
}

/// The whole flow for `lines` on behalf of `signer` (lowercase, from the
/// verified auth chain); `single` shapes the messages.
pub async fn authorize_group(
    state: &State,
    signer: &str,
    lines: &[ValidatedLine],
    source: Option<&str>,
    idempotency_key: &str,
    request_digest: Option<&str>,
    single: bool,
) -> Result<AuthorizeCreditOut, AuthorizeError> {
    let settings = &state.settings;
    if !settings.credits_onchain_authorize_enabled {
        return Err(AuthorizeError::Unavailable {
            source: "credits-authorize",
            configure: "CREDITS_ONCHAIN_AUTHORIZE_ENABLED",
            message: "on-chain credit authorization is disabled; set CREDITS_ONCHAIN_AUTHORIZE_ENABLED=true to enable treasury-claim minting".to_string(),
        });
    }
    let (Some(signer_key), Some(contract)) = (
        settings.credits_signer_key.as_ref(),
        settings.credits_manager_contract.as_ref(),
    ) else {
        return Err(AuthorizeError::Unavailable {
            source: "credits-authorize",
            configure: "CREDITS_SIGNER_KEY_FILE",
            message:
                "credit authorization is unavailable (signer key or CreditsManager contract unset)"
                    .to_string(),
        });
    };
    if lines.is_empty() {
        return Err(AuthorizeError::Line {
            index: 0,
            message: "A non-empty \"items\" array is required".to_string(),
        });
    }
    if lines.len() > MAX_LINES_PER_GROUP {
        return Err(AuthorizeError::Line {
            index: 0,
            message: format!("A checkout may hold at most {MAX_LINES_PER_GROUP} items"),
        });
    }
    let wallet = TreasuryWallet::from_hex(signer_key)
        .map_err(|e| ApiError::Internal(format!("invalid credits signer key: {e}")))?;
    let buyer = EvmAddress::parse(signer)
        .map_err(|_| ApiError::bad_request("Invalid or missing signed-fetch authentication"))?;
    let chain_id = ChainId(settings.usage.chain_id as u64);
    let manager = EvmAddress::parse(contract).map_err(|_| {
        ApiError::Internal("CREDITS_MANAGER_CONTRACT is not an address".to_string())
    })?;

    // A purchase that names itself is recovered from what it reserved, before the
    // index, the gate, the feed, the catalogue or the price is read: those decide
    // whether to issue a new credit, never whether to hand back the one it has.
    if let Some(digest) = request_digest
        && let Some(prior) = state
            .credits
            .authorization_for_key(idempotency_key, signer)
            .await?
    {
        if prior.request_digest.as_deref() != Some(digest) {
            return Err(AuthorizeError::OperationReused);
        }
        let (issued, lines) =
            replayed(state, &prior.id, buyer, manager, chain_id, None, "").await?;
        return credit_out(&wallet, issued, lines, single, signer, true);
    }

    // A trial issues new credits only to the buyers it names. This is the one place a new
    // credit starts, after a named purchase is recovered above (its own credit and terms, no
    // new TTL) and before the index, the gate or a reservation: a buyer taken off the list
    // keeps what it was already given and gets nothing new.
    if let Some(trial) = &settings.trial
        && !trial.admits(signer)
    {
        return Err(AuthorizeError::NotATrialBuyer);
    }

    if state.usage.ready_at(chrono::Utc::now()).await.is_none() {
        return Err(AuthorizeError::Unavailable {
            source: usage::SOURCE,
            configure: usage::CONFIGURE,
            message: "credit authorization is unavailable: the CreditUsed index that consumes a pledge is not ready (not configured, catching up, or its last fold is older than 10 minutes)".to_string(),
        });
    }
    if let Err(why) = state.gate.check(manager.address(), wallet.signer()).await {
        return Err(AuthorizeError::Unavailable {
            source: super::settlement::GATE_SOURCE,
            configure: super::settlement::GATE_CONFIGURE,
            message: format!("credit issuance is refused: {why}"),
        });
    }

    let mana_usd = state.pricing.fetch_mana_usd().await?;
    let rate = ManaUsdRate::parse_usd_per_mana(&mana_usd).ok_or_else(|| {
        ApiError::Internal(format!(
            "oracle rate {mana_usd:?} is not a positive decimal"
        ))
    })?;
    let session = Session::Web3 { wallet: buyer };

    let mut derived = Vec::with_capacity(lines.len());
    for (index, v) in lines.iter().enumerate() {
        derived.push(derive_line(state, chain_id, session, rate, index, v).await?);
    }
    let changed: Vec<(usize, i64, i64)> = derived
        .iter()
        .zip(lines)
        .enumerate()
        .filter(|(_, (d, v))| d.quoted.get() != v.quoted_cents)
        .map(|(i, (d, v))| (i, d.quoted.get(), v.quoted_cents))
        .collect();
    if !changed.is_empty() {
        return Err(AuthorizeError::PriceChanged { lines: changed });
    }

    let mut quoted_total = UsdCents::ZERO;
    let mut charged_total = UsdCents::ZERO;
    for d in &derived {
        quoted_total = quoted_total
            .checked_add(d.quoted)
            .ok_or_else(|| ApiError::bad_request("the group total overflows"))?;
        charged_total = charged_total
            .checked_add(d.charged)
            .ok_or_else(|| ApiError::bad_request("the group total overflows"))?;
    }
    let mut sizing = CreditSizing::new(
        rate,
        settings.credit_buffer_bps,
        settings.credit_cap_headroom_pct,
    )
    .ok_or_else(|| {
        ApiError::Internal(
            "CREDITS_CAP_HEADROOM_PCT or CREDITS_ORACLE_BUFFER_BPS out of range".to_string(),
        )
    })?;
    let authoritative: Vec<Authoritative> = derived.iter().map(|d| d.line.clone()).collect();
    let policy_refusal = |refusal: Refusal| {
        let index = match &refusal {
            Refusal::MixedGroup => derived.len().saturating_sub(1),
            _ => 0,
        };
        AuthorizeError::Policy { index, refusal }
    };
    // A trade is only spent on a marketplace this node knows on its chain:
    // the contract a trade row names is not taken on trust.
    for (index, line) in authoritative.iter().enumerate() {
        if let Offer::Trade { trade, .. } = line.offer()
            && !listed_marketplace(trade.venue)
        {
            return Err(AuthorizeError::Policy {
                index,
                refusal: Refusal::UnlistedMarketplace { venue: trade.venue },
            });
        }
    }
    let guard = |value: ManaWei, sizing: CreditSizing| {
        policy::guard_group_credit_spend(
            &authoritative,
            &SettlementOperation::AuthorizeCredit {
                chain_id,
                value,
                sizing,
            },
        )
    };
    // A USD-pegged line settles at its marketplace's own feed, so the credit
    // is sized at that feed, read now. The group's every other rule is
    // checked first (a zero value passes the cap), so a refusal is never
    // hidden behind an unavailable feed.
    if let Some(venue) = authoritative
        .iter()
        .find(|line| policy::prices_in_usd(line))
        .and_then(|line| line.venue(PaymentMethod::Credits))
    {
        guard(ManaWei::ZERO, sizing).map_err(policy_refusal)?;
        // A retry of an authorization already reserved answers from its row,
        // without the feed; the reservation itself stays the race's arbiter.
        if let Some(prior) = state
            .credits
            .authorization_for_key(idempotency_key, signer)
            .await?
        {
            if request_digest.is_some() && prior.request_digest.as_deref() != request_digest {
                return Err(AuthorizeError::OperationReused);
            }
            let prior = prior.id;
            let (issued, lines) = replayed(
                state,
                &prior,
                buyer,
                manager,
                chain_id,
                None,
                &rate.rate().to_string(),
            )
            .await?;
            return credit_out(&wallet, issued, lines, single, signer, true);
        }
        // The feed costs RPC calls: a wallet the reservation would refuse
        // (no balance, too many live credits) is refused before it is read.
        match state
            .credits
            .would_refuse(signer, charged_total.get())
            .await?
        {
            Some(ReserveRefusal::Insufficient {
                balance_cents,
                required_cents,
            }) => {
                return Err(AuthorizeError::Insufficient {
                    balance_cents,
                    required_cents,
                });
            }
            Some(ReserveRefusal::TooManyLive { balance_cents }) => {
                return Err(AuthorizeError::TooManyLive { balance_cents });
            }
            Some(ReserveRefusal::OperationReused) => return Err(AuthorizeError::OperationReused),
            None => {}
        }
        let reading = state
            .feed
            .read(venue.chain_id.0, venue.contract.address())
            .await
            .map_err(|why| {
                // The provider's own text stays in the log: it may name the
                // endpoint's project or key in ways `redact` cannot see.
                tracing::warn!(marketplace = %venue.contract.address(), error = %why, "the marketplace's MANA/USD feed cannot be used");
                AuthorizeError::Unavailable {
                    source: super::feed::SOURCE,
                    configure: super::feed::CONFIGURE,
                    message: "a credit for a USD-pegged trade is sized at its marketplace's MANA/USD feed, which cannot be read now; try again later".to_string(),
                }
            })?;
        tracing::info!(marketplace = %venue.contract.address(), aggregator = %reading.aggregator, block = reading.block, updated_at = reading.updated_at_s, rate = %reading.rate.rate(), "sizing a USD-pegged credit group at its marketplace's feed");
        sizing = sizing.with_settlement_rate(reading.rate);
    }
    let value = policy::credit_cap(&authoritative, &sizing).map_err(|refusal| match refusal {
        Refusal::SettlementNeedAboveReference { need, reference } => {
            tracing::warn!(need = %need, reference = %reference, "a USD-pegged credit group needs more at its marketplace's feed than its quote allows");
            AuthorizeError::Unavailable {
            source: super::feed::DIVERGED_SOURCE,
            configure: super::feed::DIVERGED_CONFIGURE,
            message: format!(
                "at the marketplace's MANA/USD feed this purchase needs {need} wei, above the {reference} wei its quote allows a credit to carry; try again later"
            ),
        }
        }
        refusal => policy_refusal(refusal),
    })?;
    // Defense in depth: the guard recomputes the same `credit_cap` from the
    // same inputs, so it only fails if the sizing above stops using it.
    guard(value, sizing).map_err(policy_refusal)?;
    let recorded_rate = sizing.recorded_rate();

    let salt = B256::from_slice(
        &getrandom_32().ok_or_else(|| ApiError::Internal("CSPRNG unavailable".into()))?,
    );
    let salt_hex = bytes32_hex(salt);
    let now = chrono::Utc::now();
    let expires_at_s = now.timestamp() as u64 + settings.credit_ttl_seconds;
    let expires_at = chrono::DateTime::from_timestamp(expires_at_s as i64, 0)
        .ok_or_else(|| ApiError::Internal("expiry out of range".into()))?
        .naive_utc();
    let identity = CreditIdentity {
        sender: buyer.address(),
        chain_id: chain_id.0,
        manager: manager.address(),
        value: value.get(),
        expires_at: expires_at_s,
        salt,
    };
    let credit_hash = bytes32_hex(identity.credit_hash());
    let records: Vec<GroupLine> = derived.iter().map(|d| d.record.clone()).collect();
    let group = NewGroup {
        id: &salt_hex,
        address: signer,
        chain_id: chain_id.0 as i64,
        contract: &manager.to_string(),
        credit_hash: &credit_hash,
        quoted_cents: quoted_total.get(),
        charged_cents: charged_total.get(),
        amount_wei: &value.get().to_string(),
        oracle_rate: &recorded_rate.rate().to_string(),
        oracle_decimals: recorded_rate.decimals() as i16,
        source,
        expires_at,
        lines: &records,
    };
    let reserved = match state
        .credits
        .reserve_credit_group(&group, idempotency_key, request_digest)
        .await?
    {
        Ok(reserved) => reserved,
        Err(ReserveRefusal::Insufficient {
            balance_cents,
            required_cents,
        }) => {
            return Err(AuthorizeError::Insufficient {
                balance_cents,
                required_cents,
            });
        }
        Err(ReserveRefusal::TooManyLive { balance_cents }) => {
            return Err(AuthorizeError::TooManyLive { balance_cents });
        }
        Err(ReserveRefusal::OperationReused) => return Err(AuthorizeError::OperationReused),
    };
    let (issued, issued_lines) = if reserved.replayed {
        replayed(
            state,
            &reserved.id,
            buyer,
            manager,
            chain_id,
            Some(value),
            &recorded_rate.rate().to_string(),
        )
        .await?
    } else {
        (
            (
                identity,
                value.get().to_string(),
                charged_total.get(),
                recorded_rate.rate().to_string(),
            ),
            records.clone(),
        )
    };
    credit_out(
        &wallet,
        issued,
        issued_lines,
        single,
        signer,
        reserved.replayed,
    )
}

/// What a credit answers with: its identity, signed value, cents and rate.
type Issued = (CreditIdentity, String, i64, String);

/// A reserved authorization, answered from its row: the stored value, salt,
/// expiry, rate and lines, never re-sized or re-reserved. `fallback_value`
/// and `fallback_rate` stand in only for columns an older row lacks.
async fn replayed(
    state: &State,
    id: &str,
    buyer: EvmAddress,
    manager: EvmAddress,
    chain_id: ChainId,
    fallback_value: Option<ManaWei>,
    fallback_rate: &str,
) -> Result<(Issued, Vec<GroupLine>), AuthorizeError> {
    let row = state
        .credits
        .get_authorization(id)
        .await?
        .ok_or_else(|| ApiError::Internal("the replayed authorization vanished".into()))?;
    let stored_manager = row
        .contract
        .as_deref()
        .and_then(|c| Address::from_str(c).ok())
        .unwrap_or(manager.address());
    let value = u256(&row.amount_wei)
        .or(fallback_value.map(ManaWei::get))
        .ok_or_else(|| ApiError::Internal("a replayed authorization has no value".into()))?;
    let identity = CreditIdentity {
        sender: buyer.address(),
        chain_id: row.chain_id.unwrap_or(chain_id.0 as i64) as u64,
        manager: stored_manager,
        value,
        expires_at: row.expires_at.and_utc().timestamp() as u64,
        salt: B256::from_str(&row.id).map_err(|_| {
            ApiError::Internal("a replayed authorization has a non-bytes32 id".into())
        })?,
    };
    let lines = state.credits.authorization_lines(&row.id).await?;
    Ok((
        (
            identity,
            row.amount_wei.clone(),
            row.usd_cents,
            row.oracle_rate.unwrap_or_else(|| fallback_rate.to_string()),
        ),
        lines,
    ))
}

fn credit_out(
    wallet: &TreasuryWallet,
    issued: Issued,
    issued_lines: Vec<GroupLine>,
    single: bool,
    signer: &str,
    replayed: bool,
) -> Result<AuthorizeCreditOut, AuthorizeError> {
    let (identity, amount, usd_cents, oracle_rate) = issued;
    let signature = wallet
        .sign_credit(&identity)
        .map_err(|e| ApiError::Internal(format!("credit signing failed: {e}")))?;
    tracing::info!(address = %signer, salt = %bytes32_hex(identity.salt), usd_cents, value = %amount, lines = issued_lines.len(), replayed, "authorized a credit");
    Ok(AuthorizeCreditOut {
        credit: AuthorizedCreditOut {
            id: bytes32_hex(identity.salt),
            amount: amount.clone(),
            available_amount: amount.clone(),
            expires_at: identity.expires_at as i64,
            signature,
            contract: format!("{:#x}", identity.manager),
        },
        max_credited_value: amount,
        usd_cents,
        oracle_rate,
        lines: (!single).then(|| {
            issued_lines
                .iter()
                .map(|l| LineOut {
                    usd_cents: l.charged_cents,
                    trade_id: l.trade_id.clone(),
                    contract_address: l.contract_address.clone(),
                    item_id: l.item_id.clone(),
                    name: l.registered_name.clone(),
                })
                .collect()
        }),
    })
}

/// Whether `venue` is one of the off-chain marketplaces deployed on its chain.
/// A marketplace the chain's CreditsManager lets `useCredits` call: the set
/// the contract itself enforces (its allowed `marketplaces`), so a mirrored
/// trade on any of them, the first off-chain marketplace included, can be
/// paid with credits.
fn listed_marketplace(venue: ContractRef) -> bool {
    crate::policy_entry::manager_routes(venue.chain_id)
        .is_some_and(|routes| routes.marketplaces.contains(&venue.contract.address()))
}

fn getrandom_32() -> Option<[u8; 32]> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buyer_cannot_pay_itself_with_credits() {
        let me = EvmAddress::parse("0x2222222222222222222222222222222222222222").unwrap();
        let seller = EvmAddress::parse("0x3333333333333333333333333333333333333333").unwrap();
        let err = refuse_own_sale(Session::Web3 { wallet: me }, 2, me).unwrap_err();
        assert!(
            matches!(&err, AuthorizeError::Api(ApiError::Forbidden(m)) if m.starts_with("items[2]")),
            "got {err:?}"
        );
        assert!(refuse_own_sale(Session::Web3 { wallet: me }, 0, seller).is_ok());
        let trade = json!({ "received": [
            { "beneficiary": "0x3333333333333333333333333333333333333333" },
            { "beneficiary": "0x2222222222222222222222222222222222222222" },
        ] });
        assert_eq!(
            trade_payees(&trade),
            vec![seller, me],
            "the payee may be the buyer"
        );
    }

    #[test]
    fn line_validation_is_upstreams_message_for_message() {
        let err = |v: Value| validate_line(&v).unwrap_err();
        assert_eq!(
            err(json!({})),
            "A positive integer \"usdPriceCents\" is required"
        );
        assert_eq!(
            err(json!({ "usdPriceCents": 12.5 })),
            "A positive integer \"usdPriceCents\" is required"
        );
        assert_eq!(
            err(json!({ "usdPriceCents": 0 })),
            "A positive integer \"usdPriceCents\" is required"
        );
        assert_eq!(
            err(json!({ "usdPriceCents": 30, "contractAddress": "nope", "itemId": "12" })),
            "\"contractAddress\" must be a valid address when an item is given"
        );
        assert_eq!(
            err(
                json!({ "usdPriceCents": 30, "contractAddress": "0x3b5306be0da3202a5e7b00d1acc16a46cd88dfdc", "itemId": "DROP TABLE" })
            ),
            "\"itemId\" must be a numeric id when an item is given"
        );
        assert_eq!(
            err(
                json!({ "usdPriceCents": 30, "contractAddress": "0x3b5306be0da3202a5e7b00d1acc16a46cd88dfdc" })
            ),
            "\"itemId\" must be a numeric id when an item is given"
        );
        assert_eq!(
            err(json!({ "usdPriceCents": 30, "itemId": "12" })),
            "\"contractAddress\" must be a valid address when an item is given"
        );
        for name in ["abcdefghijklmnop", "a", "drop table", "mauri.dcl.eth"] {
            assert_eq!(
                err(json!({ "usdPriceCents": 30, "name": name })),
                "\"name\" must be 2-15 alphanumeric characters",
                "{name}"
            );
        }
        assert_eq!(
            err(json!({ "usdPriceCents": 30, "tradeId": "" })),
            "\"tradeId\" must be a non-empty string of at most 100 characters"
        );
        let ok = validate_line(&json!({ "usdPriceCents": 184, "tradeId": "trade-99" })).unwrap();
        assert_eq!(
            (ok.quoted_cents, ok.charged_cents, ok.trade_id.as_deref()),
            (184, 190, Some("trade-99")),
            "the charge rounds up, the quote does not"
        );
        let ok = validate_line(&json!({ "usdPriceCents": 30, "contractAddress": "0x3B5306be0da3202a5e7b00d1acc16a46cd88dfdc", "itemId": "12", "name": "mauri" })).unwrap();
        assert_eq!(
            ok.contract_address.as_deref(),
            Some("0x3b5306be0da3202a5e7b00d1acc16a46cd88dfdc")
        );
        assert_eq!(ok.registered_name.as_deref(), Some("mauri"));
        assert_eq!(
            parse_source(Some(&json!("telegram"))).unwrap_err(),
            "\"source\" must be \"website\" or \"client\" when provided"
        );
        assert_eq!(
            parse_source(Some(&json!("wherever"))).unwrap_err(),
            "\"source\" must be \"website\" or \"client\" when provided"
        );
        assert_eq!(parse_source(Some(&json!(null))).unwrap(), None);
        assert_eq!(
            parse_source(Some(&json!("client"))).unwrap(),
            Some("client")
        );
    }

    #[test]
    fn the_refusal_bodies_carry_upstreams_fields() {
        let (status, body) = AuthorizeError::Insufficient {
            balance_cents: 150,
            required_cents: 200,
        }
        .wire(true);
        assert_eq!(status, 402);
        assert_eq!(body["error"], "Insufficient credits");
        assert_eq!(
            (
                body["balanceCents"].as_i64(),
                body["requiredCents"].as_i64()
            ),
            (Some(150), Some(200))
        );
        let (status, body) = AuthorizeError::TooManyLive {
            balance_cents: 5000,
        }
        .wire(false);
        assert_eq!((status, body["balanceCents"].as_i64()), (409, Some(5000)));
        assert_eq!(
            body["error"],
            "Too many purchases in progress. Complete or cancel them before starting another one."
        );
        let (status, body) = AuthorizeError::Line {
            index: 1,
            message: "\"itemId\" must be a numeric id when an item is given".into(),
        }
        .wire(false);
        assert_eq!(
            (status, body["error"].as_str()),
            (
                400,
                Some("items[1]: \"itemId\" must be a numeric id when an item is given")
            )
        );
        let (status, body) = AuthorizeError::PriceChanged {
            lines: vec![(0, 250, 200)],
        }
        .wire(true);
        assert_eq!(
            (status, body["error"].as_str()),
            (409, Some("Price changed"))
        );
        assert_eq!(
            body["lines"][0],
            json!({ "index": 0, "quotedCents": 250, "requestedCents": 200 })
        );
        let (status, body) = AuthorizeError::Unavailable {
            source: "x",
            configure: "Y",
            message: "m".into(),
        }
        .wire(true);
        assert_eq!(
            (
                status,
                body["source"].as_str(),
                body["configure"].as_str(),
                body["ok"].as_bool()
            ),
            (503, Some("x"), Some("Y"), Some(false))
        );
    }

    #[test]
    fn a_trade_decodes_to_the_rules_signed_trade() {
        let json = json!({
            "id": "t", "signer": "0x00000000000000000000000000000000000000AA", "chainId": 137, "contract": "0x540fb08edb56aae562864b390542c97f562825ba",
            "sent": [{ "assetType": 4, "contractAddress": "0x00000000000000000000000000000000000000c1", "itemId": "7", "extra": "0x" }],
            "received": [{ "assetType": 2, "contractAddress": "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4", "amount": "1500000000000000000", "extra": "0x" }]
        });
        let trade = decode_trade(&json).unwrap();
        assert_eq!(trade.venue.chain_id, ChainId::POLYGON);
        assert_eq!(trade.sent[0].asset_type, TradeAssetType::CollectionItem);
        assert_eq!(trade.sent[0].value, U256::from(7));
        assert_eq!(trade.received[0].asset_type, TradeAssetType::UsdPeggedMana);
        assert!(decode_trade(&json!({ "chainId": 137 })).is_err());
        assert_eq!(
            collection_store(ChainId::POLYGON).unwrap().to_string(),
            "0x214ffc0f0103735728dc66b61a22e4f163e275ae"
        );
        assert!(collection_store(ChainId::ETHEREUM).is_none());
    }
}
