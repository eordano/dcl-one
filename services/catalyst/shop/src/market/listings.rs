//! `POST /v1/trades`: a wallet-signed off-chain listing or bid accepted as
//! local authority (`shop.trades`, `mirrored_from` NULL) behind
//! `DATA_WRITES_ENABLED`. The old policy exactly: the signed-fetch chain
//! must carry `signer` dcl:marketplace|dcl:builder and intent
//! `dcl:create-trade`, the recovered address must be the trade's signer,
//! the EIP-712 signature must verify against one of the chain's off-chain
//! marketplaces (v3 first, then v2), an erc721 listing's token must be owned
//! by the signer per `ownerOf` on `TRADE_RPC_URLS` (never the mirror: a
//! stale index is not a reason to accept a listing), and a duplicate
//! signature is a 409. The RPC call goes through the ONE outbound client
//! and its SSRF guard. v2's `trades` row has no `trade_digest` column: a v3
//! cancellation digest is the signing hash, which the `trade_events` fold
//! recomputes from the stored trade (`trade_chain_state.digest`); `GET
//! /v1/trades/{hash}/accept` is served there. `GET /v1/activity` (a
//! wallet's sales, bids and orders across the squid tables) answers the
//! envelope.

use alloy_primitives::{Address, B256, U256, keccak256};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use std::str::FromStr;

use super::signed::{
    CREATE_TRADE_INTENT, MARKETPLACE_AUTH_SIGNERS, require_auth_metadata, require_signer,
};
use super::wire::ApiError;
use super::{MarketplaceState, SharedState};
use crate::comms::limits::{RateLimitDecision, RateLimiter};

use crate::types::{
    ASSET_TYPE_COLLECTION_ITEM, ASSET_TYPE_ERC20, ASSET_TYPE_ERC721, ASSET_TYPE_USD_PEGGED_MANA,
};

pub const TRADE_TYPE_BID: &str = "bid";
pub const TRADE_TYPE_PUBLIC_NFT_ORDER: &str = "public_nft_order";
pub const TRADE_TYPE_PUBLIC_ITEM_ORDER: &str = "public_item_order";

/// `TRADE_RPC_URLS`: `137=https://polygon.example,1=https://eth.example`.
#[derive(Debug, Clone, Default)]
pub struct RpcEndpoints {
    endpoints: Vec<(i64, String)>,
}

impl RpcEndpoints {
    pub fn from_env(raw: Option<&str>) -> Self {
        let endpoints = raw
            .unwrap_or("")
            .split(',')
            .filter_map(|entry| {
                let (chain, url) = entry.split_once('=')?;
                let chain = chain.trim().parse::<i64>().ok()?;
                let url = url.trim();
                if url.is_empty() {
                    return None;
                }
                Some((chain, url.to_string()))
            })
            .collect();
        RpcEndpoints { endpoints }
    }

    pub fn for_chain(&self, chain_id: i64) -> Option<&str> {
        self.endpoints
            .iter()
            .find(|(chain, _)| *chain == chain_id)
            .map(|(_, url)| url.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, &str)> {
        self.endpoints.iter().map(|(c, u)| (*c, u.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffChainMarketplace {
    pub name: &'static str,
    pub version: &'static str,
    pub address: &'static str,
    /// V3 keys a cancellation on the trade's EIP-712 digest; V1/V2 on keccak256 of the signature.
    pub cancels_by_digest: bool,
}

pub const ETHEREUM_MAINNET: i64 = 1;
pub const ETHEREUM_SEPOLIA: i64 = 11155111;
pub const MATIC_MAINNET: i64 = 137;
pub const MATIC_AMOY: i64 = 80002;

pub fn offchain_marketplace_v2(chain_id: i64) -> Option<OffChainMarketplace> {
    match chain_id {
        ETHEREUM_MAINNET | ETHEREUM_SEPOLIA => Some(OffChainMarketplace {
            name: "DecentralandMarketplaceEthereum",
            version: "1.0.0",
            address: "0x1b67d0e31eeb6b52d8eeed71d3616c2f5b33b8e7",
            cancels_by_digest: false,
        }),
        MATIC_MAINNET => Some(OffChainMarketplace {
            name: "DecentralandMarketplacePolygon",
            version: "1.0.0",
            address: "0xa40b1d129b8906888720686f3a01921ddf37716f",
            cancels_by_digest: false,
        }),
        MATIC_AMOY => Some(OffChainMarketplace {
            name: "DecentralandMarketplacePolygon",
            version: "1.0.0",
            address: "0x1b67d0e31eeb6b52d8eeed71d3616c2f5b33b8e7",
            cancels_by_digest: false,
        }),
        _ => None,
    }
}

pub fn offchain_marketplace_v3(chain_id: i64) -> Option<OffChainMarketplace> {
    match chain_id {
        ETHEREUM_MAINNET => Some(OffChainMarketplace {
            name: "DecentralandMarketplaceEthereum",
            version: "1.0.0",
            address: "0x0f11d0d1671519683bd48abf3dbe779e300941cd",
            cancels_by_digest: true,
        }),
        MATIC_MAINNET => Some(OffChainMarketplace {
            name: "DecentralandMarketplacePolygon",
            version: "1.0.0",
            address: "0xe38ef22abe871513555cba89adfe45ab4f548ada",
            cancels_by_digest: true,
        }),
        ETHEREUM_SEPOLIA => Some(OffChainMarketplace {
            name: "DecentralandMarketplaceEthereum",
            version: "1.0.0",
            address: "0x257db44ac97789c16ab277eae87dcde0c246cc9f",
            cancels_by_digest: true,
        }),
        MATIC_AMOY => Some(OffChainMarketplace {
            name: "DecentralandMarketplacePolygon",
            version: "1.0.0",
            address: "0x36fd1434a6c4b8ade80c9847c1d15033ce34488c",
            cancels_by_digest: true,
        }),
        _ => None,
    }
}

/// Newest first: an ambiguous signature settles on the newest deployment.
pub fn offchain_marketplaces(chain_id: i64) -> Vec<OffChainMarketplace> {
    [
        offchain_marketplace_v3(chain_id),
        offchain_marketplace_v2(chain_id),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn network_for_chain(chain_id: i64) -> Option<&'static str> {
    match chain_id {
        ETHEREUM_MAINNET | ETHEREUM_SEPOLIA => Some("ETHEREUM"),
        MATIC_MAINNET | MATIC_AMOY => Some("MATIC"),
        _ => None,
    }
}

pub fn is_estate_chain(chain_id: i64) -> bool {
    matches!(chain_id, ETHEREUM_MAINNET | ETHEREUM_SEPOLIA)
}

#[derive(Debug, Deserialize)]
pub struct ExternalCheckInput {
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    pub selector: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Deserialize)]
pub struct TradeChecksInput {
    #[serde(deserialize_with = "integral_u64")]
    pub uses: u64,
    #[serde(deserialize_with = "integral_i64")]
    pub expiration: i64,
    #[serde(deserialize_with = "integral_i64")]
    pub effective: i64,
    pub salt: String,
    #[serde(rename = "contractSignatureIndex", deserialize_with = "integral_u64")]
    pub contract_signature_index: u64,
    #[serde(rename = "signerSignatureIndex", deserialize_with = "integral_u64")]
    pub signer_signature_index: u64,
    #[serde(rename = "allowedRoot")]
    pub allowed_root: String,
    #[serde(rename = "allowedProof", default)]
    pub allowed_proof: Option<Vec<String>>,
    #[serde(rename = "externalChecks", default)]
    pub external_checks: Option<Vec<ExternalCheckInput>>,
}

/// ajv's `type: 'number'`: `137` and `137.0` are one chain id; a fraction is refused.
enum JsonNumber {
    Unsigned(u64),
    Signed(i64),
    Float(f64),
}

impl<'de> Deserialize<'de> for JsonNumber {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = serde_json::Number::deserialize(deserializer)?;
        if let Some(v) = number.as_u64() {
            Ok(Self::Unsigned(v))
        } else if let Some(v) = number.as_i64() {
            Ok(Self::Signed(v))
        } else {
            number
                .as_f64()
                .map(Self::Float)
                .ok_or_else(|| serde::de::Error::custom("number is outside the supported range"))
        }
    }
}

impl JsonNumber {
    fn integral<E: serde::de::Error>(value: f64) -> Result<f64, E> {
        if value.is_finite() && value.fract() == 0.0 {
            Ok(value)
        } else {
            Err(E::custom(format!("{value} is not an integer")))
        }
    }
    fn into_i64<E: serde::de::Error>(self) -> Result<i64, E> {
        match self {
            JsonNumber::Unsigned(v) => {
                i64::try_from(v).map_err(|_| E::custom(format!("{v} does not fit in an i64")))
            }
            JsonNumber::Signed(v) => Ok(v),
            JsonNumber::Float(v) => {
                let v = Self::integral(v)?;
                if v >= -(2f64.powi(63)) && v < 2f64.powi(63) {
                    Ok(v as i64)
                } else {
                    Err(E::custom(format!("{v} does not fit in an i64")))
                }
            }
        }
    }
    fn into_u64<E: serde::de::Error>(self) -> Result<u64, E> {
        match self {
            JsonNumber::Unsigned(v) => Ok(v),
            JsonNumber::Signed(v) => {
                u64::try_from(v).map_err(|_| E::custom(format!("{v} is negative")))
            }
            JsonNumber::Float(v) => {
                let v = Self::integral(v)?;
                if (0.0..2f64.powi(64)).contains(&v) {
                    Ok(v as u64)
                } else {
                    Err(E::custom(format!("{v} does not fit in a u64")))
                }
            }
        }
    }
}

fn integral_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    JsonNumber::deserialize(d)?.into_i64()
}

fn integral_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    JsonNumber::deserialize(d)?.into_u64()
}

#[derive(Debug, Deserialize)]
pub struct TradeAssetInput {
    #[serde(rename = "assetType")]
    pub asset_type: i32,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    #[serde(default)]
    pub extra: Option<String>,
    #[serde(default)]
    pub beneficiary: Option<String>,
    #[serde(default)]
    pub amount: Option<String>,
    #[serde(rename = "tokenId", default)]
    pub token_id: Option<String>,
    #[serde(rename = "itemId", default)]
    pub item_id: Option<String>,
}

impl TradeAssetInput {
    pub fn signed_value(&self) -> Result<String, SignatureError> {
        let value = match self.asset_type {
            ASSET_TYPE_ERC20 | ASSET_TYPE_USD_PEGGED_MANA => self.amount.as_ref(),
            ASSET_TYPE_ERC721 => self.token_id.as_ref(),
            ASSET_TYPE_COLLECTION_ITEM => self.item_id.as_ref(),
            other => {
                return Err(SignatureError::Malformed(format!(
                    "unsupported asset type {other}"
                )));
            }
        };
        value.cloned().ok_or_else(|| {
            SignatureError::Malformed(format!(
                "asset type {} is missing its value field",
                self.asset_type
            ))
        })
    }
    fn is_erc721(&self) -> bool {
        self.asset_type == ASSET_TYPE_ERC721
    }
    fn is_item(&self) -> bool {
        self.asset_type == ASSET_TYPE_COLLECTION_ITEM
    }
    fn is_fungible(&self) -> bool {
        matches!(
            self.asset_type,
            ASSET_TYPE_ERC20 | ASSET_TYPE_USD_PEGGED_MANA
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct TradeCreation {
    pub signer: String,
    pub signature: String,
    #[serde(rename = "type")]
    pub trade_type: String,
    pub network: String,
    #[serde(rename = "chainId", deserialize_with = "integral_i64")]
    pub chain_id: i64,
    pub checks: TradeChecksInput,
    pub sent: Vec<TradeAssetInput>,
    pub received: Vec<TradeAssetInput>,
}

const TRADE_TYPE: &str = concat!(
    "Trade(Checks checks,AssetWithoutBeneficiary[] sent,Asset[] received)",
    "Asset(uint256 assetType,address contractAddress,uint256 value,bytes extra,address beneficiary)",
    "AssetWithoutBeneficiary(uint256 assetType,address contractAddress,uint256 value,bytes extra)",
    "Checks(uint256 uses,uint256 expiration,uint256 effective,bytes32 salt,uint256 contractSignatureIndex,uint256 signerSignatureIndex,bytes32 allowedRoot,ExternalCheck[] externalChecks)",
    "ExternalCheck(address contractAddress,bytes4 selector,bytes value,bool required)",
);
const EXTERNAL_CHECK_TYPE: &str =
    "ExternalCheck(address contractAddress,bytes4 selector,bytes value,bool required)";
/// `Checks` carries the referenced `ExternalCheck` even when the array is empty.
pub(crate) const CHECKS_TYPE: &str = concat!(
    "Checks(uint256 uses,uint256 expiration,uint256 effective,bytes32 salt,uint256 contractSignatureIndex,uint256 signerSignatureIndex,bytes32 allowedRoot,ExternalCheck[] externalChecks)",
    "ExternalCheck(address contractAddress,bytes4 selector,bytes value,bool required)",
);
const ASSET_TYPE: &str = "Asset(uint256 assetType,address contractAddress,uint256 value,bytes extra,address beneficiary)";
const ASSET_WITHOUT_BENEFICIARY_TYPE: &str =
    "AssetWithoutBeneficiary(uint256 assetType,address contractAddress,uint256 value,bytes extra)";
pub(crate) const DOMAIN_TYPE_CONTRACT_SALT: &str =
    "EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)";

/// The word packing and digest math (one copy, in the auth leaf crate).
pub use crate::auth::eip712;

use eip712::{
    hash_array_of_structs, hash_dynamic, struct_hash, typed_data_digest, word_address, word_u64,
    word_u256,
};

#[derive(Debug, PartialEq, Eq)]
pub enum SignatureError {
    Malformed(String),
    Mismatch { recovered: String, expected: String },
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureError::Malformed(why) => write!(f, "invalid trade signature: {why}"),
            SignatureError::Mismatch {
                recovered,
                expected,
            } => write!(
                f,
                "trade signature recovers to {recovered}, not the signer {expected}"
            ),
        }
    }
}

pub(crate) fn hex_bytes(value: &str) -> Result<Vec<u8>, SignatureError> {
    let trimmed = value.strip_prefix("0x").unwrap_or(value);
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    hex::decode(trimmed).map_err(|e| SignatureError::Malformed(format!("bad hex {value}: {e}")))
}

fn left_padded_32(value: &str) -> Result<[u8; 32], SignatureError> {
    let raw = hex_bytes(value)?;
    if raw.len() > 32 {
        return Err(SignatureError::Malformed(format!(
            "value wider than 32 bytes: {value}"
        )));
    }
    let mut out = [0u8; 32];
    out[32 - raw.len()..].copy_from_slice(&raw);
    Ok(out)
}

fn right_padded_32(value: &str) -> Result<[u8; 32], SignatureError> {
    let raw = hex_bytes(value)?;
    if raw.len() > 32 {
        return Err(SignatureError::Malformed(format!(
            "value wider than 32 bytes: {value}"
        )));
    }
    let mut out = [0u8; 32];
    out[..raw.len()].copy_from_slice(&raw);
    Ok(out)
}

pub(crate) fn parse_address(value: &str) -> Result<Address, SignatureError> {
    Address::from_str(value)
        .map_err(|e| SignatureError::Malformed(format!("bad address {value}: {e}")))
}

fn address_word(value: &str) -> Result<[u8; 32], SignatureError> {
    Ok(word_address(parse_address(value)?))
}

fn uint_word(value: &str) -> Result<[u8; 32], SignatureError> {
    Ok(word_u256(U256::from_str(value).map_err(|e| {
        SignatureError::Malformed(format!("bad uint {value}: {e}"))
    })?))
}

fn to_seconds(ms: i64) -> u64 {
    (ms / 1000).max(0) as u64
}

fn hash_external_checks(checks: &TradeChecksInput) -> Result<[u8; 32], SignatureError> {
    let type_hash = hash_dynamic(EXTERNAL_CHECK_TYPE.as_bytes());
    let mut members = Vec::new();
    for check in checks.external_checks.iter().flatten() {
        let value = check.value.clone().unwrap_or_else(|| "0x".to_string());
        members.push(struct_hash(
            type_hash,
            &[
                address_word(&check.contract_address)?,
                right_padded_32(&check.selector)?,
                hash_dynamic(&hex_bytes(&value)?),
                word_u64(u64::from(check.required)),
            ],
        ));
    }
    Ok(hash_array_of_structs(&members))
}

pub(crate) fn hash_checks(checks: &TradeChecksInput) -> Result<[u8; 32], SignatureError> {
    Ok(struct_hash(
        hash_dynamic(CHECKS_TYPE.as_bytes()),
        &[
            word_u64(checks.uses),
            word_u64(to_seconds(checks.expiration)),
            word_u64(to_seconds(checks.effective)),
            left_padded_32(&checks.salt)?,
            word_u64(checks.contract_signature_index),
            word_u64(checks.signer_signature_index),
            left_padded_32(&checks.allowed_root)?,
            hash_external_checks(checks)?,
        ],
    ))
}

fn hash_assets(
    assets: &[TradeAssetInput],
    with_beneficiary: bool,
) -> Result<[u8; 32], SignatureError> {
    let type_hash = hash_dynamic(if with_beneficiary {
        ASSET_TYPE.as_bytes()
    } else {
        ASSET_WITHOUT_BENEFICIARY_TYPE.as_bytes()
    });
    let mut members = Vec::with_capacity(assets.len());
    for asset in assets {
        let mut fields = vec![
            word_u64(asset.asset_type as u64),
            address_word(&asset.contract_address)?,
            uint_word(&asset.signed_value()?)?,
            hash_dynamic(&hex_bytes(asset.extra.as_deref().unwrap_or("0x"))?),
        ];
        if with_beneficiary {
            let beneficiary = asset
                .beneficiary
                .clone()
                .unwrap_or_else(|| format!("{:?}", Address::ZERO));
            fields.push(address_word(&beneficiary)?);
        }
        members.push(struct_hash(type_hash, &fields));
    }
    Ok(hash_array_of_structs(&members))
}

fn trade_domain_separator(
    marketplace: &OffChainMarketplace,
    chain_id: i64,
) -> Result<[u8; 32], SignatureError> {
    Ok(struct_hash(
        hash_dynamic(DOMAIN_TYPE_CONTRACT_SALT.as_bytes()),
        &[
            hash_dynamic(marketplace.name.as_bytes()),
            hash_dynamic(marketplace.version.as_bytes()),
            word_address(parse_address(marketplace.address)?),
            word_u64(chain_id.max(0) as u64),
        ],
    ))
}

pub fn trade_struct_hash(trade: &TradeCreation) -> Result<B256, SignatureError> {
    Ok(B256::from(struct_hash(
        hash_dynamic(TRADE_TYPE.as_bytes()),
        &[
            hash_checks(&trade.checks)?,
            hash_assets(&trade.sent, false)?,
            hash_assets(&trade.received, true)?,
        ],
    )))
}

pub fn signing_hash(
    trade: &TradeCreation,
    marketplace: &OffChainMarketplace,
) -> Result<B256, SignatureError> {
    Ok(B256::from(typed_data_digest(
        trade_domain_separator(marketplace, trade.chain_id)?,
        trade_struct_hash(trade)?.0,
    )))
}

pub fn recover_signer(
    trade: &TradeCreation,
    marketplace: &OffChainMarketplace,
) -> Result<Address, SignatureError> {
    let raw = hex_bytes(&trade.signature)?;
    if raw.len() != 65 {
        return Err(SignatureError::Malformed(format!(
            "expected a 65-byte signature, got {}",
            raw.len()
        )));
    }
    if !matches!(raw[64], 0 | 1 | 27 | 28) {
        return Err(SignatureError::Malformed(format!(
            "unsupported signature v byte {}",
            raw[64]
        )));
    }
    let hash = signing_hash(trade, marketplace)?;
    let recovered = crate::auth::recover::recover_address_from_digest(
        &hash.0,
        &format!("0x{}", hex::encode(&raw)),
    )
    .map_err(|e| SignatureError::Malformed(format!("unrecoverable signature: {e}")))?;
    Address::from_str(&recovered)
        .map_err(|e| SignatureError::Malformed(format!("unrecoverable signature: {e}")))
}

pub fn verify_signature(
    trade: &TradeCreation,
    marketplace: &OffChainMarketplace,
) -> Result<(), SignatureError> {
    let recovered = recover_signer(trade, marketplace)?;
    let expected = Address::from_str(&trade.signer)
        .map_err(|e| SignatureError::Malformed(format!("bad signer {}: {e}", trade.signer)))?;
    if recovered == expected {
        Ok(())
    } else {
        Err(SignatureError::Mismatch {
            recovered: recovered.to_checksum(None),
            expected: expected.to_checksum(None),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SignatureMatch {
    pub marketplace: OffChainMarketplace,
    pub cancellation_digest: Option<String>,
}

/// A structurally invalid signature (length, v byte, a high s) is Malformed
/// on the first candidate; one that verifies nowhere reports the newest mismatch.
pub fn resolve_signature(
    trade: &TradeCreation,
    candidates: &[OffChainMarketplace],
) -> Result<SignatureMatch, SignatureError> {
    let mut first_mismatch: Option<SignatureError> = None;
    for marketplace in candidates {
        match verify_signature(trade, marketplace) {
            Ok(()) => {
                let cancellation_digest = if marketplace.cancels_by_digest {
                    Some(format!("0x{:x}", signing_hash(trade, marketplace)?))
                } else {
                    None
                };
                return Ok(SignatureMatch {
                    marketplace: *marketplace,
                    cancellation_digest,
                });
            }
            Err(malformed @ SignatureError::Malformed(_)) => return Err(malformed),
            Err(mismatch) => {
                first_mismatch.get_or_insert(mismatch);
            }
        }
    }
    Err(first_mismatch.unwrap_or_else(|| {
        SignatureError::Malformed("no off-chain marketplace to verify against".to_string())
    }))
}

#[derive(Debug)]
pub enum OwnershipError {
    NotConfigured(i64),
    Rpc(String),
    NoOwner(String),
}

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnershipError::NotConfigured(chain) => write!(
                f,
                "ownership cannot be verified: no rpc endpoint is configured for chain {chain}"
            ),
            OwnershipError::Rpc(why) => write!(f, "ownership lookup failed: {why}"),
            OwnershipError::NoOwner(token) => write!(f, "token {token} has no owner on chain"),
        }
    }
}

const OWNER_OF_SELECTOR: &str = "6352211e";

fn owner_of_calldata(token_id: &str) -> Result<String, OwnershipError> {
    let parsed = U256::from_str(token_id)
        .map_err(|e| OwnershipError::Rpc(format!("bad token id {token_id}: {e}")))?;
    Ok(format!(
        "0x{OWNER_OF_SELECTOR}{}",
        hex::encode(parsed.to_be_bytes::<32>())
    ))
}

fn address_from_word(word: &str) -> Result<Address, OwnershipError> {
    let trimmed = word.strip_prefix("0x").unwrap_or(word);
    if trimmed.len() < 40 {
        return Err(OwnershipError::NoOwner(word.to_string()));
    }
    let tail = &trimmed[trimmed.len() - 40..];
    Address::from_str(&format!("0x{tail}"))
        .map_err(|e| OwnershipError::Rpc(format!("undecodable owner word {word}: {e}")))
}

/// `ownerOf(tokenId)` on the chain's endpoint through the guarded client.
pub async fn owner_of(
    state: &MarketplaceState,
    chain_id: i64,
    contract_address: &str,
    token_id: &str,
) -> Result<Address, OwnershipError> {
    let url = state
        .settings
        .market
        .trade_rpc_urls
        .for_chain(chain_id)
        .ok_or(OwnershipError::NotConfigured(chain_id))?;
    if !state.upstream_allowed(url) {
        return Err(OwnershipError::Rpc(
            "rpc endpoint is not a publicly routable https url".to_string(),
        ));
    }
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_call", "params": [{ "to": contract_address, "data": owner_of_calldata(token_id)? }, "latest"] });
    let resp = state
        .http
        .post(url)
        .timeout(std::time::Duration::from_secs(15))
        .json(&body)
        .send()
        .await
        .map_err(|e| OwnershipError::Rpc(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OwnershipError::Rpc(format!(
            "rpc status {}",
            resp.status().as_u16()
        )));
    }
    let bytes = crate::http::read_body_capped(resp, 64 * 1024)
        .await
        .map_err(|e| OwnershipError::Rpc(e.to_string()))?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| OwnershipError::Rpc(format!("unreadable rpc response: {e}")))?;
    if let Some(error) = payload.get("error") {
        return Err(OwnershipError::Rpc(error.to_string()));
    }
    let result = payload
        .get("result")
        .and_then(|r| r.as_str())
        .ok_or_else(|| OwnershipError::Rpc("rpc response carried no result".to_string()))?;
    let owner = address_from_word(result)?;
    if owner == Address::ZERO {
        return Err(OwnershipError::NoOwner(token_id.to_string()));
    }
    Ok(owner)
}

#[derive(Debug)]
pub enum TradeCreationError {
    Expired,
    EffectiveAfterExpiration,
    SignerMismatch,
    UnsupportedChain(i64),
    NetworkMismatch {
        expected: String,
        got: String,
    },
    InvalidStructure(String),
    InvalidSignature(String),
    OwnershipUnverifiable(i64),
    OwnershipLookupFailed(String),
    NotTheOwner {
        owner: String,
    },
    /// A chain this node does not serve for the trade's network.
    WrongChain {
        expected: i64,
        got: i64,
    },
    /// The item is not in the mirrored catalogue (yet).
    ItemUnknown,
    NotTheCreator,
    Duplicate,
    Db(sqlx::Error),
}

impl std::fmt::Display for TradeCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => write!(f, "the trade has already expired"),
            Self::EffectiveAfterExpiration => {
                write!(f, "the trade becomes effective after it expires")
            }
            Self::SignerMismatch => write!(f, "the authenticated address is not the trade signer"),
            Self::UnsupportedChain(chain) => {
                write!(f, "no off-chain marketplace is deployed on chain {chain}")
            }
            Self::NetworkMismatch { expected, got } => {
                write!(f, "chain belongs to network {expected}, not {got}")
            }
            Self::InvalidStructure(why) => write!(f, "invalid trade structure: {why}"),
            Self::InvalidSignature(why) => write!(f, "{why}"),
            Self::OwnershipUnverifiable(chain) => write!(
                f,
                "ownership cannot be verified: no rpc endpoint is configured for chain {chain}"
            ),
            Self::OwnershipLookupFailed(why) => write!(f, "{why}"),
            Self::NotTheOwner { owner } => write!(
                f,
                "the signer does not own this token; it belongs to {owner}"
            ),
            Self::WrongChain { expected, got } => {
                write!(
                    f,
                    "this node serves chain {expected} for the network, not {got}"
                )
            }
            Self::ItemUnknown => write!(f, "the item is not in the catalogue yet; retry later"),
            Self::NotTheCreator => {
                write!(f, "only the item's creator may sell it as a primary sale")
            }
            Self::Duplicate => write!(f, "a trade with this signature already exists"),
            Self::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl From<sqlx::Error> for TradeCreationError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// The chain this node serves for `network` ("ETHEREUM" or "MATIC").
pub fn served_chain(indexes: &super::chain_walk::IndexSettings, network: &str) -> i64 {
    use super::chain_walk::ChainNet;
    if network.eq_ignore_ascii_case("ETHEREUM") {
        indexes.chain_id(ChainNet::Ethereum)
    } else {
        indexes.chain_id(ChainNet::Matic)
    }
}

/// A public item order is a primary sale: only the item's creator may sign it.
async fn verify_item_creator(
    pool: &PgPool,
    network: &str,
    trade: &TradeCreation,
) -> Result<(), TradeCreationError> {
    let sent = &trade.sent[0];
    let item_id = sent.item_id.as_deref().unwrap_or_default();
    let creator: Option<String> = sqlx::query_scalar(
        "SELECT creator FROM shop.items \
         WHERE network = lower($1) AND contract_address = lower($2) AND item_id = $3::numeric",
    )
    .bind(network)
    .bind(&sent.contract_address)
    .bind(item_id)
    .fetch_optional(pool)
    .await?;
    match creator {
        None => Err(TradeCreationError::ItemUnknown),
        Some(c) if c.eq_ignore_ascii_case(&trade.signer) => Ok(()),
        Some(_) => Err(TradeCreationError::NotTheCreator),
    }
}

fn validate_structure(trade: &TradeCreation) -> Result<(), TradeCreationError> {
    let invalid = |why: &str| Err(TradeCreationError::InvalidStructure(why.to_string()));
    if trade.sent.is_empty() || trade.received.is_empty() {
        return invalid("both sent and received must carry at least one asset");
    }
    match trade.trade_type.as_str() {
        TRADE_TYPE_BID => {
            if !trade.sent.iter().all(|a| a.is_fungible()) {
                return invalid("a bid must send only fungible assets");
            }
            if !trade.received.iter().all(|a| a.is_erc721() || a.is_item()) {
                return invalid("a bid must receive an nft or a collection item");
            }
        }
        TRADE_TYPE_PUBLIC_NFT_ORDER => {
            if trade.sent.len() != 1 || !trade.sent[0].is_erc721() {
                return invalid("a public nft order must send exactly one erc721");
            }
            if !trade.received.iter().all(|a| a.is_fungible()) {
                return invalid("a public nft order must receive only fungible assets");
            }
        }
        TRADE_TYPE_PUBLIC_ITEM_ORDER => {
            if trade.sent.len() != 1 || !trade.sent[0].is_item() {
                return invalid("a public item order must send exactly one collection item");
            }
            if !trade.received.iter().all(|a| a.is_fungible()) {
                return invalid("a public item order must receive only fungible assets");
            }
        }
        other => {
            return Err(TradeCreationError::InvalidStructure(format!(
                "unknown trade type {other}"
            )));
        }
    }
    if is_estate_chain(trade.chain_id)
        && trade
            .sent
            .iter()
            .chain(trade.received.iter())
            .any(|a| a.is_item())
    {
        return invalid("collection items are not tradeable on an estate chain");
    }
    Ok(())
}

async fn verify_sent_ownership(
    state: Option<&MarketplaceState>,
    trade: &TradeCreation,
) -> Result<(), TradeCreationError> {
    let Some(asset) = trade.sent.first() else {
        return Ok(());
    };
    if !asset.is_erc721() {
        return Ok(());
    }
    let token_id = asset.token_id.as_ref().ok_or_else(|| {
        TradeCreationError::InvalidStructure("erc721 asset needs a tokenId".to_string())
    })?;
    let state = state.ok_or(TradeCreationError::OwnershipUnverifiable(trade.chain_id))?;
    let owner = owner_of(state, trade.chain_id, &asset.contract_address, token_id)
        .await
        .map_err(|e| match e {
            OwnershipError::NotConfigured(chain) => {
                TradeCreationError::OwnershipUnverifiable(chain)
            }
            other => TradeCreationError::OwnershipLookupFailed(other.to_string()),
        })?;
    if !owner.to_string().eq_ignore_ascii_case(&trade.signer) {
        return Err(TradeCreationError::NotTheOwner {
            owner: owner.to_string(),
        });
    }
    Ok(())
}

fn ms_to_utc(ms: i64) -> Result<DateTime<Utc>, TradeCreationError> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .ok_or_else(|| TradeCreationError::InvalidStructure(format!("unrepresentable time {ms}")))
}

/// keccak256 of the signature's bytes, as upstream stores it
/// (`keccak256(trade.signature)` over the hex, marketplace-server ae872a4)
/// and as the contract emits it in `Traded`/`SignatureCancelled`. The old
/// port hashed the hex text, a key no chain event or mirrored row carries;
/// a value that is not hex keeps that text hash (it cannot reach the chain).
pub fn hashed_signature(signature: &str) -> String {
    match hex::decode(signature.trim().trim_start_matches("0x")) {
        Ok(bytes) if !bytes.is_empty() => format!("0x{:x}", keccak256(&bytes)),
        _ => format!("0x{:x}", keccak256(signature.as_bytes())),
    }
}

/// The canonical camelCase JSON the `checks` column holds.
pub fn checks_json(checks: &TradeChecksInput) -> serde_json::Value {
    let mut v = json!({
        "uses": checks.uses,
        "expiration": checks.expiration,
        "effective": checks.effective,
        "salt": checks.salt,
        "contractSignatureIndex": checks.contract_signature_index,
        "signerSignatureIndex": checks.signer_signature_index,
        "allowedRoot": checks.allowed_root,
        "externalChecks": checks.external_checks.iter().flatten().map(|c| json!({
            "contractAddress": c.contract_address, "selector": c.selector, "value": c.value.as_deref().unwrap_or("0x"), "required": c.required
        })).collect::<Vec<_>>(),
    });
    if let Some(proof) = &checks.allowed_proof {
        v["allowedProof"] = json!(proof);
    }
    v
}

/// The whole policy, then the insert; `state` carries the RPC access (none
/// = an erc721 listing fails closed as unverifiable).
pub async fn create_trade(
    pool: &PgPool,
    state: Option<&MarketplaceState>,
    trade: &TradeCreation,
    authenticated: &str,
    now_ms: i64,
) -> Result<String, TradeCreationError> {
    if trade.checks.expiration < now_ms {
        return Err(TradeCreationError::Expired);
    }
    if trade.checks.expiration < trade.checks.effective {
        return Err(TradeCreationError::EffectiveAfterExpiration);
    }
    if !trade.signer.eq_ignore_ascii_case(authenticated) {
        return Err(TradeCreationError::SignerMismatch);
    }
    let candidates = offchain_marketplaces(trade.chain_id);
    if candidates.is_empty() {
        return Err(TradeCreationError::UnsupportedChain(trade.chain_id));
    }
    let network = network_for_chain(trade.chain_id)
        .ok_or(TradeCreationError::UnsupportedChain(trade.chain_id))?;
    if !trade.network.eq_ignore_ascii_case(network) {
        return Err(TradeCreationError::NetworkMismatch {
            expected: network.to_string(),
            got: trade.network.clone(),
        });
    }
    if let Some(s) = state {
        let expected = served_chain(&s.settings.market.indexes, network);
        if trade.chain_id != expected {
            return Err(TradeCreationError::WrongChain {
                expected,
                got: trade.chain_id,
            });
        }
    }
    validate_structure(trade)?;
    let matched = resolve_signature(trade, &candidates)
        .map_err(|e| TradeCreationError::InvalidSignature(e.to_string()))?;
    verify_sent_ownership(state, trade).await?;
    if trade.trade_type == TRADE_TYPE_PUBLIC_ITEM_ORDER {
        verify_item_creator(pool, network, trade).await?;
    }
    // Stateless callers (tests) link to the default origins.
    let defaults;
    let settings = match state {
        Some(s) => &s.settings,
        None => {
            defaults = super::Settings::for_tests();
            &defaults
        }
    };
    // The bid_received notification is best effort: it never fails the bid.
    let bid_asset = if trade.trade_type == TRADE_TYPE_BID {
        super::trade_events::bid_asset(pool, &settings.land_image_base_url, network, trade)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "bid_received: asset lookup failed; no notification");
                None
            })
    } else {
        None
    };
    let mut tx = pool.begin().await?;
    let trade_id = insert_trade(&mut *tx, trade, network, matched.marketplace.address).await?;
    if let Some(n) = bid_asset.and_then(|a| {
        super::trade_events::bid_received_notification(
            &a,
            &settings.market.marketplace_base_url,
            &trade_id,
            network,
            trade,
        )
    }) {
        let mut savepoint = sqlx::Connection::begin(&mut *tx).await?;
        match catalyst_social::notifications::insert_notifications(&mut *savepoint, &[n]).await {
            Ok(_) => savepoint.commit().await?,
            Err(e) => {
                tracing::warn!(error = %e, trade_id, "bid_received: insert failed; the bid stands");
                savepoint.rollback().await?;
            }
        }
    }
    tx.commit().await?;
    Ok(trade_id)
}

const CREATE_TRADE_SQL: &str = "WITH t AS (
        INSERT INTO shop.trades (network, chain_id, signature, hashed_signature, checks, signer, type, expires_at, effective_since, contract)
        VALUES ($1, $2, $3, $4, $5, $6, $7::shop.trade_type, $8, $9, $10)
        ON CONFLICT (hashed_signature) DO NOTHING
        RETURNING id
     ), input AS MATERIALIZED (
        SELECT gen_random_uuid() AS id, t.id AS trade_id, r.*
        FROM t, jsonb_to_recordset($11) AS r(direction text, asset_type smallint, contract_address text, beneficiary text, extra text, value text)
     ), inserted AS (
        INSERT INTO shop.trade_assets (id, trade_id, direction, asset_type, contract_address, beneficiary, extra)
        SELECT id, trade_id, direction::shop.asset_direction_type, asset_type, contract_address, beneficiary, extra FROM input
        RETURNING id
     ), erc721 AS (
        INSERT INTO shop.trade_assets_erc721 (asset_id, token_id) SELECT i.id, i.value FROM input i JOIN inserted USING (id) WHERE i.asset_type = 3
     ), erc20 AS (
        INSERT INTO shop.trade_assets_erc20 (asset_id, amount) SELECT i.id, i.value::numeric FROM input i JOIN inserted USING (id) WHERE i.asset_type IN (1, 2)
     ), item AS (
        INSERT INTO shop.trade_assets_item (asset_id, item_id) SELECT i.id, i.value FROM input i JOIN inserted USING (id) WHERE i.asset_type = 4
     )
     SELECT id::text FROM t";

/// Head, assets and the typed sub-rows in one statement; a duplicate
/// signature inserts nothing. The signature is stored lowercase, as the
/// mirror stores upstream's, so one listing cannot land twice re-cased.
pub(crate) async fn insert_trade<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    trade: &TradeCreation,
    network: &str,
    contract: &str,
) -> Result<String, TradeCreationError> {
    let expires_at = ms_to_utc(trade.checks.expiration)?;
    let effective_since = ms_to_utc(trade.checks.effective)?;
    let mut rows = Vec::with_capacity(trade.sent.len() + trade.received.len());
    for (direction, assets) in [("sent", &trade.sent), ("received", &trade.received)] {
        for asset in assets {
            let (value, error) = match asset.asset_type {
                ASSET_TYPE_ERC721 => (&asset.token_id, "erc721 asset needs a tokenId"),
                ASSET_TYPE_ERC20 | ASSET_TYPE_USD_PEGGED_MANA => {
                    (&asset.amount, "fungible asset needs an amount")
                }
                ASSET_TYPE_COLLECTION_ITEM => (&asset.item_id, "collection item needs an itemId"),
                other => {
                    return Err(TradeCreationError::InvalidStructure(format!(
                        "unsupported asset type {other}"
                    )));
                }
            };
            let value = value
                .as_ref()
                .ok_or_else(|| TradeCreationError::InvalidStructure(error.to_owned()))?;
            rows.push(json!({
                "direction": direction,
                "asset_type": asset.asset_type,
                "contract_address": asset.contract_address.to_lowercase(),
                "beneficiary": asset.beneficiary.as_ref().map(|b| b.to_lowercase()),
                "extra": asset.extra.as_deref().filter(|e| !e.is_empty() && *e != "0x").unwrap_or(""),
                "value": value,
            }));
        }
    }
    let signature = trade.signature.to_lowercase();
    let inserted: Option<(String,)> = sqlx::query_as(CREATE_TRADE_SQL)
        .bind(network)
        .bind(trade.chain_id as i32)
        .bind(&signature)
        .bind(hashed_signature(&signature))
        .bind(checks_json(&trade.checks))
        .bind(trade.signer.to_lowercase())
        .bind(&trade.trade_type)
        .bind(expires_at.naive_utc())
        .bind(effective_since.naive_utc())
        .bind(contract)
        .bind(serde_json::Value::Array(rows))
        .fetch_optional(executor)
        .await?;
    let Some((trade_id,)) = inserted else {
        return Err(TradeCreationError::Duplicate);
    };
    Ok(trade_id)
}

fn creation_error_to_api(e: TradeCreationError) -> ApiError {
    let message = e.to_string();
    match e {
        TradeCreationError::SignerMismatch | TradeCreationError::InvalidSignature(_) => {
            ApiError::http(401, message)
        }
        TradeCreationError::NotTheOwner { .. } | TradeCreationError::NotTheCreator => {
            ApiError::http(403, message)
        }
        TradeCreationError::OwnershipUnverifiable(_)
        | TradeCreationError::OwnershipLookupFailed(_)
        | TradeCreationError::ItemUnknown => ApiError::http(503, message),
        TradeCreationError::Duplicate => ApiError::http(409, message),
        TradeCreationError::Db(_) => ApiError::http(500, message),
        _ => ApiError::bad_request(message),
    }
}

/// Trades one signer may post per minute; the map holds enough signers that a
/// burst of fresh wallets does not force the sweep that resets every bucket.
static TRADE_LIMITER: std::sync::LazyLock<RateLimiter> = std::sync::LazyLock::new(|| {
    RateLimiter::new(30, std::time::Duration::from_secs(60)).with_max_buckets(200_000)
});

/// Trades one client address may post per minute, across the wallets it signs
/// with (fresh wallets cost nothing); a request whose address is unknown or
/// shared (a gateway) is not counted here.
static TRADE_IP_LIMITER: std::sync::LazyLock<RateLimiter> = std::sync::LazyLock::new(|| {
    RateLimiter::new(120, std::time::Duration::from_secs(60)).with_max_buckets(200_000)
});

async fn post_trade_inner(
    state: &MarketplaceState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    body: &str,
) -> Result<Response, ApiError> {
    if let Some(ip) = crate::shared::client_ip::distinct_client_ip(
        headers,
        peer,
        &state.settings.market.trusted_proxy_cidrs,
    ) && TRADE_IP_LIMITER.check(&crate::shared::client_ip::source_key(&ip))
        == RateLimitDecision::Deny
    {
        return Err(ApiError::http(429, "rate limit exceeded".to_string()));
    }
    require_auth_metadata(headers, MARKETPLACE_AUTH_SIGNERS, Some(CREATE_TRADE_INTENT))?;
    let recovered = require_signer(headers, "post", "/v1/trades").await?;
    if TRADE_LIMITER.check(&recovered.to_ascii_lowercase()) == RateLimitDecision::Deny {
        return Err(ApiError::http(429, "rate limit exceeded".to_string()));
    }
    let trade: TradeCreation = serde_json::from_str(body)
        .map_err(|e| ApiError::bad_request(format!("invalid trade body: {e}")))?;
    // A creator's own item order is the Shop's primary sale; every other trade (a resale listing, a bid, and any type
    // added later) is the Marketplace's.
    if trade.trade_type != TRADE_TYPE_PUBLIC_ITEM_ORDER {
        state.require_marketplace()?;
    }
    let id = match create_trade(
        &state.pool,
        Some(state),
        &trade,
        &recovered,
        Utc::now().timestamp_millis(),
    )
    .await
    {
        Ok(id) => id,
        // The mirror catches up within a pass: tell the client when to retry.
        Err(TradeCreationError::ItemUnknown) => {
            let mut response =
                creation_error_to_api(TradeCreationError::ItemUnknown).into_response();
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("60"),
            );
            return Ok(response);
        }
        Err(e) => return Err(creation_error_to_api(e)),
    };
    state.catalog_cache.invalidate();
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "data": { "id": id } })),
    )
        .into_response())
}

pub async fn post_trade(
    State(state): State<SharedState>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let peer = peer.map(|axum::Extension(axum::extract::ConnectInfo(p))| p);
    post_trade_inner(&state, &headers, peer, &body)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use alloy::signers::SignerSync;
    use alloy::signers::local::PrivateKeySigner;

    pub fn signer() -> PrivateKeySigner {
        "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318"
            .parse()
            .unwrap()
    }

    pub fn trade_json_on(signer_address: &str, signature: &str, chain_id: i64) -> String {
        format!(
            r#"{{
              "signer": "{signer_address}", "signature": "{signature}", "type": "public_nft_order", "network": "MATIC", "chainId": {chain_id},
              "checks": {{ "uses": 1, "expiration": 4102444800000, "effective": 0, "salt": "0x1234", "contractSignatureIndex": 0, "signerSignatureIndex": 0, "allowedRoot": "0x", "externalChecks": [] }},
              "sent": [ {{"assetType": 3, "contractAddress": "0x1111111111111111111111111111111111111111", "tokenId": "42", "extra": "0x"}} ],
              "received": [ {{"assetType": 1, "contractAddress": "0x2222222222222222222222222222222222222222", "amount": "1000", "extra": "0x", "beneficiary": "0x3333333333333333333333333333333333333333"}} ]
            }}"#
        )
    }

    pub fn parse(json: &str) -> TradeCreation {
        serde_json::from_str(json).expect("trade parses")
    }

    pub fn signed_trade_on(chain_id: i64, marketplace: &OffChainMarketplace) -> TradeCreation {
        let wallet = signer();
        let mut trade = parse(&trade_json_on(
            &wallet.address().to_checksum(None),
            "0x00",
            chain_id,
        ));
        sign(&mut trade, marketplace);
        trade
    }

    pub fn sign(trade: &mut TradeCreation, marketplace: &OffChainMarketplace) {
        let hash = signing_hash(trade, marketplace).unwrap();
        let sig = signer().sign_hash_sync(&hash).unwrap();
        trade.signature = format!("0x{}", hex::encode(sig.as_bytes()));
    }

    pub fn signed_trade() -> TradeCreation {
        signed_trade_on(
            MATIC_MAINNET,
            &offchain_marketplace_v2(MATIC_MAINNET).unwrap(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn endpoints_parse_per_chain_and_owner_words_decode() {
        let e = RpcEndpoints::from_env(Some("137=https://polygon.example, 1=https://eth.example"));
        assert_eq!(e.for_chain(137), Some("https://polygon.example"));
        assert_eq!(e.for_chain(42), None);
        assert!(RpcEndpoints::from_env(Some("137=")).is_empty());
        let data = owner_of_calldata("42").unwrap();
        assert!(data.starts_with("0x6352211e") && data.len() == 2 + 8 + 64 && data.ends_with("2a"));
        assert_eq!(
            address_from_word("0x000000000000000000000000abcdefabcdefabcdefabcdefabcdefabcdefabcd")
                .unwrap(),
            Address::from_str("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap()
        );
        assert!(matches!(
            address_from_word("0x00"),
            Err(OwnershipError::NoOwner(_))
        ));
    }

    #[test]
    fn a_real_mainnet_trade_signature_recovers_its_signer() {
        const SIGNER: &str = "0x02d0bb59a5f04a12d883751dc1605e15b4959b7e";
        let marketplace = OffChainMarketplace {
            name: "DecentralandMarketplacePolygon",
            version: "1.0.0",
            address: "0x540fb08edb56aae562864b390542c97f562825ba",
            cancels_by_digest: false,
        };
        let mut trade = parse(&format!(
            r#"{{ "signer": "{SIGNER}", "signature": "0x2860a680deb41ba57ee26d6972c21d49d6cca25c74613ca04b9ed15d48a154f205fd3554d71836277e9d3f0143a62afad6f5c1636a7cd3d8f691dc4b0d8ccd011b",
                 "type": "public_nft_order", "network": "MATIC", "chainId": 137,
                 "checks": {{ "uses": 1, "expiration": 1798783200000, "effective": 1733927535000, "salt": "0x199a4082c5", "contractSignatureIndex": 0, "signerSignatureIndex": 0, "allowedRoot": "0x", "externalChecks": [] }},
                 "sent": [ {{"assetType": 3, "contractAddress": "0xe9e86941b23fbe9d8f4dd0c5b7e5f89722936878", "tokenId": "283", "extra": "0x"}} ],
                 "received": [ {{"assetType": 1, "contractAddress": "0xa1c57f48f0deb89f569dfbe6e2b7f46d33606fd4", "amount": "1000000000000000000", "extra": "0x", "beneficiary": "{SIGNER}"}} ] }}"#
        ));
        assert_eq!(
            hex::encode(signing_hash(&trade, &marketplace).unwrap()),
            "d4d6a86e2a1f0ab327b88353ef9cbd59ddde578a73fdcc176d8b07564c6f7718"
        );
        verify_signature(&trade, &marketplace).expect("a real signature verifies");
        trade.checks.uses = 2;
        assert!(verify_signature(&trade, &marketplace).is_err());
    }

    #[test]
    fn the_signing_hash_matches_its_captured_vectors() {
        let address = signer().address().to_checksum(None);
        let polygon = offchain_marketplace_v2(MATIC_MAINNET).unwrap();
        let ethereum = offchain_marketplace_v2(ETHEREUM_MAINNET).unwrap();
        let plain = parse(&trade_json_on(&address, "0x00", MATIC_MAINNET));
        assert_eq!(
            hex::encode(signing_hash(&plain, &polygon).unwrap()),
            "3d05ec81149b209755d224fbdc951ac74a14e0dce7ae990544483f3d9e98b6c8"
        );
        assert_eq!(
            hex::encode(signing_hash(&plain, &ethereum).unwrap()),
            "cf23221fb47430c0c2cda5f501d69f68426ddeab266e9fb96be581c994154108"
        );
        let checked = parse(&format!(
            r#"{{ "signer": "{address}", "signature": "0x00", "type": "public_item_order", "network": "MATIC", "chainId": 137,
                 "checks": {{ "uses": 2, "expiration": 4102444800123, "effective": 1733691659485, "salt": "0x988c1638e5", "contractSignatureIndex": 3, "signerSignatureIndex": 5, "allowedRoot": "0xabcdef",
                   "externalChecks": [ {{"contractAddress": "0x4444444444444444444444444444444444444444", "selector": "0xdeadbeef", "value": "0x01f4", "required": true}}, {{"contractAddress": "0x5555555555555555555555555555555555555555", "selector": "0x12345678", "required": false}} ] }},
                 "sent": [ {{"assetType": 4, "contractAddress": "0x6666666666666666666666666666666666666666", "itemId": "7", "extra": "0xc0ffee"}} ],
                 "received": [ {{"assetType": 2, "contractAddress": "0x7777777777777777777777777777777777777777", "amount": "150000000000000000000"}} ] }}"#
        ));
        assert_eq!(
            hex::encode(signing_hash(&checked, &polygon).unwrap()),
            "082dcf8039c260f5ca1e02f811d3beb9ec6c2c4a64607086c912697b85c39a3c"
        );
    }

    #[test]
    fn signatures_resolve_newest_first_and_tampering_breaks_them() {
        let v3 = offchain_marketplace_v3(MATIC_AMOY).unwrap();
        let trade = signed_trade_on(MATIC_AMOY, &v3);
        let matched = resolve_signature(&trade, &offchain_marketplaces(MATIC_AMOY)).unwrap();
        assert_eq!(matched.marketplace, v3);
        assert_eq!(
            matched.cancellation_digest.as_deref().map(str::len),
            Some(66)
        );
        let v2 = offchain_marketplace_v2(MATIC_AMOY).unwrap();
        let matched = resolve_signature(
            &signed_trade_on(MATIC_AMOY, &v2),
            &offchain_marketplaces(MATIC_AMOY),
        )
        .unwrap();
        assert_eq!(
            (matched.marketplace, matched.cancellation_digest),
            (v2, None)
        );
        let trade = signed_trade();
        assert!(matches!(
            resolve_signature(&trade, &offchain_marketplaces(MATIC_AMOY)).unwrap_err(),
            SignatureError::Mismatch { .. }
        ));
        let mut tampered = signed_trade();
        tampered.sent[0].token_id = Some("43".into());
        assert!(matches!(
            verify_signature(&tampered, &offchain_marketplace_v2(MATIC_MAINNET).unwrap()),
            Err(SignatureError::Mismatch { .. })
        ));
        let mut short = signed_trade();
        short.signature = "0xdeadbeef".into();
        assert!(matches!(
            verify_signature(&short, &offchain_marketplace_v2(MATIC_MAINNET).unwrap()),
            Err(SignatureError::Malformed(_))
        ));
        let mut high_s = signed_trade();
        let mut raw = hex::decode(high_s.signature.trim_start_matches("0x")).unwrap();
        let n = U256::from_str_radix(
            "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141",
            16,
        )
        .unwrap();
        let s = U256::from_be_slice(&raw[32..64]);
        raw[32..64].copy_from_slice(&(n - s).to_be_bytes::<32>());
        raw[64] = match raw[64] {
            27 => 28,
            28 => 27,
            other => other ^ 1,
        };
        high_s.signature = format!("0x{}", hex::encode(&raw));
        assert!(
            matches!(
                verify_signature(&high_s, &offchain_marketplace_v2(MATIC_MAINNET).unwrap()),
                Err(SignatureError::Malformed(_))
            ),
            "malleated signatures read as invalid, not as a server fault"
        );
    }

    #[test]
    fn the_body_is_read_like_ajv_and_the_checks_are_stored_canonically() {
        let body = trade_json_on("0x1111111111111111111111111111111111111111", "0x00", 137)
            .replace(r#""chainId": 137"#, r#""chainId": 137.0"#)
            .replace(r#""uses": 1"#, r#""uses": 1.0"#);
        let t: TradeCreation = serde_json::from_str(&body).unwrap();
        assert_eq!((t.chain_id, t.checks.uses), (137, 1));
        for bad in [r#""uses": 1.5"#, r#""signerSignatureIndex": -1"#] {
            let body = trade_json_on("0x1", "0x00", 137).replace(
                if bad.starts_with("\"uses\"") {
                    r#""uses": 1"#
                } else {
                    r#""signerSignatureIndex": 0"#
                },
                bad,
            );
            assert!(
                serde_json::from_str::<TradeCreation>(&body).is_err(),
                "{bad}"
            );
        }
        let proved = parse(
            &trade_json_on("0x1111111111111111111111111111111111111111", "0x00", 137).replace(
                r#""allowedRoot": "0x","#,
                r#""allowedRoot": "0x", "allowedProof": ["0x01"],"#,
            ),
        );
        assert_eq!(checks_json(&proved.checks)["allowedProof"], json!(["0x01"]));
        let plain = parse(&trade_json_on(
            "0x1111111111111111111111111111111111111111",
            "0x00",
            137,
        ));
        assert!(checks_json(&plain.checks).get("allowedProof").is_none());
        let polygon = offchain_marketplace_v2(MATIC_MAINNET).unwrap();
        assert_eq!(
            signing_hash(&plain, &polygon).unwrap(),
            signing_hash(&proved, &polygon).unwrap(),
            "the proof is not signed over"
        );
        assert_eq!(
            hashed_signature("0xabcd"),
            format!("0x{:x}", keccak256([0xab_u8, 0xcd]))
        );
        // A mirrored upstream row (marketplace_squid lab copy, 2026-09-24):
        // the stored hash is keccak256 of the signature's bytes.
        let sig = "0x7fd2afc813e91b1b5ddabe66cd81b132a881ff9a5d73440c100afe0a338d2d6b3598857ffa3add6acc1389d4b9d067ea14b138d9ad7488865b966920ee7e41181b";
        assert_eq!(
            hashed_signature(sig),
            "0xf573a15c62efabc7ad5328d649fc89fd0bb10c66f7d524e336d4b1f5c10ad673"
        );
        assert_eq!(
            hashed_signature("0xabc"),
            format!("0x{:x}", keccak256(b"0xabc")),
            "odd hex keeps the text hash"
        );
    }

    /// The post limit is per client address: one address flooding the route
    /// is refused, and another client is not.
    #[tokio::test]
    async fn one_client_address_cannot_exhaust_trade_posts_for_others() {
        let state = crate::market::tests::support::state_without_db();
        let status = |ip: &str| {
            let (state, peer) = (state.clone(), format!("{ip}:443").parse().ok());
            async move {
                match post_trade_inner(&state, &HeaderMap::new(), peer, "{}").await {
                    Ok(r) => r.status().as_u16(),
                    Err(e) => e.into_response().status().as_u16(),
                }
            }
        };
        for _ in 0..120 {
            assert_ne!(status("203.0.113.71").await, 429);
        }
        assert_eq!(status("203.0.113.71").await, 429);
        assert_ne!(status("203.0.113.72").await, 429, "another client posts");
        // An IPv6 client is one /64: rotating addresses inside it shares the budget.
        for i in 0..120 {
            assert_ne!(status(&format!("[2001:db8:7:1::{i:x}]")).await, 429);
        }
        assert_eq!(status("[2001:db8:7:1::ffff]").await, 429);
        assert_ne!(status("[2001:db8:7:2::1]").await, 429, "another /64 posts");
    }

    #[tokio::test]
    async fn the_policy_refuses_before_any_signature_or_database_work() {
        let state = crate::market::tests::support::state_without_db();
        let pool = &state.pool;
        let mut expired = signed_trade();
        expired.checks.expiration = 1_000;
        assert!(matches!(
            create_trade(pool, None, &expired, &expired.signer.clone(), 2_000)
                .await
                .unwrap_err(),
            TradeCreationError::Expired
        ));
        let trade = signed_trade();
        assert!(matches!(
            create_trade(
                pool,
                None,
                &trade,
                "0x9999999999999999999999999999999999999999",
                1_000
            )
            .await
            .unwrap_err(),
            TradeCreationError::SignerMismatch
        ));
        assert!(
            matches!(
                create_trade(pool, None, &trade, &trade.signer.clone(), 1_000)
                    .await
                    .unwrap_err(),
                TradeCreationError::OwnershipUnverifiable(137)
            ),
            "an unverifiable erc721 listing fails closed"
        );
        assert!(
            matches!(
                create_trade(pool, Some(&state), &trade, &trade.signer.clone(), 1_000)
                    .await
                    .unwrap_err(),
                TradeCreationError::OwnershipUnverifiable(137)
            ),
            "no TRADE_RPC_URLS entry for the chain"
        );
        let testnet = signed_trade_on(MATIC_AMOY, &offchain_marketplace_v3(MATIC_AMOY).unwrap());
        assert!(
            matches!(
                create_trade(pool, Some(&state), &testnet, &testnet.signer.clone(), 1_000)
                    .await
                    .unwrap_err(),
                TradeCreationError::WrongChain {
                    expected: 137,
                    got: MATIC_AMOY
                }
            ),
            "a node serving Polygon mainnet refuses an Amoy-signed trade"
        );
        let mut other_chain = signed_trade();
        other_chain.chain_id = 42;
        assert!(matches!(
            create_trade(pool, None, &other_chain, &other_chain.signer.clone(), 1_000)
                .await
                .unwrap_err(),
            TradeCreationError::UnsupportedChain(42)
        ));
        let mut wrong_network = signed_trade();
        wrong_network.network = "ETHEREUM".into();
        assert!(matches!(
            create_trade(
                pool,
                None,
                &wrong_network,
                &wrong_network.signer.clone(),
                1_000
            )
            .await
            .unwrap_err(),
            TradeCreationError::NetworkMismatch { .. }
        ));
    }
}
