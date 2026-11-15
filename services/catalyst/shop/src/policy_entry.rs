//! How an entry turns what it holds into the [`policy`](super::policy) inputs:
//! the authoritative facts a line is judged on and the decoded call an
//! operation executes. Nothing here decides; it reads and decodes, and every
//! reader answers [`Lookup::Unknown`] when its source cannot say, so the rule
//! refuses with `Incomplete` instead of guessing.
//!
//! Sources, in the order the rule trusts them:
//!
//! | fact | source | reader |
//! | --- | --- | --- |
//! | a contract's role | the static registry of the LAND, estate and NAME registrars per chain, then `shop.collections` (the collections mirror, only after a complete pass) | [`item_facts`], [`contract_role`] |
//! | a collection item | `shop.items` (the items mirror, only after a complete pass) | [`item_facts`] |
//! | a signed trade | `shop.trades` (+ its typed asset rows) or the decoded calldata | [`signed_trade_from_trade_in`], [`signed_trade_from_sol`], [`decode_use_credits`] |
//! | a NAME's registration | `DCLRegistrar.ownerOf(tokenId)` on the NAMEs chain | [`name_token_from_owner_call`] |
//! | the CreditsManager's typed routes | the pinned deployment (docs/shop/CONTRACTS.md, "Addresses") | [`manager_routes`] |
//!
//! `useCredits` is decoded as the contract itself routes it
//! (`CreditsManagerPolygon._handlePreExecution` at the pin): the legacy
//! marketplace takes `executeOrder`, an allowed marketplace `accept` or
//! `acceptWithCoupon`, the CollectionStore `buy`, anything else is a custom
//! call whose `data` the contract never decodes, and neither do we
//! ([`CustomEffect::Opaque`]).

use alloy_primitives::{Address, U256, address};
use alloy_sol_types::{SolCall, SolValue};
use sqlx::PgPool;
use sqlx::Row;

use super::types::{
    AssetId, ChainId, ContractRef, ContractRole, CustomEffect, EvmAddress, ExternalCall, ItemKind,
    ItemRecord, Lookup, ManaWei, SignedTrade, StoreItem, TokenRecord, TradeAsset, TradeAssetType,
    TradeCoupon,
};
use crate::checkout::trade::{SolTrade, TradeIn};
use crate::evm::abi::{ItemToBuy, buyCall, executeOrderCall, offchain};

/// `useCredits(UseCreditsArgs)` (tests/fixtures/shop/offchain-marketplace-contract@87350c63e/derived/selectors.json).
pub const USE_CREDITS_SELECTOR: [u8; 4] = [0x18, 0x63, 0x57, 0x2d];
/// `RegisterNameCrossChainExecutor.execute((address,bytes,bytes))`, the one
/// custom call the shared manager allows on its executors.
pub const EXECUTOR_EXECUTE_SELECTOR: [u8; 4] = [0xfd, 0x16, 0x5a, 0x73];

/// The CreditsManagers upstream operates (docs/shop/CONTRACTS.md): v2 never
/// issues a credit against one of them, whatever their flags read today.
pub const SHARED_UPSTREAM_MANAGERS: [Address; 4] = [
    address!("0x8b3a40ca1b6f5cafc99d112a4d02e897d1fd8cc5"),
    address!("0xe9f961e6ded4e1476bbee4faab886d63a2493eb9"),
    address!("0x8052a560e6e6ac86eeb7e711a4497f639b322fb3"),
    address!("0x037566bc90f85e76587e1b07f9184585f09c1420"),
];

/// The NAME cross-chain executors: pass-through targets whose forwarded
/// calldata nothing decodes; a manager that allows one can reach a resale.
pub fn passthrough_executors(chain_id: ChainId) -> &'static [Address] {
    const POLYGON: [Address; 3] = [
        address!("0x84c682b1b862859730446bca5b853a5693aefc84"),
        address!("0xc9a9c416fbfd424d09e3f905142799abb10c2fa7"),
        address!("0x5e359ebd4cc379f46020fac720261dc9c1d80404"),
    ];
    match chain_id {
        ChainId::POLYGON => &POLYGON,
        _ => &[],
    }
}

/// dcl-one's own CreditsManagerPolygon on Amoy, the one tooling/dev.py's testnet
/// profile names in CREDITS_MANAGER_CONTRACT (`CreditsManager_DCL_ONE` in
/// contracts.decentraland.org's `amoy` list). It is the shared manager's code on
/// the same routes (its legacy marketplace, CollectionStore and MANA are Amoy's,
/// as [`manager_routes`] names them), so a relayed `useCredits` to it is decoded
/// and judged like one to the shared manager. It is not a shared upstream manager:
/// credits are issued for it.
pub const DCL_ONE_AMOY_MANAGER: Address = address!("0xb65fd692d15c30c3adcacdd5ccf80e1facecfc7c");

/// dcl-one's own CreditsManagerPolygon on Polygon mainnet (`CreditsManager_DCL_ONE`
/// under `matic` in `evm/addresses.json`), deployed in block 97077690 from the
/// creation bytecode of [`DCL_ONE_AMOY_MANAGER`]. It runs the shared manager's code,
/// so a relayed `useCredits` to it is decoded on the routes [`manager_routes`] names
/// for Polygon and judged like one to the shared manager; which of those routes its
/// own `marketplaces` allow is the contract's state, not this table's. It is not a
/// shared upstream manager: v2 may issue credits for it once a deployment names it
/// in CREDITS_MANAGER_CONTRACT and its issuance gate opens.
pub const DCL_ONE_POLYGON_MANAGER: Address = address!("0xe21289022b9324f9b9145a91ee59297ceb040c06");

/// The CreditsManagers a meta-transaction may target on a chain: the live and
/// the deprecated upstream one, and dcl-one's own; the relay's audited-domain
/// table names the same set.
pub fn credits_managers(chain_id: ChainId) -> &'static [Address] {
    const POLYGON: [Address; 3] = [
        address!("0x8b3a40ca1b6f5cafc99d112a4d02e897d1fd8cc5"),
        address!("0xe9f961e6ded4e1476bbee4faab886d63a2493eb9"),
        DCL_ONE_POLYGON_MANAGER,
    ];
    const AMOY: [Address; 3] = [
        address!("0x8052a560e6e6ac86eeb7e711a4497f639b322fb3"),
        address!("0x037566bc90f85e76587e1b07f9184585f09c1420"),
        DCL_ONE_AMOY_MANAGER,
    ];
    match chain_id {
        ChainId::POLYGON => &POLYGON,
        ChainId::AMOY => &AMOY,
        _ => &[],
    }
}

pub fn is_credits_manager(chain_id: ChainId, target: Address) -> bool {
    credits_managers(chain_id).contains(&target)
}

/// The typed routes a manager on `chain_id` executes through: its immutable
/// `legacyMarketplace` and `collectionStore` and its allowed `marketplaces`
/// (docs/shop/CONTRACTS.md, "Deployed state").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerRoutes {
    pub chain_id: ChainId,
    pub legacy_marketplace: Address,
    pub collection_store: Address,
    pub marketplaces: &'static [Address],
}

pub fn manager_routes(chain_id: ChainId) -> Option<ManagerRoutes> {
    const POLYGON_MARKETPLACES: [Address; 3] = [
        address!("0x540fb08edb56aae562864b390542c97f562825ba"),
        address!("0xa40b1d129b8906888720686f3a01921ddf37716f"),
        address!("0xe38ef22abe871513555cba89adfe45ab4f548ada"),
    ];
    const AMOY_MARKETPLACES: [Address; 3] = [
        address!("0x6ab20ae56673ed65f520b7be332aeb61b3ed727d"),
        address!("0x1b67d0e31eeb6b52d8eeed71d3616c2f5b33b8e7"),
        address!("0x36fd1434a6c4b8ade80c9847c1d15033ce34488c"),
    ];
    match chain_id {
        ChainId::POLYGON => Some(ManagerRoutes {
            chain_id,
            legacy_marketplace: address!("0x480a0f4e360e8964e68858dd231c2922f1df45ef"),
            collection_store: address!("0x214ffc0f0103735728dc66b61a22e4f163e275ae"),
            marketplaces: &POLYGON_MARKETPLACES,
        }),
        ChainId::AMOY => Some(ManagerRoutes {
            chain_id,
            legacy_marketplace: address!("0x0c8ad1f6aadf89d2eb19f01a100a6143108fe2b0"),
            collection_store: address!("0xe36abc9ec616c83caaa386541380829106149d68"),
            marketplaces: &AMOY_MARKETPLACES,
        }),
        _ => None,
    }
}

/// The asset registries whose role no mirror has to say: LAND, estates and
/// the NAME registrar on the NAMEs chain (docs/shop/CONTRACTS.md,
/// "Addresses"; the Sepolia pair from decentraland-transactions).
pub fn static_role(chain_id: ChainId, contract: Address) -> Option<ContractRole> {
    let table: &[(Address, ContractRole)] = match chain_id {
        ChainId::ETHEREUM => &[
            (
                address!("0xf87e31492faf9a91b02ee0deaad50d51d56d5d4d"),
                ContractRole::Land,
            ),
            (
                address!("0x959e104e1a4db6317fa58f8295f586e1a978c297"),
                ContractRole::Estate,
            ),
            (
                address!("0x2a187453064356c898cae034eaed119e1663acb8"),
                ContractRole::NameRegistrar,
            ),
        ],
        ChainId::SEPOLIA => &[
            (
                address!("0x42f4ba48791e2de32f5fbf553441c2672864bb33"),
                ContractRole::Land,
            ),
            (
                address!("0x369a7fbe718c870c79f99fb423882e8dd8b20486"),
                ContractRole::Estate,
            ),
            (
                address!("0x7518456ae93eb98f3e64571b689c626616bb7f30"),
                ContractRole::NameRegistrar,
            ),
        ],
        _ => &[],
    };
    table
        .iter()
        .find(|(a, _)| *a == contract)
        .map(|(_, role)| *role)
}

/// The `shop.items`/`shop.collections` network of a chain, `None` for one
/// the mirrors do not cover.
pub fn mirror_network(chain_id: ChainId) -> Option<&'static str> {
    crate::evm::contracts_addrs::mirror_network(chain_id.0)
}

/// What the registry and the items mirror say about one collection item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemFacts {
    pub contract: Lookup<ContractRole>,
    pub item: Lookup<ItemRecord>,
}

fn item_kind(item_type: &str) -> ItemKind {
    if item_type.starts_with("emote") {
        ItemKind::Emote
    } else {
        ItemKind::Wearable
    }
}

/// One statement: both mirrors' pass markers, the collection row and the
/// item row, by their primary keys (schemas/30_shop.sql; no new index).
const ITEM_FACTS_SQL: &str = "SELECT \
     (SELECT last_complete_pass_at IS NOT NULL FROM shop.mirror_state WHERE mirror = 'collections' AND network = $1) AS collections_ready, \
     (SELECT last_complete_pass_at IS NOT NULL FROM shop.mirror_state WHERE mirror = 'items' AND network = $1) AS items_ready, \
     EXISTS (SELECT 1 FROM shop.collections WHERE network = $1 AND contract_address = $2) AS is_collection, \
     i.item_type, i.creator, i.available::text AS available \
     FROM (SELECT 1) AS one \
     LEFT JOIN shop.items i ON i.network = $1 AND i.contract_address = $2 AND i.item_id = $3::numeric";

/// What the collections and items mirrors of `network` say about
/// `(collection, item_id)`, with the item's remaining supply when the record
/// is found. Each answer is `Unknown` until its mirror's first complete pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorFacts {
    pub contract: Lookup<ContractRole>,
    pub item: Lookup<ItemRecord>,
    pub available: Option<i64>,
}

/// The mirrors' answer alone, without the static registry. Both payment
/// paths read contract roles and item records through this one statement,
/// so the credits guards and the MANA resolver cannot judge the same item
/// on different facts.
pub async fn mirror_facts(
    pool: &PgPool,
    chain_id: ChainId,
    network: &str,
    collection: EvmAddress,
    item_id: U256,
) -> Result<MirrorFacts, sqlx::Error> {
    let row = sqlx::query(ITEM_FACTS_SQL)
        .bind(network)
        .bind(collection.to_string())
        .bind(item_id.to_string())
        .fetch_one(pool)
        .await?;
    let collections_ready: Option<bool> = row.get("collections_ready");
    let items_ready: Option<bool> = row.get("items_ready");
    let is_collection: bool = row.get("is_collection");
    let item_type: Option<String> = row.get("item_type");
    let creator: Option<String> = row.get("creator");
    let available: Option<String> = row.get("available");
    let contract = match collections_ready {
        Some(true) => {
            if is_collection {
                Lookup::Found(ContractRole::Collection)
            } else {
                Lookup::Absent
            }
        }
        _ => Lookup::Unknown,
    };
    let (item, available) = match (items_ready, item_type) {
        (Some(true), Some(item_type)) => {
            match creator.as_deref().and_then(|c| EvmAddress::parse(c).ok()) {
                Some(creator) => (
                    Lookup::Found(ItemRecord {
                        chain_id,
                        collection,
                        item_id,
                        kind: item_kind(&item_type),
                        creator,
                    }),
                    available.and_then(|a| a.parse::<i64>().ok()),
                ),
                None => (Lookup::Unknown, None),
            }
        }
        (Some(true), None) => (Lookup::Absent, None),
        _ => (Lookup::Unknown, None),
    };
    Ok(MirrorFacts {
        contract,
        item,
        available,
    })
}

/// The collections mirror's answer alone about `collection`'s role: the
/// contract part of [`mirror_facts`]. The statement's item lookup is
/// ignored, so any item id gives the same answer.
pub async fn mirror_contract_role(
    pool: &PgPool,
    chain_id: ChainId,
    network: &str,
    collection: EvmAddress,
) -> Result<Lookup<ContractRole>, sqlx::Error> {
    Ok(
        mirror_facts(pool, chain_id, network, collection, U256::ZERO)
            .await?
            .contract,
    )
}

/// The contract's role and the item record of `(collection, item_id)` on
/// `chain_id`, from the static registry and the two mirrors.
pub async fn item_facts(
    pool: &PgPool,
    chain_id: ChainId,
    collection: EvmAddress,
    item_id: U256,
) -> Result<ItemFacts, sqlx::Error> {
    if let Some(role) = static_role(chain_id, collection.address()) {
        return Ok(ItemFacts {
            contract: Lookup::Found(role),
            item: Lookup::Absent,
        });
    }
    let Some(network) = mirror_network(chain_id) else {
        return Ok(ItemFacts {
            contract: Lookup::Absent,
            item: Lookup::Absent,
        });
    };
    let facts = mirror_facts(pool, chain_id, network, collection, item_id).await?;
    Ok(ItemFacts {
        contract: facts.contract,
        item: facts.item,
    })
}

/// The registry's answer alone (an existing token, a legacy order).
pub async fn contract_role(
    pool: &PgPool,
    chain_id: ChainId,
    contract: EvmAddress,
) -> Result<Lookup<ContractRole>, sqlx::Error> {
    Ok(item_facts(pool, chain_id, contract, U256::ZERO)
        .await?
        .contract)
}

/// The item record for a line the items mirror already answered (the
/// credits pricing's `ItemInfo`): the mirror holds factory collections
/// only, so the contract role is `Collection` by construction.
pub fn item_facts_from_mirror(
    chain_id: ChainId,
    collection: EvmAddress,
    item_id: U256,
    category: &str,
    creator: EvmAddress,
) -> ItemFacts {
    let kind = if category == "emote" {
        ItemKind::Emote
    } else {
        ItemKind::Wearable
    };
    ItemFacts {
        contract: Lookup::Found(ContractRole::Collection),
        item: Lookup::Found(ItemRecord {
            chain_id,
            collection,
            item_id,
            kind,
            creator,
        }),
    }
}

/// `DCLRegistrar.ownerOf(tokenId)` as a NAME registry answer: an owner is a
/// registered NAME, a revert (`ERC721: owner query for nonexistent token`)
/// an available one, a transport failure `Unknown`.
pub fn name_token_from_owner_call(
    registrar: ContractRef,
    token_id: U256,
    answer: Result<Option<Address>, ()>,
) -> Lookup<TokenRecord> {
    match answer {
        Ok(Some(owner)) => Lookup::Found(TokenRecord {
            asset: AssetId::token(registrar.chain_id, registrar.contract, token_id),
            item_id: None,
            owner: EvmAddress::new(owner),
        }),
        Ok(None) => Lookup::Absent,
        Err(()) => Lookup::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("the trade's assets do not decode: {0}")]
    Trade(String),
    #[error("the useCredits arguments do not decode: {0}")]
    Args(String),
    #[error("a coupon of discount type {discount_type} cannot be priced; only a rate coupon can")]
    Coupon { discount_type: u64 },
    #[error("credits do not settle on chain {0}")]
    UnknownChain(ChainId),
}

fn decimal(label: &str, text: &str) -> Result<U256, DecodeError> {
    super::types::parse_decimal(text.trim())
        .map_err(|e| DecodeError::Trade(format!("{label}: {e}")))
}

fn evm(label: &str, text: &str) -> Result<EvmAddress, DecodeError> {
    EvmAddress::parse(&text.trim().to_ascii_lowercase())
        .map_err(|e| DecodeError::Trade(format!("{label}: {e}")))
}

fn asset_from_trade_in(
    asset: &crate::checkout::trade::TradeAssetIn,
) -> Result<TradeAsset, DecodeError> {
    let asset_type = TradeAssetType::from_code(u64::try_from(asset.asset_type).unwrap_or(u64::MAX));
    let value = match asset_type {
        TradeAssetType::Erc20 | TradeAssetType::UsdPeggedMana => asset
            .amount
            .as_deref()
            .ok_or_else(|| DecodeError::Trade("a fungible asset needs an amount".into()))?,
        TradeAssetType::Erc721 => asset
            .token_id
            .as_deref()
            .ok_or_else(|| DecodeError::Trade("an ERC-721 asset needs a tokenId".into()))?,
        TradeAssetType::CollectionItem => asset
            .item_id
            .as_deref()
            .ok_or_else(|| DecodeError::Trade("a collection item needs an itemId".into()))?,
        TradeAssetType::Unsupported(code) => {
            return Err(DecodeError::Trade(format!("unsupported asset type {code}")));
        }
    };
    Ok(TradeAsset {
        asset_type,
        contract: evm("asset contract", &asset.contract_address)?,
        value: decimal("asset value", value)?,
    })
}

/// A stored or mirrored trade (`shop.trades`, as the trades read serializes
/// it) as the rule's signed trade; `venue` is the marketplace it was signed for.
pub fn signed_trade_from_trade_in(
    trade: &TradeIn,
    venue: ContractRef,
) -> Result<SignedTrade, DecodeError> {
    Ok(SignedTrade {
        venue,
        signer: evm("signer", &trade.signer)?,
        sent: trade
            .sent
            .iter()
            .map(asset_from_trade_in)
            .collect::<Result<_, _>>()?,
        received: trade
            .received
            .iter()
            .map(asset_from_trade_in)
            .collect::<Result<_, _>>()?,
    })
}

/// The broker's validated trade (its on-chain form) as the rule's signed trade.
pub fn signed_trade_from_sol(trade: &SolTrade, venue: ContractRef) -> SignedTrade {
    let asset = |a: &crate::checkout::trade::SolAsset| TradeAsset {
        asset_type: TradeAssetType::from_code(a.assetType.try_into().unwrap_or(u64::MAX)),
        contract: EvmAddress::new(a.contractAddress),
        value: a.value,
    };
    SignedTrade {
        venue,
        signer: EvmAddress::new(trade.signer),
        sent: trade.sent.iter().map(asset).collect(),
        received: trade.received.iter().map(asset).collect(),
    }
}

fn signed_trade_from_offchain(trade: &offchain::Trade, venue: ContractRef) -> SignedTrade {
    let asset = |a: &offchain::Asset| TradeAsset {
        asset_type: TradeAssetType::from_code(a.assetType.try_into().unwrap_or(u64::MAX)),
        contract: EvmAddress::new(a.contractAddress),
        value: a.value,
    };
    SignedTrade {
        venue,
        signer: EvmAddress::new(trade.signer),
        sent: trade.sent.iter().map(asset).collect(),
        received: trade.received.iter().map(asset).collect(),
    }
}

const DISCOUNT_TYPE_RATE: u64 = crate::coupons::signature::DISCOUNT_TYPE_RATE as u64;

/// A CollectionDiscountCoupon's `data` (`abi.encode(discountType, discount,
/// root)`): the rate discount in parts per million.
fn coupon_from_offchain(coupon: &offchain::Coupon) -> Result<TradeCoupon, DecodeError> {
    let (discount_type, discount, _root) =
        <(U256, U256, alloy_primitives::B256)>::abi_decode(&coupon.data)
            .map_err(|e| DecodeError::Args(format!("coupon data: {e}")))?;
    let discount_type = u64::try_from(discount_type).unwrap_or(u64::MAX);
    if discount_type != DISCOUNT_TYPE_RATE {
        return Err(DecodeError::Coupon { discount_type });
    }
    let discount_ppm = u32::try_from(discount)
        .map_err(|_| DecodeError::Args("coupon discount overflows".into()))?;
    Ok(TradeCoupon {
        contract: EvmAddress::new(coupon.couponAddress),
        discount_ppm,
    })
}

mod credits_abi {
    alloy_sol_types::sol! {
        struct Credit {
            uint256 value;
            uint256 expiresAt;
            bytes32 salt;
        }

        struct ExternalCall {
            address target;
            bytes4 selector;
            bytes data;
            uint256 expiresAt;
            bytes32 salt;
        }

        struct UseCreditsArgs {
            Credit[] credits;
            bytes[] creditsSignatures;
            ExternalCall externalCall;
            bytes customExternalCallSignature;
            uint256 maxUncreditedValue;
            uint256 maxCreditedValue;
        }

        function useCredits(UseCreditsArgs _args) external;
    }
}

/// One credit named by a `useCredits` call, as signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditRef {
    pub value: ManaWei,
    pub expires_at: U256,
    pub salt: [u8; 32],
}

/// A decoded `useCredits`: the credits it spends, the call it executes and
/// the caller's two bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseCreditsCall {
    pub credits: Vec<CreditRef>,
    pub call: ExternalCall,
    pub max_uncredited: ManaWei,
    pub max_credited: ManaWei,
}

/// Decodes a `useCredits` call (selector included) against the manager's
/// typed routes on `chain_id`.
pub fn decode_use_credits(
    calldata: &[u8],
    chain_id: ChainId,
) -> Result<UseCreditsCall, DecodeError> {
    let routes = manager_routes(chain_id).ok_or(DecodeError::UnknownChain(chain_id))?;
    let call = credits_abi::useCreditsCall::abi_decode(calldata)
        .map_err(|e| DecodeError::Args(e.to_string()))?;
    let args = call._args;
    let external = &args.externalCall;
    let target = external.target;
    let selector: [u8; 4] = external.selector.0;
    let venue = ContractRef {
        chain_id,
        contract: EvmAddress::new(target),
    };
    let data: &[u8] = &external.data;
    let opaque = || ExternalCall::Custom {
        venue,
        selector,
        effect: CustomEffect::Opaque,
    };
    let call = if target == routes.legacy_marketplace {
        if selector == executeOrderCall::SELECTOR {
            let order = executeOrderCall::abi_decode_raw(data)
                .map_err(|e| DecodeError::Args(format!("executeOrder: {e}")))?;
            ExternalCall::LegacyExecuteOrder {
                venue,
                nft: EvmAddress::new(order.nftAddress),
                token_id: order.assetId,
                price: ManaWei::new(order.price),
            }
        } else {
            opaque()
        }
    } else if routes.marketplaces.contains(&target) {
        if selector == offchain::acceptCall::SELECTOR {
            let accept = offchain::acceptCall::abi_decode_raw(data)
                .map_err(|e| DecodeError::Args(format!("accept: {e}")))?;
            ExternalCall::MarketplaceAccept {
                venue,
                trades: accept
                    ._trades
                    .iter()
                    .map(|t| signed_trade_from_offchain(t, venue))
                    .collect(),
                coupons: Vec::new(),
            }
        } else if selector == offchain::acceptWithCouponCall::SELECTOR {
            let accept = offchain::acceptWithCouponCall::abi_decode_raw(data)
                .map_err(|e| DecodeError::Args(format!("acceptWithCoupon: {e}")))?;
            ExternalCall::MarketplaceAccept {
                venue,
                trades: accept
                    ._trades
                    .iter()
                    .map(|t| signed_trade_from_offchain(t, venue))
                    .collect(),
                coupons: accept
                    ._coupons
                    .iter()
                    .map(coupon_from_offchain)
                    .collect::<Result<_, _>>()?,
            }
        } else {
            opaque()
        }
    } else if target == routes.collection_store {
        if selector == buyCall::SELECTOR {
            let buy = buyCall::abi_decode_raw(data)
                .map_err(|e| DecodeError::Args(format!("buy: {e}")))?;
            ExternalCall::CollectionStoreBuy {
                venue,
                items: store_items(&buy.itemsToBuy)?,
            }
        } else {
            opaque()
        }
    } else {
        opaque()
    };
    Ok(UseCreditsCall {
        credits: args
            .credits
            .iter()
            .map(|c| CreditRef {
                value: ManaWei::new(c.value),
                expires_at: c.expiresAt,
                salt: c.salt.0,
            })
            .collect(),
        call,
        max_uncredited: ManaWei::new(args.maxUncreditedValue),
        max_credited: ManaWei::new(args.maxCreditedValue),
    })
}

/// `CollectionStore.buy`'s entries, one [`StoreItem`] per unit bought; a
/// ragged entry (ids and prices of different lengths) does not decode.
pub fn store_items(entries: &[ItemToBuy]) -> Result<Vec<StoreItem>, DecodeError> {
    let mut items = Vec::new();
    for entry in entries {
        if entry.ids.len() != entry.prices.len() {
            return Err(DecodeError::Args(
                "buy: ids and prices differ in length".into(),
            ));
        }
        for (id, price) in entry.ids.iter().zip(&entry.prices) {
            items.push(StoreItem {
                collection: EvmAddress::new(entry.collection),
                item_id: *id,
                price: ManaWei::new(*price),
            });
        }
    }
    Ok(items)
}

/// `useCredits` calldata for the tests and the fixtures: the inverse of
/// [`decode_use_credits`] over one credit and one external call.
pub fn encode_use_credits(
    target: Address,
    selector: [u8; 4],
    data: Vec<u8>,
    credit_value: U256,
    max_uncredited: U256,
    max_credited: U256,
) -> Vec<u8> {
    let args = credits_abi::UseCreditsArgs {
        credits: vec![credits_abi::Credit {
            value: credit_value,
            expiresAt: U256::from(4_102_444_800u64),
            salt: [0x11u8; 32].into(),
        }],
        creditsSignatures: vec![alloy_primitives::Bytes::from(vec![0u8; 65])],
        externalCall: credits_abi::ExternalCall {
            target,
            selector: selector.into(),
            data: alloy_primitives::Bytes::from(data),
            expiresAt: U256::ZERO,
            salt: [0u8; 32].into(),
        },
        customExternalCallSignature: alloy_primitives::Bytes::new(),
        maxUncreditedValue: max_uncredited,
        maxCreditedValue: max_credited,
    };
    credits_abi::useCreditsCall { _args: args }.abi_encode()
}

/// The `data` of a typed external call: the ABI-encoded arguments without the
/// selector, as the manager appends it (`abi.encodePacked(selector, data)`).
pub fn call_args<C: SolCall>(call: &C) -> Vec<u8> {
    call.abi_encode()[4..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    fn fixture(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/../tests/fixtures/shop/offchain-marketplace-contract@87350c63e/derived/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    #[test]
    fn the_static_and_mana_registries_are_the_land_authz_registries() {
        use crate::land_authz::{
            ESTATE_REGISTRY_MAINNET, ESTATE_REGISTRY_SEPOLIA, LAND_REGISTRY_MAINNET,
            LAND_REGISTRY_SEPOLIA,
        };
        let addr = |s: &str| s.parse::<Address>().unwrap();
        for (chain_id, land, estate) in [
            (
                ChainId::ETHEREUM,
                LAND_REGISTRY_MAINNET,
                ESTATE_REGISTRY_MAINNET,
            ),
            (
                ChainId::SEPOLIA,
                LAND_REGISTRY_SEPOLIA,
                ESTATE_REGISTRY_SEPOLIA,
            ),
        ] {
            assert_eq!(static_role(chain_id, addr(land)), Some(ContractRole::Land));
            assert_eq!(
                static_role(chain_id, addr(estate)),
                Some(ContractRole::Estate)
            );
        }
        let book = crate::mana::venues::Book::production(|_| None);
        let ethereum = book.chain(ChainId::ETHEREUM).unwrap();
        assert_eq!(ethereum.land, Some(addr(LAND_REGISTRY_MAINNET)));
        assert_eq!(ethereum.estate, Some(addr(ESTATE_REGISTRY_MAINNET)));
    }

    #[test]
    fn the_selectors_are_the_compiled_contracts() {
        let selectors = fixture("selectors.json");
        let manager = &selectors["CreditsManagerPolygon"]["functions"];
        let use_credits = manager["useCredits(((uint256,uint256,bytes32)[],bytes[],(address,bytes4,bytes,uint256,bytes32),bytes,uint256,uint256))"].as_str().unwrap();
        assert_eq!(
            use_credits,
            format!("0x{}", hex::encode(USE_CREDITS_SELECTOR))
        );
        assert_eq!(credits_abi::useCreditsCall::SELECTOR, USE_CREDITS_SELECTOR);
        let executor = &selectors["RegisterNameCrossChainExecutor"]["functions"];
        let execute = executor
            .as_object()
            .unwrap()
            .iter()
            .find(|(k, _)| k.starts_with("execute("))
            .map(|(_, v)| v.as_str().unwrap())
            .unwrap();
        assert_eq!(
            execute,
            format!("0x{}", hex::encode(EXECUTOR_EXECUTE_SELECTOR))
        );
    }

    #[test]
    fn the_struct_layout_is_the_compiled_abi() {
        let abi = fixture("abi/CreditsManagerPolygon.json");
        let use_credits = abi["abi"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "useCredits")
            .unwrap();
        let args = &use_credits["inputs"][0]["components"];
        let names: Vec<&str> = args
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "credits",
                "creditsSignatures",
                "externalCall",
                "customExternalCallSignature",
                "maxUncreditedValue",
                "maxCreditedValue"
            ]
        );
        let external: Vec<&str> = args[2]["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            external,
            ["target", "selector", "data", "expiresAt", "salt"]
        );
    }

    fn trade(sent_type: u64, contract: Address, value: u64) -> offchain::Trade {
        offchain::Trade {
            signer: address!("0x00000000000000000000000000000000000000a1"),
            signature: Bytes::new(),
            checks: offchain::Checks {
                uses: U256::from(1),
                expiration: U256::from(4_102_444_800u64),
                effective: U256::ZERO,
                salt: [0u8; 32].into(),
                contractSignatureIndex: U256::ZERO,
                signerSignatureIndex: U256::ZERO,
                allowedRoot: [0u8; 32].into(),
                allowedProof: vec![],
                externalChecks: vec![],
            },
            sent: vec![offchain::Asset {
                assetType: U256::from(sent_type),
                contractAddress: contract,
                value: U256::from(value),
                beneficiary: Address::ZERO,
                extra: Bytes::new(),
            }],
            received: vec![offchain::Asset {
                assetType: U256::from(2),
                contractAddress: address!("0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4"),
                value: U256::from(5_000_000_000_000_000_000u128),
                beneficiary: address!("0x00000000000000000000000000000000000000a1"),
                extra: Bytes::new(),
            }],
        }
    }

    #[test]
    fn use_credits_decodes_as_the_manager_routes_it() {
        let routes = manager_routes(ChainId::POLYGON).unwrap();
        let collection = address!("0xeede64bfaf8055492aa500846ec7c6e6a9f533d5");
        let accept = offchain::acceptCall {
            _trades: vec![trade(4, collection, 7)],
        };
        let data = encode_use_credits(
            routes.marketplaces[0],
            offchain::acceptCall::SELECTOR,
            call_args(&accept),
            U256::from(10),
            U256::ZERO,
            U256::from(10),
        );
        let decoded = decode_use_credits(&data, ChainId::POLYGON).unwrap();
        match &decoded.call {
            ExternalCall::MarketplaceAccept {
                trades,
                coupons,
                venue,
            } => {
                assert_eq!(venue.contract.address(), routes.marketplaces[0]);
                assert!(coupons.is_empty());
                assert_eq!(trades[0].sent[0].asset_type, TradeAssetType::CollectionItem);
                assert_eq!(trades[0].sent[0].value, U256::from(7));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(decoded.credits[0].value, ManaWei::new(U256::from(10)));

        let buy = buyCall {
            itemsToBuy: vec![ItemToBuy {
                collection,
                ids: vec![U256::from(4), U256::from(4)],
                prices: vec![U256::from(1), U256::from(1)],
                beneficiaries: vec![Address::ZERO; 2],
            }],
        };
        let data = encode_use_credits(
            routes.collection_store,
            buyCall::SELECTOR,
            call_args(&buy),
            U256::from(2),
            U256::ZERO,
            U256::from(2),
        );
        match decode_use_credits(&data, ChainId::POLYGON).unwrap().call {
            ExternalCall::CollectionStoreBuy { items, .. } => assert_eq!(items.len(), 2),
            other => panic!("{other:?}"),
        }

        let order = executeOrderCall {
            nftAddress: collection,
            assetId: U256::from(9),
            price: U256::from(3),
        };
        let data = encode_use_credits(
            routes.legacy_marketplace,
            executeOrderCall::SELECTOR,
            call_args(&order),
            U256::from(3),
            U256::ZERO,
            U256::from(3),
        );
        assert!(
            matches!(decode_use_credits(&data, ChainId::POLYGON).unwrap().call, ExternalCall::LegacyExecuteOrder { token_id, .. } if token_id == U256::from(9))
        );

        let executor = passthrough_executors(ChainId::POLYGON)[0];
        let data = encode_use_credits(
            executor,
            EXECUTOR_EXECUTE_SELECTOR,
            vec![0xde, 0xad],
            U256::from(3),
            U256::ZERO,
            U256::from(3),
        );
        assert!(
            matches!(decode_use_credits(&data, ChainId::POLYGON).unwrap().call, ExternalCall::Custom { effect: CustomEffect::Opaque, selector, .. } if selector == EXECUTOR_EXECUTE_SELECTOR)
        );

        let data = encode_use_credits(
            routes.collection_store,
            executeOrderCall::SELECTOR,
            call_args(&order),
            U256::from(3),
            U256::ZERO,
            U256::from(3),
        );
        assert!(
            matches!(
                decode_use_credits(&data, ChainId::POLYGON).unwrap().call,
                ExternalCall::Custom { .. }
            ),
            "a wrong selector on a typed target is not a typed route"
        );
        assert!(matches!(
            decode_use_credits(&data, ChainId::ETHEREUM),
            Err(DecodeError::UnknownChain(_))
        ));
        assert!(matches!(
            decode_use_credits(&data[..40], ChainId::POLYGON),
            Err(DecodeError::Args(_))
        ));
    }

    #[test]
    fn a_flat_coupon_does_not_decode_and_a_rate_coupon_does() {
        let routes = manager_routes(ChainId::POLYGON).unwrap();
        let collection = address!("0xeede64bfaf8055492aa500846ec7c6e6a9f533d5");
        let coupon = |kind: u64| offchain::Coupon {
            signature: Bytes::new(),
            checks: trade(4, collection, 1).checks,
            couponAddress: address!("0xc914507fe297b2dddd1232ac3a8903f1c125e794"),
            data: Bytes::from(
                (
                    U256::from(kind),
                    U256::from(250_000u64),
                    alloy_primitives::B256::ZERO,
                )
                    .abi_encode(),
            ),
            callerData: Bytes::new(),
        };
        let with = |kind: u64| {
            let call = offchain::acceptWithCouponCall {
                _trades: vec![trade(4, collection, 1)],
                _coupons: vec![coupon(kind)],
            };
            encode_use_credits(
                routes.marketplaces[2],
                offchain::acceptWithCouponCall::SELECTOR,
                call_args(&call),
                U256::from(1),
                U256::ZERO,
                U256::from(1),
            )
        };
        match decode_use_credits(&with(1), ChainId::POLYGON).unwrap().call {
            ExternalCall::MarketplaceAccept { coupons, .. } => {
                assert_eq!(coupons[0].discount_ppm, 250_000)
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decode_use_credits(&with(2), ChainId::POLYGON).unwrap_err(),
            DecodeError::Coupon { discount_type: 2 }
        );
    }

    #[test]
    fn a_stored_trade_maps_to_the_signed_trade() {
        let json = serde_json::json!({
            "id": "t", "signer": "0x00000000000000000000000000000000000000A1", "signature": "0x00", "type": "public_nft_order",
            "checks": { "uses": 1, "expiration": 4102444800000i64, "effective": 0, "salt": "0x", "contractSignatureIndex": 0, "signerSignatureIndex": 0 },
            "sent": [{ "assetType": 3, "contractAddress": "0xeede64bfaf8055492aa500846ec7c6e6a9f533d5", "tokenId": "42" }],
            "received": [{ "assetType": 1, "contractAddress": "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4", "amount": "1000000000000000000" }],
        });
        let trade: TradeIn = serde_json::from_value(json).unwrap();
        let venue = ContractRef {
            chain_id: ChainId::POLYGON,
            contract: EvmAddress::new(manager_routes(ChainId::POLYGON).unwrap().marketplaces[0]),
        };
        let signed = signed_trade_from_trade_in(&trade, venue).unwrap();
        assert_eq!(signed.sent[0].asset_type, TradeAssetType::Erc721);
        assert_eq!(signed.sent[0].value, U256::from(42));
        assert!(signed.received[0].asset_type.is_mana());
        assert_eq!(
            signed.signer.to_string(),
            "0x00000000000000000000000000000000000000a1"
        );
    }

    #[test]
    fn the_static_registry_and_the_manager_lists() {
        assert_eq!(
            static_role(
                ChainId::ETHEREUM,
                address!("0x2a187453064356c898cae034eaed119e1663acb8")
            ),
            Some(ContractRole::NameRegistrar)
        );
        assert_eq!(
            static_role(
                ChainId::ETHEREUM,
                address!("0xf87e31492faf9a91b02ee0deaad50d51d56d5d4d")
            ),
            Some(ContractRole::Land)
        );
        assert_eq!(
            static_role(
                ChainId::POLYGON,
                address!("0xf87e31492faf9a91b02ee0deaad50d51d56d5d4d")
            ),
            None
        );
        assert!(is_credits_manager(
            ChainId::POLYGON,
            SHARED_UPSTREAM_MANAGERS[0]
        ));
        assert!(!is_credits_manager(
            ChainId::POLYGON,
            SHARED_UPSTREAM_MANAGERS[2]
        ));
        assert_eq!(passthrough_executors(ChainId::AMOY).len(), 0);
        assert_eq!(mirror_network(ChainId::POLYGON), Some("matic"));
        assert_eq!(
            name_token_from_owner_call(
                ContractRef {
                    chain_id: ChainId::ETHEREUM,
                    contract: EvmAddress::new(Address::ZERO)
                },
                U256::ZERO,
                Ok(None)
            ),
            Lookup::Absent
        );
    }

    #[test]
    fn dcl_ones_amoy_manager_is_a_credits_manager_on_amoy_only_and_never_a_shared_one() {
        assert!(is_credits_manager(ChainId::AMOY, DCL_ONE_AMOY_MANAGER));
        assert!(!is_credits_manager(ChainId::POLYGON, DCL_ONE_AMOY_MANAGER));
        assert!(!is_credits_manager(ChainId::ETHEREUM, DCL_ONE_AMOY_MANAGER));
        assert!(
            !SHARED_UPSTREAM_MANAGERS.contains(&DCL_ONE_AMOY_MANAGER),
            "credits are issued for it: it must never read as a shared upstream manager"
        );
        assert!(
            !crate::credits::settlement::SHARED_MANAGERS
                .contains(&format!("{DCL_ONE_AMOY_MANAGER:#x}").as_str())
        );
        // The shared Amoy pair stays a manager beside it.
        assert!(is_credits_manager(
            ChainId::AMOY,
            SHARED_UPSTREAM_MANAGERS[2]
        ));
        assert!(is_credits_manager(
            ChainId::AMOY,
            SHARED_UPSTREAM_MANAGERS[3]
        ));
    }

    #[test]
    fn dcl_ones_polygon_manager_is_a_credits_manager_on_polygon_only_and_never_a_shared_one() {
        assert!(is_credits_manager(
            ChainId::POLYGON,
            DCL_ONE_POLYGON_MANAGER
        ));
        assert!(!is_credits_manager(ChainId::AMOY, DCL_ONE_POLYGON_MANAGER));
        assert!(!is_credits_manager(
            ChainId::ETHEREUM,
            DCL_ONE_POLYGON_MANAGER
        ));
        assert!(
            !SHARED_UPSTREAM_MANAGERS.contains(&DCL_ONE_POLYGON_MANAGER),
            "credits are issued for it: it must never read as a shared upstream manager"
        );
        assert!(
            !crate::credits::settlement::SHARED_MANAGERS
                .contains(&format!("{DCL_ONE_POLYGON_MANAGER:#x}").as_str())
        );
        // The shared Polygon pair stays a manager beside it, and still a shared one.
        for shared in &SHARED_UPSTREAM_MANAGERS[..2] {
            assert!(is_credits_manager(ChainId::POLYGON, *shared));
            assert!(
                crate::credits::settlement::SHARED_MANAGERS
                    .contains(&format!("{shared:#x}").as_str())
            );
        }
    }

    /// The routes the guard decodes a Polygon manager's `useCredits` on: the legacy
    /// marketplace and the CollectionStore dcl-one's Polygon manager was constructed
    /// with, and the three off-chain marketplaces the shared manager allows. This pins
    /// the decoding table only; it does not read the deployed contract, whose own
    /// `marketplaces(address)` decides which of them it executes.
    #[test]
    fn dcl_ones_polygon_manager_routes_are_the_polygon_routes() {
        let routes = manager_routes(ChainId::POLYGON).expect("Polygon routes");
        assert_eq!(
            routes.legacy_marketplace,
            address!("0x480a0f4e360e8964e68858dd231c2922f1df45ef")
        );
        assert_eq!(
            routes.collection_store,
            address!("0x214ffc0f0103735728dc66b61a22e4f163e275ae")
        );
        assert_eq!(
            routes.marketplaces,
            &[
                address!("0x540fb08edb56aae562864b390542c97f562825ba"),
                address!("0xa40b1d129b8906888720686f3a01921ddf37716f"),
                address!("0xe38ef22abe871513555cba89adfe45ab4f548ada"),
            ]
        );
        assert!(manager_routes(ChainId::ETHEREUM).is_none());
    }
}
