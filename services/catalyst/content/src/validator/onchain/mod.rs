//! Deploy access read from the chain, as the reference catalyst's on-chain
//! validator reads it: `eth_call`s to the Checker contract of
//! `@dcl/catalyst-contracts` (its bytecode injected as a state override at
//! the checker address, calls batched through its `multicall`), item
//! ownership through `ownerOf`/`balanceOf` in JSON-RPC batches, every
//! question asked at the block of the entity timestamp and again at the
//! block five minutes before it. Blocks come from a binary search over the
//! RPC ([`dcl_http::jsonrpc::BlockSearch`]).
//!
//! The vendored contract data is in `contracts/` with its provenance
//! (`upstream.lock.json`). A call that cannot be decided (no RPC, a failing
//! RPC, a node that cannot answer for that block, a check past its deadline)
//! is an `Err`, never a grant; a call the chain reverts is a "no".

pub(crate) mod items;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use futures::{StreamExt, TryStreamExt};

use alloy_primitives::{Address, B256, Bytes, I256, U256};
use alloy_sol_types::{SolCall, sol};
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::ValidatorError;
use crate::http::Outbound;
use crate::http::jsonrpc::{
    BlockInfo, BlockSearch, BlockTag, Rpc, RpcError, hex_of, request, result_of,
};

pub use items::v1_collection_contract;

/// A collection URN naming no item the ownership check can ask about.
pub(crate) fn is_malformed_item(urn: &str) -> bool {
    matches!(items::item_ref(urn), items::ItemRef::Malformed)
}

sol! {
    interface Checker {
        function checkLAND(address _sender, address _land, address _estate, int256 _x, int256 _y) external view returns (bool);
        function checkName(address _sender, address _registrar, string _name) external view returns (bool);
        function multicall(bytes[] data) external view returns (bool[]);
        function validateThirdParty(address _tpRegistry, string _tpId, bytes32 _root) external view returns (bool);
        function validateWearables(address _sender, address[] _factories, address _collection, uint256 _itemId, string _contentHash, address[] _committees) external view returns (bool);
    }
    interface Erc721 {
        function ownerOf(uint256 tokenId) external view returns (address);
    }
    interface Erc1155 {
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

const CHECKER_BYTECODE: &str = include_str!("contracts/checker.bytecode");
const CONTRACTS_JSON: &str = include_str!("contracts/contracts.json");

/// The Checker's runtime bytecode, injected at the checker address.
pub fn checker_bytecode() -> &'static str {
    CHECKER_BYTECODE.trim()
}

/// Distinct item URNs asked about per check; more refuses the check. An
/// avatar wears at most 18 wearable categories and 10 emotes, an outfits
/// entity about ten avatars of those; `@dcl/schemas` caps neither list.
pub(crate) const MAX_ITEMS_ASKED: usize = 200;
/// Distinct third-party NFT contracts asked about per check; more refuses
/// the check. Their standard probes are batched, so this matches the item cap.
pub(crate) const MAX_NFT_CONTRACTS_ASKED: usize = MAX_ITEMS_ASKED;

/// Distinct claimed names asked about per check; more refuses the check. A
/// profile names one per avatar.
pub(crate) const MAX_NAMES_ASKED: usize = MAX_ITEMS_ASKED;
/// Longer names are answered as not held without being asked: no registrable
/// NAME comes near it, and it bounds the calldata one name sends.
pub(crate) const MAX_NAME_BYTES: usize = 64;

/// How many items or names one ownership check asks about, and what more is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ItemCaps {
    pub items: usize,
    pub nft_contracts: usize,
    pub names: usize,
    /// Past a cap the check is undecided instead of [`ValidatorError::TooManyItems`].
    pub undecided_past_cap: bool,
}

/// A local deploy: more is [`ValidatorError::TooManyItems`].
pub(crate) const LOCAL_ITEM_CAPS: ItemCaps = ItemCaps {
    items: MAX_ITEMS_ASKED,
    nft_contracts: MAX_NFT_CONTRACTS_ASKED,
    names: MAX_NAMES_ASKED,
    undecided_past_cap: false,
};

/// A synced entity, which the reference judged with no cap: wider caps, and
/// past them undecided, never refused.
pub(crate) const SYNCED_ITEM_CAPS: ItemCaps = ItemCaps {
    items: 1000,
    nft_contracts: 1000,
    names: 1000,
    undecided_past_cap: true,
};
/// `eth_call`s per JSON-RPC batch request, and sub-calls per Checker `multicall`.
pub(crate) const CALLS_PER_BATCH: usize = 100;
/// Single calls in flight at once when an endpoint answers no batch.
const CALLS_IN_FLIGHT: usize = 8;
/// Third-party NFT standards kept before the memo starts over.
const NFT_KINDS_CAP: usize = 10_000;
/// How long a contract of no known standard is not probed again.
const UNKNOWN_KIND_TTL: Duration = Duration::from_secs(300);
/// How long the blocks of a timestamp are reused, and how many are kept.
const BLOCKS_MEMO_TTL: Duration = Duration::from_secs(60);
const BLOCKS_MEMO_CAP: usize = 4096;
/// One JSON-RPC request's timeout.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest one access check takes before it is undecided.
const CHECK_DEADLINE: Duration = Duration::from_secs(30);

/// An address as the vendored JSON spells it.
#[derive(Debug, Clone, Copy)]
struct Addr(Address);

impl<'de> Deserialize<'de> for Addr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map(Addr).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
struct VendoredContracts {
    #[serde(rename = "l1Contracts")]
    l1: HashMap<String, L1Contracts>,
    #[serde(rename = "l2Contracts")]
    l2: HashMap<String, L2Contracts>,
}

#[derive(Debug, Clone, Deserialize)]
struct L1Contracts {
    registrar: Addr,
    land: Addr,
    #[serde(rename = "state")]
    estate: Addr,
    checker: Addr,
}

#[derive(Debug, Clone, Deserialize)]
struct SinceBlock {
    address: Addr,
    #[serde(rename = "sinceBlock")]
    since_block: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct L2Contracts {
    checker: Addr,
    #[serde(rename = "thirdParty")]
    third_party: Addr,
    factories: Vec<SinceBlock>,
    #[serde(rename = "commitees")]
    committees: Vec<SinceBlock>,
}

static CONTRACTS: LazyLock<VendoredContracts> =
    LazyLock::new(|| serde_json::from_str(CONTRACTS_JSON).expect("vendored contracts.json"));

/// The L2 network paired with an `ETH_NETWORK`.
fn l2_network(eth_network: &str) -> Option<&'static str> {
    match eth_network {
        "mainnet" => Some("polygon"),
        "sepolia" => Some("amoy"),
        _ => None,
    }
}

/// The `eth_chainId` each network's RPC answers.
fn chain_id(network: &str) -> Option<u64> {
    match network {
        "mainnet" => Some(1),
        "sepolia" => Some(11_155_111),
        "polygon" => Some(137),
        "amoy" => Some(80_002),
        _ => None,
    }
}

/// The blocks a check is asked at: the entity timestamp's and five minutes
/// before it (`timestampBounds` of the reference client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocks {
    pub at_deployment: Option<u64>,
    pub five_min_before: Option<u64>,
}

impl Blocks {
    fn each(self) -> impl Iterator<Item = u64> {
        let at = self.at_deployment.filter(|b| *b > 0);
        let before = self.five_min_before.filter(|b| *b > 0 && Some(*b) != at);
        [at, before].into_iter().flatten()
    }
}

/// `(upper, lower)` seconds for an entity timestamp in ms: up to 8 s after it
/// and 5 min 7 s before that.
pub fn timestamp_bounds(timestamp_ms: i64) -> (i64, i64) {
    let upper = timestamp_ms.div_euclid(1000) + i64::from(timestamp_ms.rem_euclid(1000) > 0) + 8;
    (upper, (upper - 60 * 5 - 7).max(0))
}

fn query_failed(what: &str, e: impl std::fmt::Display) -> ValidatorError {
    ValidatorError::BlockchainQuery(format!("{what}: {e}"))
}

/// The `eth_call` params: the call, the block and the state override.
fn call_params(to: Address, data: &[u8], block: BlockTag, overrides: Option<&Value>) -> Value {
    let mut params = vec![
        json!({ "to": to.to_string(), "data": format!("0x{}", hex::encode(data)) }),
        json!(block.param()),
    ];
    if let Some(o) = overrides {
        params.push(o.clone());
    }
    Value::Array(params)
}

/// One chain's RPC and the blocks read from it.
struct Chain {
    http: Outbound,
    url: String,
    search: BlockSearch,
    /// The blocks of recent entity timestamps (ms), when they were found.
    memo: parking_lot::Mutex<HashMap<i64, (Instant, Blocks)>>,
    name: &'static str,
    chain_id: Option<u64>,
}

impl Chain {
    fn new(http: Outbound, url: String, name: &'static str, chain_id: Option<u64>) -> Self {
        Self {
            http,
            url,
            search: BlockSearch::new(),
            memo: parking_lot::Mutex::new(HashMap::new()),
            name,
            chain_id,
        }
    }

    fn rpc(&self) -> Rpc<'_> {
        Rpc::new(&self.http, &self.url).with_timeout(RPC_TIMEOUT)
    }

    async fn block_at(&self, timestamp: i64) -> Result<Option<BlockInfo>, ValidatorError> {
        let rpc = self.rpc();
        self.search
            .block_at(timestamp, |tag| rpc.block(tag))
            .await
            .map_err(|e| query_failed(&format!("{} block lookup", self.name), e))
    }

    /// The blocks of an entity timestamp, searched once per timestamp while
    /// the memo holds them.
    async fn blocks_for(&self, timestamp_ms: i64) -> Result<Blocks, ValidatorError> {
        if let Some((found, blocks)) = self.memo.lock().get(&timestamp_ms)
            && found.elapsed() < BLOCKS_MEMO_TTL
        {
            return Ok(*blocks);
        }
        let (upper, lower) = timestamp_bounds(timestamp_ms);
        let (at, before) = futures::try_join!(self.block_at(upper), self.block_at(lower))?;
        let at_deployment = at.map(|b| b.number);
        let blocks = Blocks {
            at_deployment,
            // The first block at or after `lower`, as the blocks subgraph
            // answered, never past the deploy block (the head when the
            // timestamp is ahead of the chain).
            five_min_before: before.map(|b| {
                let n = b.number + u64::from(b.timestamp < lower);
                at_deployment.map_or(n, |at| n.min(at))
            }),
        };
        let mut memo = self.memo.lock();
        if memo.len() >= BLOCKS_MEMO_CAP {
            memo.clear();
        }
        memo.insert(timestamp_ms, (Instant::now(), blocks));
        Ok(blocks)
    }

    /// One call's data, `None` when the chain reverted it.
    fn call_answer(
        &self,
        answer: Result<Value, RpcError>,
    ) -> Result<Option<Vec<u8>>, ValidatorError> {
        match answer.and_then(hex_of) {
            Ok(hex) => hex::decode(hex.trim_start_matches("0x"))
                .map(Some)
                .map_err(|e| query_failed(&format!("{} eth_call", self.name), e)),
            Err(e) if e.is_revert() => Ok(None),
            Err(e) => Err(query_failed(&format!("{} eth_call", self.name), e)),
        }
    }

    /// `eth_call` at `block`, with `overrides` as the state override; `None`
    /// when the chain reverts the call.
    async fn call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockTag,
        overrides: Option<&Value>,
    ) -> Result<Option<Vec<u8>>, ValidatorError> {
        let answer = self
            .rpc()
            .call("eth_call", call_params(to, data, block, overrides))
            .await;
        self.call_answer(answer)
    }

    /// `eth_call`s at `block` in JSON-RPC batches, each answer in order;
    /// a call the endpoint left out of its batch answer is sent alone.
    async fn call_each(
        &self,
        calls: &[(Address, Vec<u8>)],
        tag: BlockTag,
    ) -> Result<Vec<Option<Vec<u8>>>, ValidatorError> {
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(CALLS_PER_BATCH) {
            let requests: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, (to, data))| {
                    request(i as u64, "eth_call", call_params(*to, data, tag, None))
                })
                .collect();
            let answered = self
                .rpc()
                .batch(&requests)
                .await
                .map_err(|e| query_failed(&format!("{} eth_call batch", self.name), e))?;
            let mut by_id: Vec<Option<Value>> = vec![None; chunk.len()];
            for envelope in answered {
                if let Some(slot) = envelope
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|id| by_id.get_mut(usize::try_from(id).ok()?))
                {
                    *slot = Some(envelope);
                }
            }
            let pending: Vec<_> = chunk
                .iter()
                .zip(by_id)
                .map(|((to, data), envelope)| async move {
                    match envelope {
                        Some(envelope) => self.call_answer(result_of(envelope)),
                        None => self.call(*to, data, tag, None).await,
                    }
                })
                .collect();
            let answers: Vec<Option<Vec<u8>>> = futures::stream::iter(pending)
                .buffered(CALLS_IN_FLIGHT)
                .try_collect()
                .await?;
            out.extend(answers);
        }
        Ok(out)
    }

    /// Whether each address holds code at the head, in JSON-RPC batches; an
    /// address left out of the batch answer is asked alone, and one that gets
    /// no answer counts as holding none.
    async fn has_code(&self, addresses: &[Address]) -> Vec<bool> {
        let mut out = Vec::with_capacity(addresses.len());
        for chunk in addresses.chunks(CALLS_PER_BATCH) {
            let requests: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, a)| request(i as u64, "eth_getCode", code_params(a)))
                .collect();
            let mut by_id: Vec<Option<Value>> = vec![None; chunk.len()];
            for envelope in self.rpc().batch(&requests).await.unwrap_or_default() {
                if let Some(slot) = envelope
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|id| by_id.get_mut(usize::try_from(id).ok()?))
                {
                    *slot = Some(envelope);
                }
            }
            let pending: Vec<_> = chunk
                .iter()
                .zip(by_id)
                .map(|(a, envelope)| async move {
                    holds_code(match envelope {
                        Some(envelope) => result_of(envelope),
                        None => self.rpc().call("eth_getCode", code_params(a)).await,
                    })
                })
                .collect();
            let code: Vec<bool> = futures::stream::iter(pending)
                .buffered(CALLS_IN_FLIGHT)
                .collect()
                .await;
            out.extend(code);
        }
        out
    }

    /// The Checker's `multicall` over `calls` at `block`, [`CALLS_PER_BATCH`] calls per
    /// `eth_call`; a batch that reverts answers all false.
    async fn multicall(
        &self,
        checker: Address,
        calls: Vec<Vec<u8>>,
        block: u64,
    ) -> Result<Vec<bool>, ValidatorError> {
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(CALLS_PER_BATCH) {
            out.extend(self.multicall_batch(checker, chunk.to_vec(), block).await?);
        }
        Ok(out)
    }

    async fn multicall_batch(
        &self,
        checker: Address,
        calls: Vec<Vec<u8>>,
        block: u64,
    ) -> Result<Vec<bool>, ValidatorError> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let n = calls.len();
        let data = Checker::multicallCall {
            data: calls.into_iter().map(Bytes::from).collect(),
        }
        .abi_encode();
        let overrides = json!({ checker.to_string(): { "code": checker_bytecode() } });
        let Some(out) = self
            .call(checker, &data, BlockTag::Number(block), Some(&overrides))
            .await?
        else {
            return Ok(vec![false; n]);
        };
        let results = Checker::multicallCall::abi_decode_returns(&out)
            .map_err(|e| query_failed(&format!("{} checker answer", self.name), e))?;
        if results.len() != n {
            return Err(query_failed(
                &format!("{} checker answer", self.name),
                format!("{} results for {n} calls", results.len()),
            ));
        }
        Ok(results)
    }

    /// Whether the RPC answers for the chain the configuration names; an
    /// RPC that cannot be reached is only logged.
    async fn verify_chain_id(&self) -> anyhow::Result<()> {
        let Some(expected) = self.chain_id else {
            return Ok(());
        };
        let answer = match self.rpc().call_hex("eth_chainId", json!([])).await {
            Ok(hex) => hex,
            Err(e) => {
                tracing::warn!(chain = self.name, error = %e, "could not read the RPC's chain id");
                return Ok(());
            }
        };
        match u64::from_str_radix(answer.trim_start_matches("0x"), 16) {
            Ok(id) if id == expected => Ok(()),
            _ => anyhow::bail!(
                "the {} RPC answers chain id {answer}, not {expected} as ETH_NETWORK names",
                self.name
            ),
        }
    }
}

/// Asks `check` at the deploy block and, for what it did not grant, at the
/// block five minutes before: per entry (LAND parcels, as the reference's
/// scene check). An entry neither block granted is an `Err` when either ask
/// failed; no block at all grants nothing.
async fn at_deploy_or_before<F, Fut>(
    n: usize,
    blocks: Blocks,
    check: F,
) -> Result<Vec<bool>, ValidatorError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Result<Vec<bool>, ValidatorError>>,
{
    let mut granted = vec![false; n];
    let mut failure = None;
    for block in blocks.each() {
        match check(block).await {
            Ok(answer) => {
                for (g, a) in granted.iter_mut().zip(answer) {
                    *g |= a;
                }
            }
            Err(e) => failure = Some(e),
        }
        if granted.iter().all(|g| *g) {
            return Ok(granted);
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(granted),
    }
}

/// Asks `check` at the deploy block and then at the block five minutes
/// before; every entry must pass at one block (names and items, as the
/// reference's `ownsNamesAtTimestamp` and `ownsItemsAtTimestampInBlockchain`).
/// The answer is the first block's that grants all, else the last block's;
/// an `Err` when none granted all and an ask failed.
async fn all_at_one_block<F, Fut>(
    n: usize,
    blocks: Blocks,
    check: F,
) -> Result<Vec<bool>, ValidatorError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Result<Vec<bool>, ValidatorError>>,
{
    let mut last = vec![false; n];
    let mut failure = None;
    for block in blocks.each() {
        match check(block).await {
            Ok(answer) if answer.iter().all(|a| *a) => return Ok(answer),
            Ok(answer) => last = answer,
            Err(e) => failure = Some(e),
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(last),
    }
}

struct L1 {
    chain: Chain,
    contracts: L1Contracts,
}

struct L2 {
    chain: Chain,
    contracts: L2Contracts,
}

/// The deploy access checks of every entity type, at an entity timestamp:
/// local deploys and entities received from peers ask the same questions.
pub struct OnChainAccess {
    l1: Result<L1, String>,
    l2: Result<L2, String>,
    /// The standard of each third-party NFT contract and when it was probed.
    nft_kinds: parking_lot::Mutex<HashMap<(bool, Address), (NftKind, Instant)>>,
    deadline: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NftKind {
    Erc721,
    Erc1155,
    Unknown,
}

impl OnChainAccess {
    /// `eth_network` picks the vendored contracts (mainnet with polygon,
    /// sepolia with amoy); `l1_url`/`l2_url` are `ETH_RPC_URL` and
    /// `RPC_ENDPOINT_POLYGON`. A missing one leaves its checks undecidable.
    pub fn new(
        http: Outbound,
        eth_network: &str,
        l1_url: Option<String>,
        l2_url: Option<String>,
    ) -> Self {
        let l1 = match (CONTRACTS.l1.get(eth_network), l1_url) {
            (None, _) => Err(format!(
                "ETH_NETWORK {eth_network} has no checker contracts"
            )),
            (_, None) => Err("no L1 RPC is configured (ETH_RPC_URL)".to_string()),
            (Some(c), Some(url)) => Ok(L1 {
                chain: Chain::new(http.clone(), url, "L1", chain_id(eth_network)),
                contracts: c.clone(),
            }),
        };
        let l2_name = l2_network(eth_network);
        let l2 = match (l2_name.and_then(|n| CONTRACTS.l2.get(n)), l2_url) {
            (None, _) => Err(format!(
                "ETH_NETWORK {eth_network} has no L2 checker contracts"
            )),
            (_, None) => Err("no L2 RPC is configured (RPC_ENDPOINT_POLYGON)".to_string()),
            (Some(c), Some(url)) => Ok(L2 {
                chain: Chain::new(http, url, "L2", l2_name.and_then(chain_id)),
                contracts: c.clone(),
            }),
        };
        Self {
            l1,
            l2,
            nft_kinds: parking_lot::Mutex::new(HashMap::new()),
            deadline: CHECK_DEADLINE,
        }
    }

    #[cfg(test)]
    fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    fn l1(&self) -> Result<&L1, ValidatorError> {
        self.l1
            .as_ref()
            .map_err(|m| ValidatorError::Unconfigured(m.clone()))
    }

    fn l2(&self) -> Result<&L2, ValidatorError> {
        self.l2
            .as_ref()
            .map_err(|m| ValidatorError::Unconfigured(m.clone()))
    }

    /// `check`, undecided once it runs past the deadline.
    async fn within_deadline<T>(
        &self,
        what: &str,
        check: impl Future<Output = Result<T, ValidatorError>>,
    ) -> Result<T, ValidatorError> {
        tokio::time::timeout(self.deadline, check)
            .await
            .unwrap_or_else(|_| {
                Err(ValidatorError::BlockchainQuery(format!(
                    "{what} check did not finish within {} s",
                    self.deadline.as_secs_f32()
                )))
            })
    }

    /// Each configured RPC answers for the chain `ETH_NETWORK` names.
    pub async fn verify_chain_ids(&self) -> anyhow::Result<()> {
        if let Ok(l1) = &self.l1 {
            l1.chain.verify_chain_id().await?;
        }
        if let Ok(l2) = &self.l2 {
            l2.chain.verify_chain_id().await?;
        }
        Ok(())
    }

    /// The last L1 block at or before `timestamp_ms`, from the same search the
    /// access checks use; `None` before the first block.
    pub async fn l1_block_at(&self, timestamp_ms: i64) -> Result<Option<u64>, ValidatorError> {
        let chain = &self.l1()?.chain;
        let block = self
            .within_deadline("L1 block", chain.block_at(timestamp_ms.div_euclid(1000)))
            .await?;
        Ok(block.map(|b| b.number))
    }

    /// Per parcel, whether `address` may deploy on it (owner, operator,
    /// update operator, update manager or approved, of the LAND or its estate).
    pub async fn land(
        &self,
        address: &str,
        parcels: &[(i32, i32)],
        timestamp_ms: i64,
    ) -> Result<Vec<bool>, ValidatorError> {
        self.within_deadline("LAND", async {
            let l1 = self.l1()?;
            let Ok(sender) = address.parse::<Address>() else {
                return Ok(vec![false; parcels.len()]);
            };
            let calls: Vec<Vec<u8>> = parcels
                .iter()
                .map(|(x, y)| {
                    Checker::checkLANDCall {
                        _sender: sender,
                        _land: l1.contracts.land.0,
                        _estate: l1.contracts.estate.0,
                        _x: I256::try_from(*x).unwrap_or_default(),
                        _y: I256::try_from(*y).unwrap_or_default(),
                    }
                    .abi_encode()
                })
                .collect();
            let blocks = l1.chain.blocks_for(timestamp_ms).await?;
            at_deploy_or_before(parcels.len(), blocks, |block| {
                l1.chain
                    .multicall(l1.contracts.checker.0, calls.clone(), block)
            })
            .await
        })
        .await
    }

    /// Per name, whether `address` holds it; all of them at one block. More than
    /// [`MAX_NAMES_ASKED`] distinct names is [`ValidatorError::TooManyItems`].
    pub async fn names(
        &self,
        address: &str,
        names: &[String],
        timestamp_ms: i64,
    ) -> Result<Vec<bool>, ValidatorError> {
        self.names_capped(address, names, timestamp_ms, LOCAL_ITEM_CAPS)
            .await
    }

    /// [`Self::names`] under `caps`; each distinct name is asked once.
    pub(crate) async fn names_capped(
        &self,
        address: &str,
        names: &[String],
        timestamp_ms: i64,
        caps: ItemCaps,
    ) -> Result<Vec<bool>, ValidatorError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut distinct: Vec<&String> = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            if seen.insert(name) {
                distinct.push(name);
            }
        }
        if distinct.len() > caps.names {
            let message = format!(
                "{} distinct names, at most {} are checked",
                distinct.len(),
                caps.names
            );
            return Err(if caps.undecided_past_cap {
                ValidatorError::BlockchainQuery(message)
            } else {
                ValidatorError::TooManyItems(message)
            });
        }
        let askable: Vec<&String> = distinct
            .into_iter()
            .filter(|n| n.len() <= MAX_NAME_BYTES)
            .collect();
        let owned = self.distinct_names(address, &askable, timestamp_ms).await?;
        let by_name: HashMap<&String, bool> = askable.into_iter().zip(owned).collect();
        Ok(names
            .iter()
            .map(|n| by_name.get(n).copied().unwrap_or(false))
            .collect())
    }

    async fn distinct_names(
        &self,
        address: &str,
        names: &[&String],
        timestamp_ms: i64,
    ) -> Result<Vec<bool>, ValidatorError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        self.within_deadline("names", async {
            let l1 = self.l1()?;
            let Ok(sender) = address.parse::<Address>() else {
                return Ok(vec![false; names.len()]);
            };
            let calls: Vec<Vec<u8>> = names
                .iter()
                .map(|name| {
                    Checker::checkNameCall {
                        _sender: sender,
                        _registrar: l1.contracts.registrar.0,
                        _name: (*name).clone(),
                    }
                    .abi_encode()
                })
                .collect();
            let blocks = l1.chain.blocks_for(timestamp_ms).await?;
            all_at_one_block(names.len(), blocks, |block| {
                l1.chain
                    .multicall(l1.contracts.checker.0, calls.clone(), block)
            })
            .await
        })
        .await
    }

    /// Whether `address` may deploy item `item_id` of the L2 collection
    /// `contract` with content hashing to one of `hashes` (the creator or a
    /// manager of an editable collection, or a committee member for an item
    /// whose content hash is set).
    pub async fn collection_item(
        &self,
        address: &str,
        contract: &str,
        item_id: &str,
        hashes: &[String],
        timestamp_ms: i64,
    ) -> Result<bool, ValidatorError> {
        self.within_deadline("collection item", async {
            let l2 = self.l2()?;
            let (Ok(sender), Ok(collection), Some(item)) = (
                address.parse::<Address>(),
                contract.parse::<Address>(),
                items::parse_uint(item_id),
            ) else {
                return Ok(false);
            };
            let blocks = l2.chain.blocks_for(timestamp_ms).await?;
            let active = |list: &[SinceBlock], block: u64| -> Vec<Address> {
                list.iter()
                    .filter(|s| block >= s.since_block)
                    .map(|s| s.address.0)
                    .collect()
            };
            let granted = at_deploy_or_before(1, blocks, |block| {
                let calls: Vec<Vec<u8>> = hashes
                    .iter()
                    .map(|hash| {
                        Checker::validateWearablesCall {
                            _sender: sender,
                            _factories: active(&l2.contracts.factories, block),
                            _collection: collection,
                            _itemId: item,
                            _contentHash: hash.clone(),
                            _committees: active(&l2.contracts.committees, block),
                        }
                        .abi_encode()
                    })
                    .collect();
                async move {
                    let answers = l2
                        .chain
                        .multicall(l2.contracts.checker.0, calls, block)
                        .await?;
                    Ok(vec![answers.into_iter().any(|a| a)])
                }
            })
            .await?;
            Ok(granted[0])
        })
        .await
    }

    /// Whether the third party `tp_id` is approved with merkle root `root`.
    pub async fn third_party_root(
        &self,
        tp_id: &str,
        root: [u8; 32],
        timestamp_ms: i64,
    ) -> Result<bool, ValidatorError> {
        self.within_deadline("third-party root", async {
            let l2 = self.l2()?;
            let call = Checker::validateThirdPartyCall {
                _tpRegistry: l2.contracts.third_party.0,
                _tpId: tp_id.to_string(),
                _root: B256::from(root),
            }
            .abi_encode();
            let blocks = l2.chain.blocks_for(timestamp_ms).await?;
            let granted = at_deploy_or_before(1, blocks, |block| {
                l2.chain
                    .multicall(l2.contracts.checker.0, vec![call.clone()], block)
            })
            .await?;
            Ok(granted[0])
        })
        .await
    }

    /// Per URN, whether `address` owns it: a collection item's token
    /// (`ownerOf`), a third-party item's linked NFT (`ownerOf` or
    /// `balanceOf`), the items of each chain all at one block. Assets without
    /// a token and URNs that are not collection items are not asked about and
    /// count as owned; a collection URN naming no askable item is never owned.
    /// More than [`MAX_ITEMS_ASKED`] distinct items or
    /// [`MAX_NFT_CONTRACTS_ASKED`] third-party contracts is
    /// [`ValidatorError::TooManyItems`] ([`LOCAL_ITEM_CAPS`]).
    pub async fn items(
        &self,
        address: &str,
        urns: &[String],
        timestamp_ms: i64,
    ) -> Result<Vec<bool>, ValidatorError> {
        self.items_capped(address, urns, timestamp_ms, LOCAL_ITEM_CAPS)
            .await
    }

    /// [`Self::items`] under `caps`.
    pub(crate) async fn items_capped(
        &self,
        address: &str,
        urns: &[String],
        timestamp_ms: i64,
        caps: ItemCaps,
    ) -> Result<Vec<bool>, ValidatorError> {
        let refs: Vec<items::ItemRef> = urns.iter().map(|u| items::item_ref(u)).collect();
        let mut owned: Vec<bool> = refs
            .iter()
            .map(|r| matches!(r, items::ItemRef::NotAsked))
            .collect();
        let mut asked: Vec<&items::ItemRef> = Vec::new();
        let mut seen = HashSet::new();
        let mut contracts = HashSet::new();
        for r in &refs {
            if r.on_l1().is_none() || seen.contains(r) {
                continue;
            }
            if let items::ItemRef::ThirdParty { l1, contract, .. } = r {
                contracts.insert((*l1, *contract));
            }
            seen.insert(r);
            asked.push(r);
        }
        if asked.len() > caps.items || contracts.len() > caps.nft_contracts {
            let message = format!(
                "{} distinct items from {} NFT contracts, at most {} items and {} contracts \
                 are checked",
                asked.len(),
                contracts.len(),
                caps.items,
                caps.nft_contracts
            );
            return Err(if caps.undecided_past_cap {
                ValidatorError::BlockchainQuery(message)
            } else {
                ValidatorError::TooManyItems(message)
            });
        }
        let Ok(owner) = address.parse::<Address>() else {
            return Ok(owned);
        };
        if asked.is_empty() {
            return Ok(owned);
        }
        let (on_l1, on_l2): (Vec<&items::ItemRef>, Vec<&items::ItemRef>) =
            asked.into_iter().partition(|r| r.on_l1() == Some(true));
        let answers = self
            .within_deadline("items", async {
                let l1 = async {
                    if on_l1.is_empty() {
                        return Ok(Vec::new());
                    }
                    self.owned_on(&self.l1()?.chain, true, owner, &on_l1, timestamp_ms)
                        .await
                };
                let l2 = async {
                    if on_l2.is_empty() {
                        return Ok(Vec::new());
                    }
                    self.owned_on(&self.l2()?.chain, false, owner, &on_l2, timestamp_ms)
                        .await
                };
                futures::try_join!(l1, l2)
            })
            .await?;
        let answers: HashMap<&items::ItemRef, bool> = on_l1
            .into_iter()
            .zip(answers.0)
            .chain(on_l2.into_iter().zip(answers.1))
            .collect();
        for (o, r) in owned.iter_mut().zip(&refs) {
            if let Some(a) = answers.get(r) {
                *o = *a;
            }
        }
        Ok(owned)
    }

    /// Whether `owner` holds each of `refs` on one chain, all at one block.
    async fn owned_on(
        &self,
        chain: &Chain,
        l1_layer: bool,
        owner: Address,
        refs: &[&items::ItemRef],
        timestamp_ms: i64,
    ) -> Result<Vec<bool>, ValidatorError> {
        // Each third-party contract's standard, before the calls that need it.
        let mut contracts: Vec<Address> = refs
            .iter()
            .filter_map(|r| match r {
                items::ItemRef::ThirdParty { contract, .. } => Some(*contract),
                _ => None,
            })
            .collect();
        contracts.sort();
        contracts.dedup();
        let kinds = self.nft_kinds(chain, l1_layer, &contracts).await?;
        let blocks = chain.blocks_for(timestamp_ms).await?;
        all_at_one_block(refs.len(), blocks, |block| {
            self.owns_at(chain, owner, refs, &kinds, block)
        })
        .await
    }

    /// One batch of `ownerOf`/`balanceOf` calls for `refs` at `block`.
    async fn owns_at(
        &self,
        chain: &Chain,
        owner: Address,
        refs: &[&items::ItemRef],
        kinds: &HashMap<Address, NftKind>,
        block: u64,
    ) -> Result<Vec<bool>, ValidatorError> {
        let owner_of = |contract: Address, token: U256| {
            (
                contract,
                Erc721::ownerOfCall { tokenId: token }.abi_encode(),
                true,
            )
        };
        let asks: Vec<Option<(Address, Vec<u8>, bool)>> = refs
            .iter()
            .map(|r| match r {
                items::ItemRef::Token {
                    contract: Some(contract),
                    token,
                    ..
                } => Some(owner_of(*contract, *token)),
                items::ItemRef::ThirdParty {
                    contract, token, ..
                } => match kinds.get(contract) {
                    Some(NftKind::Erc721) => Some(owner_of(*contract, *token)),
                    Some(NftKind::Erc1155) => Some((
                        *contract,
                        Erc1155::balanceOfCall {
                            account: owner,
                            id: *token,
                        }
                        .abi_encode(),
                        false,
                    )),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let calls: Vec<(Address, Vec<u8>)> = asks
            .iter()
            .flatten()
            .map(|(to, data, _)| (*to, data.clone()))
            .collect();
        let mut answers = chain
            .call_each(&calls, BlockTag::Number(block))
            .await?
            .into_iter();
        Ok(asks
            .iter()
            .map(|ask| {
                let Some((_, _, erc721)) = ask else {
                    return false;
                };
                match answers.next().flatten() {
                    Some(out) if *erc721 => {
                        Erc721::ownerOfCall::abi_decode_returns(&out).is_ok_and(|a| a == owner)
                    }
                    Some(out) => Erc1155::balanceOfCall::abi_decode_returns(&out)
                        .is_ok_and(|b| b > U256::ZERO),
                    None => false,
                }
            })
            .collect())
    }

    /// Whether each third-party NFT contract answers as ERC-1155, else
    /// ERC-721, probed at the head as the reference checker does, in one batch
    /// per standard; a contract that is neither is probed again once
    /// [`UNKNOWN_KIND_TTL`] has passed.
    async fn nft_kinds(
        &self,
        chain: &Chain,
        l1_layer: bool,
        contracts: &[Address],
    ) -> Result<HashMap<Address, NftKind>, ValidatorError> {
        let mut kinds = HashMap::new();
        let mut probe = Vec::new();
        {
            let memo = self.nft_kinds.lock();
            for c in contracts {
                match memo.get(&(l1_layer, *c)) {
                    Some((NftKind::Unknown, at)) if at.elapsed() >= UNKNOWN_KIND_TTL => {
                        probe.push(*c)
                    }
                    Some((kind, _)) => {
                        kinds.insert(*c, *kind);
                    }
                    None => probe.push(*c),
                }
            }
        }
        if probe.is_empty() {
            return Ok(kinds);
        }
        let erc1155: Vec<(Address, Vec<u8>)> = probe
            .iter()
            .map(|c| {
                let data = Erc1155::balanceOfCall {
                    account: *c,
                    id: U256::ZERO,
                }
                .abi_encode();
                (*c, data)
            })
            .collect();
        let answers = chain.call_each(&erc1155, BlockTag::Latest).await?;
        let mut found = Vec::with_capacity(probe.len());
        let mut erc721 = Vec::new();
        for (c, answer) in probe.iter().zip(answers) {
            match answer {
                Some(out) if out.len() >= 32 => found.push((*c, NftKind::Erc1155)),
                _ => erc721.push((
                    *c,
                    Erc721::ownerOfCall {
                        tokenId: U256::ZERO,
                    }
                    .abi_encode(),
                )),
            }
        }
        let answers = chain.call_each(&erc721, BlockTag::Latest).await?;
        let mut unknown = Vec::new();
        for ((c, _), answer) in erc721.iter().zip(answers) {
            match answer {
                None => found.push((*c, NftKind::Erc721)),
                Some(out) if out.len() >= 32 => found.push((*c, NftKind::Erc721)),
                Some(_) => unknown.push(*c),
            }
        }
        // An address without code may be a contract a lagging node has not
        // seen yet: it stays unknown for this check only.
        let deployed = chain.has_code(&unknown).await;
        for (c, has_code) in unknown.into_iter().zip(deployed) {
            if has_code {
                found.push((c, NftKind::Unknown));
            } else {
                kinds.insert(c, NftKind::Unknown);
            }
        }
        let now = Instant::now();
        let mut memo = self.nft_kinds.lock();
        if memo.len() + found.len() > NFT_KINDS_CAP {
            memo.clear();
        }
        for (c, kind) in found {
            memo.insert((l1_layer, c), (kind, now));
            kinds.insert(c, kind);
        }
        Ok(kinds)
    }
}

fn code_params(address: &Address) -> Value {
    json!([address.to_string(), BlockTag::Latest.param()])
}

fn holds_code(answer: Result<Value, RpcError>) -> bool {
    answer
        .and_then(hex_of)
        .is_ok_and(|hex| !hex.trim_start_matches("0x").is_empty())
}

#[cfg(test)]
mod tests;
