//! The canonical Shop identities, money units and the authoritative facts the
//! payment rule reads (SHOP.md, "Data design"). Nothing here is a wire DTO yet;
//! later slices serialize these through their own route types.
//!
//! Identity: an asset is chain + contract + [`TokenRef`], which keeps a
//! collection item id (what a mint sells) apart from a minted token id (what a
//! resale sells). A NAME is the registrar's token `uint256(keccak256(lower(label)))`,
//! whether or not it has been registered yet.
//!
//! Money: [`UsdCents`] and [`ManaWei`] are exact integers. The rounding helpers
//! reproduce the pinned upstream rules (credits-server@465e628b
//! `src/logic/{usd-credit-lines,usd-credit-cap,oracle}.ts`, shop@94584a5c
//! `app/src/lib/{currency,payment-options,api,mana,mana-convert,cart-checkout}.ts`);
//! no float touches either.

use std::fmt;

use alloy_primitives::{Address, U256, keccak256};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChainId(pub u64);

impl ChainId {
    pub const ETHEREUM: Self = Self(1);
    pub const SEPOLIA: Self = Self(11_155_111);
    pub const POLYGON: Self = Self(137);
    pub const AMOY: Self = Self(80_002);

    pub fn is_ethereum(self) -> bool {
        self == Self::ETHEREUM || self == Self::SEPOLIA
    }

    /// The chains a CreditsManager lives on: credits only ever settle here.
    pub fn is_polygon(self) -> bool {
        self == Self::POLYGON || self == Self::AMOY
    }

    /// Ethereum purchases pay native gas from the buyer's wallet; Polygon
    /// purchases go through the meta-transaction relay.
    pub fn native_gas_required(self) -> bool {
        !self.is_polygon()
    }
}

impl fmt::Display for ChainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("an address is 0x followed by 40 hex digits")]
    Address,
    #[error("a decimal integer is expected")]
    Decimal,
    #[error("a NAME is 2-15 ASCII letters or digits")]
    Name,
}

/// Whether `text` is `0x` followed by exactly 40 hex digits (either case).
/// `0X` is refused; callers trim or lowercase before asking.
pub fn is_hex_address(text: &str) -> bool {
    text.strip_prefix("0x")
        .is_some_and(|hex| hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A transaction hash as stored: `raw` trimmed, then `0x` plus 64 hex digits,
/// lowercased; `None` for anything else.
pub fn normalize_tx_hash(raw: &str) -> Option<String> {
    let text = raw.trim();
    let hex = text.strip_prefix("0x")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| text.to_lowercase())
}

/// An EVM address, compared by its bytes and always printed lowercase.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvmAddress(Address);

impl EvmAddress {
    pub const fn new(address: Address) -> Self {
        Self(address)
    }

    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let hex_digits = text.strip_prefix("0x").ok_or(ParseError::Address)?;
        if hex_digits.len() != 40 {
            return Err(ParseError::Address);
        }
        let bytes = hex::decode(hex_digits).map_err(|_| ParseError::Address)?;
        Ok(Self(Address::from_slice(&bytes)))
    }

    pub const fn address(self) -> Address {
        self.0
    }
}

impl From<Address> for EvmAddress {
    fn from(address: Address) -> Self {
        Self(address)
    }
}

impl fmt::Display for EvmAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0.as_slice()))
    }
}

impl fmt::Debug for EvmAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for EvmAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EvmAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(D::Error::custom)
    }
}

mod u256_decimal {
    use alloy_primitives::U256;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &U256, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<U256, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::parse_decimal(&text).map_err(D::Error::custom)
    }
}

pub fn parse_decimal(text: &str) -> Result<U256, ParseError> {
    if text.is_empty() || text.len() > 78 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Decimal);
    }
    U256::from_str_radix(text, 10).map_err(|_| ParseError::Decimal)
}

/// What a line sells on its contract: a collection item (a mint issues a new
/// token of it) or one existing token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TokenRef {
    Item {
        #[serde(rename = "itemId", with = "u256_decimal")]
        item_id: U256,
    },
    Token {
        #[serde(rename = "tokenId", with = "u256_decimal")]
        token_id: U256,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetId {
    pub chain_id: ChainId,
    pub contract: EvmAddress,
    pub token: TokenRef,
}

impl AssetId {
    pub fn item(chain_id: ChainId, contract: EvmAddress, item_id: U256) -> Self {
        Self {
            chain_id,
            contract,
            token: TokenRef::Item { item_id },
        }
    }

    pub fn token(chain_id: ChainId, contract: EvmAddress, token_id: U256) -> Self {
        Self {
            chain_id,
            contract,
            token: TokenRef::Token { token_id },
        }
    }

    /// The registrar token a NAME is (or will be once registered).
    pub fn name(registrar: ContractRef, label: &NameLabel) -> Self {
        Self::token(registrar.chain_id, registrar.contract, label.token_id())
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.token {
            TokenRef::Item { item_id } => {
                write!(f, "{}:{}:item:{}", self.chain_id, self.contract, item_id)
            }
            TokenRef::Token { token_id } => {
                write!(f, "{}:{}:token:{}", self.chain_id, self.contract, token_id)
            }
        }
    }
}

/// A contract on a chain: a settlement target (marketplace, CollectionStore,
/// registrar controller, cross-chain executor) or an asset registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractRef {
    pub chain_id: ChainId,
    pub contract: EvmAddress,
}

/// A NAME label as the registrar accepts it (upstream `validateCreditLine`:
/// 2-15 ASCII letters or digits). Kept as typed; identity is case-insensitive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct NameLabel(String);

impl NameLabel {
    pub fn parse(label: &str) -> Result<Self, ParseError> {
        let ok =
            (2..=15).contains(&label.len()) && label.bytes().all(|b| b.is_ascii_alphanumeric());
        if ok {
            Ok(Self(label.to_string()))
        } else {
            Err(ParseError::Name)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `DCLRegistrar.getTokenId`: `uint256(keccak256(lower(label)))`.
    pub fn token_id(&self) -> U256 {
        U256::from_be_bytes(keccak256(self.0.to_ascii_lowercase().as_bytes()).0)
    }
}

impl<'de> Deserialize<'de> for NameLabel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AssetKind {
    Wearable,
    Emote,
    Name,
    Parcel,
    Estate,
    Rental,
    Other,
}

impl AssetKind {
    pub const ALL: [AssetKind; 7] = [
        Self::Wearable,
        Self::Emote,
        Self::Name,
        Self::Parcel,
        Self::Estate,
        Self::Rental,
        Self::Other,
    ];
}

/// Derived by the policy from the offer and the indexed records, never read
/// from a client (hence no `Deserialize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SaleKind {
    /// A CollectionStore buy or a creator item order: the purchase mints.
    PrimaryMint,
    /// A NAME that is not registered yet, bought from the registrar.
    PrimaryNameRegistration,
    /// An existing token changes hands, whoever sells it (a creator reselling
    /// a token already minted included).
    Secondary,
}

impl SaleKind {
    pub const ALL: [SaleKind; 3] = [
        Self::PrimaryMint,
        Self::PrimaryNameRegistration,
        Self::Secondary,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PaymentMethod {
    Credits,
    Mana,
}

/// Exact US cents. One credit is ten cents (upstream `USD_CENTS_PER_CREDIT`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct UsdCents(i64);

impl UsdCents {
    pub const CENTS_PER_CREDIT: i64 = 10;
    /// The same peg as the numeric text the SQL pricing paths bind (dollars
    /// per credit); `credit_peg_is_one_constant` in the credits tests ties
    /// the two.
    pub const CREDIT_USD: &'static str = "0.10";
    pub const ZERO: Self = Self(0);

    pub const fn new(cents: i64) -> Self {
        Self(cents)
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub fn from_credits(credits: i64) -> Option<Self> {
        credits.checked_mul(Self::CENTS_PER_CREDIT).map(Self)
    }

    /// What a quoted price is debited at: up to the next whole credit
    /// (upstream `chargeForQuote`). A quote that is not positive has no
    /// charge; upstream refuses it before rounding.
    pub fn charge_for_quote(self) -> Option<Self> {
        if self.0 <= 0 {
            return None;
        }
        self.credits_ceil()
            .checked_mul(Self::CENTS_PER_CREDIT)
            .map(Self)
    }

    /// Credits for anything the buyer must cover: a price, a charge, a
    /// purchase in the history (upstream `Math.ceil(cents / 10)`).
    pub fn credits_ceil(self) -> i64 {
        self.credits_floor() + i64::from(self.0.rem_euclid(Self::CENTS_PER_CREDIT) != 0)
    }

    /// Credits for anything the buyer holds: a balance, a held amount
    /// (upstream `Math.floor(cents / 10)`).
    pub fn credits_floor(self) -> i64 {
        self.0.div_euclid(Self::CENTS_PER_CREDIT)
    }

    /// The credits leg a balance can fund in a mixed payment: whole credits
    /// only, so the server's round-up never asks for more than is held.
    pub fn whole_credits_floor(self) -> Self {
        Self(self.credits_floor() * Self::CENTS_PER_CREDIT)
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    /// Upstream shop `usdWeiToCents`: a USD-pegged amount (1e18 = $1) in
    /// cents, rounded up so a credit never under-covers the trade.
    pub fn from_usd_wei_ceil(usd_wei: U256) -> Option<Self> {
        let cents = usd_wei.div_ceil(U256::from(USD_WEI_PER_CENT));
        i64::try_from(cents).ok().map(Self)
    }

    /// Upstream shop `discountedUsdCents`: the price after a rate coupon of
    /// `discount_ppm`, rounded up.
    pub fn discounted(self, discount_ppm: u32) -> Option<Self> {
        if self.0 <= 0 {
            return Some(self);
        }
        let kept = i128::from(PPM - u64::from(discount_ppm).min(PPM));
        let cents = (i128::from(self.0) * kept + i128::from(PPM) - 1) / i128::from(PPM);
        i64::try_from(cents).ok().map(Self)
    }
}

impl fmt::Display for UsdCents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}c", self.0)
    }
}

/// Exact MANA base units (18 decimals). A displayed conversion never changes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ManaWei(U256);

const WEI_PER_MANA: u64 = 1_000_000_000_000_000_000;
const USD_WEI_PER_CENT: u64 = 10_000_000_000_000_000;
const BPS: u64 = 10_000;
const PPM: u64 = 1_000_000;

/// `answer / 10^decimals` as exact decimal text, trailing zeros dropped.
pub fn decimal_text(answer: U256, decimals: u8) -> String {
    let digits = answer.to_string();
    let scale = usize::from(decimals);
    if scale == 0 {
        return digits;
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

/// The MANA/USD reference rate, USD per MANA scaled by `10^decimals`, never
/// taken from a client. The credits authorize path prices quotes in cents
/// from the price module's polled snapshot ([`Self::parse_usd_per_mana`]);
/// for a group with a USD-pegged trade it also reads the answer of that
/// marketplace's own `manaUsdAggregator` (`credits::feed`), which sizes
/// those lines, since the two can differ. Settlement rebuilds the rate a
/// credit recorded from its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManaUsdRate {
    rate: U256,
    decimals: u8,
}

impl ManaUsdRate {
    /// None for a non-positive answer (upstream refuses it) or more than 36
    /// decimals.
    pub fn new(rate: U256, decimals: u8) -> Option<Self> {
        (!rate.is_zero() && decimals <= 36).then_some(Self { rate, decimals })
    }

    pub fn rate(self) -> U256 {
        self.rate
    }

    pub fn decimals(self) -> u8 {
        self.decimals
    }

    /// USD per MANA as exact decimal text ("0.26960836"), for a NUMERIC bind.
    pub fn decimal_text(self) -> String {
        decimal_text(self.rate, self.decimals)
    }

    /// The price module's USD-per-MANA decimal text (`"0.25"`) as an
    /// aggregator answer at 18 decimals, flooring past the 18th place; None
    /// for anything but a positive decimal.
    pub fn parse_usd_per_mana(text: &str) -> Option<Self> {
        let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
        let frac: String = frac.chars().take(18).collect();
        if whole.is_empty() && frac.is_empty()
            || !whole
                .bytes()
                .chain(frac.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let whole = if whole.is_empty() {
            U256::ZERO
        } else {
            U256::from_str_radix(whole, 10).ok()?
        };
        let frac = if frac.is_empty() {
            U256::ZERO
        } else {
            U256::from_str_radix(&format!("{frac:0<18}"), 10).ok()?
        };
        Self::new(
            whole
                .checked_mul(U256::from(WEI_PER_MANA))?
                .checked_add(frac)?,
            18,
        )
    }

    fn scale(self) -> U256 {
        U256::from(10u64).pow(U256::from(self.decimals))
    }
}

impl ManaWei {
    pub const ZERO: Self = Self(U256::ZERO);

    pub const fn new(wei: U256) -> Self {
        Self(wei)
    }

    pub const fn get(self) -> U256 {
        self.0
    }

    pub fn from_mana(whole: u64) -> Self {
        Self(U256::from(whole) * U256::from(WEI_PER_MANA))
    }

    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// Upstream `oracle.usdCentsToManaWei`: `cents * 1e18 / 100`, times
    /// `10^decimals / rate`, then `(10000 + buffer_bps) / 10000`, each step
    /// flooring. `rate` is the aggregator answer (USD per MANA scaled by
    /// `10^decimals`). None where upstream throws (a non-positive amount or
    /// rate) or on overflow.
    pub fn from_usd_cents(
        cents: UsdCents,
        rate: U256,
        decimals: u8,
        buffer_bps: u32,
    ) -> Option<Self> {
        if cents.get() <= 0 || rate.is_zero() {
            return None;
        }
        let usd_wei = U256::from(cents.get() as u64).checked_mul(U256::from(WEI_PER_MANA))?
            / U256::from(100u64);
        let scale = U256::from(10u64).checked_pow(U256::from(decimals))?;
        let base = usd_wei.checked_mul(scale)? / rate;
        let buffered = base.checked_mul(U256::from(BPS + u64::from(buffer_bps)))? / U256::from(BPS);
        Some(Self(buffered))
    }

    /// Upstream shop `manaWeiToUsdCents`: USD wei `= wei * rate / 10^decimals`
    /// (flooring), then cents rounded up.
    pub fn usd_cents_ceil(self, rate: ManaUsdRate) -> Option<UsdCents> {
        let usd_wei = self.0.checked_mul(rate.rate)? / rate.scale();
        UsdCents::from_usd_wei_ceil(usd_wei)
    }

    /// Upstream shop `readTradeManaPriceWei`: the MANA a USD-pegged amount
    /// settles for, `usd_wei * 10^decimals / rate`, flooring.
    pub fn from_usd_wei(usd_wei: U256, rate: ManaUsdRate) -> Option<Self> {
        Some(Self(usd_wei.checked_mul(rate.scale())? / rate.rate))
    }

    /// Upstream shop `discountedManaWei`: the amount after a rate coupon of
    /// `discount_ppm`, rounded up so an approval is never a wei short.
    pub fn discounted(self, discount_ppm: u32) -> Option<Self> {
        let kept = U256::from(PPM - u64::from(discount_ppm).min(PPM));
        Some(Self(self.0.checked_mul(kept)?.div_ceil(U256::from(PPM))))
    }

    /// The amount plus `buffer_bps`, flooring (the buffer step of
    /// [`Self::from_usd_cents`], for a need already in MANA).
    pub fn buffered(self, buffer_bps: u32) -> Option<Self> {
        Some(Self(
            self.0
                .checked_mul(U256::from(BPS + u64::from(buffer_bps)))?
                / U256::from(BPS),
        ))
    }

    /// Upstream `applyCapHeadroom`: the signed spend cap is `quoted * pct / 100`,
    /// flooring (the operator percentage is already a whole number here).
    pub fn with_cap_headroom(self, headroom_pct: u32) -> Option<Self> {
        Some(Self(
            self.0.checked_mul(U256::from(headroom_pct))? / U256::from(100u64),
        ))
    }

    /// Upstream shop `manaForRemainder`: the MANA that covers `remainder` of a
    /// purchase whose whole `price` costs `price_wei`, rounded up so a mixed
    /// payment never under-funds the price.
    pub fn for_remainder(remainder: UsdCents, price: UsdCents, price_wei: ManaWei) -> Option<Self> {
        if remainder.get() <= 0 || price.get() <= 0 {
            return Some(Self::ZERO);
        }
        if remainder >= price {
            return Some(price_wei);
        }
        let numerator = price_wei
            .0
            .checked_mul(U256::from(remainder.get() as u64))?;
        let denominator = U256::from(price.get() as u64);
        Some(Self(numerator.div_ceil(denominator)))
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }
}

impl fmt::Display for ManaWei {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} wei", self.0)
    }
}

impl Serialize for ManaWei {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        u256_decimal::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for ManaWei {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        u256_decimal::deserialize(deserializer).map(Self)
    }
}

/// One confirmation group of a cart: a payment method, a chain, the
/// contract the purchase executes on and the call it makes there. A group is
/// exactly what one transaction settles; credits groups execute through the
/// chain's CreditsManager, one external call each, so `contract` and `call`
/// are that call's target and shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementGroupKey {
    pub method: PaymentMethod,
    pub chain_id: ChainId,
    pub contract: EvmAddress,
    pub call: SettlementCall,
}

/// The call a settlement group makes on its contract: a batch that carries
/// every line of the group, or a call that settles a single asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SettlementCall {
    /// `accept(trades)` on an off-chain marketplace.
    Accept,
    /// `acceptWithCoupon(trades, coupons)`: discounted trades, never mixed
    /// with plain ones (upstream shop `purchaseGroupKey`).
    AcceptWithCoupon,
    /// `CollectionStore.buy(items)`, across collections.
    StoreBuy,
    /// A call that settles one asset: a legacy `executeOrder`, a rental, or a
    /// NAME registration (the controller's `register`, or under credits one
    /// executor `execute` per label).
    One { asset: AssetId },
}

/// Who is buying, from the authenticated session. A wallet capability is a
/// session fact (a connected web3 wallet signed the auth chain); it is never
/// inferred from an address or a balance, and never deserialized from a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Session {
    SignedOut,
    Web2 { account: EvmAddress },
    Web3 { wallet: EvmAddress },
}

impl Session {
    pub fn has_wallet(self) -> bool {
        matches!(self, Self::Web3 { .. })
    }
}

/// An answer from an index or mirror. `Absent` is a complete source saying
/// there is none; `Unknown` is a source that cannot say (not configured,
/// catching up, failed), which the rule never reads as absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    Found(T),
    Absent,
    Unknown,
}

/// What the contract registry (the contracts address book and the collections
/// mirror) says an asset contract is. `Lookup::Absent` is "not a Decentraland
/// asset contract".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractRole {
    /// A collection created by a Decentraland CollectionFactory.
    Collection,
    Land,
    Estate,
    NameRegistrar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    Wearable,
    Emote,
}

/// A collection item as mirrored from the collections subgraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRecord {
    pub chain_id: ChainId,
    pub collection: EvmAddress,
    pub item_id: U256,
    pub kind: ItemKind,
    pub creator: EvmAddress,
}

/// A minted token as indexed (ownership and the item it was issued from).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    pub asset: AssetId,
    pub item_id: Option<U256>,
    pub owner: EvmAddress,
}

/// The off-chain marketplace asset type codes as stored (`shop.trade_assets`)
/// and signed; [`TradeAssetType::from_code`] reads them.
pub const ASSET_TYPE_ERC20: i32 = 1;
pub const ASSET_TYPE_USD_PEGGED_MANA: i32 = 2;
pub const ASSET_TYPE_ERC721: i32 = 3;
pub const ASSET_TYPE_COLLECTION_ITEM: i32 = 4;

/// The off-chain marketplace asset types (`CreditsManagerPolygon`,
/// `DecentralandMarketplacePolygonAssetTypes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TradeAssetType {
    Erc20,
    UsdPeggedMana,
    Erc721,
    CollectionItem,
    Unsupported(u64),
}

impl TradeAssetType {
    pub fn from_code(code: u64) -> Self {
        match code {
            1 => Self::Erc20,
            2 => Self::UsdPeggedMana,
            3 => Self::Erc721,
            4 => Self::CollectionItem,
            other => Self::Unsupported(other),
        }
    }

    pub fn is_mana(self) -> bool {
        matches!(self, Self::Erc20 | Self::UsdPeggedMana)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeAsset {
    pub asset_type: TradeAssetType,
    pub contract: EvmAddress,
    /// The amount for MANA, the token id for an ERC-721, the item id for a
    /// collection item.
    pub value: U256,
}

/// A signed off-chain marketplace trade, decoded from the signed order: the
/// assets the signer sends and those the signer receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTrade {
    pub venue: ContractRef,
    pub signer: EvmAddress,
    pub sent: Vec<TradeAsset>,
    pub received: Vec<TradeAsset>,
}

/// A creator's discount the buyer applies at purchase through
/// `acceptWithCoupon`, decoded from the coupon the catalog serves. The signed
/// trade is unchanged; the coupon lowers its price at settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TradeCoupon {
    /// `Coupon.couponAddress` (the CollectionDiscountCoupon contract).
    pub contract: EvmAddress,
    /// The rate discount in parts per million, `0 < discount_ppm < 1_000_000`.
    pub discount_ppm: u32,
}

impl TradeCoupon {
    pub const PPM: u32 = 1_000_000;
}

/// What a line offers to sell, from an authoritative source: a decoded signed
/// order, a CollectionStore listing, a registrar or a rentals listing. Each
/// carries the price that source sets, which is the only input a quote is
/// priced from (`policy::quote`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// `CollectionStore.buy` of an item: mints, at the store's price per unit.
    StoreMint {
        venue: ContractRef,
        collection: EvmAddress,
        item_id: U256,
        price: ManaWei,
    },
    /// An off-chain marketplace trade: `accept`, or `acceptWithCoupon` when
    /// the line carries a coupon. One call is all discounted or none, so the
    /// two never share a settlement group. The price is the trade's signed
    /// received asset.
    Trade {
        trade: SignedTrade,
        coupon: Option<TradeCoupon>,
    },
    /// A legacy marketplace order (`executeOrder`): always an existing token.
    LegacyOrder {
        venue: ContractRef,
        nft: EvmAddress,
        token_id: U256,
        price: ManaWei,
    },
    /// A NAME registration: MANA through the registrar controller on
    /// Ethereum, credits through the cross-chain executor on Polygon; `price`
    /// is the controller's `PRICE` (the executor's `NAME_PRICE`).
    NameRegistration {
        label: NameLabel,
        registrar: ContractRef,
        mana_venue: ContractRef,
        credits_venue: ContractRef,
        price: ManaWei,
    },
    /// A rental listing of a parcel or estate; `price` is the rent for the
    /// chosen period (the signed price per day times one of its signed periods).
    Rental {
        venue: ContractRef,
        contract: EvmAddress,
        token_id: U256,
        price: ManaWei,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreItem {
    pub collection: EvmAddress,
    pub item_id: U256,
    pub price: ManaWei,
}

/// A decoded contract call a purchase executes (the `externalCall` of
/// `useCredits`, a broker buy, or a direct MANA call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCall {
    /// `accept(trades)` when `coupons` is empty, `acceptWithCoupon(trades,
    /// coupons)` otherwise, with one coupon per trade at the same index.
    MarketplaceAccept {
        venue: ContractRef,
        trades: Vec<SignedTrade>,
        coupons: Vec<TradeCoupon>,
    },
    LegacyExecuteOrder {
        venue: ContractRef,
        nft: EvmAddress,
        token_id: U256,
        price: ManaWei,
    },
    /// `CollectionStore.buy`, one entry per unit minted.
    CollectionStoreBuy {
        venue: ContractRef,
        items: Vec<StoreItem>,
    },
    /// A registrar controller registration paid in MANA on Ethereum.
    NameRegister {
        venue: ContractRef,
        registrar: ContractRef,
        label: NameLabel,
    },
    /// A `useCredits` custom external call, with what its arguments decode to.
    Custom {
        venue: ContractRef,
        selector: [u8; 4],
        effect: CustomEffect,
    },
}

impl ExternalCall {
    pub fn venue(&self) -> ContractRef {
        match self {
            Self::MarketplaceAccept { venue, .. }
            | Self::LegacyExecuteOrder { venue, .. }
            | Self::CollectionStoreBuy { venue, .. }
            | Self::NameRegister { venue, .. }
            | Self::Custom { venue, .. } => *venue,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomEffect {
    /// A NAME registration, and nothing else. Only a decoder that proves the
    /// forwarded calldata is exactly the registrar controller's `register`
    /// may produce it; the shared manager's executors forward router calldata
    /// nothing decodes (docs/shop/CONTRACTS.md), which is `Opaque`.
    NameRegistration {
        registrar: ContractRef,
        label: NameLabel,
    },
    /// The custom target forwards to another decoded call. Never
    /// credit-spendable, whatever the inner call is: the manager's typed
    /// routes already reach the marketplaces, the CollectionStore and the
    /// legacy marketplace, and the outer target (a pass-through executor,
    /// drawing its own fixed amount) is not bound to the decoded inner call.
    /// The policy refuses it rather than judging the inner call.
    Call(Box<ExternalCall>),
    /// Arguments nobody decoded: never spendable with credits.
    Opaque,
}

/// How far a credit's signed value may exceed its group's quoted need: the
/// reference rate the need is priced at, the oracle buffer and the cap
/// headroom (upstream credits-server `oracle.usdCentsToManaWei` and
/// `applyCapHeadroom`). Every point of headroom is value the buyer can still
/// spend in a direct `useCredits` after the purchase (docs/shop/CONTRACTS.md).
///
/// Two rates can take part. `rate` prices the quote in cents (the polled
/// snapshot) and sizes every MANA-priced line. `settlement_rate`, when set,
/// is the answer of the feed the group's marketplace converts a USD-pegged
/// trade with when it settles, and sizes those lines alone
/// (`policy::credit_cap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditSizing {
    rate: ManaUsdRate,
    settlement_rate: Option<ManaUsdRate>,
    buffer_bps: u32,
    headroom_pct: u32,
}

impl CreditSizing {
    /// Upstream's default headroom; the slice-4 gate picks the value, never above it.
    pub const MAX_HEADROOM_PCT: u32 = 150;
    /// Upstream's oracle buffer is 2%; more than 10% is a misconfiguration.
    pub const MAX_BUFFER_BPS: u32 = 1_000;

    pub fn new(rate: ManaUsdRate, buffer_bps: u32, headroom_pct: u32) -> Option<Self> {
        let ok = buffer_bps <= Self::MAX_BUFFER_BPS
            && (100..=Self::MAX_HEADROOM_PCT).contains(&headroom_pct);
        ok.then_some(Self {
            rate,
            settlement_rate: None,
            buffer_bps,
            headroom_pct,
        })
    }

    /// The same sizing with the rate a USD-pegged line settles at.
    pub fn with_settlement_rate(self, settlement_rate: ManaUsdRate) -> Self {
        Self {
            settlement_rate: Some(settlement_rate),
            ..self
        }
    }

    /// The largest value a credit for `quoted` may sign: the MANA the quote
    /// costs at the rate plus the buffer, times the headroom.
    pub fn cap(self, quoted: UsdCents) -> Option<ManaWei> {
        ManaWei::from_usd_cents(
            quoted,
            self.rate.rate(),
            self.rate.decimals(),
            self.buffer_bps,
        )?
        .with_cap_headroom(self.headroom_pct)
    }

    /// The largest value a credit for a need already in MANA may sign: the
    /// need plus the buffer, times the headroom, each step flooring as
    /// [`Self::cap`] does.
    pub fn cap_for_need(self, need: ManaWei) -> Option<ManaWei> {
        need.buffered(self.buffer_bps)?
            .with_cap_headroom(self.headroom_pct)
    }

    pub fn rate(self) -> ManaUsdRate {
        self.rate
    }

    pub fn settlement_rate(self) -> Option<ManaUsdRate> {
        self.settlement_rate
    }

    /// The rate a credit records for pricing a later remainder: the one its
    /// USD-pegged lines were sized at, else the quote's.
    pub fn recorded_rate(self) -> ManaUsdRate {
        self.settlement_rate.unwrap_or(self.rate)
    }
}

/// The operation an entry is about to authorize, sign, relay or execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementOperation {
    /// Signing one ephemeral credit of `value` for a settlement group (single
    /// or batch `/credits/authorize`); the group's lines are the plan. The
    /// contract lets the buyer spend whatever part of `value` a call leaves in
    /// later `useCredits` calls until it expires, so the rule bounds it by the
    /// group's own quoted need (`sizing`).
    AuthorizeCredit {
        chain_id: ChainId,
        value: ManaWei,
        sizing: CreditSizing,
    },
    /// `CreditsManager.useCredits`, submitted directly or relayed as a
    /// meta-transaction. `max_uncredited` above zero is a mixed payment.
    UseCredits {
        chain_id: ChainId,
        max_uncredited: ManaWei,
        call: ExternalCall,
    },
    /// The credits ledger debited and the broker buying with treasury MANA.
    LedgerBroker { debit: UsdCents, call: ExternalCall },
    /// Importing a listing as a credit-priced (USD-pegged) Shop listing.
    CreditListing { trade: SignedTrade },
    /// The buyer pays MANA directly; no credit is touched.
    Mana { call: ExternalCall },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_type_codes_are_the_ones_from_code_reads() {
        for (code, kind) in [
            (ASSET_TYPE_ERC20, TradeAssetType::Erc20),
            (ASSET_TYPE_USD_PEGGED_MANA, TradeAssetType::UsdPeggedMana),
            (ASSET_TYPE_ERC721, TradeAssetType::Erc721),
            (ASSET_TYPE_COLLECTION_ITEM, TradeAssetType::CollectionItem),
        ] {
            assert_eq!(TradeAssetType::from_code(code as u64), kind);
        }
    }

    #[test]
    fn tx_hashes_are_trimmed_checked_and_lowercased() {
        let hash = format!("0x{}", "Ab".repeat(32));
        assert_eq!(
            normalize_tx_hash(&format!(" {hash}\n")),
            Some(hash.to_lowercase())
        );
        assert_eq!(normalize_tx_hash(&hash.replacen("0x", "0X", 1)), None);
        assert_eq!(normalize_tx_hash(&hash[..65]), None);
        assert_eq!(normalize_tx_hash(&format!("{hash}0")), None);
        assert_eq!(normalize_tx_hash(&format!("0x{}g", "a".repeat(63))), None);
        assert_eq!(normalize_tx_hash(&"a".repeat(66)), None);
        assert_eq!(normalize_tx_hash(""), None);
    }

    #[test]
    fn hex_address_shape_is_0x_and_forty_hex_digits() {
        let lower = format!("0x{}", "ab".repeat(20));
        assert!(is_hex_address(&lower));
        assert!(is_hex_address(
            &lower.to_ascii_uppercase().replacen("0X", "0x", 1)
        ));
        assert!(!is_hex_address(&lower.replacen("0x", "0X", 1)));
        assert!(!is_hex_address(&lower[..41]));
        assert!(!is_hex_address(&format!("{lower}0")));
        assert!(!is_hex_address(&format!(" {lower}")));
        assert!(!is_hex_address(&format!("0x{}g", "a".repeat(39))));
        assert!(!is_hex_address(""));
    }
}
