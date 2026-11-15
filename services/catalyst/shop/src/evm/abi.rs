// from: catalyrst/crates/catalyrst-economy/src/ports/abi.rs, transactions-server src/ports/transaction/validation/checkSalePrice.ts
//! The calldata shapes the relay decodes (both `executeMetaTransaction`
//! overloads, the sale calls behind the minimum-price rule) and the broker
//! encodes (store buy, marketplace order, escrow forward, NAME
//! register/transfer, MANA approve).

use std::collections::HashSet;
use std::sync::LazyLock;

use alloy_primitives::{Address, U256, address, keccak256};
use alloy_sol_types::{SolCall, sol};

use super::meta_tx::{OFF_CHAIN_MARKETPLACE_DOMAIN_NAME, audited_domains_for};

sol! {
    function executeMetaTransaction(
        address userAddress,
        bytes functionSignature,
        bytes32 sigR,
        bytes32 sigS,
        uint8 sigV
    ) external returns (bytes);

    struct ItemToBuy {
        address collection;
        uint256[] ids;
        uint256[] prices;
        address[] beneficiaries;
    }
    function buy(ItemToBuy[] itemsToBuy) external;

    function executeOrder(address nftAddress, uint256 assetId, uint256 price) external;

    function placeBid(
        address _tokenAddress,
        uint256 _tokenId,
        uint256 _price,
        uint256 _duration
    ) external;

    function reclaim(address collection, uint256 tokenId) external;
    function release(address collection, uint256 tokenId, address buyer) external;

    function safeTransferFrom(address from, address to, uint256 tokenId, bytes data) external;

    function register(string _name, address _beneficiary) external;

    function approve(address spender, uint256 amount) external returns (bool);

    function getNonce(address user) external view returns (uint256 nonce);

    function balanceOf(address owner) external view returns (uint256 balance);

    function transfer(address to, uint256 amount) external returns (bool);
}

pub mod combined_sig {
    alloy_sol_types::sol! {
        function executeMetaTransaction(
            address userAddress,
            bytes functionData,
            bytes signature
        ) external returns (bytes);
    }
}

pub const ERC721_TRANSFER_TOPIC0: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];

pub const EXEC_META_TX_SELECTORS: [[u8; 4]; 2] =
    [[0x0c, 0x53, 0xc5, 0x1c], [0xd8, 0xed, 0x1a, 0xcc]];

pub fn is_execute_meta_tx(full_data: &[u8]) -> bool {
    full_data.len() >= 4
        && EXEC_META_TX_SELECTORS.contains(&[
            full_data[0],
            full_data[1],
            full_data[2],
            full_data[3],
        ])
}

pub fn decode_meta_tx(full_data: &[u8]) -> Option<Vec<u8>> {
    executeMetaTransactionCall::abi_decode(full_data)
        .ok()
        .map(|c| c.functionSignature.to_vec())
}

pub fn decode_meta_tx_user_address(full_data: &[u8]) -> Option<Address> {
    if let Ok(call) = executeMetaTransactionCall::abi_decode(full_data) {
        return Some(call.userAddress);
    }
    combined_sig::executeMetaTransactionCall::abi_decode(full_data)
        .ok()
        .map(|call| call.userAddress)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedMetaTx {
    pub user_address: Address,
    pub function_signature: Vec<u8>,
    pub signature: Vec<u8>,
}

pub fn decode_meta_tx_full(full_data: &[u8]) -> Option<DecodedMetaTx> {
    if let Ok(call) = executeMetaTransactionCall::abi_decode(full_data) {
        let mut signature = Vec::with_capacity(65);
        signature.extend_from_slice(call.sigR.as_slice());
        signature.extend_from_slice(call.sigS.as_slice());
        signature.push(call.sigV);
        return Some(DecodedMetaTx {
            user_address: call.userAddress,
            function_signature: call.functionSignature.to_vec(),
            signature,
        });
    }
    combined_sig::executeMetaTransactionCall::abi_decode(full_data)
        .ok()
        .map(|call| DecodedMetaTx {
            user_address: call.userAddress,
            function_signature: call.functionData.to_vec(),
            signature: call.signature.to_vec(),
        })
}

/// The inner call's selector when it is itself an `executeMetaTransaction`
/// (either overload). The contract would run it: it dispatches on the
/// selector, checks the inner signature and calls itself again, so every
/// check that reads one level would price and admit the outer wrapper while
/// the inner signer is who acts. `None` for an ordinary inner call and for
/// calldata that does not decode (refused elsewhere).
pub fn nested_meta_tx_selector(full_data: &[u8]) -> Option<[u8; 4]> {
    let inner = decode_meta_tx_full(full_data)?.function_signature;
    let selector: [u8; 4] = inner.get(..4)?.try_into().ok()?;
    EXEC_META_TX_SELECTORS
        .contains(&selector)
        .then_some(selector)
}

pub mod offchain {
    alloy_sol_types::sol! {
        struct ExternalCheck {
            address contractAddress;
            bytes4 selector;
            bytes value;
            bool required;
        }

        struct Checks {
            uint256 uses;
            uint256 expiration;
            uint256 effective;
            bytes32 salt;
            uint256 contractSignatureIndex;
            uint256 signerSignatureIndex;
            bytes32 allowedRoot;
            bytes32[] allowedProof;
            ExternalCheck[] externalChecks;
        }

        struct Asset {
            uint256 assetType;
            address contractAddress;
            uint256 value;
            address beneficiary;
            bytes extra;
        }

        struct Trade {
            address signer;
            bytes signature;
            Checks checks;
            Asset[] sent;
            Asset[] received;
        }

        struct Coupon {
            bytes signature;
            Checks checks;
            address couponAddress;
            bytes data;
            bytes callerData;
        }

        function accept(Trade[] _trades) external;
        function cancelSignature(Trade[] _trades) external;
        function acceptWithCoupon(Trade[] _trades, Coupon[] _coupons) external;
    }
}

/// The Estate spellings of the order-book sales, each carrying a fingerprint.
pub mod estate {
    alloy_sol_types::sol! {
        function safeExecuteOrder(address nftAddress, uint256 assetId, uint256 price, bytes fingerprint) external;
        function placeBid(address _tokenAddress, uint256 _tokenId, uint256 _price, uint256 _duration, bytes _fingerprint) external;
    }
}

/// A relay target whose sales the minimum-price rule prices. Every one is
/// closed on its deployed surface: a selector outside it is priced at zero
/// (refused), since a method nobody reviewed cannot be shown not to be a sale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaleKind {
    CollectionStore,
    /// The legacy order book, same sale ABI as MarketplaceV2, settling in MANA.
    Marketplace,
    MarketplaceV2,
    BidV2,
    /// Every off-chain marketplace version (`accept`/`acceptWithCoupon` are
    /// identical across them).
    OffChainMarketplace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaleMethod {
    Buy,
    ExecuteOrder,
    SafeExecuteOrder,
    PlaceBid,
    PlaceBidWithFingerprint,
    Accept,
    AcceptWithCoupon,
}

const COLLECTION_STORE_SURFACE: &[&str] = &[
    "acceptedToken()",
    "BASE_FEE()",
    "buy((address,uint256[],uint256[],address[])[])",
    "domainSeparator()",
    "executeMetaTransaction(address,bytes,bytes32,bytes32,uint8)",
    "fee()",
    "feeOwner()",
    "getChainId()",
    "getItemBuyData(address,uint256)",
    "getNonce(address)",
    "owner()",
    "renounceOwnership()",
    "setFeeOwner(address)",
    "setFee(uint256)",
    "transferOwnership(address)",
];

const MARKETPLACE_SURFACE: &[&str] = &[
    "acceptedToken()",
    "cancelOrder(address,uint256)",
    "createOrder(address,uint256,uint256,uint256)",
    "domainSeparator()",
    "ERC721_Interface()",
    "executeMetaTransaction(address,bytes,bytes32,bytes32,uint8)",
    "executeOrder(address,uint256,uint256)",
    "getChainId()",
    "getNonce(address)",
    "InterfaceId_ValidateFingerprint()",
    "orderByAssetId(address,uint256)",
    "owner()",
    "ownerCutPerMillion()",
    "paused()",
    "publicationFeeInWei()",
    "renounceOwnership()",
    "safeExecuteOrder(address,uint256,uint256,bytes)",
    "setOwnerCutPerMillion(uint256)",
    "setPublicationFee(uint256)",
    "transferOwnership(address)",
];

const MARKETPLACE_V2_SURFACE: &[&str] = &[
    "acceptedToken()",
    "cancelOrder(address,uint256)",
    "createOrder(address,uint256,uint256,uint256)",
    "domainSeparator()",
    "ERC721_Interface()",
    "executeMetaTransaction(address,bytes,bytes32,bytes32,uint8)",
    "executeOrder(address,uint256,uint256)",
    "feesCollector()",
    "feesCollectorCutPerMillion()",
    "getChainId()",
    "getNonce(address)",
    "InterfaceId_ValidateFingerprint()",
    "orderByAssetId(address,uint256)",
    "owner()",
    "paused()",
    "publicationFeeInWei()",
    "renounceOwnership()",
    "royaltiesCutPerMillion()",
    "royaltiesManager()",
    "safeExecuteOrder(address,uint256,uint256,bytes)",
    "setFeesCollector(address)",
    "setFeesCollectorCutPerMillion(uint256)",
    "setPublicationFee(uint256)",
    "setRoyaltiesCutPerMillion(uint256)",
    "setRoyaltiesManager(address)",
    "transferOwnership(address)",
];

/// The bundled `Bid` ABI the BidV2 deployment is registered with.
const BID_SURFACE: &[&str] = &[
    "addPauser(address)",
    "bidCounterByToken(address,uint256)",
    "bidIdByTokenAndBidder(address,uint256,address)",
    "bidIndexByBidId(bytes32)",
    "cancelBid(address,uint256)",
    "ERC721Composable_ValidateFingerprint()",
    "ERC721_Interface()",
    "ERC721_Received()",
    "getBidByBidder(address,uint256,address)",
    "getBidByToken(address,uint256,uint256)",
    "isOwner()",
    "isPauser(address)",
    "manaToken()",
    "MAX_BID_DURATION()",
    "MIN_BID_DURATION()",
    "onERC721Received(address,address,uint256,bytes)",
    "ONE_MILLION()",
    "owner()",
    "ownerCutPerMillion()",
    "pause()",
    "paused()",
    "placeBid(address,uint256,uint256,uint256)",
    "placeBid(address,uint256,uint256,uint256,bytes)",
    "removeExpiredBids(address[],uint256[],address[])",
    "renounceOwnership()",
    "renouncePauser()",
    "setOwnerCutPerMillion(uint256)",
    "transferOwnership(address)",
    "unpause()",
];

/// Selectors the deployed BidV2 answers that its bundled ABI omits, read from
/// the deployment and reviewed one by one: views, owner-only setters,
/// `getChainId` and the meta-transaction entry point. None moves value, so
/// closing the target on them refuses no working call.
const BID_V2_NOT_IN_ABI: &[&str] = &[
    "executeMetaTransaction(address,bytes,bytes32,bytes32,uint8)",
    "getNonce(address)",
    "getChainId()",
    "setFeesCollector(address)",
    "feesCollectorCutPerMillion()",
    "setRoyaltiesManager(address)",
    "setFeesCollectorCutPerMillion(uint256)",
    "setRoyaltiesCutPerMillion(uint256)",
    "royaltiesManager()",
    "feesCollector()",
    "royaltiesCutPerMillion()",
    "domainSeparator()",
];

const OFF_CHAIN_MARKETPLACE_SURFACE: &[&str] = &[
    "accept((address,bytes,(uint256,uint256,uint256,bytes32,uint256,uint256,bytes32,bytes32[],(address,bytes4,bytes,bool)[]),(uint256,address,uint256,address,bytes)[],(uint256,address,uint256,address,bytes)[])[])",
    "acceptWithCoupon((address,bytes,(uint256,uint256,uint256,bytes32,uint256,uint256,bytes32,bytes32[],(address,bytes4,bytes,bool)[]),(uint256,address,uint256,address,bytes)[],(uint256,address,uint256,address,bytes)[])[],(bytes,(uint256,uint256,uint256,bytes32,uint256,uint256,bytes32,bytes32[],(address,bytes4,bytes,bool)[]),address,bytes,bytes)[])",
    "ASSET_TYPE_COLLECTION_ITEM()",
    "ASSET_TYPE_ERC20()",
    "ASSET_TYPE_ERC721()",
    "ASSET_TYPE_USD_PEGGED_MANA()",
    "cancelledSignatures(bytes32)",
    "cancelSignature((address,bytes,(uint256,uint256,uint256,bytes32,uint256,uint256,bytes32,bytes32[],(address,bytes4,bytes,bool)[]),(uint256,address,uint256,address,bytes)[],(uint256,address,uint256,address,bytes)[])[])",
    "contractSignatureIndex()",
    "couponManager()",
    "executeMetaTransaction(address,bytes,bytes)",
    "feeCollector()",
    "feeRate()",
    "getNonce(address)",
    "getTradeId((address,bytes,(uint256,uint256,uint256,bytes32,uint256,uint256,bytes32,bytes32[],(address,bytes4,bytes,bool)[]),(uint256,address,uint256,address,bytes)[],(uint256,address,uint256,address,bytes)[]),address)",
    "increaseContractSignatureIndex()",
    "increaseSignerSignatureIndex()",
    "manaAddress()",
    "manaUsdAggregator()",
    "manaUsdAggregatorTolerance()",
    "owner()",
    "pause()",
    "paused()",
    "renounceOwnership()",
    "royaltiesManager()",
    "royaltiesRate()",
    "signatureUses(bytes32)",
    "signerSignatureIndex(address)",
    "transferOwnership(address)",
    "unpause()",
    "updateCouponManager(address)",
    "updateFeeCollector(address)",
    "updateFeeRate(uint256)",
    "updateManaUsdAggregator(address,uint256)",
    "updateRoyaltiesManager(address)",
    "updateRoyaltiesRate(uint256)",
    "usedTradeIds(bytes32)",
];

pub fn function_selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

fn selector_set(lists: &[&[&str]]) -> HashSet<[u8; 4]> {
    lists
        .iter()
        .flat_map(|list| list.iter())
        .map(|sig| function_selector(sig))
        .collect()
}

impl SaleKind {
    fn sale_method(self, selector: [u8; 4]) -> Option<SaleMethod> {
        let method = match self {
            SaleKind::CollectionStore if selector == buyCall::SELECTOR => SaleMethod::Buy,
            SaleKind::Marketplace | SaleKind::MarketplaceV2
                if selector == executeOrderCall::SELECTOR =>
            {
                SaleMethod::ExecuteOrder
            }
            SaleKind::Marketplace | SaleKind::MarketplaceV2
                if selector == estate::safeExecuteOrderCall::SELECTOR =>
            {
                SaleMethod::SafeExecuteOrder
            }
            SaleKind::BidV2 if selector == placeBidCall::SELECTOR => SaleMethod::PlaceBid,
            SaleKind::BidV2 if selector == estate::placeBidCall::SELECTOR => {
                SaleMethod::PlaceBidWithFingerprint
            }
            SaleKind::OffChainMarketplace if selector == offchain::acceptCall::SELECTOR => {
                SaleMethod::Accept
            }
            SaleKind::OffChainMarketplace
                if selector == offchain::acceptWithCouponCall::SELECTOR =>
            {
                SaleMethod::AcceptWithCoupon
            }
            _ => return None,
        };
        Some(method)
    }

    /// Every selector the deployed target answers, sales included.
    pub fn known_selectors(self) -> &'static HashSet<[u8; 4]> {
        static STORE: LazyLock<HashSet<[u8; 4]>> =
            LazyLock::new(|| selector_set(&[COLLECTION_STORE_SURFACE]));
        static MARKETPLACE: LazyLock<HashSet<[u8; 4]>> =
            LazyLock::new(|| selector_set(&[MARKETPLACE_SURFACE]));
        static MARKETPLACE_V2: LazyLock<HashSet<[u8; 4]>> =
            LazyLock::new(|| selector_set(&[MARKETPLACE_V2_SURFACE]));
        static BID_V2: LazyLock<HashSet<[u8; 4]>> =
            LazyLock::new(|| selector_set(&[BID_SURFACE, BID_V2_NOT_IN_ABI]));
        static OFF_CHAIN: LazyLock<HashSet<[u8; 4]>> =
            LazyLock::new(|| selector_set(&[OFF_CHAIN_MARKETPLACE_SURFACE]));
        match self {
            SaleKind::CollectionStore => &STORE,
            SaleKind::Marketplace => &MARKETPLACE,
            SaleKind::MarketplaceV2 => &MARKETPLACE_V2,
            SaleKind::BidV2 => &BID_V2,
            SaleKind::OffChainMarketplace => &OFF_CHAIN,
        }
    }
}

/// The registered sale target at `contract` on `chain_id`: the address-book
/// sale contracts, plus every audited domain constructed as the off-chain
/// marketplace, so a version cannot be made relayable without its trades being
/// priced.
pub fn sale_target(chain_id: u64, contract: Address) -> Option<SaleKind> {
    let registry: &[(Address, SaleKind)] = match chain_id {
        137 => &[
            (
                address!("0x214ffc0f0103735728dc66b61a22e4f163e275ae"),
                SaleKind::CollectionStore,
            ),
            (
                address!("0x02080031b45a3c67d338dd4a2cc309d28756a160"),
                SaleKind::Marketplace,
            ),
            (
                address!("0x480a0f4e360e8964e68858dd231c2922f1df45ef"),
                SaleKind::MarketplaceV2,
            ),
            (
                address!("0xb96697fa4a3361ba35b774a42c58daccaad1b8e1"),
                SaleKind::BidV2,
            ),
            (
                address!("0x540fb08edb56aae562864b390542c97f562825ba"),
                SaleKind::OffChainMarketplace,
            ),
        ],
        80002 => &[
            (
                address!("0xe36abc9ec616c83caaa386541380829106149d68"),
                SaleKind::CollectionStore,
            ),
            (
                address!("0x0c8ad1f6aadf89d2eb19f01a100a6143108fe2b0"),
                SaleKind::MarketplaceV2,
            ),
            (
                address!("0x4b66eab79cc03a96fb6275cfcdf23c0db431606d"),
                SaleKind::BidV2,
            ),
            (
                address!("0x6ab20ae56673ed65f520b7be332aeb61b3ed727d"),
                SaleKind::OffChainMarketplace,
            ),
        ],
        _ => &[],
    };
    if let Some((_, kind)) = registry.iter().find(|(addr, _)| *addr == contract) {
        return Some(*kind);
    }
    audited_domains_for(chain_id)
        .into_iter()
        .any(|(addr, domain)| addr == contract && domain.name == OFF_CHAIN_MARKETPLACE_DOMAIN_NAME)
        .then_some(SaleKind::OffChainMarketplace)
}

/// The coupon implementations whose settlement can be priced, per chain. The
/// CouponManager dispatches to `ICoupon(couponAddress).applyCoupon` for any
/// implementation its owner allow-lists, so the address decides what `data`
/// means; one not recorded here is priced at zero rather than read as
/// CollectionDiscountCoupon's format.
pub fn modelled_coupons(chain_id: u64) -> &'static [Address] {
    const MAINNET: [Address; 1] = [address!("0xc914507fe297b2dddd1232ac3a8903f1c125e794")];
    const AMOY: [Address; 2] = [
        address!("0x4ee8f6b87f4917a3bbc7c8bb3a06db8555f83db9"),
        address!("0x54df9990a3fd9443b9e84f9f5944904704756074"),
    ];
    match chain_id {
        137 => &MAINNET,
        80002 => &AMOY,
        _ => &[],
    }
}

const DISCOUNT_TYPE_RATE: u64 = crate::coupons::signature::DISCOUNT_TYPE_RATE as u64;
const DISCOUNT_TYPE_FLAT: u64 = 2;
const ONE_MILLION: u64 = 1_000_000;

/// What a CollectionDiscountCoupon leaves of one received amount, mirroring
/// its `applyCoupon` arithmetic: `data` is `abi.encode(discountType, discount,
/// root)`; the rate truncates the product before subtracting, the flat kind
/// subtracts a fixed amount. `None` for anything unmodelled (unknown type, a
/// payload that does not decode, a flat discount above the amount, an
/// overflow), all of which revert on chain.
pub fn discounted_amount(data: &[u8], value: U256) -> Option<U256> {
    if data.len() < 96 {
        return None;
    }
    let discount_type = U256::from_be_slice(&data[0..32]);
    let discount = U256::from_be_slice(&data[32..64]);
    if discount_type == U256::from(DISCOUNT_TYPE_RATE) {
        let cut = value.checked_mul(discount)? / U256::from(ONE_MILLION);
        return value.checked_sub(cut);
    }
    if discount_type == U256::from(DISCOUNT_TYPE_FLAT) {
        return value.checked_sub(discount);
    }
    None
}

/// A batch is priced at its minimum, entry by entry: an entry carrying no
/// amount is unpriced rather than free, so it fails the whole call closed, as
/// does a call with no entries.
fn min_entry_amount(entries: impl IntoIterator<Item = Vec<U256>>) -> U256 {
    let mut min: Option<U256> = None;
    for values in entries {
        if values.is_empty() {
            return U256::ZERO;
        }
        for value in values {
            min = Some(min.map_or(value, |m| m.min(value)));
        }
    }
    min.unwrap_or(U256::ZERO)
}

fn min_received_value(trades: &[offchain::Trade]) -> U256 {
    min_entry_amount(
        trades
            .iter()
            .map(|t| t.received.iter().map(|a| a.value).collect()),
    )
}

/// `acceptWithCoupon` settles trade i after coupon i discounts it, so the
/// checked amount is the discounted one. Fewer coupons than trades reverts on
/// chain (priced at zero); extra coupons are never read.
fn min_discounted_received_value(
    trades: &[offchain::Trade],
    coupons: &[offchain::Coupon],
    chain_id: u64,
) -> U256 {
    if coupons.len() < trades.len() {
        return U256::ZERO;
    }
    let modelled = modelled_coupons(chain_id);
    let mut entries = Vec::with_capacity(trades.len());
    for (trade, coupon) in trades.iter().zip(coupons) {
        if !modelled.contains(&coupon.couponAddress) {
            return U256::ZERO;
        }
        let mut discounted = Vec::with_capacity(trade.received.len());
        for asset in &trade.received {
            let Some(amount) = discounted_amount(&coupon.data, asset.value) else {
                return U256::ZERO;
            };
            discounted.push(amount);
        }
        entries.push(discounted);
    }
    min_entry_amount(entries)
}

impl SaleMethod {
    fn price(self, inner: &[u8], chain_id: u64) -> Option<U256> {
        Some(match self {
            SaleMethod::Buy => min_entry_amount(
                buyCall::abi_decode(inner)
                    .ok()?
                    .itemsToBuy
                    .into_iter()
                    .map(|item| item.prices),
            ),
            SaleMethod::ExecuteOrder => executeOrderCall::abi_decode(inner).ok()?.price,
            SaleMethod::SafeExecuteOrder => {
                estate::safeExecuteOrderCall::abi_decode(inner).ok()?.price
            }
            SaleMethod::PlaceBid => placeBidCall::abi_decode(inner).ok()?._price,
            SaleMethod::PlaceBidWithFingerprint => {
                estate::placeBidCall::abi_decode(inner).ok()?._price
            }
            SaleMethod::Accept => {
                min_received_value(&offchain::acceptCall::abi_decode(inner).ok()?._trades)
            }
            SaleMethod::AcceptWithCoupon => {
                let call = offchain::acceptWithCouponCall::abi_decode(inner).ok()?;
                min_discounted_received_value(&call._trades, &call._coupons, chain_id)
            }
        })
    }
}

/// The price a meta-transaction aimed at a registered sale target settles at:
/// `None` for one of the target's reviewed non-sale methods (nothing to
/// price); the decoded price for a sale; zero (fail closed) when the wrapper
/// or a sale's payload does not decode, or the selector is outside the
/// target's known surface.
pub fn get_sale_price(full_data: &[u8], chain_id: u64, kind: SaleKind) -> Option<U256> {
    let Some(inner) = decode_meta_tx_full(full_data).map(|d| d.function_signature) else {
        return Some(U256::ZERO);
    };
    let Some(selector) = inner.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return Some(U256::ZERO);
    };
    match kind.sale_method(selector) {
        Some(method) => Some(method.price(&inner, chain_id).unwrap_or(U256::ZERO)),
        None if kind.known_selectors().contains(&selector) => None,
        None => Some(U256::ZERO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_both_execute_meta_tx_selectors() {
        assert!(is_execute_meta_tx(&[0x0c, 0x53, 0xc5, 0x1c, 0x00]));
        assert!(is_execute_meta_tx(&[0xd8, 0xed, 0x1a, 0xcc]));
    }

    #[test]
    fn rejects_other_selectors() {
        assert!(!is_execute_meta_tx(&[0xa9, 0x05, 0x9c, 0xbb]));
        assert!(!is_execute_meta_tx(&[0x0c, 0x53, 0xc5]));
        assert!(!is_execute_meta_tx(&[]));
    }

    #[test]
    fn accepted_selectors_are_the_two_execute_meta_tx_overloads() {
        assert_eq!(
            <executeMetaTransactionCall as SolCall>::SELECTOR,
            EXEC_META_TX_SELECTORS[0]
        );
        assert_eq!(
            <combined_sig::executeMetaTransactionCall as SolCall>::SELECTOR,
            EXEC_META_TX_SELECTORS[1]
        );
    }

    #[test]
    fn recovers_the_signed_user_address_from_both_overloads() {
        let user = alloy_primitives::address!("0x2222222222222222222222222222222222222222");
        let split = executeMetaTransactionCall {
            userAddress: user,
            functionSignature: alloy_primitives::Bytes::from_static(&[0xaa]),
            sigR: alloy_primitives::FixedBytes::<32>::repeat_byte(0x11),
            sigS: alloy_primitives::FixedBytes::<32>::repeat_byte(0x22),
            sigV: 27,
        }
        .abi_encode();
        assert!(is_execute_meta_tx(&split));
        assert_eq!(decode_meta_tx_user_address(&split), Some(user));

        let combined = combined_sig::executeMetaTransactionCall {
            userAddress: user,
            functionData: alloy_primitives::Bytes::from_static(&[0xaa]),
            signature: alloy_primitives::Bytes::from_static(&[0x33; 65]),
        }
        .abi_encode();
        assert!(is_execute_meta_tx(&combined));
        assert_eq!(decode_meta_tx_user_address(&combined), Some(user));
    }

    #[test]
    fn refuses_to_invent_a_user_address_for_undecodable_calldata() {
        assert_eq!(
            decode_meta_tx_user_address(&[0x0c, 0x53, 0xc5, 0x1c, 0xff, 0xff]),
            None
        );
        assert_eq!(decode_meta_tx_user_address(&[]), None);
    }

    fn wrap_split(inner: Vec<u8>) -> Vec<u8> {
        executeMetaTransactionCall {
            userAddress: Address::repeat_byte(0x22),
            functionSignature: inner.into(),
            sigR: Default::default(),
            sigS: Default::default(),
            sigV: 27,
        }
        .abi_encode()
    }

    fn wrap_combined(inner: Vec<u8>) -> Vec<u8> {
        combined_sig::executeMetaTransactionCall {
            userAddress: Address::repeat_byte(0x22),
            functionData: inner.into(),
            signature: alloy_primitives::Bytes::from(vec![0u8; 65]),
        }
        .abi_encode()
    }

    fn checks() -> offchain::Checks {
        offchain::Checks {
            uses: U256::from(1u64),
            expiration: U256::from(u64::MAX),
            effective: U256::ZERO,
            salt: Default::default(),
            contractSignatureIndex: U256::ZERO,
            signerSignatureIndex: U256::ZERO,
            allowedRoot: Default::default(),
            allowedProof: vec![],
            externalChecks: vec![],
        }
    }

    fn trade(received: &[u64]) -> offchain::Trade {
        let asset = |value: u64| offchain::Asset {
            assetType: U256::from(1u64),
            contractAddress: Address::repeat_byte(0xaa),
            value: U256::from(value),
            beneficiary: Address::ZERO,
            extra: Default::default(),
        };
        offchain::Trade {
            signer: Address::repeat_byte(0x33),
            signature: Default::default(),
            checks: checks(),
            sent: vec![asset(1)],
            received: received.iter().map(|v| asset(*v)).collect(),
        }
    }

    fn coupon(address: Address, discount_type: u64, discount: u64) -> offchain::Coupon {
        let mut data = Vec::with_capacity(96);
        data.extend_from_slice(&U256::from(discount_type).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(discount).to_be_bytes::<32>());
        data.extend_from_slice(&[0x77; 32]);
        offchain::Coupon {
            signature: Default::default(),
            checks: checks(),
            couponAddress: address,
            data: data.into(),
            callerData: Default::default(),
        }
    }

    const MAINNET_COUPON: Address = address!("0xc914507fe297b2dddd1232ac3a8903f1c125e794");

    fn order(price: u64) -> Vec<u8> {
        executeOrderCall {
            nftAddress: Address::ZERO,
            assetId: U256::from(1u64),
            price: U256::from(price),
        }
        .abi_encode()
    }

    #[test]
    fn the_recorded_surfaces_are_the_deployed_ones() {
        assert_eq!(SaleKind::CollectionStore.known_selectors().len(), 15);
        assert_eq!(SaleKind::Marketplace.known_selectors().len(), 20);
        assert_eq!(SaleKind::MarketplaceV2.known_selectors().len(), 26);
        assert_eq!(SaleKind::BidV2.known_selectors().len(), 29 + 12);
        assert_eq!(SaleKind::OffChainMarketplace.known_selectors().len(), 37);
        let hex: Vec<String> = BID_V2_NOT_IN_ABI
            .iter()
            .map(|s| hex::encode(function_selector(s)))
            .collect();
        for expected in [
            "0c53c51c", "2d0335ab", "3408e470", "373071f2", "48f4e32b", "688f09da", "8780dfcf",
            "93dc06e1", "967b0049", "9cf160f6", "ce2243b4", "f698da25",
        ] {
            assert!(hex.contains(&expected.to_string()), "{expected}");
        }
        assert_eq!(
            function_selector(OFF_CHAIN_MARKETPLACE_SURFACE[0]),
            offchain::acceptCall::SELECTOR
        );
        assert_eq!(
            function_selector(OFF_CHAIN_MARKETPLACE_SURFACE[1]),
            offchain::acceptWithCouponCall::SELECTOR
        );
        assert_eq!(
            function_selector(COLLECTION_STORE_SURFACE[2]),
            buyCall::SELECTOR
        );
        for (kind, sales) in [
            (SaleKind::CollectionStore, vec![buyCall::SELECTOR]),
            (
                SaleKind::Marketplace,
                vec![
                    executeOrderCall::SELECTOR,
                    estate::safeExecuteOrderCall::SELECTOR,
                ],
            ),
            (
                SaleKind::MarketplaceV2,
                vec![
                    executeOrderCall::SELECTOR,
                    estate::safeExecuteOrderCall::SELECTOR,
                ],
            ),
            (
                SaleKind::BidV2,
                vec![placeBidCall::SELECTOR, estate::placeBidCall::SELECTOR],
            ),
            (
                SaleKind::OffChainMarketplace,
                vec![
                    offchain::acceptCall::SELECTOR,
                    offchain::acceptWithCouponCall::SELECTOR,
                ],
            ),
        ] {
            for sale in sales {
                assert!(kind.known_selectors().contains(&sale), "{kind:?}");
                assert!(kind.sale_method(sale).is_some(), "{kind:?}");
            }
        }
    }

    #[test]
    fn every_polygon_sale_contract_and_audited_marketplace_is_a_sale_target() {
        for (chain, addr, kind) in [
            (
                137,
                "0x214ffc0f0103735728dc66b61a22e4f163e275ae",
                SaleKind::CollectionStore,
            ),
            (
                137,
                "0x02080031b45a3c67d338dd4a2cc309d28756a160",
                SaleKind::Marketplace,
            ),
            (
                137,
                "0x480a0f4e360e8964e68858dd231c2922f1df45ef",
                SaleKind::MarketplaceV2,
            ),
            (
                137,
                "0xb96697fa4a3361ba35b774a42c58daccaad1b8e1",
                SaleKind::BidV2,
            ),
            (
                137,
                "0x540fb08edb56aae562864b390542c97f562825ba",
                SaleKind::OffChainMarketplace,
            ),
            (
                137,
                "0xa40b1d129b8906888720686f3a01921ddf37716f",
                SaleKind::OffChainMarketplace,
            ),
            (
                137,
                "0xe38ef22abe871513555cba89adfe45ab4f548ada",
                SaleKind::OffChainMarketplace,
            ),
            (
                80002,
                "0x1b67d0e31eeb6b52d8eeed71d3616c2f5b33b8e7",
                SaleKind::OffChainMarketplace,
            ),
            (
                80002,
                "0x36fd1434a6c4b8ade80c9847c1d15033ce34488c",
                SaleKind::OffChainMarketplace,
            ),
            (
                80002,
                "0x0c8ad1f6aadf89d2eb19f01a100a6143108fe2b0",
                SaleKind::MarketplaceV2,
            ),
        ] {
            assert_eq!(
                sale_target(chain, addr.parse().unwrap()),
                Some(kind),
                "{chain} {addr}"
            );
        }
        let credits = "0x8b3a40ca1b6f5cafc99d112a4d02e897d1fd8cc5"
            .parse()
            .unwrap();
        assert_eq!(
            sale_target(137, credits),
            None,
            "an audited domain that is not a marketplace is not priced"
        );
        assert_eq!(
            sale_target(
                1,
                "0x02080031b45a3c67d338dd4a2cc309d28756a160"
                    .parse()
                    .unwrap()
            ),
            None
        );
    }

    #[test]
    fn a_wrapped_meta_transaction_is_recognised_in_either_overload() {
        let direct = wrap_split(order(5));
        assert_eq!(nested_meta_tx_selector(&direct), None);
        assert_eq!(
            nested_meta_tx_selector(&wrap_split(wrap_split(order(5)))),
            Some(EXEC_META_TX_SELECTORS[0])
        );
        assert_eq!(
            nested_meta_tx_selector(&wrap_split(wrap_combined(order(5)))),
            Some(EXEC_META_TX_SELECTORS[1])
        );
        assert_eq!(
            nested_meta_tx_selector(&wrap_combined(wrap_split(order(5)))),
            Some(EXEC_META_TX_SELECTORS[0])
        );
        assert_eq!(
            nested_meta_tx_selector(&wrap_combined(wrap_combined(order(5)))),
            Some(EXEC_META_TX_SELECTORS[1])
        );
        assert_eq!(nested_meta_tx_selector(&[0x0c, 0x53]), None);
    }

    #[test]
    fn a_wrapped_sale_is_not_priced_as_the_outer_call_would_suggest() {
        // One level down the executeOrder is invisible to the price check, which
        // is why the relay refuses the nesting before it prices anything.
        let wrapped = wrap_split(wrap_split(order(5)));
        assert_eq!(get_sale_price(&wrapped, 137, SaleKind::MarketplaceV2), None);
        assert_eq!(
            get_sale_price(&wrap_split(order(5)), 137, SaleKind::MarketplaceV2),
            Some(U256::from(5u64))
        );
    }

    #[test]
    fn both_order_book_marketplaces_price_both_sale_spellings() {
        let estate = estate::safeExecuteOrderCall {
            nftAddress: Address::ZERO,
            assetId: U256::from(9u64),
            price: U256::from(7u64),
            fingerprint: alloy_primitives::Bytes::from_static(&[1, 2, 3]),
        }
        .abi_encode();
        for kind in [SaleKind::Marketplace, SaleKind::MarketplaceV2] {
            assert_eq!(
                get_sale_price(&wrap_split(order(3)), 137, kind),
                Some(U256::from(3u64))
            );
            assert_eq!(
                get_sale_price(&wrap_split(estate.clone()), 137, kind),
                Some(U256::from(7u64))
            );
            let cancel = function_selector("cancelOrder(address,uint256)").to_vec();
            assert_eq!(
                get_sale_price(&wrap_split([cancel, vec![0; 64]].concat()), 137, kind),
                None
            );
        }
    }

    #[test]
    fn bids_price_both_overloads_and_the_deployed_extras_are_not_refused() {
        let plain = placeBidCall {
            _tokenAddress: Address::ZERO,
            _tokenId: U256::from(1u64),
            _price: U256::from(11u64),
            _duration: U256::from(86400u64),
        }
        .abi_encode();
        let fingerprinted = estate::placeBidCall {
            _tokenAddress: Address::ZERO,
            _tokenId: U256::from(1u64),
            _price: U256::from(12u64),
            _duration: U256::from(86400u64),
            _fingerprint: Default::default(),
        }
        .abi_encode();
        assert_eq!(
            get_sale_price(&wrap_split(plain), 137, SaleKind::BidV2),
            Some(U256::from(11u64))
        );
        assert_eq!(
            get_sale_price(&wrap_split(fingerprinted), 137, SaleKind::BidV2),
            Some(U256::from(12u64))
        );
        let setter = [
            function_selector("setRoyaltiesManager(address)").to_vec(),
            vec![0; 32],
        ]
        .concat();
        assert_eq!(
            get_sale_price(&wrap_split(setter), 137, SaleKind::BidV2),
            None
        );
    }

    #[test]
    fn an_unreviewed_selector_on_an_admitted_target_is_priced_at_zero() {
        let unknown = [vec![0xde, 0xad, 0xbe, 0xef], vec![0; 64]].concat();
        for kind in [
            SaleKind::CollectionStore,
            SaleKind::Marketplace,
            SaleKind::MarketplaceV2,
            SaleKind::BidV2,
            SaleKind::OffChainMarketplace,
        ] {
            assert_eq!(
                get_sale_price(&wrap_split(unknown.clone()), 137, kind),
                Some(U256::ZERO),
                "{kind:?}"
            );
            assert_eq!(
                get_sale_price(&wrap_split(vec![0x01]), 137, kind),
                Some(U256::ZERO),
                "{kind:?}"
            );
            assert_eq!(
                get_sale_price(&[0x0c, 0x53, 0xc5, 0x1c, 0xff], 137, kind),
                Some(U256::ZERO),
                "{kind:?}"
            );
        }
        let broken_sale = [buyCall::SELECTOR.to_vec(), vec![0xff; 7]].concat();
        assert_eq!(
            get_sale_price(&wrap_split(broken_sale), 137, SaleKind::CollectionStore),
            Some(U256::ZERO)
        );
    }

    #[test]
    fn a_store_batch_is_priced_at_its_cheapest_entry_and_an_unpriced_entry_fails_closed() {
        let item = |prices: &[u64]| ItemToBuy {
            collection: Address::ZERO,
            ids: prices.iter().map(|_| U256::from(1u64)).collect(),
            prices: prices.iter().map(|p| U256::from(*p)).collect(),
            beneficiaries: vec![],
        };
        let buy = |items: Vec<ItemToBuy>| wrap_split(buyCall { itemsToBuy: items }.abi_encode());
        assert_eq!(
            get_sale_price(
                &buy(vec![item(&[9, 4]), item(&[6])]),
                137,
                SaleKind::CollectionStore
            ),
            Some(U256::from(4u64))
        );
        assert_eq!(
            get_sale_price(
                &buy(vec![item(&[9]), item(&[])]),
                137,
                SaleKind::CollectionStore
            ),
            Some(U256::ZERO)
        );
        assert_eq!(
            get_sale_price(&buy(vec![]), 137, SaleKind::CollectionStore),
            Some(U256::ZERO)
        );
    }

    #[test]
    fn an_off_chain_accept_is_priced_at_the_cheapest_received_asset_of_the_batch() {
        let accept = |trades: Vec<offchain::Trade>| {
            wrap_combined(offchain::acceptCall { _trades: trades }.abi_encode())
        };
        assert_eq!(
            get_sale_price(
                &accept(vec![trade(&[50, 20]), trade(&[30])]),
                137,
                SaleKind::OffChainMarketplace
            ),
            Some(U256::from(20u64))
        );
        assert_eq!(
            get_sale_price(
                &accept(vec![trade(&[50]), trade(&[])]),
                137,
                SaleKind::OffChainMarketplace
            ),
            Some(U256::ZERO)
        );
        assert_eq!(
            get_sale_price(&accept(vec![]), 137, SaleKind::OffChainMarketplace),
            Some(U256::ZERO)
        );
        let cancel_signature = function_selector(OFF_CHAIN_MARKETPLACE_SURFACE[7]);
        let trades = offchain::acceptCall {
            _trades: vec![trade(&[1])],
        }
        .abi_encode();
        let cancel_call = [cancel_signature.to_vec(), trades[4..].to_vec()].concat();
        assert_eq!(
            get_sale_price(
                &wrap_combined(cancel_call),
                137,
                SaleKind::OffChainMarketplace
            ),
            None
        );
    }

    #[test]
    fn a_coupon_purchase_is_priced_at_what_settles() {
        let with_coupon = |trades: Vec<offchain::Trade>, coupons: Vec<offchain::Coupon>| {
            wrap_combined(
                offchain::acceptWithCouponCall {
                    _trades: trades,
                    _coupons: coupons,
                }
                .abi_encode(),
            )
        };
        let price = |data: Vec<u8>| get_sale_price(&data, 137, SaleKind::OffChainMarketplace);
        // A 70% rate on 1000 settles 300.
        assert_eq!(
            price(with_coupon(
                vec![trade(&[1000])],
                vec![coupon(MAINNET_COUPON, 1, 700_000)]
            )),
            Some(U256::from(300u64))
        );
        // The rate truncates the product before subtracting: 999 - floor(999*333333/1e6) = 999 - 332 = 667,
        // where scaling by the remaining share would give floor(999*666667/1e6) = 666.
        assert_eq!(
            price(with_coupon(
                vec![trade(&[999])],
                vec![coupon(MAINNET_COUPON, 1, 333_333)]
            )),
            Some(U256::from(667u64))
        );
        assert_eq!(
            price(with_coupon(
                vec![trade(&[1000])],
                vec![coupon(MAINNET_COUPON, 2, 250)]
            )),
            Some(U256::from(750u64))
        );
        assert_eq!(
            price(with_coupon(
                vec![trade(&[100])],
                vec![coupon(MAINNET_COUPON, 2, 101)]
            )),
            Some(U256::ZERO)
        );
        assert_eq!(
            price(with_coupon(
                vec![trade(&[100])],
                vec![coupon(MAINNET_COUPON, 3, 1)]
            )),
            Some(U256::ZERO)
        );
        assert_eq!(
            price(with_coupon(
                vec![trade(&[100]), trade(&[100])],
                vec![coupon(MAINNET_COUPON, 2, 1)]
            )),
            Some(U256::ZERO)
        );
        assert_eq!(
            price(with_coupon(
                vec![trade(&[1000]), trade(&[500])],
                vec![
                    coupon(MAINNET_COUPON, 1, 100_000),
                    coupon(MAINNET_COUPON, 2, 100),
                    coupon(Address::ZERO, 9, 9)
                ]
            )),
            Some(U256::from(400u64))
        );
        let unmodelled = Address::repeat_byte(0x44);
        assert_eq!(
            price(with_coupon(
                vec![trade(&[1000])],
                vec![coupon(unmodelled, 1, 0)]
            )),
            Some(U256::ZERO)
        );
        let amoy_second = address!("0x54df9990a3fd9443b9e84f9f5944904704756074");
        let amoy = with_coupon(vec![trade(&[1000])], vec![coupon(amoy_second, 1, 500_000)]);
        assert_eq!(
            get_sale_price(&amoy, 80002, SaleKind::OffChainMarketplace),
            Some(U256::from(500u64))
        );
        assert_eq!(
            get_sale_price(&amoy, 137, SaleKind::OffChainMarketplace),
            Some(U256::ZERO),
            "modelled per chain"
        );
    }

    #[test]
    fn the_modelled_coupons_include_the_catalog_collection_discount_coupon() {
        use crate::coupons::contracts::coupon_contracts;
        for chain in [137u64, 80002] {
            for contracts in coupon_contracts(chain as i64) {
                let addr: Address = contracts.collection_discount_coupon.parse().unwrap();
                assert!(modelled_coupons(chain).contains(&addr), "{chain}");
            }
        }
    }

    #[test]
    fn erc721_transfer_topic0_matches_keccak() {
        let computed = alloy_primitives::keccak256("Transfer(address,address,uint256)".as_bytes());
        assert_eq!(computed.as_slice(), &ERC721_TRANSFER_TOPIC0);
    }
}
