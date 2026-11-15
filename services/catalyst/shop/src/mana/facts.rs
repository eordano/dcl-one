// from: shop@94584a5c:app/src/lib/api.ts (fetchTrade, fetchClassicItemOrders, fetchSecondarySaleState: which record a cart line names), marketplace@f3956826:webapp/src/modules/{order,rental,ens}/sagas.ts (what an order, a rental and a NAME claim read before paying), SHOP.md "Enforce payment eligibility at every entry"
//! What a cart line offers, from the authoritative record it names and
//! nothing else: a stored signed trade (`shop.trades`), a legacy order the
//! nft index folded (`shop.nft_order`), a rentals listing this node stores
//! (`signatures.rentals`) or a NAME label against the registrar. Each
//! resolves to the rule's [`Offer`] and the [`Facts`] it is judged on; the
//! contract role, the item and the token come from the static registry, the
//! mirrors and the indexes, and read [`Lookup::Unknown`] when their source
//! has no complete pass, so the rule answers `Incomplete` rather than a
//! narrower choice. A client-supplied `primary`, price or sale kind has no
//! field to arrive through.

use alloy_primitives::{Address, U256};
use sqlx::{PgPool, Row};

use super::venues::{Book, Chain};
use super::wire::{LineRequest, ListingRef};
use crate::checkout::trade::TradeIn;
use crate::market::chain_walk::{self, ChainNet, INDEXER_NFT};
use crate::market::trades;
use crate::policy::Facts;
use crate::policy_entry::{mirror_contract_role, mirror_facts};
use crate::rentals::calls::{RentTerms, rent_terms};
use crate::rentals::types::RentalListing;
use crate::types::{
    AssetId, ChainId, ContractRef, ContractRole, EvmAddress, ItemRecord, Lookup, ManaWei,
    NameLabel, Offer, Session, SignedTrade, TokenRecord, TokenRef, TradeAsset, TradeAssetType,
};

/// A legacy order as the nft index folded it (`shop.nft_order`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyOrder {
    pub order_id: String,
    pub marketplace: Address,
    pub status: String,
    pub seller: Address,
    pub price: U256,
    /// Milliseconds.
    pub expires_at: i64,
    pub updated_block: i64,
}

/// The record a line resolved to, kept beside the offer for the
/// verification and the calldata.
#[derive(Debug, Clone)]
pub enum Listing {
    Store {
        venue: Address,
        price: ManaWei,
    },
    Trade {
        trade: TradeIn,
        venue: Address,
        hashed_signature: String,
        /// The trade-events verdict (`open` | `sold` | `cancelled`) and its
        /// executions, when the index is published; None when it is not.
        verdict: Option<(String, i32)>,
        index_ready: bool,
        /// The creator's live coupon a primary sale settles with, in full,
        /// so the calldata is the `acceptWithCoupon` the offer prices.
        coupon: Option<super::calls::CouponIn>,
    },
    LegacyOrder {
        order: LegacyOrder,
        index_ready: bool,
    },
    Rental {
        listing: RentalListing,
        terms: RentTerms,
        rentals: Address,
        status_index_ready: bool,
    },
    Name {
        label: NameLabel,
        registrar: Address,
        controller: Address,
    },
}

impl Listing {
    pub fn source(&self) -> &'static str {
        match self {
            Self::Store { .. } => "items-mirror",
            Self::Trade { .. } => "trades-mirror+trade-events-index",
            Self::LegacyOrder { .. } => "nft-index",
            Self::Rental { .. } => "rentals-store+rentals-index",
            Self::Name { .. } => "name-registrar",
        }
    }

    pub fn index_ready(&self) -> bool {
        match self {
            Self::Trade { index_ready, .. } | Self::LegacyOrder { index_ready, .. } => *index_ready,
            Self::Rental {
                status_index_ready, ..
            } => *status_index_ready,
            Self::Store { .. } | Self::Name { .. } => true,
        }
    }

    pub fn verdict(&self) -> Option<String> {
        match self {
            Self::Trade { verdict, .. } => verdict.as_ref().map(|(s, _)| s.clone()),
            Self::LegacyOrder { order, .. } => Some(order.status.clone()),
            Self::Rental { listing, .. } => Some(listing.status.clone()),
            Self::Store { .. } | Self::Name { .. } => None,
        }
    }
}

/// One resolved line.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub chain: Chain,
    pub asset: AssetId,
    pub offer: Offer,
    pub facts: Facts,
    pub listing: Listing,
    /// Remaining mintable supply of a primary item (the items mirror).
    pub available: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("a line names an asset or a NAME")]
    NoAsset,
    #[error("chain {0} is not one the Shop settles on")]
    UnknownChain(ChainId),
    #[error("a line names one listing: tradeId, orderId, rentalId, name or acquisition=store")]
    Ambiguous,
    #[error(
        "a line names the listing it buys (tradeId, orderId, rentalId, name or acquisition=store)"
    )]
    NoListing,
    #[error("{0} is not an approved item offered by the CollectionStore")]
    StoreUnavailable(AssetId),
    #[error("trade {0} is not known to this node")]
    TradeNotFound(String),
    #[error("trade {id} is on chain {trade_chain}, the line on {line_chain}")]
    TradeChain {
        id: String,
        trade_chain: u64,
        line_chain: u64,
    },
    #[error(
        "trade {0} was signed for a contract that is not an off-chain marketplace of its chain"
    )]
    TradeVenue(String),
    #[error("trade {0} does not sell the line's asset")]
    TradeAsset(String),
    #[error("trade {0} is malformed: {1}")]
    TradeShape(String, String),
    #[error("{0} has no legacy order the nft index knows")]
    OrderNotFound(AssetId),
    #[error("the order named is {named}, the token's current one {current}")]
    OrderMismatch { named: String, current: String },
    #[error("rental listing {0} is not known to this node")]
    RentalNotFound(String),
    #[error("rental listing {0} is for another asset")]
    RentalAsset(String),
    #[error("the rent terms: {0}")]
    Rent(#[from] crate::rentals::calls::RentError),
    #[error("the chain has no {0}")]
    NoVenue(&'static str),
    #[error("{0}")]
    Name(String),
    #[error("database: {0}")]
    Db(String),
    /// The source that holds the listing is not published yet; the route
    /// answers the unavailable envelope naming it.
    #[error("{index} is not published yet")]
    IndexNotReady { index: &'static str },
}

impl From<sqlx::Error> for ResolveError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.to_string())
    }
}

fn evm(a: Address) -> EvmAddress {
    EvmAddress::new(a)
}

/// The contract's role: the static registry first, then the collections
/// mirror (only after a complete pass, else `Unknown`).
pub async fn contract_role(
    pool: &PgPool,
    chain: &Chain,
    contract: Address,
) -> Result<Lookup<ContractRole>, sqlx::Error> {
    if chain.is_land_registry(contract) {
        return Ok(Lookup::Found(ContractRole::Land));
    }
    if chain.is_estate_registry(contract) {
        return Ok(Lookup::Found(ContractRole::Estate));
    }
    if chain.name_registrar == Some(contract) {
        return Ok(Lookup::Found(ContractRole::NameRegistrar));
    }
    let Some(net) = chain.net else {
        return Ok(Lookup::Absent);
    };
    mirror_contract_role(pool, chain.chain_id, net.db_name(), evm(contract)).await
}

/// The items mirror's record of `(collection, item_id)` and its remaining
/// supply; `Unknown` before the mirror's first complete pass.
pub async fn item_record(
    pool: &PgPool,
    chain: &Chain,
    collection: Address,
    item_id: U256,
) -> Result<(Lookup<ItemRecord>, Option<i64>), sqlx::Error> {
    let Some(net) = chain.net else {
        return Ok((Lookup::Absent, None));
    };
    let facts = mirror_facts(
        pool,
        chain.chain_id,
        net.db_name(),
        evm(collection),
        item_id,
    )
    .await?;
    Ok((facts.item, facts.available))
}

/// The indexed token (its owner and item): `shop.nft` for collections and
/// names, `land_authz.token_right` for parcels and estates, each only while
/// its index is published; `Unknown` otherwise.
pub async fn token_record(
    pool: &PgPool,
    chain: &Chain,
    role: &Lookup<ContractRole>,
    contract: Address,
    token_id: U256,
) -> Result<Lookup<TokenRecord>, sqlx::Error> {
    let asset = AssetId::token(chain.chain_id, evm(contract), token_id);
    let now = chrono::Utc::now();
    match role {
        Lookup::Found(ContractRole::Land) | Lookup::Found(ContractRole::Estate) => {
            let Some(mut land) = crate::market::nft_reads::land_snapshot(pool).await else {
                return Ok(Lookup::Unknown);
            };
            let owner: Option<Option<String>> = sqlx::query_scalar("SELECT owner FROM land_authz.token_right WHERE token_address = $1 AND token_id = $2::numeric")
                .bind(format!("{contract:#x}"))
                .bind(token_id.to_string())
                .fetch_optional(&mut *land.tx)
                .await?;
            Ok(
                match owner.flatten().and_then(|o| EvmAddress::parse(&o).ok()) {
                    Some(owner) => Lookup::Found(TokenRecord {
                        asset,
                        item_id: None,
                        owner,
                    }),
                    None => Lookup::Absent,
                },
            )
        }
        _ => {
            let Some(net) = chain.net else {
                return Ok(Lookup::Unknown);
            };
            if chain_walk::ready_cursor(pool, INDEXER_NFT, net, now)
                .await
                .is_none()
            {
                return Ok(Lookup::Unknown);
            }
            let row = sqlx::query("SELECT owner, item_id::text AS item_id FROM shop.nft WHERE network = $1 AND contract_address = $2 AND token_id = $3::numeric")
                .bind(net.db_name())
                .bind(format!("{contract:#x}"))
                .bind(token_id.to_string())
                .fetch_optional(pool)
                .await?;
            Ok(match row {
                Some(r) => match r
                    .get::<Option<String>, _>("owner")
                    .and_then(|o| EvmAddress::parse(&o).ok())
                {
                    Some(owner) => Lookup::Found(TokenRecord {
                        asset,
                        item_id: r
                            .get::<Option<String>, _>("item_id")
                            .and_then(|i| U256::from_str_radix(&i, 10).ok()),
                        owner,
                    }),
                    None => Lookup::Absent,
                },
                None => Lookup::Absent,
            })
        }
    }
}

fn trade_asset(a: &crate::checkout::trade::TradeAssetIn) -> Result<TradeAsset, String> {
    let asset_type = TradeAssetType::from_code(u64::try_from(a.asset_type).unwrap_or(u64::MAX));
    let value = match asset_type {
        TradeAssetType::Erc20 | TradeAssetType::UsdPeggedMana => a.amount.as_deref(),
        TradeAssetType::Erc721 => a.token_id.as_deref(),
        TradeAssetType::CollectionItem => a.item_id.as_deref(),
        TradeAssetType::Unsupported(code) => return Err(format!("unsupported asset type {code}")),
    }
    .ok_or("an asset without its value")?;
    Ok(TradeAsset {
        asset_type,
        contract: EvmAddress::parse(&a.contract_address.trim().to_ascii_lowercase())
            .map_err(|e| format!("asset contract: {e}"))?,
        value: crate::types::parse_decimal(value.trim())
            .map_err(|e| format!("asset value: {e}"))?,
    })
}

/// A stored trade as the rule's signed trade.
pub fn signed_trade(trade: &TradeIn, venue: ContractRef) -> Result<SignedTrade, String> {
    Ok(SignedTrade {
        venue,
        signer: EvmAddress::parse(&trade.signer.trim().to_ascii_lowercase())
            .map_err(|e| format!("signer: {e}"))?,
        sent: trade
            .sent
            .iter()
            .map(trade_asset)
            .collect::<Result<_, _>>()?,
        received: trade
            .received
            .iter()
            .map(trade_asset)
            .collect::<Result<_, _>>()?,
    })
}

/// The stored trade by id in `scope`, as the trades read serializes it,
/// whatever its copy's age: the chain checks every action on a trade.
pub async fn load_trade(
    pool: &PgPool,
    id: &str,
    scope: trades::TradeScope,
) -> Result<Option<TradeIn>, ResolveError> {
    let Some(trade) = trades::get_trade_by_id(pool, id, None, scope).await? else {
        return Ok(None);
    };
    let value = serde_json::to_value(&trade).map_err(|e| ResolveError::Db(e.to_string()))?;
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| ResolveError::TradeShape(id.to_string(), e.to_string()))
}

/// The trade-events verdict of a stored trade, when its network is published.
pub async fn trade_verdict(
    pool: &PgPool,
    net: ChainNet,
    trade_id: &str,
) -> Result<(Option<(String, i32)>, bool), sqlx::Error> {
    let now = chrono::Utc::now();
    if chain_walk::ready_cursor(pool, chain_walk::INDEXER_TRADE_EVENTS, net, now)
        .await
        .is_none()
    {
        return Ok((None, false));
    }
    let row: Option<(String, i32)> = sqlx::query_as("SELECT status, executions FROM shop.trade_chain_state WHERE trade_id = $1::uuid AND folded_block >= 0")
        .bind(trade_id)
        .fetch_optional(pool)
        .await?;
    Ok((row.or(Some(("open".to_string(), 0))), true))
}

/// The legacy order of a token, when the nft index is published.
pub async fn legacy_order(
    pool: &PgPool,
    chain: &Chain,
    contract: Address,
    token_id: U256,
) -> Result<(Option<LegacyOrder>, bool), sqlx::Error> {
    let Some(net) = chain.net else {
        return Ok((None, false));
    };
    if chain_walk::ready_cursor(pool, INDEXER_NFT, net, chrono::Utc::now())
        .await
        .is_none()
    {
        return Ok((None, false));
    }
    let row = sqlx::query(
        "SELECT order_id, marketplace_address, status, seller, price::text AS price, expires_at, updated_block \
         FROM shop.nft_order WHERE network = $1 AND contract_address = $2 AND token_id = $3::numeric",
    )
    .bind(net.db_name())
    .bind(format!("{contract:#x}"))
    .bind(token_id.to_string())
    .fetch_optional(pool)
    .await?;
    let order = row.and_then(|r| {
        Some(LegacyOrder {
            order_id: r.get("order_id"),
            marketplace: r.get::<String, _>("marketplace_address").parse().ok()?,
            status: r.get("status"),
            seller: r.get::<String, _>("seller").parse().ok()?,
            price: U256::from_str_radix(&r.get::<String, _>("price"), 10).ok()?,
            expires_at: r.get("expires_at"),
            updated_block: r.get("updated_block"),
        })
    });
    Ok((order, true))
}

fn listing_count(l: &ListingRef) -> usize {
    [
        l.trade_id.is_some(),
        l.order_id.is_some(),
        l.rental_id.is_some(),
        l.name.is_some(),
        l.acquisition.as_deref() == Some("store"),
    ]
    .into_iter()
    .filter(|b| *b)
    .count()
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use crate::policy::{Authoritative, Quote, eligibility, quote};
    use crate::types::{ManaUsdRate, PaymentMethod, SaleKind};

    #[tokio::test]
    async fn store_quotes_use_mirrored_identity_price_and_mint_permission() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
            return;
        };
        let _mirror_rows = crate::market::tests::support::MIRROR_STATE_LOCK
            .lock()
            .await;
        let collection = "0x000000000000000000000000000000000000c011";
        let creator = "0x000000000000000000000000000000000000c4ea";
        sqlx::query("INSERT INTO shop.mirror_state (mirror, network, source_url, last_complete_pass_at) VALUES ('items','matic','fixture',now()), ('collections','matic','fixture',now()) ON CONFLICT (mirror,network) DO UPDATE SET last_complete_pass_at = now()")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO shop.collections (network,contract_address,chain_id,name,creator,owner,urn,is_completed,is_approved,is_editable,created_at,updated_at,reviewed_at,mirrored_at) VALUES ('matic',$1,137,'Store fixture',$2,$2,'urn:store-quote',true,true,false,0,0,0,now())")
            .bind(collection).bind(creator).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO shop.items (network,contract_address,item_id,chain_id,item_type,urn,creator,rarity,total_supply,max_supply,available,price,name,category,search_is_store_minter,search_is_collection_approved,created_at,updated_at,reviewed_at,mirrored_at) VALUES ('matic',$1,7,137,'wearable_v2','urn:store-quote:7',$2,'common',0,10,10,2300000000000000001,'Hat','hat',true,true,0,0,0,now())")
            .bind(collection).bind(creator).execute(&pool).await.unwrap();
        let book = Book::production(|_| None);
        let rentals = crate::rentals::db::Database::new(pool.clone());
        let mut request: LineRequest = serde_json::from_value(serde_json::json!({
            "asset": {"chainId":137,"contract":collection,"token":{"kind":"item","itemId":"7"}},
            "listing":{"acquisition":"store"}, "price": "1", "sale": "secondary"
        }))
        .unwrap();
        let buyer = EvmAddress::parse(creator).unwrap();
        for session in [
            Session::Web2 { account: buyer },
            Session::Web3 { wallet: buyer },
        ] {
            let resolved = resolve(&pool, &rentals, &book, session, &request)
                .await
                .unwrap();
            assert_eq!(resolved.available, Some(10));
            assert_eq!(resolved.listing.source(), "items-mirror");
            let line = Authoritative::derive(resolved.facts).unwrap();
            assert_eq!(line.sale(), SaleKind::PrimaryMint);
            let choices = eligibility(&line);
            assert_eq!(choices.allowed.first(), Some(&PaymentMethod::Credits));
            if matches!(session, Session::Web3 { .. }) {
                assert_eq!(
                    choices.allowed,
                    vec![PaymentMethod::Credits, PaymentMethod::Mana]
                );
                assert_eq!(
                    quote(&line, PaymentMethod::Mana, None).unwrap(),
                    Quote::Mana {
                        amount: ManaWei::new(U256::from(2_300_000_000_000_000_001u64))
                    }
                );
            }
            let rate = ManaUsdRate::parse_usd_per_mana("0.5").unwrap();
            // shop@94584a5c mana-convert.ts floors USD wei before rounding cents up.
            assert!(
                matches!(quote(&line, PaymentMethod::Credits, Some(&rate)).unwrap(), Quote::Credits { quoted } if quoted.get() == 115)
            );
        }
        sqlx::query("UPDATE shop.items SET price=2300000000000000002 WHERE contract_address=$1")
            .bind(collection)
            .execute(&pool)
            .await
            .unwrap();
        let repriced = resolve(
            &pool,
            &rentals,
            &book,
            Session::Web2 { account: buyer },
            &request,
        )
        .await
        .unwrap();
        let repriced = Authoritative::derive(repriced.facts).unwrap();
        let rate = ManaUsdRate::parse_usd_per_mana("0.5").unwrap();
        assert!(
            matches!(quote(&repriced, PaymentMethod::Credits, Some(&rate)).unwrap(), Quote::Credits { quoted } if quoted.get() == 116)
        );
        request.listing.order_id = Some("cannot-combine-with-store".into());
        assert_eq!(
            resolve(&pool, &rentals, &book, Session::SignedOut, &request)
                .await
                .unwrap_err(),
            ResolveError::Ambiguous
        );
        request.listing.order_id = None;
        let asset = request.asset.unwrap();
        request.asset = Some(AssetId::token(
            asset.chain_id,
            asset.contract,
            U256::from(7),
        ));
        assert!(matches!(
            resolve(&pool, &rentals, &book, Session::SignedOut, &request).await,
            Err(ResolveError::StoreUnavailable(_))
        ));
        request.asset = Some(asset);
        sqlx::query("UPDATE shop.items SET search_is_store_minter=false WHERE contract_address=$1")
            .bind(collection)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            resolve(&pool, &rentals, &book, Session::SignedOut, &request).await,
            Err(ResolveError::StoreUnavailable(_))
        ));
        sqlx::query("UPDATE shop.items SET search_is_store_minter=true, search_is_collection_approved=false WHERE contract_address=$1").bind(collection).execute(&pool).await.unwrap();
        assert!(matches!(
            resolve(&pool, &rentals, &book, Session::SignedOut, &request).await,
            Err(ResolveError::StoreUnavailable(_))
        ));
        sqlx::query("UPDATE shop.items SET search_is_collection_approved=true, available=0 WHERE contract_address=$1").bind(collection).execute(&pool).await.unwrap();
        let sold = resolve(&pool, &rentals, &book, Session::SignedOut, &request)
            .await
            .unwrap();
        let (checks, _, _) = crate::mana::verify::checks(&sold, None).await;
        assert!(!crate::mana::verify::executable(&checks));
        sqlx::query("UPDATE shop.mirror_state SET last_complete_pass_at=NULL WHERE mirror='items' AND network='matic'").execute(&pool).await.unwrap();
        assert!(matches!(
            resolve(&pool, &rentals, &book, Session::SignedOut, &request).await,
            Err(ResolveError::IndexNotReady { .. })
        ));
        sqlx::query("DELETE FROM shop.items WHERE contract_address=$1")
            .bind(collection)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM shop.collections WHERE contract_address=$1")
            .bind(collection)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM shop.mirror_state WHERE mirror IN ('items','collections') AND network='matic'").execute(&pool).await.unwrap();
    }
}

#[cfg(test)]
mod payment_path_facts_tests {
    //! The credits guards (`policy_entry`) and the MANA resolver read the
    //! same contract role and item record; these cases pin that both payment
    //! paths answer alike for every mirror state.
    use super::*;
    use crate::policy_entry;
    use crate::types::ItemKind;

    const COLLECTION: &str = "0x000000000000000000000000000000000000fac7";
    const CREATOR: &str = "0x000000000000000000000000000000000000c4ea";

    async fn set_ready(pool: &PgPool, mirror: &str, ready: bool) {
        sqlx::query("INSERT INTO shop.mirror_state (mirror, network, source_url, last_complete_pass_at) VALUES ($1, 'matic', 'fixture', CASE WHEN $2 THEN now() END) ON CONFLICT (mirror, network) DO UPDATE SET last_complete_pass_at = EXCLUDED.last_complete_pass_at")
            .bind(mirror)
            .bind(ready)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn insert_item(pool: &PgPool, item_id: u64, item_type: &str, creator: &str) {
        sqlx::query("INSERT INTO shop.items (network,contract_address,item_id,chain_id,item_type,urn,creator,rarity,total_supply,max_supply,available,price,name,category,created_at,updated_at,reviewed_at,mirrored_at) VALUES ('matic',$1,$2::numeric,137,$3,'urn:facts:' || $2,$4,'common',0,10,4,1,'Hat','hat',0,0,0,now())")
            .bind(COLLECTION)
            .bind(item_id.to_string())
            .bind(item_type)
            .bind(creator)
            .execute(pool)
            .await
            .unwrap();
    }

    /// Both paths' answers for `(COLLECTION, item_id)` on Polygon, asserting
    /// they agree and returning the shared answer.
    async fn both(
        pool: &PgPool,
        book: &Book,
        item_id: u64,
    ) -> (Lookup<ContractRole>, Lookup<ItemRecord>, Option<i64>) {
        let chain = book.chain(ChainId::POLYGON).unwrap();
        let collection = EvmAddress::parse(COLLECTION).unwrap();
        let item_id = U256::from(item_id);
        let credits = policy_entry::item_facts(pool, ChainId::POLYGON, collection, item_id)
            .await
            .unwrap();
        let credits_role = policy_entry::contract_role(pool, ChainId::POLYGON, collection)
            .await
            .unwrap();
        let mana_role = contract_role(pool, chain, collection.address())
            .await
            .unwrap();
        let (mana_item, available) = item_record(pool, chain, collection.address(), item_id)
            .await
            .unwrap();
        assert_eq!(credits.contract, credits_role);
        assert_eq!(credits.contract, mana_role);
        assert_eq!(credits.item, mana_item);
        (mana_role, mana_item, available)
    }

    #[tokio::test]
    async fn credits_and_mana_read_the_same_contract_role_and_item_record() {
        // Its own database, so the mirror_state, collection and item rows it
        // writes never reach another test, even when an assertion fails.
        let Some(pool) = crate::db::scratch::isolated_pool(2).await else {
            eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
            return;
        };
        let book = Book::production(|_| None);
        let record = |item_id: u64, kind: ItemKind| {
            Lookup::Found(ItemRecord {
                chain_id: ChainId::POLYGON,
                collection: EvmAddress::parse(COLLECTION).unwrap(),
                item_id: U256::from(item_id),
                kind,
                creator: EvmAddress::parse(CREATOR).unwrap(),
            })
        };

        // No mirror has a complete pass: neither source can say.
        assert_eq!(
            both(&pool, &book, 1).await,
            (Lookup::Unknown, Lookup::Unknown, None)
        );
        set_ready(&pool, "collections", false).await;
        set_ready(&pool, "items", false).await;
        insert_item(&pool, 1, "wearable_v2", CREATOR).await;
        assert_eq!(
            both(&pool, &book, 1).await,
            (Lookup::Unknown, Lookup::Unknown, None)
        );

        // Both passed, the contract is no collection: absent, not unknown.
        set_ready(&pool, "collections", true).await;
        set_ready(&pool, "items", true).await;
        assert_eq!(
            both(&pool, &book, 2).await,
            (Lookup::Absent, Lookup::Absent, None)
        );

        sqlx::query("INSERT INTO shop.collections (network,contract_address,chain_id,name,creator,owner,urn,is_completed,is_approved,is_editable,created_at,updated_at,reviewed_at,mirrored_at) VALUES ('matic',$1,137,'Facts fixture',$2,$2,'urn:facts',true,true,false,0,0,0,now())")
            .bind(COLLECTION)
            .bind(CREATOR)
            .execute(&pool)
            .await
            .unwrap();
        insert_item(&pool, 3, "emote_v1", CREATOR).await;
        insert_item(&pool, 4, "wearable_v2", "not-an-address").await;
        let found = Lookup::Found(ContractRole::Collection);
        assert_eq!(
            both(&pool, &book, 1).await,
            (found.clone(), record(1, ItemKind::Wearable), Some(4))
        );
        assert_eq!(
            both(&pool, &book, 3).await,
            (found.clone(), record(3, ItemKind::Emote), Some(4))
        );
        // An unparseable creator is a record the rule cannot judge.
        assert_eq!(
            both(&pool, &book, 4).await,
            (found.clone(), Lookup::Unknown, None)
        );
        assert_eq!(
            both(&pool, &book, 9).await,
            (found.clone(), Lookup::Absent, None)
        );

        // Collections passed, items still catching up.
        set_ready(&pool, "items", false).await;
        assert_eq!(both(&pool, &book, 1).await, (found, Lookup::Unknown, None));
    }

    #[tokio::test]
    async fn credits_and_mana_give_the_static_registries_the_same_role() {
        let Some(pool) = crate::db::scratch::pool(1).await else {
            eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
            return;
        };
        let book = Book::production(|_| None);
        let ethereum = book.chain(ChainId::ETHEREUM).unwrap();
        for (contract, role) in [
            (ethereum.land.unwrap(), ContractRole::Land),
            (ethereum.estate.unwrap(), ContractRole::Estate),
            (
                ethereum.name_registrar.unwrap(),
                ContractRole::NameRegistrar,
            ),
        ] {
            let credits = policy_entry::contract_role(&pool, ChainId::ETHEREUM, evm(contract))
                .await
                .unwrap();
            let mana = contract_role(&pool, ethereum, contract).await.unwrap();
            assert_eq!(credits, Lookup::Found(role));
            assert_eq!(mana, Lookup::Found(role));
        }
    }
}

/// The order-book price of a legacy order: the record's `price`.
fn legacy_offer(chain: &Chain, order: &LegacyOrder, contract: Address, token_id: U256) -> Offer {
    Offer::LegacyOrder {
        venue: chain.contract(order.marketplace),
        nft: evm(contract),
        token_id,
        price: ManaWei::new(order.price),
    }
}

/// Resolves one line against the book, the mirrors, the indexes and the
/// rentals store. `session` is carried into the facts unchanged.
pub async fn resolve(
    pool: &PgPool,
    rentals: &crate::rentals::db::Database,
    book: &Book,
    session: Session,
    line: &LineRequest,
) -> Result<Resolved, ResolveError> {
    match listing_count(&line.listing) {
        0 => return Err(ResolveError::NoListing),
        1 => {}
        _ => return Err(ResolveError::Ambiguous),
    }
    if let Some(name) = &line.listing.name {
        return resolve_name(book, session, line, name).await;
    }
    let asset = line.asset.ok_or(ResolveError::NoAsset)?;
    let chain = book
        .chain(asset.chain_id)
        .ok_or(ResolveError::UnknownChain(asset.chain_id))?
        .clone();
    if line.listing.acquisition.as_deref() == Some("store") {
        return resolve_store(pool, chain, session, asset).await;
    }
    if let Some(id) = &line.listing.trade_id {
        return resolve_trade(pool, chain, session, asset, id).await;
    }
    let TokenRef::Token { token_id } = asset.token else {
        return Err(ResolveError::OrderNotFound(asset));
    };
    if let Some(rental_id) = &line.listing.rental_id {
        return resolve_rental(
            pool, rentals, chain, session, asset, token_id, rental_id, line,
        )
        .await;
    }
    let (order, ready) = legacy_order(pool, &chain, asset.contract.address(), token_id).await?;
    if !ready {
        return Err(ResolveError::IndexNotReady { index: "nft-index" });
    }
    let order = order.ok_or(ResolveError::OrderNotFound(asset))?;
    if let Some(named) = line
        .listing
        .order_id
        .as_deref()
        .filter(|n| !n.eq_ignore_ascii_case(&order.order_id))
    {
        return Err(ResolveError::OrderMismatch {
            named: named.to_string(),
            current: order.order_id.clone(),
        });
    }
    let role = contract_role(pool, &chain, asset.contract.address()).await?;
    let token = token_record(pool, &chain, &role, asset.contract.address(), token_id).await?;
    let offer = legacy_offer(&chain, &order, asset.contract.address(), token_id);
    Ok(Resolved {
        facts: Facts {
            session,
            offer: offer.clone(),
            contract: role,
            item: Lookup::Unknown,
            token,
        },
        chain,
        asset,
        offer,
        listing: Listing::LegacyOrder {
            order,
            index_ready: true,
        },
        available: None,
    })
}

/// A quote reads only the requested item's mint state and exact unit price. The existing
/// (network, contract_address, item_id) key serves this bounded lookup; no new projection.
async fn resolve_store(
    pool: &PgPool,
    chain: Chain,
    session: Session,
    asset: AssetId,
) -> Result<Resolved, ResolveError> {
    let TokenRef::Item { item_id } = asset.token else {
        return Err(ResolveError::StoreUnavailable(asset));
    };
    let venue = crate::credits::batch::collection_store(chain.chain_id)
        .ok_or(ResolveError::NoVenue("CollectionStore"))?;
    let (item, available) = item_record(pool, &chain, asset.contract.address(), item_id).await?;
    match &item {
        Lookup::Unknown => {
            return Err(ResolveError::IndexNotReady {
                index: "items-mirror",
            });
        }
        Lookup::Absent => return Err(ResolveError::StoreUnavailable(asset)),
        Lookup::Found(_) => {}
    }
    let net = chain.net.ok_or(ResolveError::StoreUnavailable(asset))?;
    let price: Option<String> = sqlx::query_scalar(
        "SELECT price::text FROM shop.items WHERE network = $1 AND contract_address = $2 AND item_id = $3::numeric \
         AND search_is_store_minter AND search_is_collection_approved",
    )
    .bind(net.db_name()).bind(asset.contract.to_string()).bind(item_id.to_string()).fetch_optional(pool).await?;
    let price = price.ok_or(ResolveError::StoreUnavailable(asset))?;
    let price = ManaWei::new(
        crate::types::parse_decimal(&price)
            .map_err(|_| ResolveError::Db("invalid CollectionStore price".into()))?,
    );
    let role = contract_role(pool, &chain, asset.contract.address()).await?;
    let offer = Offer::StoreMint {
        venue: chain.contract(venue.address()),
        collection: asset.contract,
        item_id,
        price,
    };
    Ok(Resolved {
        facts: Facts {
            session,
            offer: offer.clone(),
            contract: role,
            item,
            token: Lookup::Absent,
        },
        chain,
        asset,
        offer,
        listing: Listing::Store {
            venue: venue.address(),
            price,
        },
        available,
    })
}

/// The coupon managers wired into the marketplace `venue` on `chain_id`: a
/// coupon is redeemable only through those (the catalogue's pairings).
pub(crate) fn paired_managers(chain_id: ChainId, venue: Address) -> Vec<String> {
    let venue = format!("{venue:#x}");
    crate::market::shop::coupon_pairings()
        .into_iter()
        .filter(|(chain, marketplace, _)| *chain == chain_id.0 as i64 && *marketplace == venue)
        .map(|(_, _, manager)| manager)
        .collect()
}

/// The coupon a primary trade's purchase offer carries ([`resolve_trade`]),
/// as the policy prices it: the same selection, so the credits lane
/// authorizes the price the offer quoted.
pub(crate) async fn primary_trade_coupon(
    pool: &PgPool,
    trade: &SignedTrade,
    trade_type: &str,
) -> Result<Option<crate::types::TradeCoupon>, ResolveError> {
    Ok(trade_coupon(pool, trade, trade_type)
        .await?
        .map(|c| crate::types::TradeCoupon {
            contract: EvmAddress::new(c.coupon_address),
            discount_ppm: c.discount_ppm,
        }))
}

/// The creator's coupon a trade settles with, by the catalogue's rule: only
/// a `public_item_order` that sells collection items for USD-pegged MANA
/// (the native branch `coupon_join` joins), and only a coupon that covers
/// every collection the trade sells, since `acceptWithCoupon` needs a proof
/// for each sent asset. Anything else settles at the signed price.
async fn trade_coupon(
    pool: &PgPool,
    trade: &SignedTrade,
    trade_type: &str,
) -> Result<Option<super::calls::CouponIn>, ResolveError> {
    let eligible = trade_type == "public_item_order"
        && !trade.sent.is_empty()
        && trade
            .sent
            .iter()
            .all(|a| a.asset_type == TradeAssetType::CollectionItem)
        && !trade.received.is_empty()
        && trade
            .received
            .iter()
            .all(|a| a.asset_type == TradeAssetType::UsdPeggedMana);
    if !eligible {
        return Ok(None);
    }
    let coupon = live_coupon(
        pool,
        trade.venue.chain_id,
        trade.venue.contract.address(),
        &trade.signer.to_string(),
        trade.sent[0].contract.address(),
    )
    .await?;
    Ok(coupon.filter(|c| {
        trade.sent.iter().all(|a| {
            let collection = format!("{:#x}", a.contract.address());
            c.collections
                .iter()
                .any(|covered| covered.eq_ignore_ascii_case(&collection))
        })
    }))
}

/// The best live creator coupon for a primary trade, by the catalogue's rule
/// (`market::shop::coupon_join`): signed by the trade's signer on its chain,
/// against a manager paired with its marketplace, covering its collection,
/// inside its window, neither cancelled nor revoked, with uses left as of the
/// last on-chain read. Biggest discount wins, then the one ending soonest.
async fn live_coupon(
    pool: &PgPool,
    chain_id: ChainId,
    venue: Address,
    signer: &str,
    collection: Address,
) -> Result<Option<super::calls::CouponIn>, ResolveError> {
    let managers = paired_managers(chain_id, venue);
    if managers.is_empty() {
        return Ok(None);
    }
    type Row = (
        String,
        String,
        i16,
        i32,
        String,
        Vec<String>,
        String,
        serde_json::Value,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT c.id::text, c.coupon_address, c.discount_type, c.discount_ppm, c.root, c.collections, c.signature, c.checks \
         FROM shop.coupons c \
         LEFT JOIN shop.coupon_state cs ON cs.coupon_id = c.id \
         WHERE c.signer = lower($1) \
           AND c.chain_id = $2 \
           AND lower(c.coupon_manager) = ANY($3) \
           AND c.effective_since <= timezone('UTC', now()) AND c.expires_at > timezone('UTC', now()) \
           AND lower($4) = ANY(c.collections) \
           AND c.discount_type = 1 AND c.discount_ppm > 0 AND c.discount_ppm < 1000000 \
           AND NOT COALESCE(cs.cancelled, false) AND NOT COALESCE(cs.revoked, false) \
           AND COALESCE(cs.uses, 0) < (c.checks->>'uses')::numeric \
         ORDER BY c.discount_ppm DESC, c.expires_at ASC, c.id ASC \
         LIMIT 1",
    )
    .bind(signer)
    .bind(chain_id.0 as i32)
    .bind(&managers)
    .bind(format!("{collection:#x}"))
    .fetch_optional(pool)
    .await
    .map_err(|e| ResolveError::Db(e.to_string()))?;
    let Some((id, address, discount_type, ppm, root, collections, signature, checks)) = row else {
        return Ok(None);
    };
    // The SQL already keeps only rate coupons with a discount in range. A
    // row that still does not decode is logged and the trade sells at its
    // signed price: one bad row must not take the listing down.
    let decoded = (|| {
        let coupon_address: Address = address
            .trim()
            .parse()
            .map_err(|_| format!("address {address}"))?;
        let discount_ppm = u32::try_from(ppm)
            .ok()
            .filter(|p| *p > 0 && *p < crate::types::TradeCoupon::PPM)
            .ok_or_else(|| format!("discount {ppm} ppm"))?;
        let discount_type =
            u64::try_from(discount_type).map_err(|_| format!("discount type {discount_type}"))?;
        let checks = serde_json::from_value(checks).map_err(|e| format!("checks: {e}"))?;
        Ok::<_, String>((coupon_address, discount_ppm, discount_type, checks))
    })();
    let (coupon_address, discount_ppm, discount_type, checks) = match decoded {
        Ok(decoded) => decoded,
        Err(what) => {
            tracing::warn!(coupon = %id, "skipping a coupon row that does not decode: {what}");
            return Ok(None);
        }
    };
    Ok(Some(super::calls::CouponIn {
        id,
        coupon_address,
        discount_type,
        discount_ppm,
        root,
        collections,
        signature,
        checks,
    }))
}

pub(crate) async fn resolve_trade(
    pool: &PgPool,
    chain: Chain,
    session: Session,
    asset: AssetId,
    id: &str,
) -> Result<Resolved, ResolveError> {
    // A quote offers only what the node lists.
    let trade = load_trade(pool, id, trades::TradeScope::Served)
        .await?
        .ok_or_else(|| ResolveError::TradeNotFound(id.to_string()))?;
    if let Some(trade_chain) = trade.chain_id
        && trade_chain != chain.chain_id.0
    {
        return Err(ResolveError::TradeChain {
            id: id.to_string(),
            trade_chain,
            line_chain: chain.chain_id.0,
        });
    }
    let venue: Address = trade
        .contract
        .as_deref()
        .and_then(|c| c.trim().parse().ok())
        .ok_or_else(|| ResolveError::TradeVenue(id.to_string()))?;
    if chain.marketplace(venue).is_none() {
        return Err(ResolveError::TradeVenue(id.to_string()));
    }
    let signed = signed_trade(&trade, chain.contract(venue))
        .map_err(|e| ResolveError::TradeShape(id.to_string(), e))?;
    let subject = signed
        .sent
        .first()
        .ok_or_else(|| ResolveError::TradeShape(id.to_string(), "no sent asset".into()))?;
    let sells = match (subject.asset_type, asset.token) {
        (TradeAssetType::Erc721, TokenRef::Token { token_id }) => subject.value == token_id,
        (TradeAssetType::CollectionItem, TokenRef::Item { item_id }) => subject.value == item_id,
        _ => false,
    } && subject.contract == asset.contract;
    if !sells {
        return Err(ResolveError::TradeAsset(id.to_string()));
    }
    let hashed_signature = hashed(&trade.signature);
    let (verdict, index_ready) = match chain.net {
        Some(net) => trade_verdict(pool, net, &trade.id).await?,
        None => (None, false),
    };
    let role = contract_role(pool, &chain, asset.contract.address()).await?;
    let (item, token, available) = match asset.token {
        TokenRef::Item { item_id } => {
            let (item, available) =
                item_record(pool, &chain, asset.contract.address(), item_id).await?;
            (item, Lookup::Absent, available)
        }
        TokenRef::Token { token_id } => {
            let token =
                token_record(pool, &chain, &role, asset.contract.address(), token_id).await?;
            let item = match &token {
                Lookup::Found(t) if t.item_id.is_some() => {
                    item_record(pool, &chain, asset.contract.address(), t.item_id.unwrap())
                        .await?
                        .0
                }
                _ => Lookup::Unknown,
            };
            (item, token, None)
        }
    };
    // A primary sale carries the creator's best live coupon, as the catalogue
    // shows it: priced and settled through acceptWithCoupon, never at the list
    // price while the page shows the sale.
    let coupon = trade_coupon(pool, &signed, &trade.trade_type).await?;
    let offer = Offer::Trade {
        trade: signed,
        coupon: coupon.as_ref().map(|c| crate::types::TradeCoupon {
            contract: EvmAddress::new(c.coupon_address),
            discount_ppm: c.discount_ppm,
        }),
    };
    Ok(Resolved {
        facts: Facts {
            session,
            offer: offer.clone(),
            contract: role,
            item,
            token,
        },
        chain,
        asset,
        offer,
        listing: Listing::Trade {
            trade,
            venue,
            hashed_signature,
            verdict,
            index_ready,
            coupon,
        },
        available,
    })
}

/// keccak256 of a trade's signature as the events and `shop.trades` spell it.
pub fn hashed(signature: &str) -> String {
    super::calls::hashed_signature(signature)
        .map(|h| format!("{h:#x}"))
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
async fn resolve_rental(
    pool: &PgPool,
    rentals: &crate::rentals::db::Database,
    chain: Chain,
    session: Session,
    asset: AssetId,
    token_id: U256,
    rental_id: &str,
    line: &LineRequest,
) -> Result<Resolved, ResolveError> {
    let listing = rentals
        .get_listing_by_id(rental_id)
        .await?
        .ok_or_else(|| ResolveError::RentalNotFound(rental_id.to_string()))?;
    let same_asset = listing
        .contract_address
        .eq_ignore_ascii_case(&asset.contract.to_string())
        && listing.token_id == token_id.to_string()
        && listing.chain_id == chain.chain_id.0 as i64;
    if !same_asset {
        return Err(ResolveError::RentalAsset(rental_id.to_string()));
    }
    let rentals_contract = chain
        .rentals
        .ok_or(ResolveError::NoVenue("rentals contract"))?;
    let terms = rent_terms(&listing, line.period_index.unwrap_or(0), line.rental_days)?;
    let role = contract_role(pool, &chain, asset.contract.address()).await?;
    let token = token_record(pool, &chain, &role, asset.contract.address(), token_id).await?;
    let offer = Offer::Rental {
        venue: chain.contract(rentals_contract),
        contract: asset.contract,
        token_id,
        price: terms.total,
    };
    let status_index_ready = crate::rentals::index::ready(pool, chrono::Utc::now()).await;
    Ok(Resolved {
        facts: Facts {
            session,
            offer: offer.clone(),
            contract: role,
            item: Lookup::Unknown,
            token,
        },
        chain,
        asset,
        offer,
        listing: Listing::Rental {
            listing,
            terms,
            rentals: rentals_contract,
            status_index_ready,
        },
        available: None,
    })
}

async fn resolve_name(
    book: &Book,
    session: Session,
    line: &LineRequest,
    name: &str,
) -> Result<Resolved, ResolveError> {
    let label = NameLabel::parse(name).map_err(|e| ResolveError::Name(e.to_string()))?;
    let chain = match line.asset {
        Some(asset) => book
            .chain(asset.chain_id)
            .ok_or(ResolveError::UnknownChain(asset.chain_id))?
            .clone(),
        None => book
            .chains()
            .find(|c| c.name_registrar.is_some() && c.name_controller.is_some())
            .ok_or(ResolveError::NoVenue("NAME registrar"))?
            .clone(),
    };
    let registrar = chain
        .name_registrar
        .ok_or(ResolveError::NoVenue("NAME registrar"))?;
    let controller = chain
        .name_controller
        .ok_or(ResolveError::NoVenue("NAME controller"))?;
    let asset = AssetId::name(chain.contract(registrar), &label);
    if let Some(named) = line.asset
        && named != asset
    {
        return Err(ResolveError::Name(format!(
            "the asset named is not the registrar token of {}",
            label.as_str()
        )));
    }
    // The price and the registration state are chain reads the verifier
    // makes; the offer carries the controller's fixed price once read, so
    // the resolver leaves it to `verify::name` and starts from zero here.
    let offer = Offer::NameRegistration {
        label: label.clone(),
        registrar: chain.contract(registrar),
        mana_venue: chain.contract(controller),
        credits_venue: chain.contract(controller),
        price: ManaWei::ZERO,
    };
    Ok(Resolved {
        facts: Facts {
            session,
            offer: offer.clone(),
            contract: Lookup::Found(ContractRole::NameRegistrar),
            item: Lookup::Absent,
            token: Lookup::Unknown,
        },
        chain,
        asset,
        offer,
        listing: Listing::Name {
            label,
            registrar,
            controller,
        },
        available: None,
    })
}

/// A NAME line once the registrar answered: the price the controller
/// charges and whether the label is registered.
pub fn with_name_reads(
    mut resolved: Resolved,
    price: ManaWei,
    registered_owner: Option<Address>,
) -> Resolved {
    if let Offer::NameRegistration { price: p, .. } = &mut resolved.offer {
        *p = price;
    }
    resolved.facts.offer = resolved.offer.clone();
    resolved.facts.token = match registered_owner {
        Some(owner) => Lookup::Found(TokenRecord {
            asset: resolved.asset,
            item_id: None,
            owner: evm(owner),
        }),
        None => Lookup::Absent,
    };
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_names_exactly_one_listing() {
        let one = ListingRef {
            trade_id: Some("t".into()),
            ..Default::default()
        };
        assert_eq!(listing_count(&one), 1);
        let two = ListingRef {
            trade_id: Some("t".into()),
            order_id: Some("o".into()),
            ..Default::default()
        };
        assert_eq!(listing_count(&two), 2);
        assert_eq!(listing_count(&ListingRef::default()), 0);
    }

    /// A primary trade on Polygon's V3 is offered with the creator's best coupon
    /// whose manager is paired with V3, as the catalogue joins it: a bigger
    /// coupon of the V2 manager never applies, nor an expired, not-yet-effective,
    /// cancelled, revoked, used-up, foreign-signer or other-collection one.
    #[tokio::test]
    async fn a_primary_trade_carries_the_creators_live_coupon_for_its_venue() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let fresh = || -> Address {
            format!(
                "0x{}{}",
                &uuid::Uuid::new_v4().simple().to_string()[..32],
                "00000000"
            )
            .parse()
            .unwrap()
        };
        let creator = format!("{:#x}", fresh());
        let collection = fresh();
        let other = fresh();
        let v3: Address = "0xe38ef22abe871513555cba89adfe45ab4f548ada"
            .parse()
            .unwrap();
        let v3_manager = "0x655fdfa91d69ea49f4ce1a8f7f7e2622c8630813";
        let v2_manager = "0x3fd3056ee72a2a85e9392fab3a450e7736536081";
        let live =
            "timezone('UTC', now()) - interval '1 day', timezone('UTC', now()) + interval '1 day'";
        let expired =
            "timezone('UTC', now()) - interval '2 days', timezone('UTC', now()) - interval '1 day'";
        let future =
            "timezone('UTC', now()) + interval '1 day', timezone('UTC', now()) + interval '2 days'";
        let mut created: Vec<String> = Vec::new();
        let insert = async |created: &mut Vec<String>,
                            manager: &str,
                            ppm: i32,
                            window: &str,
                            covered: Address,
                            signer: &str,
                            state: Option<&str>| {
            let sig = format!("0x{}", uuid::Uuid::new_v4().simple());
            let id: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "INSERT INTO shop.coupons (network, chain_id, signer, signature, hashed_signature, state_key, coupon_manager, coupon_address, checks, \
                    discount_type, discount_ppm, root, collections, effective_since, expires_at) \
                 VALUES ('MATIC', 137, $1, $2, $2, $2, $3, '0xc914507fe297b2dddd1232ac3a8903f1c125e794', \
                    '{{\"uses\": 10, \"expiration\": 4102444800000, \"effective\": 0, \"salt\": \"0x01\", \"contractSignatureIndex\": 0, \"signerSignatureIndex\": 0, \"allowedRoot\": \"0x\", \"externalChecks\": []}}'::jsonb, \
                    1, $4, '0x{root}', ARRAY[$5], {window}) RETURNING id::text",
                root = "00".repeat(32)
            )))
            .bind(signer)
            .bind(&sig)
            .bind(manager)
            .bind(ppm)
            .bind(format!("{covered:#x}"))
            .fetch_one(&pool)
            .await
            .unwrap();
            if let Some(state) = state {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "INSERT INTO shop.coupon_state (coupon_id, {state}) VALUES ($1::uuid, {})",
                    if state == "uses" { "10" } else { "true" }
                )))
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
            }
            created.push(id.clone());
            id
        };
        let lookup = async |venue: Address| {
            live_coupon(&pool, ChainId::POLYGON, venue, &creator, collection)
                .await
                .unwrap()
        };
        assert!(lookup(v3).await.is_none(), "no coupon yet");
        insert(
            &mut created,
            v2_manager,
            700_000,
            live,
            collection,
            &creator,
            None,
        )
        .await;
        insert(
            &mut created,
            v3_manager,
            300_000,
            expired,
            collection,
            &creator,
            None,
        )
        .await;
        insert(
            &mut created,
            v3_manager,
            350_000,
            future,
            collection,
            &creator,
            None,
        )
        .await;
        insert(
            &mut created,
            v3_manager,
            400_000,
            live,
            other,
            &creator,
            None,
        )
        .await;
        insert(
            &mut created,
            v3_manager,
            450_000,
            live,
            collection,
            "0x00000000000000000000000000000000000000aa",
            None,
        )
        .await;
        insert(
            &mut created,
            v3_manager,
            600_000,
            live,
            collection,
            &creator,
            Some("uses"),
        )
        .await;
        insert(
            &mut created,
            v3_manager,
            550_000,
            live,
            collection,
            &creator,
            Some("cancelled"),
        )
        .await;
        insert(
            &mut created,
            v3_manager,
            500_000,
            live,
            collection,
            &creator,
            Some("revoked"),
        )
        .await;
        assert!(
            lookup(v3).await.is_none(),
            "none of these is a live V3 coupon of this creator's collection"
        );
        insert(
            &mut created,
            v3_manager,
            100_000,
            live,
            collection,
            &creator,
            None,
        )
        .await;
        let best = insert(
            &mut created,
            v3_manager,
            200_000,
            live,
            collection,
            &creator,
            None,
        )
        .await;
        let coupon = lookup(v3).await.expect("the live V3 coupon");
        assert_eq!(
            (coupon.id.as_str(), coupon.discount_ppm),
            (best.as_str(), 200_000),
            "the biggest live discount wins"
        );
        assert_eq!(
            format!("{:#x}", coupon.coupon_address),
            "0xc914507fe297b2dddd1232ac3a8903f1c125e794"
        );

        // A bigger coupon that is not a rate coupon never ranks.
        let flat = insert(
            &mut created,
            v3_manager,
            650_000,
            live,
            collection,
            &creator,
            None,
        )
        .await;
        sqlx::query("UPDATE shop.coupons SET discount_type = 2 WHERE id = $1::uuid")
            .bind(&flat)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            lookup(v3).await.map(|c| c.id),
            Some(best.clone()),
            "only rate coupons rank"
        );
        // One that ranks first but does not decode is skipped, not an error:
        // the trade sells at its signed price rather than not at all.
        let broken = insert(
            &mut created,
            v3_manager,
            690_000,
            live,
            collection,
            &creator,
            None,
        )
        .await;
        sqlx::query("UPDATE shop.coupons SET checks = '{\"uses\": 10}'::jsonb WHERE id = $1::uuid")
            .bind(&broken)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            live_coupon(&pool, ChainId::POLYGON, v3, &creator, collection)
                .await
                .expect("a bad row is not a resolve error")
                .is_none(),
            "a coupon row that does not decode prices the trade without a coupon"
        );
        sqlx::query("DELETE FROM shop.coupons WHERE id = $1::uuid")
            .bind(&broken)
            .execute(&pool)
            .await
            .unwrap();
        let v2: Address = "0xa40b1d129b8906888720686f3a01921ddf37716f"
            .parse()
            .unwrap();
        assert_eq!(
            lookup(v2).await.unwrap().discount_ppm,
            700_000,
            "a V2 trade takes the V2 manager's coupon"
        );
        assert!(paired_managers(ChainId::POLYGON, Address::ZERO).is_empty());

        // Which trades settle with it: the catalogue's native primaries only,
        // and only when it covers every collection they sell.
        use crate::types::{TradeAsset, TradeAssetType as T};
        let asset = |asset_type: T, contract: Address| TradeAsset {
            asset_type,
            contract: EvmAddress::new(contract),
            value: U256::from(1u8),
        };
        let mana: Address = "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4"
            .parse()
            .unwrap();
        let signed = |sent: Vec<TradeAsset>, paid: T| SignedTrade {
            venue: ContractRef {
                chain_id: ChainId::POLYGON,
                contract: EvmAddress::new(v3),
            },
            signer: EvmAddress::parse(&creator).unwrap(),
            sent,
            received: vec![asset(paid, mana)],
        };
        let settles = async |trade: SignedTrade, kind: &str| {
            trade_coupon(&pool, &trade, kind)
                .await
                .unwrap()
                .map(|c| c.discount_ppm)
        };
        let item = asset(T::CollectionItem, collection);
        assert_eq!(
            settles(
                signed(vec![item.clone()], T::UsdPeggedMana),
                "public_item_order"
            )
            .await,
            Some(200_000),
            "a USD-pegged item order takes it"
        );
        assert_eq!(
            settles(signed(vec![item.clone()], T::Erc20), "public_item_order").await,
            None,
            "a MANA-priced primary is not in the catalogue's coupon branch: no hidden discount"
        );
        assert_eq!(
            settles(
                signed(
                    vec![item.clone(), asset(T::CollectionItem, other)],
                    T::UsdPeggedMana
                ),
                "public_item_order"
            )
            .await,
            None,
            "a coupon that does not cover every sold collection cannot be proven for each asset"
        );
        assert_eq!(
            settles(
                signed(vec![item.clone()], T::UsdPeggedMana),
                "public_nft_order"
            )
            .await,
            None,
            "a resale never takes a creator coupon"
        );
        sqlx::query("DELETE FROM shop.coupons WHERE id = ANY($1::uuid[])")
            .bind(&created)
            .execute(&pool)
            .await
            .unwrap();
    }
}
