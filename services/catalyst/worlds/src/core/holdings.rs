//! A wallet's storage allowance read from its on-chain holdings, as the old
//! dcl-name-stats `account-holdings` computed it: 100 MiB per DCL NAME, per
//! LAND (estates counted by their LAND) and per 2000 MANA across Ethereum
//! and Polygon. The Ethereum reads go in one JSON-RPC batch at `latest`; a
//! failed or reverted call is an error, never a zero holding.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::{Address as EvmAddress, U256, address};
use alloy_sol_types::{SolCall, sol};
use serde_json::Value;

use crate::ens::{Call, call_params};
use crate::http::Outbound;
use crate::http::jsonrpc::{Rpc, RpcError, hex_of, request, result_of};
use crate::shared::identity::Address;

use super::quota::{MIB, SpaceAllowance, Unlimited};

sol! {
    interface BalanceOf {
        function balanceOf(address owner) external view returns (uint256);
    }
    interface EstateSize {
        function getLANDsSize(address owner) external view returns (uint256);
    }
}

/// MiB granted per NAME, LAND and [`MANA_PER_UNIT`].
const MIB_PER_UNIT: u64 = 100;
const MANA_PER_UNIT_WEI: u128 = 2_000 * 1_000_000_000_000_000_000;
/// 2000 MANA in wei.
const MANA_PER_UNIT: U256 = U256::from_limbs([
    MANA_PER_UNIT_WEI as u64,
    (MANA_PER_UNIT_WEI >> 64) as u64,
    0,
    0,
]);
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
const ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(300);
/// How long an answered allowance is reused.
const ALLOWANCE_CACHE_TTL: Duration = Duration::from_secs(60);
/// How long a failed lookup is answered from memory.
const ALLOWANCE_FAILURE_TTL: Duration = Duration::from_secs(15);
const ALLOWANCE_CACHE_MAX: usize = 10_000;
/// Allowance lookups in flight at once, node-wide.
const ALLOWANCE_MAX_IN_FLIGHT: usize = 8;
/// How long a publish or `/limits` lookup waits for a free slot.
const ALLOWANCE_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
/// Unauthenticated lookups (`/wallet/{w}/stats`) in flight, apart from the above.
const ALLOWANCE_IDLE_MAX_IN_FLIGHT: usize = 2;

/// The contracts of one network (`@dcl/catalyst-contracts` `l1Contracts` /
/// `l2Contracts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldingsContracts {
    pub registrar: EvmAddress,
    pub land: EvmAddress,
    pub estate: EvmAddress,
    pub l1_mana: EvmAddress,
    pub l2_mana: EvmAddress,
}

impl HoldingsContracts {
    /// `ETH_NETWORK`'s contracts: mainnet with Polygon, sepolia with Amoy.
    pub fn for_network(network: &str) -> Option<Self> {
        match network.trim() {
            "mainnet" => Some(Self {
                registrar: address!("2a187453064356c898cae034eaed119e1663acb8"),
                land: address!("f87e31492faf9a91b02ee0deaad50d51d56d5d4d"),
                estate: address!("959e104e1a4db6317fa58f8295f586e1a978c297"),
                l1_mana: address!("0f5d2fb29fb7d3cfee444a200298f468908cc942"),
                l2_mana: address!("a1c57f48f0deb89f569dfbe6e2b7f46d33606fd4"),
            }),
            "sepolia" => Some(Self {
                registrar: address!("7518456ae93eb98f3e64571b689c626616bb7f30"),
                land: address!("42f4ba48791e2de32f5fbf553441c2672864bb33"),
                estate: address!("369a7fbe718c870c79f99fb423882e8dd8b20486"),
                l1_mana: address!("fa04d2e2ba9aec166c93dfeeba7427b2303befa9"),
                l2_mana: address!("7ad72b9f944ea9793cf4055d88f81138cc2c63a0"),
            }),
            _ => None,
        }
    }
}

/// The allowance source for `ETH_NETWORK` over `ETH_RPC_URL` and
/// `RPC_ENDPOINT_POLYGON`; unlimited, with a warning, without both or
/// without the network's contracts.
pub fn configured(
    http: Outbound,
    l1_rpc_url: Option<&str>,
    l2_rpc_url: Option<&str>,
    network: &str,
) -> Arc<dyn SpaceAllowance> {
    let l1 = l1_rpc_url.map(str::trim).filter(|u| !u.is_empty());
    let l2 = l2_rpc_url.map(str::trim).filter(|u| !u.is_empty());
    let unlimited = |reason: &str| -> Arc<dyn SpaceAllowance> {
        tracing::warn!("worlds storage allowance is unlimited: {reason}");
        Arc::new(Unlimited)
    };
    let Some(contracts) = HoldingsContracts::for_network(network) else {
        return unlimited(&format!(
            "no holdings contracts for ETH_NETWORK={}",
            network.trim()
        ));
    };
    match (l1, l2) {
        (Some(l1), Some(l2)) => Arc::new(ChainHoldings::new(http, l1, l2, contracts)),
        (None, _) => unlimited("ETH_RPC_URL is not configured"),
        (_, None) => unlimited("RPC_ENDPOINT_POLYGON is not configured"),
    }
}

type CachedAllowance = (Instant, Result<i64, String>);

/// The allowance computed from chain reads, cached per wallet.
pub struct ChainHoldings {
    http: Outbound,
    l1_rpc_url: String,
    l2_rpc_url: String,
    contracts: HoldingsContracts,
    cache: dashmap::DashMap<Address, CachedAllowance>,
    in_flight: tokio::sync::Semaphore,
    idle_in_flight: tokio::sync::Semaphore,
}

/// A failed read; a revert is not retried.
struct Fault {
    retry: bool,
    message: String,
}

impl Fault {
    fn of(what: &str, e: RpcError) -> Self {
        Self {
            retry: !e.is_revert(),
            message: format!("{what}: {e}"),
        }
    }

    fn malformed(message: String) -> Self {
        Self {
            retry: true,
            message,
        }
    }
}

impl ChainHoldings {
    pub fn new(
        http: Outbound,
        l1_rpc_url: &str,
        l2_rpc_url: &str,
        contracts: HoldingsContracts,
    ) -> Self {
        Self {
            http,
            l1_rpc_url: l1_rpc_url.to_string(),
            l2_rpc_url: l2_rpc_url.to_string(),
            contracts,
            cache: dashmap::DashMap::new(),
            in_flight: tokio::sync::Semaphore::new(ALLOWANCE_MAX_IN_FLIGHT),
            idle_in_flight: tokio::sync::Semaphore::new(ALLOWANCE_IDLE_MAX_IN_FLIGHT),
        }
    }

    fn fresh(entry: &CachedAllowance) -> bool {
        let ttl = match entry.1 {
            Ok(_) => ALLOWANCE_CACHE_TTL,
            Err(_) => ALLOWANCE_FAILURE_TTL,
        };
        entry.0.elapsed() < ttl
    }

    fn cached(&self, wallet: &Address) -> Option<Result<i64, String>> {
        self.cache
            .get(wallet)
            .filter(|e| Self::fresh(e))
            .map(|e| e.1.clone())
    }

    /// Full of fresh entries, a new answer is not kept (no wholesale clear).
    fn remember(&self, wallet: &Address, answer: Result<i64, String>) {
        if self.cache.len() >= ALLOWANCE_CACHE_MAX && !self.cache.contains_key(wallet) {
            self.cache.retain(|_, e| Self::fresh(e));
            if self.cache.len() >= ALLOWANCE_CACHE_MAX {
                return;
            }
        }
        self.cache.insert(wallet.clone(), (Instant::now(), answer));
    }

    async fn fetch(&self, wallet: &Address) -> Result<i64, String> {
        let owner: EvmAddress = wallet
            .as_str()
            .parse()
            .map_err(|e| format!("holdings: bad wallet: {e}"))?;
        let mut attempt = 0;
        loop {
            match self.holdings(owner).await {
                Ok(bytes) => return Ok(bytes),
                Err(f) if !f.retry || attempt + 1 >= ATTEMPTS => return Err(f.message),
                Err(f) => {
                    let delay = RETRY_BASE_DELAY * 2u32.pow(attempt);
                    tracing::warn!(error = %f.message, attempt = attempt + 1, ?delay, "holdings read failed, retrying");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// The allowance in bytes from one Ethereum batch and one Polygon call.
    async fn holdings(&self, owner: EvmAddress) -> Result<i64, Fault> {
        let c = &self.contracts;
        let balance = BalanceOf::balanceOfCall { owner }.abi_encode();
        let l1_calls: [Call; 4] = [
            (c.registrar, balance.clone()),
            (c.land, balance.clone()),
            (
                c.estate,
                EstateSize::getLANDsSizeCall { owner }.abi_encode(),
            ),
            (c.l1_mana, balance.clone()),
        ];
        let l2_call: Call = (c.l2_mana, balance);
        let (l1, l2) = tokio::join!(
            self.batch(&self.l1_rpc_url, &l1_calls),
            self.one(&self.l2_rpc_url, &l2_call)
        );
        let [names, lands, estate_lands, l1_mana] = l1?;
        Ok(allowance_bytes(
            names,
            lands,
            estate_lands,
            l1_mana.saturating_add(l2?),
        ))
    }

    /// `calls` in one JSON-RPC batch, each answer in order; an endpoint
    /// without batches is asked one call at a time.
    async fn batch<const N: usize>(
        &self,
        url: &str,
        calls: &[Call; N],
    ) -> Result<[U256; N], Fault> {
        let requests: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, call)| request(i as u64, "eth_call", call_params(call)))
            .collect();
        let items = Rpc::new(&self.http, url)
            .with_timeout(RPC_TIMEOUT)
            .batch(&requests)
            .await
            .map_err(|e| Fault::of("holdings batch", e))?;
        let mut out = [U256::ZERO; N];
        if items.is_empty() {
            for (slot, call) in out.iter_mut().zip(calls) {
                *slot = self.one(url, call).await?;
            }
            return Ok(out);
        }
        let mut by_id: std::collections::HashMap<u64, Value> = items
            .into_iter()
            .filter_map(|v| Some((v.get("id")?.as_u64()?, v)))
            .collect();
        for (i, slot) in out.iter_mut().enumerate() {
            let envelope = by_id
                .remove(&(i as u64))
                .ok_or_else(|| Fault::malformed("holdings: no answer in the batch".into()))?;
            *slot = decode(result_of(envelope).and_then(hex_of))?;
        }
        Ok(out)
    }

    async fn one(&self, url: &str, call: &Call) -> Result<U256, Fault> {
        decode(
            Rpc::new(&self.http, url)
                .with_timeout(RPC_TIMEOUT)
                .call_hex("eth_call", call_params(call))
                .await,
        )
    }
}

/// A `uint256` answer; a revert or an empty answer is a fault.
fn decode(answer: Result<String, RpcError>) -> Result<U256, Fault> {
    let hex = answer.map_err(|e| Fault::of("holdings eth_call", e))?;
    let bytes = hex::decode(hex.trim_start_matches("0x"))
        .map_err(|e| Fault::malformed(format!("holdings: bad hex: {e}")))?;
    if bytes.len() != 32 {
        return Err(Fault::malformed(format!(
            "holdings: a {}-byte answer is not a uint256",
            bytes.len()
        )));
    }
    Ok(U256::from_be_slice(&bytes))
}

/// `(names + lands + estate lands + floor(mana / 2000 MANA)) * 100 MiB`, in
/// bytes, saturating at `i64::MAX`.
fn allowance_bytes(names: U256, lands: U256, estate_lands: U256, mana_wei: U256) -> i64 {
    let units = names
        .saturating_add(lands)
        .saturating_add(estate_lands)
        .saturating_add(mana_wei / MANA_PER_UNIT);
    let bytes = units
        .saturating_mul(U256::from(MIB_PER_UNIT))
        .saturating_mul(U256::from(MIB as u64));
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

#[async_trait::async_trait]
impl SpaceAllowance for ChainHoldings {
    async fn allowance_bytes(&self, wallet: &Address) -> Result<i64, String> {
        if let Some(answer) = self.cached(wallet) {
            return answer;
        }
        let _permit = tokio::time::timeout(ALLOWANCE_QUEUE_TIMEOUT, self.in_flight.acquire())
            .await
            .map_err(|_| "holdings: too many lookups in flight".to_string())?
            .map_err(|_| "holdings closed".to_string())?;
        // A caller queued behind the same wallet's lookup reuses its answer.
        if let Some(answer) = self.cached(wallet) {
            return answer;
        }
        let answer = self.fetch(wallet).await;
        self.remember(wallet, answer.clone());
        answer
    }

    async fn allowance_bytes_if_idle(&self, wallet: &Address) -> Result<i64, String> {
        if let Some(answer) = self.cached(wallet) {
            return answer;
        }
        let _permit = self
            .idle_in_flight
            .try_acquire()
            .map_err(|_| "holdings: too many lookups in flight".to_string())?;
        let answer = self.fetch(wallet).await;
        self.remember(wallet, answer.clone());
        answer
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::json;

    use super::*;

    const WEI: u128 = 1_000_000_000_000_000_000;

    /// A view: its contract and selector.
    type View = (EvmAddress, [u8; 4]);

    /// One fake chain: `(contract, selector)` -> uint256 per owner; a
    /// missing entry reverts.
    #[derive(Clone, Default)]
    struct FakeChain {
        values: Arc<Mutex<HashMap<View, U256>>>,
        /// `(contract, selector)` answered with an RPC fault, not a revert.
        faulty: Arc<Mutex<Vec<View>>>,
        /// HTTP requests received (a batch is one).
        requests: Arc<AtomicUsize>,
        /// Request bodies received.
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    impl FakeChain {
        fn set(&self, contract: EvmAddress, selector: [u8; 4], value: U256) {
            self.values
                .lock()
                .unwrap()
                .insert((contract, selector), value);
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    async fn answer(State(chain): State<FakeChain>, Json(body): Json<Value>) -> Json<Value> {
        chain.requests.fetch_add(1, Ordering::SeqCst);
        chain.bodies.lock().unwrap().push(body.clone());
        match body {
            Value::Array(calls) => Json(Value::Array(
                calls.iter().map(|c| answer_one(&chain, c)).collect(),
            )),
            call => Json(answer_one(&chain, &call)),
        }
    }

    fn answer_one(chain: &FakeChain, body: &Value) -> Value {
        let id = body["id"].clone();
        let call = &body["params"][0];
        assert_eq!(body["params"][1], "latest");
        let to: EvmAddress = call["to"].as_str().unwrap().parse().unwrap();
        let data = hex::decode(call["data"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let selector: [u8; 4] = data[..4].try_into().unwrap();
        if chain.faulty.lock().unwrap().contains(&(to, selector)) {
            return json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": "header not found" } });
        }
        match chain.values.lock().unwrap().get(&(to, selector)) {
            Some(v) => json!({
                "jsonrpc": "2.0", "id": id,
                "result": format!("0x{}", hex::encode(v.to_be_bytes::<32>()))
            }),
            None => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": 3, "message": "execution reverted" }
            }),
        }
    }

    async fn serve(chain: &FakeChain) -> String {
        let app = Router::new()
            .route("/", post(answer))
            .with_state(chain.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    fn contracts() -> HoldingsContracts {
        HoldingsContracts::for_network("mainnet").unwrap()
    }

    const BALANCE: [u8; 4] = BalanceOf::balanceOfCall::SELECTOR;
    const ESTATE: [u8; 4] = EstateSize::getLANDsSizeCall::SELECTOR;

    /// Both chains answering zero for every holding.
    async fn chains() -> (ChainHoldings, FakeChain, FakeChain) {
        let (l1, l2) = (FakeChain::default(), FakeChain::default());
        let c = contracts();
        for contract in [c.registrar, c.land, c.l1_mana] {
            l1.set(contract, BALANCE, U256::ZERO);
        }
        l1.set(c.estate, ESTATE, U256::ZERO);
        l2.set(c.l2_mana, BALANCE, U256::ZERO);
        let holdings = ChainHoldings::new(
            Outbound::for_tests(),
            &serve(&l1).await,
            &serve(&l2).await,
            c,
        );
        (holdings, l1, l2)
    }

    fn wallet() -> Address {
        Address::parse("0x1111111111111111111111111111111111111111").unwrap()
    }

    #[test]
    fn mana_unit_is_2000_mana() {
        assert_eq!(MANA_PER_UNIT, U256::from(2000u128 * WEI));
    }

    #[tokio::test]
    async fn the_allowance_counts_names_lands_estate_lands_and_mana_across_chains() {
        let (holdings, l1, l2) = chains().await;
        let c = contracts();
        l1.set(c.registrar, BALANCE, U256::from(2));
        l1.set(c.land, BALANCE, U256::from(3));
        l1.set(c.estate, ESTATE, U256::from(4));
        // 1500 + 2499.9... MANA is one 2000-MANA unit after the floor.
        l1.set(c.l1_mana, BALANCE, U256::from(1500 * WEI));
        l2.set(c.l2_mana, BALANCE, U256::from(2500 * WEI - 1));
        assert_eq!(
            holdings.allowance_bytes(&wallet()).await,
            Ok((2 + 3 + 4 + 1) * 100 * MIB)
        );
    }

    #[tokio::test]
    async fn mana_reaching_the_threshold_only_across_both_chains_counts() {
        let (holdings, l1, l2) = chains().await;
        let c = contracts();
        l1.set(c.l1_mana, BALANCE, U256::from(1000 * WEI));
        l2.set(c.l2_mana, BALANCE, U256::from(3000 * WEI));
        assert_eq!(holdings.allowance_bytes(&wallet()).await, Ok(2 * 100 * MIB));
    }

    #[tokio::test]
    async fn the_ethereum_reads_are_one_batch_at_latest_and_answers_are_reused() {
        let (holdings, l1, l2) = chains().await;
        assert_eq!(holdings.allowance_bytes(&wallet()).await, Ok(0));
        assert_eq!(holdings.allowance_bytes(&wallet()).await, Ok(0));
        assert_eq!((l1.requests(), l2.requests()), (1, 1));
        let bodies = l1.bodies.lock().unwrap();
        let batch = bodies[0].as_array().expect("a batch");
        let c = contracts();
        let targets: Vec<EvmAddress> = batch
            .iter()
            .map(|r| r["params"][0]["to"].as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(targets, vec![c.registrar, c.land, c.estate, c.l1_mana]);
        let owner = format!("{:0>64}", "1111111111111111111111111111111111111111");
        assert!(batch.iter().all(|r| r["method"] == "eth_call"
            && r["params"][0]["data"].as_str().unwrap().ends_with(&owner)));
        assert_eq!(
            l2.bodies.lock().unwrap()[0]["params"][0]["to"]
                .as_str()
                .unwrap()
                .parse::<EvmAddress>()
                .unwrap(),
            c.l2_mana
        );
    }

    #[tokio::test]
    async fn a_reverted_call_is_an_error_not_a_zero_holding() {
        let (holdings, l1, _) = chains().await;
        l1.values
            .lock()
            .unwrap()
            .remove(&(contracts().estate, ESTATE));
        let err = holdings.allowance_bytes(&wallet()).await.unwrap_err();
        assert!(err.contains("reverted"), "{err}");
        assert_eq!(l1.requests(), 1, "a revert is not retried");
        // The failure is answered from memory for a while.
        assert!(holdings.allowance_bytes(&wallet()).await.is_err());
        assert_eq!(l1.requests(), 1);
    }

    #[tokio::test]
    async fn an_rpc_fault_on_either_chain_is_retried_then_an_error() {
        let (holdings, _, l2) = chains().await;
        l2.faulty
            .lock()
            .unwrap()
            .push((contracts().l2_mana, BALANCE));
        assert!(holdings.allowance_bytes(&wallet()).await.is_err());
        assert_eq!(l2.requests(), ATTEMPTS as usize);
    }

    #[test]
    fn an_empty_answer_is_not_a_zero() {
        assert!(decode(Ok("0x".into())).is_err());
        assert_eq!(
            decode(Ok(format!("0x{:0>64}", "2a"))).ok(),
            Some(U256::from(42))
        );
    }

    #[test]
    fn a_huge_holding_saturates() {
        assert_eq!(
            allowance_bytes(U256::MAX, U256::ZERO, U256::ZERO, U256::MAX),
            i64::MAX
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lookup_waits_a_bounded_time_for_a_free_slot() {
        let (holdings, l1, _) = chains().await;
        let _all = holdings
            .in_flight
            .acquire_many(ALLOWANCE_MAX_IN_FLIGHT as u32)
            .await
            .unwrap();
        let err = holdings.allowance_bytes(&wallet()).await.unwrap_err();
        assert!(err.contains("in flight"), "{err}");
        assert_eq!(l1.requests(), 0);
    }

    #[tokio::test]
    async fn an_idle_lookup_never_queues_and_uses_its_own_slots() {
        let (holdings, l1, _) = chains().await;
        let publish_slots = holdings
            .in_flight
            .acquire_many(ALLOWANCE_MAX_IN_FLIGHT as u32)
            .await
            .unwrap();
        assert_eq!(holdings.allowance_bytes_if_idle(&wallet()).await, Ok(0));
        drop(publish_slots);
        let _idle = holdings
            .idle_in_flight
            .acquire_many(ALLOWANCE_IDLE_MAX_IN_FLIGHT as u32)
            .await
            .unwrap();
        let other = Address::parse("0x2222222222222222222222222222222222222222").unwrap();
        let err = holdings.allowance_bytes_if_idle(&other).await.unwrap_err();
        assert!(err.contains("in flight"), "{err}");
        assert_eq!(l1.requests(), 1);
        // A cached answer needs no slot; publishes keep theirs.
        assert_eq!(holdings.allowance_bytes_if_idle(&wallet()).await, Ok(0));
        assert_eq!(holdings.allowance_bytes(&other).await, Ok(0));
    }

    #[tokio::test]
    async fn missing_rpcs_or_network_leave_the_allowance_unlimited() {
        let http = Outbound::for_tests;
        let rpc = Some("https://rpc.example");
        assert!(!configured(http(), None, rpc, "mainnet").is_limited());
        assert!(!configured(http(), rpc, Some(" "), "mainnet").is_limited());
        assert!(!configured(http(), rpc, rpc, "goerli").is_limited());
        assert!(configured(http(), rpc, rpc, "sepolia").is_limited());
        assert!(configured(http(), rpc, rpc, "mainnet").is_limited());
    }
}
