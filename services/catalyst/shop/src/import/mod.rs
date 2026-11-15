// from: shop@94584a5c:app/src/lib/import.ts (fetchImportable, importListing, ImportRestrictedError: the two-phase import and its primary-only restriction), marketplace-server@ae872a4a:src/controllers/routes.ts:133 (GET /v3/catalog/importable), shop/src/paths.rs CREDIT_PATHS (the import-check row)
//! Listing import: a seller's classic MANA listings moving into the Shop as
//! credit-buyable, USD-pegged listings. The read is the catalog's
//! `GET /v3/catalog/importable` (`market::shop::importable_listings`, its
//! wire unchanged); this module is the rule in front of the write.
//!
//! Upstream re-signed any of the seller's listings, an owned token
//! included, as a USD-pegged order the credits checkout could fulfil; the
//! Shop restricts the import to eligible PRIMARY inventory (SHOP.md,
//! "Restrict the credits listing-import flow to eligible primary
//! inventory"): a collection item the signer created, listed as a
//! `COLLECTION_ITEM` trade on Polygon. A token the seller owns, their own
//! creation included once minted, is a resale: its existing listing stays
//! a MANA listing, and a USD-pegged resale order settles in MANA at a
//! current quote (`settlesNowWei`). `POST /v1/import-checks` (signed by the
//! seller) answers that decision BEFORE the client cancels the old listing,
//! from authoritative records only: the decoded assets of the old trade
//! (`shop.trade_assets*`), the collections and items mirrors (the contract
//! role, the item's creator and kind) and the payment rule
//! (`policy::guard_credit_spend` with `SettlementOperation::CreditListing`).
//! Nothing here trusts the `listingType` the catalog row or the client
//! says. A mirror without a complete pass is the unavailable envelope,
//! never "not eligible": an unknown answer is not a refusal.
//!
//! Money: MANA in exact wei, USD in integer cents, one credit = 10 cents,
//! whole-credit rounding up (`types::UsdCents::credits_ceil`); the rate is
//! the `economy.price_snapshots` decimal text read exactly, no float.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};

use super::market::listings::offchain_marketplace_v3;
use super::market::mirrors::{Mirror, require_ready};
use super::market::signed::{require_not_scene_signer, require_signer};
use super::market::wire::ApiError;
use super::market::{MarketplaceState, SharedState, is_valid_eth_address};
use super::policy::{Authoritative, Facts, Reason, Refusal, eligibility, guard_credit_spend};
use super::types::{
    AssetKind, ChainId, ContractRef, ContractRole, EvmAddress, ItemKind, ItemRecord, Lookup,
    ManaUsdRate, ManaWei, Offer, PaymentMethod, SaleKind, Session, SettlementOperation,
    SignedTrade, TradeAsset, TradeAssetType, parse_decimal,
};

pub const CHECK_PATH: &str = "/v1/import-checks";

/// `POST /v1/import-checks`: the listing to import, by the old trade this
/// node stores or by the asset itself.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCheckBody {
    pub old_trade_id: Option<String>,
    pub chain_id: Option<u64>,
    pub contract_address: Option<String>,
    pub item_id: Option<String>,
    pub token_id: Option<String>,
}

/// How the imported listing would settle for a buyer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Settlement {
    /// A USD-pegged listing buyers pay in credits (MANA second for a web3 wallet).
    Credits,
    /// MANA only: the listing keeps its MANA terms, a USD-pegged one settles at a current quote.
    Mana,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RefusalOut {
    pub code: String,
    pub message: String,
}

impl From<&Refusal> for RefusalOut {
    fn from(r: &Refusal) -> Self {
        let debug = format!("{r:?}");
        let code: String = debug
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        Self {
            code: lower_camel(&code),
            message: r.to_string(),
        }
    }
}

fn lower_camel(pascal: &str) -> String {
    let mut chars = pascal.chars();
    match chars.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetOut {
    pub chain_id: u64,
    pub contract_address: String,
    pub item_id: Option<String>,
    pub token_id: Option<String>,
}

/// The seller's old price and what it is in credits today; for a USD-pegged
/// resale order, the MANA it settles for now.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QuoteOut {
    pub mana_wei: Option<String>,
    pub usd_cents: Option<i64>,
    pub credits: Option<i64>,
    pub rate_usd_per_mana: String,
    pub rate_taken_at: i64,
    pub settles_now_wei: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImportCheck {
    pub sale_kind: Option<SaleKind>,
    pub asset_kind: Option<AssetKind>,
    pub eligible: bool,
    pub settlement: Option<Settlement>,
    pub reasons: Vec<Reason>,
    pub refusal: Option<RefusalOut>,
    pub seller_is_creator: bool,
    pub asset: AssetOut,
    pub quote: Option<QuoteOut>,
    /// Why `quote` is null: the source that has no answer yet.
    pub quote_unavailable: Option<&'static str>,
}

/// One old listing as the decoded trade rows describe it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredListing {
    pub trade_id: String,
    pub signer: EvmAddress,
    pub chain_id: ChainId,
    pub venue: EvmAddress,
    pub sent: Vec<TradeAsset>,
    /// `(asset type, amount)` of the received fungible asset.
    pub received: Option<(TradeAssetType, alloy_primitives::U256)>,
}

impl StoredListing {
    /// The sale kind from the decoded assets alone: any ERC-721 sent is an
    /// existing token, a resale, whoever the signer is.
    pub fn sale_kind(&self) -> SaleKind {
        if self
            .sent
            .iter()
            .any(|a| a.asset_type == TradeAssetType::Erc721)
        {
            SaleKind::Secondary
        } else {
            SaleKind::PrimaryMint
        }
    }

    /// The old price in MANA wei (a MANA listing), or None for a USD-pegged one.
    pub fn mana_price(&self) -> Option<ManaWei> {
        match self.received {
            Some((TradeAssetType::Erc20, amount)) => Some(ManaWei::new(amount)),
            _ => None,
        }
    }

    pub fn usd_pegged_wei(&self) -> Option<alloy_primitives::U256> {
        match self.received {
            Some((TradeAssetType::UsdPeggedMana, amount)) => Some(amount),
            _ => None,
        }
    }
}

const STORED_LISTING_SQL: &str = "SELECT t.id::text AS trade_id, t.signer, t.chain_id, t.contract AS venue, a.direction::text AS direction, a.asset_type, a.contract_address, \
        COALESCE(e.token_id, it.item_id, f.amount::text) AS value \
     FROM shop.trades t JOIN shop.trade_assets a ON a.trade_id = t.id \
     LEFT JOIN shop.trade_assets_erc721 e ON e.asset_id = a.id \
     LEFT JOIN shop.trade_assets_item it ON it.asset_id = a.id \
     LEFT JOIN shop.trade_assets_erc20 f ON f.asset_id = a.id \
     WHERE t.id = $1::uuid ORDER BY a.created_at, a.id";

pub async fn stored_listing(
    pool: &PgPool,
    trade_id: &str,
) -> Result<Option<StoredListing>, ApiError> {
    if !super::market::lists::is_uuid(trade_id) {
        return Ok(None);
    }
    let rows = sqlx::query(STORED_LISTING_SQL)
        .bind(trade_id)
        .fetch_all(pool)
        .await?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let signer = EvmAddress::parse(&first.get::<String, _>("signer"))
        .map_err(|e| ApiError::internal(format!("stored signer: {e}")))?;
    let venue_text: String = first.get("venue");
    let venue =
        EvmAddress::parse(&venue_text).unwrap_or(EvmAddress::new(alloy_primitives::Address::ZERO));
    let chain_id = ChainId(first.get::<i32, _>("chain_id") as u64);
    let mut sent = Vec::new();
    let mut received = None;
    for r in &rows {
        let asset_type = TradeAssetType::from_code(r.get::<i16, _>("asset_type") as u64);
        let contract = EvmAddress::parse(&r.get::<String, _>("contract_address"))
            .map_err(|e| ApiError::internal(format!("stored asset contract: {e}")))?;
        let value = parse_decimal(&r.get::<Option<String>, _>("value").unwrap_or_default())
            .map_err(|e| ApiError::internal(format!("stored asset value: {e}")))?;
        if r.get::<String, _>("direction") == "sent" {
            sent.push(TradeAsset {
                asset_type,
                contract,
                value,
            });
        } else if received.is_none() && asset_type.is_mana() {
            received = Some((asset_type, value));
        }
    }
    Ok(Some(StoredListing {
        trade_id: first.get("trade_id"),
        signer,
        chain_id,
        venue,
        sent,
        received,
    }))
}

/// What the mirrors say about the collection and the item; `Unknown` only
/// when the mirror is not ready, which the caller has already refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorFacts {
    pub contract: Lookup<ContractRole>,
    pub item: Lookup<ItemRecord>,
    /// The collection's creator, minters and managers: the wallets whose
    /// item listing is a creator's own.
    pub sellers: Vec<EvmAddress>,
}

pub async fn mirror_facts(
    pool: &PgPool,
    chain_id: ChainId,
    contract: EvmAddress,
    item_id: Option<alloy_primitives::U256>,
) -> Result<MirrorFacts, ApiError> {
    let contract_text = contract.to_string().to_lowercase();
    let collection = sqlx::query("SELECT creator, minters, managers FROM shop.collections WHERE contract_address = $1 ORDER BY (chain_id = $2) DESC LIMIT 1")
        .bind(&contract_text)
        .bind(chain_id.0 as i32)
        .fetch_optional(pool)
        .await?;
    let mut sellers = Vec::new();
    let role = match &collection {
        Some(row) => {
            let mut names: Vec<String> = vec![row.get("creator")];
            names.extend(row.get::<Vec<String>, _>("minters"));
            names.extend(row.get::<Vec<String>, _>("managers"));
            sellers = names
                .iter()
                .filter_map(|a| EvmAddress::parse(a).ok())
                .collect();
            Lookup::Found(ContractRole::Collection)
        }
        None => Lookup::Absent,
    };
    let item = match item_id {
        Some(item_id) => {
            let row = sqlx::query("SELECT creator, item_type, chain_id FROM shop.items WHERE contract_address = $1 AND item_id = $2::numeric ORDER BY (chain_id = $3) DESC LIMIT 1")
                .bind(&contract_text)
                .bind(item_id.to_string())
                .bind(chain_id.0 as i32)
                .fetch_optional(pool)
                .await?;
            match row {
                Some(r) => {
                    let creator = EvmAddress::parse(&r.get::<String, _>("creator"))
                        .map_err(|e| ApiError::internal(format!("mirrored creator: {e}")))?;
                    let kind = if r
                        .get::<String, _>("item_type")
                        .to_lowercase()
                        .starts_with("emote")
                    {
                        ItemKind::Emote
                    } else {
                        ItemKind::Wearable
                    };
                    Lookup::Found(ItemRecord {
                        chain_id: ChainId(r.get::<i32, _>("chain_id") as u64),
                        collection: contract,
                        item_id,
                        kind,
                        creator,
                    })
                }
                None => Lookup::Absent,
            }
        }
        None => Lookup::Absent,
    };
    Ok(MirrorFacts {
        contract: role,
        item,
        sellers,
    })
}

/// The proposed Shop listing as a decoded trade: the same assets sent, one
/// USD-pegged MANA asset received (the amount is not the rule's input).
pub fn proposed_trade(
    chain_id: ChainId,
    signer: EvmAddress,
    sent: Vec<TradeAsset>,
) -> Option<SignedTrade> {
    let market = offchain_marketplace_v3(chain_id.0 as i64)?;
    let venue = ContractRef {
        chain_id,
        contract: EvmAddress::parse(market.address).ok()?,
    };
    let mana = crate::evm::contracts_addrs::DclContracts::for_chain(chain_id.0)
        .map(|c| EvmAddress::new(c.mana_token))
        .unwrap_or(EvmAddress::new(alloy_primitives::Address::ZERO));
    Some(SignedTrade {
        venue,
        signer,
        sent,
        received: vec![TradeAsset {
            asset_type: TradeAssetType::UsdPeggedMana,
            contract: mana,
            value: alloy_primitives::U256::from(1u64),
        }],
    })
}

/// The decision without its quote: the rule over the facts, plus the
/// creator check an item listing needs (an item order by a wallet the
/// collection does not name is nobody's creation).
pub fn decide(signer: EvmAddress, trade: SignedTrade, facts: &MirrorFacts) -> ImportCheck {
    let first = trade.sent.first().cloned();
    let asset = AssetOut {
        chain_id: trade.venue.chain_id.0,
        contract_address: first
            .as_ref()
            .map(|a| a.contract.to_string().to_lowercase())
            .unwrap_or_default(),
        item_id: first
            .as_ref()
            .filter(|a| a.asset_type == TradeAssetType::CollectionItem)
            .map(|a| a.value.to_string()),
        token_id: first
            .as_ref()
            .filter(|a| a.asset_type == TradeAssetType::Erc721)
            .map(|a| a.value.to_string()),
    };
    let seller_is_creator = match &facts.item {
        Lookup::Found(item) => item.creator == signer || facts.sellers.contains(&signer),
        _ => facts.sellers.contains(&signer),
    };
    let token = match first.as_ref().map(|a| a.asset_type) {
        Some(TradeAssetType::Erc721) => Lookup::Unknown,
        _ => Lookup::Absent,
    };
    let facts_for_rule = Facts {
        session: Session::Web3 { wallet: signer },
        offer: Offer::Trade {
            trade: trade.clone(),
            coupon: None,
        },
        contract: facts.contract.clone(),
        item: facts.item.clone(),
        token,
    };
    let line = match Authoritative::derive(facts_for_rule) {
        Ok(line) => line,
        Err(refusal) => {
            return ImportCheck {
                sale_kind: None,
                asset_kind: None,
                eligible: false,
                settlement: None,
                reasons: Vec::new(),
                refusal: Some(RefusalOut::from(&refusal)),
                seller_is_creator,
                asset,
                quote: None,
                quote_unavailable: None,
            };
        }
    };
    let offered = eligibility(&line);
    let credit_listing = guard_credit_spend(&line, &SettlementOperation::CreditListing { trade });
    let (eligible, settlement, refusal) = match credit_listing {
        Ok(()) if line.sale() == SaleKind::PrimaryMint && !seller_is_creator => (
            false,
            None,
            Some(RefusalOut {
                code: "notTheCreator".into(),
                message:
                    "only the creator of an item lists it for credits; a token you own is a resale"
                        .into(),
            }),
        ),
        Ok(()) => (true, Some(Settlement::Credits), None),
        Err(refusal) => (
            false,
            offered
                .allows(PaymentMethod::Mana)
                .then_some(Settlement::Mana),
            Some(RefusalOut::from(&refusal)),
        ),
    };
    ImportCheck {
        sale_kind: Some(line.sale()),
        asset_kind: Some(line.kind()),
        eligible,
        settlement,
        reasons: offered.reasons,
        refusal,
        seller_is_creator,
        asset,
        quote: None,
        quote_unavailable: None,
    }
}

/// The Shop's MANA/USD as an exact rate and when it was read: the
/// marketplace's feed where it is wired (`mana::feed_rate::current`; no
/// current answer is no rate), else the newest price snapshot of any source.
pub async fn current_rate(pool: &PgPool) -> Result<Option<(ManaUsdRate, i64)>, ApiError> {
    if let Some(feed) = crate::mana::feed_rate::current() {
        return Ok(feed
            .ok()
            .map(|shop| (shop.rate, shop.updated_at.timestamp())));
    }
    let row = crate::mana::quote::newest_spot(pool).await?;
    Ok(row.and_then(|(text, taken_at)| {
        ManaUsdRate::parse_usd_per_mana(&text).map(|rate| (rate, taken_at))
    }))
}

/// Upstream `fetchImportable`'s suggested price: the old MANA price in
/// credits at today's rate, rounded up; a USD-pegged old price in the MANA
/// it settles for now.
pub fn quote_for(listing: Option<&StoredListing>, rate: ManaUsdRate, taken_at: i64) -> QuoteOut {
    let mana_price = listing.and_then(StoredListing::mana_price);
    let cents = mana_price.and_then(|wei| wei.usd_cents_ceil(rate));
    QuoteOut {
        mana_wei: mana_price.map(|w| w.get().to_string()),
        usd_cents: cents.map(|c| c.get()),
        credits: cents.map(|c| c.credits_ceil()),
        rate_usd_per_mana: rate_text(rate),
        rate_taken_at: taken_at,
        settles_now_wei: listing
            .and_then(StoredListing::usd_pegged_wei)
            .and_then(|usd_wei| ManaWei::from_usd_wei(usd_wei, rate))
            .map(|w| w.get().to_string()),
    }
}

fn rate_text(rate: ManaUsdRate) -> String {
    rate.decimal_text()
}

fn bad(message: impl Into<String>) -> Response {
    ApiError::bad_request(message).into_response()
}

async fn check_inner(
    state: &MarketplaceState,
    headers: &HeaderMap,
    body: ImportCheckBody,
) -> Result<Response, Response> {
    require_not_scene_signer(headers).map_err(IntoResponse::into_response)?;
    let signer_text = require_signer(headers, "post", CHECK_PATH)
        .await
        .map_err(IntoResponse::into_response)?;
    let signer =
        EvmAddress::parse(&signer_text).map_err(|_| bad("the signer is not an address"))?;
    require_ready(state, Mirror::Collections).await?;
    require_ready(state, Mirror::Items).await?;
    let listing = match &body.old_trade_id {
        Some(id) => {
            let listing = stored_listing(&state.pool, id)
                .await
                .map_err(IntoResponse::into_response)?
                .ok_or_else(|| {
                    ApiError::not_found("the old listing is not one this node stores")
                        .into_response()
                })?;
            if listing.signer != signer {
                return Err(
                    ApiError::http(403, "the old listing is not the signer's").into_response()
                );
            }
            Some(listing)
        }
        None => None,
    };
    let (chain_id, sent) = match &listing {
        Some(l) => (l.chain_id, l.sent.clone()),
        None => {
            let chain_id = ChainId(
                body.chain_id
                    .ok_or_else(|| bad("chainId is required without oldTradeId"))?,
            );
            let contract_text = body
                .contract_address
                .as_deref()
                .filter(|a| is_valid_eth_address(a))
                .ok_or_else(|| bad("contractAddress must be an address"))?;
            let contract = EvmAddress::parse(contract_text)
                .map_err(|_| bad("contractAddress must be an address"))?;
            let asset = match (&body.item_id, &body.token_id) {
                (Some(item), None) => TradeAsset {
                    asset_type: TradeAssetType::CollectionItem,
                    contract,
                    value: parse_decimal(item).map_err(|_| bad("itemId must be a decimal"))?,
                },
                (None, Some(token)) => TradeAsset {
                    asset_type: TradeAssetType::Erc721,
                    contract,
                    value: parse_decimal(token).map_err(|_| bad("tokenId must be a decimal"))?,
                },
                _ => return Err(bad("exactly one of itemId or tokenId is required")),
            };
            (chain_id, vec![asset])
        }
    };
    let Some(first) = sent.first().cloned() else {
        return Err(bad("the listing sends no asset"));
    };
    let item_id = (first.asset_type == TradeAssetType::CollectionItem).then_some(first.value);
    let facts = mirror_facts(&state.pool, chain_id, first.contract, item_id)
        .await
        .map_err(IntoResponse::into_response)?;
    let Some(trade) = proposed_trade(chain_id, signer, sent) else {
        return Err(bad(format!(
            "no off-chain marketplace is deployed on chain {}",
            chain_id.0
        )));
    };
    let mut check = decide(signer, trade, &facts);
    match current_rate(&state.pool)
        .await
        .map_err(IntoResponse::into_response)?
    {
        Some((rate, taken_at)) => check.quote = Some(quote_for(listing.as_ref(), rate, taken_at)),
        None => check.quote_unavailable = Some("mana-usd-rate"),
    }
    Ok(Json(json!({ "ok": true, "data": check })).into_response())
}

/// `POST /v1/import-checks` (signed by the seller; a read, open under the
/// write switch): whether the listing may be imported for credits, or
/// stays a MANA listing, and the price it suggests.
pub async fn check_import(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Option<Json<ImportCheckBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    match check_inner(&state, &headers, body).await {
        Ok(r) | Err(r) => r,
    }
}

pub fn router() -> Router<SharedState> {
    Router::new().route(CHECK_PATH, post(check_import))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    const CREATOR: &str = "0xcccccccccccccccccccccccccccccccccccccccc";
    const OTHER: &str = "0xdddddddddddddddddddddddddddddddddddddddd";
    const COLLECTION: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn addr(s: &str) -> EvmAddress {
        EvmAddress::parse(s).unwrap()
    }

    fn item_facts(creator: &str) -> MirrorFacts {
        MirrorFacts {
            contract: Lookup::Found(ContractRole::Collection),
            item: Lookup::Found(ItemRecord {
                chain_id: ChainId::POLYGON,
                collection: addr(COLLECTION),
                item_id: U256::from(7u64),
                kind: ItemKind::Wearable,
                creator: addr(creator),
            }),
            sellers: vec![addr(creator)],
        }
    }

    fn item_asset() -> TradeAsset {
        TradeAsset {
            asset_type: TradeAssetType::CollectionItem,
            contract: addr(COLLECTION),
            value: U256::from(7u64),
        }
    }

    #[test]
    fn a_creators_item_order_imports_for_credits() {
        let trade = proposed_trade(ChainId::POLYGON, addr(CREATOR), vec![item_asset()]).unwrap();
        let check = decide(addr(CREATOR), trade, &item_facts(CREATOR));
        assert!(check.eligible, "{check:?}");
        assert_eq!(
            (check.sale_kind, check.settlement, check.seller_is_creator),
            (Some(SaleKind::PrimaryMint), Some(Settlement::Credits), true)
        );
        assert_eq!(
            check.reasons,
            vec![Reason::CreditsFirst, Reason::ManaSecond]
        );
        assert_eq!(
            check.asset,
            AssetOut {
                chain_id: 137,
                contract_address: COLLECTION.into(),
                item_id: Some("7".into()),
                token_id: None
            }
        );
        let v = serde_json::to_value(&check).unwrap();
        assert_eq!(v["saleKind"], "primaryMint");
        assert_eq!(v["settlement"], "credits");
        assert!(v["refusal"].is_null());
    }

    #[test]
    fn an_owned_token_is_a_resale_and_stays_mana() {
        let token = TradeAsset {
            asset_type: TradeAssetType::Erc721,
            contract: addr(COLLECTION),
            value: U256::from_str_radix(
                "105312291668557186697918027683670432318895095400549111254310977537",
                10,
            )
            .unwrap(),
        };
        let trade = proposed_trade(ChainId::POLYGON, addr(CREATOR), vec![token]).unwrap();
        let check = decide(addr(CREATOR), trade, &item_facts(CREATOR));
        assert!(!check.eligible);
        assert_eq!(
            (check.sale_kind, check.settlement),
            (Some(SaleKind::Secondary), Some(Settlement::Mana)),
            "the creator's own minted token is a resale: {check:?}"
        );
        assert!(check.reasons.contains(&Reason::SecondarySale));
        assert_eq!(
            check.refusal.as_ref().map(|r| r.code.as_str()),
            Some("creditsNotAllowed")
        );
        assert!(check.asset.token_id.is_some() && check.asset.item_id.is_none());
    }

    #[test]
    fn someone_elses_item_is_not_an_import() {
        let trade = proposed_trade(ChainId::POLYGON, addr(OTHER), vec![item_asset()]).unwrap();
        let check = decide(addr(OTHER), trade, &item_facts(CREATOR));
        assert!(!check.eligible && !check.seller_is_creator);
        assert_eq!(
            check.refusal.as_ref().map(|r| r.code.as_str()),
            Some("notTheCreator")
        );
        assert_eq!(
            check.settlement, None,
            "not their listing to make in any currency"
        );
    }

    #[test]
    fn a_contract_the_registry_does_not_know_is_not_a_decentraland_item() {
        let facts = MirrorFacts {
            contract: Lookup::Absent,
            item: Lookup::Absent,
            sellers: Vec::new(),
        };
        let trade = proposed_trade(ChainId::POLYGON, addr(CREATOR), vec![item_asset()]).unwrap();
        let check = decide(addr(CREATOR), trade, &facts);
        assert!(!check.eligible);
        assert!(
            check.reasons.contains(&Reason::NotDecentralandItem),
            "{check:?}"
        );
        assert_eq!(check.settlement, Some(Settlement::Mana));
    }

    #[test]
    fn a_mint_on_ethereum_has_no_credits_manager() {
        let facts = MirrorFacts {
            contract: Lookup::Found(ContractRole::Collection),
            item: Lookup::Found(ItemRecord {
                chain_id: ChainId::ETHEREUM,
                collection: addr(COLLECTION),
                item_id: U256::from(7u64),
                kind: ItemKind::Wearable,
                creator: addr(CREATOR),
            }),
            sellers: vec![addr(CREATOR)],
        };
        let trade = proposed_trade(ChainId::ETHEREUM, addr(CREATOR), vec![item_asset()]);
        assert!(
            trade.is_none() || !decide(addr(CREATOR), trade.unwrap(), &facts).eligible,
            "no v3 marketplace on Ethereum, or the rule refuses"
        );
    }

    #[test]
    fn the_quote_is_exact_integer_money() {
        let rate = ManaUsdRate::parse_usd_per_mana("0.25").unwrap();
        let listing = StoredListing {
            trade_id: "t".into(),
            signer: addr(CREATOR),
            chain_id: ChainId::POLYGON,
            venue: addr(COLLECTION),
            sent: vec![item_asset()],
            received: Some((
                TradeAssetType::Erc20,
                U256::from(5_000_000_000_000_000_000u128),
            )),
        };
        let q = quote_for(Some(&listing), rate, 1_700_000_000);
        assert_eq!(
            (
                q.mana_wei.as_deref(),
                q.usd_cents,
                q.credits,
                q.rate_usd_per_mana.as_str()
            ),
            (Some("5000000000000000000"), Some(125), Some(13), "0.25")
        );
        assert_eq!(q.settles_now_wei, None);
        // 4.01 MANA at 0.25 = 100.25 cents -> 101 cents -> 11 credits: never rounded down.
        let odd = StoredListing {
            received: Some((
                TradeAssetType::Erc20,
                U256::from(4_010_000_000_000_000_000u128),
            )),
            ..listing.clone()
        };
        let q = quote_for(Some(&odd), rate, 0);
        assert_eq!((q.usd_cents, q.credits), (Some(101), Some(11)));
        // A USD-pegged resale order: 2.5 USD settles for 10 MANA today.
        let pegged = StoredListing {
            received: Some((
                TradeAssetType::UsdPeggedMana,
                U256::from(2_500_000_000_000_000_000u128),
            )),
            ..listing
        };
        let q = quote_for(Some(&pegged), rate, 0);
        assert_eq!(
            (q.mana_wei, q.credits, q.settles_now_wei.as_deref()),
            (None, None, Some("10000000000000000000"))
        );
        assert_eq!(
            rate_text(ManaUsdRate::parse_usd_per_mana("1").unwrap()),
            "1"
        );
        assert_eq!(
            rate_text(ManaUsdRate::parse_usd_per_mana("0.123456789").unwrap()),
            "0.123456789"
        );
    }

    #[test]
    fn the_stored_listing_reads_its_sale_kind_from_the_assets() {
        let mut l = StoredListing {
            trade_id: "t".into(),
            signer: addr(CREATOR),
            chain_id: ChainId::POLYGON,
            venue: addr(COLLECTION),
            sent: vec![item_asset()],
            received: None,
        };
        assert_eq!(l.sale_kind(), SaleKind::PrimaryMint);
        l.sent.push(TradeAsset {
            asset_type: TradeAssetType::Erc721,
            contract: addr(COLLECTION),
            value: U256::from(1u64),
        });
        assert_eq!(
            l.sale_kind(),
            SaleKind::Secondary,
            "a bundle with one token is a resale"
        );
        assert_eq!(RefusalOut::from(&Refusal::NotSignedIn).code, "notSignedIn");
        assert_eq!(RefusalOut::from(&Refusal::EmptyGroup).code, "emptyGroup");
    }

    #[test]
    fn the_body_takes_the_old_trade_or_the_asset() {
        let b: ImportCheckBody =
            serde_json::from_value(json!({ "oldTradeId": "6a0e4b1e-0f6e-4c7a-9d2b-2f1c9a1a0001" }))
                .unwrap();
        assert_eq!(
            b.old_trade_id.as_deref(),
            Some("6a0e4b1e-0f6e-4c7a-9d2b-2f1c9a1a0001")
        );
        let b: ImportCheckBody = serde_json::from_value(
            json!({ "chainId": 137, "contractAddress": COLLECTION, "itemId": "7" }),
        )
        .unwrap();
        assert_eq!((b.chain_id, b.item_id.as_deref()), (Some(137), Some("7")));
    }
}
