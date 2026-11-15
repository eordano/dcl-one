//! The ENS owner of a `*.eth` world (`ALLOW_ENS_DOMAINS`), read over
//! `ETH_RPC_URL` as the worlds content server reads it: a second-level name
//! is the BaseRegistrar's `ownerOf(labelhash)`, unless that owner is the
//! NameWrapper, whose `ownerOf(namehash)` then answers, as it does for every
//! deeper name. A wrapped name counts only while the ENS Registry's
//! `owner(namehash)` is still the NameWrapper (a parent can reclaim an
//! unwrapped parent's subname). A reverted call or the zero address is
//! "nobody holds it"; an
//! RPC fault is an error, never an owner. Answers are memoized per name for
//! [`ENS_OWNER_TTL`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address as EvmAddress, B256, U256, address, keccak256};
use alloy_sol_types::{SolCall, sol};
use serde_json::{Value, json};

use crate::comms::world_identity::WorldName;
use crate::http::Outbound;
use crate::http::jsonrpc::{Rpc, RpcError, hex_of, request, result_of};
use crate::shared::cache::TtlMap;
use crate::shared::identity::Address;

sol! {
    interface OwnerOf {
        function ownerOf(uint256 id) external view returns (address);
    }
    interface RegistryOwner {
        function owner(bytes32 node) external view returns (address);
    }
}

/// The upstream's one-minute name-ownership memo.
pub const ENS_OWNER_TTL: Duration = Duration::from_secs(60);
const CACHE_MAX_ENTRIES: usize = 20_000;
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// `eth_call`s sent in one JSON-RPC batch.
const CALLS_PER_BATCH: usize = 100;

/// The ENS contracts of one network (the upstream's `ensContracts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnsContracts {
    pub registry: EvmAddress,
    pub base_registrar: EvmAddress,
    pub name_wrapper: EvmAddress,
}

impl EnsContracts {
    /// `ETH_NETWORK`'s contracts; `None` for a network ENS is not read on.
    pub fn for_network(network: &str) -> Option<Self> {
        let registry = address!("00000000000C2E074eC69A0dFb2997BA6C7d2e1e");
        let base_registrar = address!("57f1887a8BF19b14fC0dF6Fd9B2acc9Af147eA85");
        match network {
            "mainnet" => Some(Self {
                registry,
                base_registrar,
                name_wrapper: address!("D4416b13d2b3a9aBae7AcD5D6C2BbDBE25686401"),
            }),
            "sepolia" => Some(Self {
                registry,
                base_registrar,
                name_wrapper: address!("0635513f179D50A207757E05759CbD106d7dFcE8"),
            }),
            _ => None,
        }
    }
}

/// ENSIP-1 namehash of an already normalized name.
pub fn namehash(name: &str) -> B256 {
    if name.is_empty() {
        return B256::ZERO;
    }
    name.rsplit('.').fold(B256::ZERO, |node, label| {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(node.as_slice());
        buf[32..].copy_from_slice(keccak256(label.as_bytes()).as_slice());
        keccak256(buf)
    })
}

/// Names this reader resolves: those the upstream's UTS46 normalization
/// (`toUnicode`, STD3 rules) keeps byte for byte. Anything it would rewrite or
/// reject (`xn--` and other `??--` labels, `_`, an edge `-`, non-ASCII) has no
/// owner here.
pub fn resolvable(name: &str) -> bool {
    name.split('.').all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
            && bytes.get(2..4) != Some(b"--".as_slice())
    })
}

pub struct EnsResolver {
    http: Outbound,
    rpc_url: String,
    contracts: EnsContracts,
    cache: TtlMap<String, Arc<Option<Address>>>,
}

impl EnsResolver {
    pub fn new(http: Outbound, rpc_url: &str, contracts: EnsContracts) -> Self {
        Self {
            http,
            rpc_url: rpc_url.to_string(),
            contracts,
            cache: TtlMap::bounded(ENS_OWNER_TTL, CACHE_MAX_ENTRIES),
        }
    }

    /// The resolver `ALLOW_ENS_DOMAINS=true` starts with; refuses a missing
    /// `ETH_RPC_URL` or an `ETH_NETWORK` without ENS contracts.
    pub fn configured(
        http: Outbound,
        rpc_url: Option<&str>,
        network: &str,
    ) -> anyhow::Result<Self> {
        let rpc_url = rpc_url.filter(|u| !u.is_empty()).ok_or_else(|| {
            anyhow::anyhow!("ALLOW_ENS_DOMAINS=true needs ETH_RPC_URL to read ENS owners")
        })?;
        let contracts = EnsContracts::for_network(network).ok_or_else(|| {
            anyhow::anyhow!("ALLOW_ENS_DOMAINS=true: no ENS contracts for ETH_NETWORK={network}")
        })?;
        Ok(Self::new(http, rpc_url, contracts))
    }

    /// Drops every memoized answer (tests).
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Records `owner` as the holder another authority answer just proved.
    pub fn note_owner(&self, world: &WorldName, owner: Option<&Address>) {
        if world.is_ens_world() {
            self.cache
                .insert(world.key().to_string(), Arc::new(owner.cloned()));
        }
    }

    /// The holder of the ENS world; `Ok(None)` when nobody holds it.
    pub async fn owner(&self, world: &WorldName) -> Result<Option<Address>, String> {
        let name = world.key().to_string();
        if !world.is_ens_world() || !resolvable(&name) {
            return Ok(None);
        }
        self.cache
            .get_or_fetch(name.clone(), || async move {
                let mut owners = self.owners_uncached(std::slice::from_ref(&name)).await;
                owners.remove(&name).unwrap_or(Ok(None)).map(Arc::new)
            })
            .await
            .map(|owner| (*owner).clone())
    }

    /// The holders of many ENS names in batched calls, bypassing the memo
    /// (the background reconcile). A name it cannot resolve maps to `Ok(None)`.
    pub async fn owners_uncached(
        &self,
        names: &[String],
    ) -> HashMap<String, Result<Option<Address>, String>> {
        let mut out = HashMap::new();
        let (mut second_level, mut wrapped) = (Vec::new(), Vec::new());
        for name in names {
            let ens = WorldName::parse(name).is_ok_and(|w| w.is_ens_world() && w.key() == name);
            if !ens || !resolvable(name) {
                out.insert(name.clone(), Ok(None));
            } else if name.matches('.').count() == 1 {
                second_level.push(name.clone());
            } else {
                wrapped.push(name.clone());
            }
        }
        let calls: Vec<Call> = second_level
            .iter()
            .map(|name| {
                let label = name.split('.').next().unwrap_or_default();
                let id = U256::from_be_bytes(keccak256(label.as_bytes()).0);
                owner_of_call(self.contracts.base_registrar, id)
            })
            .collect();
        for (name, owner) in second_level.into_iter().zip(self.owners_of(&calls).await) {
            match owner {
                Ok(Some(owner)) if owner == self.contracts.name_wrapper => wrapped.push(name),
                owner => {
                    out.insert(name, owner.map(|o| o.map(to_address)));
                }
            }
        }
        // The NameWrapper answers only for a node the Registry still gives it.
        let calls: Vec<Call> = wrapped
            .iter()
            .map(|name| registry_owner_call(self.contracts.registry, namehash(name)))
            .collect();
        let mut held = Vec::new();
        for (name, owner) in wrapped.into_iter().zip(self.owners_of(&calls).await) {
            match owner {
                Ok(Some(owner)) if owner == self.contracts.name_wrapper => held.push(name),
                Ok(_) => {
                    out.insert(name, Ok(None));
                }
                Err(e) => {
                    out.insert(name, Err(e));
                }
            }
        }
        let calls: Vec<Call> = held
            .iter()
            .map(|name| {
                let id = U256::from_be_bytes(namehash(name).0);
                owner_of_call(self.contracts.name_wrapper, id)
            })
            .collect();
        for (name, owner) in held.into_iter().zip(self.owners_of(&calls).await) {
            out.insert(name, owner.map(|o| o.map(to_address)));
        }
        out
    }

    /// Each address-returning `(contract, calldata)` in JSON-RPC batches of
    /// [`CALLS_PER_BATCH`] (one call goes alone), in `calls`' order.
    async fn owners_of(&self, calls: &[Call]) -> Vec<Result<Option<EvmAddress>, String>> {
        if let [call] = calls {
            return vec![self.owner_of(call).await];
        }
        let rpc = Rpc::new(&self.http, &self.rpc_url).with_timeout(RPC_TIMEOUT);
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(CALLS_PER_BATCH) {
            let requests: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, call)| request(i as u64, "eth_call", call_params(call)))
                .collect();
            match rpc.batch(&requests).await {
                Err(e) => out.extend(chunk.iter().map(|_| Err(format!("ENS ownerOf: {e}")))),
                // An endpoint without batches: one call each.
                Ok(items) if items.is_empty() => {
                    for call in chunk {
                        out.push(self.owner_of(call).await);
                    }
                }
                Ok(items) => {
                    let mut by_id: HashMap<u64, Value> = items
                        .into_iter()
                        .filter_map(|v| Some((v.get("id")?.as_u64()?, v)))
                        .collect();
                    out.extend((0..chunk.len()).map(|i| match by_id.remove(&(i as u64)) {
                        Some(envelope) => decode(result_of(envelope).and_then(hex_of)),
                        None => Err("ENS ownerOf: no answer in the batch".to_string()),
                    }));
                }
            }
        }
        out
    }

    /// One address-returning call at the head; `None` when reverted or zero.
    async fn owner_of(&self, call: &Call) -> Result<Option<EvmAddress>, String> {
        decode(
            Rpc::new(&self.http, &self.rpc_url)
                .with_timeout(RPC_TIMEOUT)
                .call_hex("eth_call", call_params(call))
                .await,
        )
    }
}

/// A contract and the calldata of a view returning one address.
pub(crate) type Call = (EvmAddress, Vec<u8>);

fn owner_of_call(contract: EvmAddress, id: U256) -> Call {
    (contract, OwnerOf::ownerOfCall { id }.abi_encode())
}

fn registry_owner_call(registry: EvmAddress, node: B256) -> Call {
    (registry, RegistryOwner::ownerCall { node }.abi_encode())
}

pub(crate) fn call_params((contract, data): &Call) -> Value {
    json!([
        { "to": format!("{contract:#x}"), "data": format!("0x{}", hex::encode(data)) },
        "latest"
    ])
}

/// An `ownerOf` answer: `None` when reverted, empty or zero.
fn decode(answer: Result<String, RpcError>) -> Result<Option<EvmAddress>, String> {
    let hex = match answer {
        Ok(hex) => hex,
        Err(e) if e.is_revert() => return Ok(None),
        Err(e) => return Err(format!("ENS ownerOf: {e}")),
    };
    let bytes = hex::decode(hex.trim_start_matches("0x"))
        .map_err(|e| format!("ENS ownerOf: bad hex: {e}"))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let owner = OwnerOf::ownerOfCall::abi_decode_returns(&bytes)
        .map_err(|e| format!("ENS ownerOf: bad answer: {e}"))?;
    Ok(Some(owner).filter(|o| !o.is_zero()))
}

fn to_address(owner: EvmAddress) -> Address {
    Address::parse(&format!("{owner:#x}")).expect("a 20-byte address is a valid address")
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address as EvmAddress, U256, keccak256};
    use alloy_sol_types::SolCall;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::{EnsContracts, EnsResolver, OwnerOf, RegistryOwner, namehash};
    use crate::http::Outbound;
    use crate::shared::identity::Address;

    /// The ENS registry the fake chain answers from.
    #[derive(Clone, Default)]
    pub struct FakeEns {
        /// `(contract, id)` -> owner; a missing id reverts.
        owners: Arc<Mutex<HashMap<(EvmAddress, U256), EvmAddress>>>,
        pub down: Arc<AtomicBool>,
        /// Every request is held unanswered.
        pub hang: Arc<AtomicBool>,
        /// HTTP requests received (a batch is one).
        pub requests: Arc<AtomicUsize>,
        pub calls: Arc<Mutex<Vec<(EvmAddress, U256)>>>,
        /// A batch is answered with one error object, not an array.
        pub no_batches: Arc<AtomicBool>,
        /// A batch answer leaves out its last envelope.
        pub drop_last: Arc<AtomicBool>,
    }

    pub fn contracts() -> EnsContracts {
        EnsContracts::for_network("mainnet").unwrap()
    }

    fn evm(a: &Address) -> EvmAddress {
        a.as_str().parse().unwrap()
    }

    impl FakeEns {
        /// A second-level name registered directly to `owner`.
        pub fn register(&self, name: &str, owner: &Address) {
            let label = name.split('.').next().unwrap();
            let id = U256::from_be_bytes(keccak256(label.as_bytes()).0);
            self.owners
                .lock()
                .unwrap()
                .insert((contracts().base_registrar, id), evm(owner));
        }

        /// A name held through the NameWrapper (the registrar and the
        /// Registry name the wrapper).
        pub fn wrap(&self, name: &str, owner: &Address) {
            let c = contracts();
            let labels: Vec<&str> = name.split('.').collect();
            let mut owners = self.owners.lock().unwrap();
            if labels.len() == 2 {
                let id = U256::from_be_bytes(keccak256(labels[0].as_bytes()).0);
                owners.insert((c.base_registrar, id), c.name_wrapper);
            }
            let id = U256::from_be_bytes(namehash(name).0);
            owners.insert((c.registry, id), c.name_wrapper);
            owners.insert((c.name_wrapper, id), evm(owner));
        }

        /// The Registry gives `name`'s node to `owner` (a parent's
        /// `setSubnodeOwner`); the NameWrapper's record is left stale.
        pub fn reclaim(&self, name: &str, owner: &Address) {
            let id = U256::from_be_bytes(namehash(name).0);
            self.owners
                .lock()
                .unwrap()
                .insert((contracts().registry, id), evm(owner));
        }

        pub fn set_down(&self, down: bool) {
            self.down.store(down, Ordering::Relaxed);
        }
    }

    async fn answer(
        State(ens): State<FakeEns>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        ens.requests.fetch_add(1, Ordering::Relaxed);
        if ens.hang.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
        if ens.down.load(Ordering::Relaxed) {
            return (axum::http::StatusCode::BAD_GATEWAY, "down").into_response();
        }
        match body {
            Value::Array(_) if ens.no_batches.load(Ordering::Relaxed) => Json(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32600, "message": "batch requests are not supported" }
            }))
            .into_response(),
            Value::Array(calls) => {
                let mut out: Vec<Value> = calls.iter().map(|call| answer_one(&ens, call)).collect();
                if ens.drop_last.load(Ordering::Relaxed) {
                    out.pop();
                }
                Json(Value::Array(out)).into_response()
            }
            call => Json(answer_one(&ens, &call)).into_response(),
        }
    }

    fn answer_one(ens: &FakeEns, body: &Value) -> Value {
        let id = body["id"].clone();
        let call = &body["params"][0];
        let to: EvmAddress = call["to"].as_str().unwrap().parse().unwrap();
        let data = hex::decode(call["data"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let token = if data[..4] == OwnerOf::ownerOfCall::SELECTOR {
            OwnerOf::ownerOfCall::abi_decode(&data).unwrap().id
        } else {
            U256::from_be_bytes(RegistryOwner::ownerCall::abi_decode(&data).unwrap().node.0)
        };
        ens.calls.lock().unwrap().push((to, token));
        let owner = ens.owners.lock().unwrap().get(&(to, token)).copied();
        match owner {
            Some(owner) => json!({
                "jsonrpc": "2.0", "id": id,
                "result": format!("0x{}", hex::encode(OwnerOf::ownerOfCall::abi_encode_returns(&owner)))
            }),
            None => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": 3, "message": "execution reverted: ERC721: invalid token ID" }
            }),
        }
    }

    /// A resolver over a loopback fake chain, plus the fake's registry.
    pub async fn resolver() -> (Arc<EnsResolver>, FakeEns) {
        let ens = FakeEns::default();
        let app = Router::new()
            .route("/", post(answer))
            .with_state(ens.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("http://{addr}/");
        (
            Arc::new(EnsResolver::new(Outbound::for_tests(), &url, contracts())),
            ens,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn wallet() -> Address {
        Address::parse(&format!("0x00000000{}", uuid::Uuid::new_v4().simple())).unwrap()
    }

    fn world(name: &str) -> WorldName {
        WorldName::parse(name).unwrap()
    }

    #[test]
    fn namehash_matches_the_ensip1_vectors() {
        assert_eq!(namehash(""), B256::ZERO);
        assert_eq!(
            format!("{:#x}", namehash("eth")),
            "0x93cdeb708b7545dc668eb9280176169d1c33cfd8ed6f04690a0bcc88a93fc4ae"
        );
        assert_eq!(
            format!("{:#x}", namehash("foo.eth")),
            "0xde9b09fd7c5f901e23a3f19fecc54828e9c848539801e86591bd9801b019f84f"
        );
    }

    #[tokio::test]
    async fn a_second_level_name_is_the_registrars_and_a_wrapped_one_the_wrappers() {
        let (r, ens) = resolver().await;
        let (direct, wrapped, sub) = (wallet(), wallet(), wallet());
        ens.register("direct.eth", &direct);
        ens.wrap("wrapped.eth", &wrapped);
        ens.wrap("a.wrapped.eth", &sub);
        assert_eq!(r.owner(&world("Direct.eth")).await.unwrap(), Some(direct));
        assert_eq!(r.owner(&world("wrapped.eth")).await.unwrap(), Some(wrapped));
        assert_eq!(r.owner(&world("a.wrapped.eth")).await.unwrap(), Some(sub));
        assert_eq!(
            r.owner(&world("nobody.eth")).await.unwrap(),
            None,
            "reverted"
        );
        assert_eq!(r.owner(&world("x.nobody.eth")).await.unwrap(), None);
        for unresolved in ["foo.dcl.eth", "foo", "café.eth", "a!b.eth"] {
            assert_eq!(
                r.owner(&world(unresolved)).await.unwrap(),
                None,
                "{unresolved}"
            );
        }
    }

    #[tokio::test]
    async fn an_rpc_fault_is_an_error_and_answers_are_memoized() {
        let (r, ens) = resolver().await;
        let owner = wallet();
        ens.register("memo.eth", &owner);
        assert_eq!(
            r.owner(&world("memo.eth")).await.unwrap(),
            Some(owner.clone())
        );
        ens.set_down(true);
        assert_eq!(
            r.owner(&world("memo.eth")).await.unwrap(),
            Some(owner),
            "within the TTL"
        );
        assert!(r.owner(&world("other.eth")).await.is_err());
        r.clear_cache();
        assert!(r.owner(&world("memo.eth")).await.is_err());
    }

    /// A label the upstream's UTS46 normalization would rewrite or refuse has
    /// no owner here, even when its literal hash is registered.
    #[tokio::test]
    async fn a_label_normalization_would_change_has_no_owner() {
        let (r, ens) = resolver().await;
        let squatter = wallet();
        for name in [
            "xn--caf-dma.eth",
            "a_b.eth",
            "-ab.eth",
            "ab-.eth",
            "ab--c.eth",
            "x.xn--p1ai.eth",
        ] {
            ens.register(name, &squatter);
            ens.wrap(name, &squatter);
            assert!(!resolvable(name), "{name}");
            assert_eq!(r.owner(&world(name)).await.unwrap(), None, "{name}");
        }
        assert!(ens.calls.lock().unwrap().is_empty());
        for fine in ["abc.eth", "a-b.eth", "a--b.eth", "x.y-z.eth", "123.eth"] {
            assert!(resolvable(fine), "{fine}");
        }
    }

    #[tokio::test]
    async fn owners_uncached_batches_and_leaves_the_memo_alone() {
        let (r, ens) = resolver().await;
        let (direct, wrapped, sub) = (wallet(), wallet(), wallet());
        ens.register("bdirect.eth", &direct);
        ens.wrap("bwrapped.eth", &wrapped);
        ens.wrap("s.bwrapped.eth", &sub);
        let names: Vec<String> = [
            "bdirect.eth",
            "bwrapped.eth",
            "s.bwrapped.eth",
            "bnobody.eth",
            "xn--caf-dma.eth",
        ]
        .map(String::from)
        .to_vec();
        let owners = r.owners_uncached(&names).await;
        assert_eq!(owners["bdirect.eth"], Ok(Some(direct)));
        assert_eq!(owners["bwrapped.eth"], Ok(Some(wrapped)));
        assert_eq!(owners["s.bwrapped.eth"], Ok(Some(sub)));
        assert_eq!(owners["bnobody.eth"], Ok(None));
        assert_eq!(owners["xn--caf-dma.eth"], Ok(None));
        assert_eq!(
            ens.requests.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "one registrar batch, one registry batch, then one wrapper batch"
        );
        ens.set_down(true);
        assert!(
            r.owner(&world("bdirect.eth")).await.is_err(),
            "nothing memoized"
        );
        let owners = r.owners_uncached(&names[..2]).await;
        assert!(owners.values().all(Result::is_err));
    }

    #[tokio::test]
    async fn a_reclaimed_subname_has_no_owner() {
        let (r, ens) = resolver().await;
        let (wrapped, parent) = (wallet(), wallet());
        ens.wrap("sub.reclaim.eth", &wrapped);
        ens.wrap("kept.reclaim.eth", &wrapped);
        ens.wrap("rewrapped.eth", &wrapped);
        ens.reclaim("sub.reclaim.eth", &parent);
        ens.reclaim("rewrapped.eth", &parent);
        assert_eq!(r.owner(&world("sub.reclaim.eth")).await.unwrap(), None);
        assert_eq!(r.owner(&world("rewrapped.eth")).await.unwrap(), None);
        assert_eq!(
            r.owner(&world("kept.reclaim.eth")).await.unwrap(),
            Some(wrapped.clone())
        );
        let names = ["sub.reclaim.eth", "kept.reclaim.eth"].map(String::from);
        let owners = r.owners_uncached(&names).await;
        assert_eq!(owners["sub.reclaim.eth"], Ok(None));
        assert_eq!(owners["kept.reclaim.eth"], Ok(Some(wrapped)));
        ens.set_down(true);
        r.clear_cache();
        assert!(r.owner(&world("kept.reclaim.eth")).await.is_err());
    }

    #[tokio::test]
    async fn an_endpoint_without_batches_is_asked_one_call_at_a_time() {
        let (r, ens) = resolver().await;
        let (a, b) = (wallet(), wallet());
        ens.register("nb1.eth", &a);
        ens.wrap("x.nb2.eth", &b);
        ens.wrap("y.nb2.eth", &b);
        ens.no_batches
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let names = ["nb1.eth", "nb3.eth", "x.nb2.eth", "y.nb2.eth"].map(String::from);
        let owners = r.owners_uncached(&names).await;
        assert_eq!(owners["nb1.eth"], Ok(Some(a)));
        assert_eq!(owners["nb3.eth"], Ok(None));
        assert_eq!(owners["x.nb2.eth"], Ok(Some(b.clone())));
        assert_eq!(owners["y.nb2.eth"], Ok(Some(b)));
    }

    #[tokio::test]
    async fn a_name_missing_from_the_batch_answer_is_an_error() {
        let (r, ens) = resolver().await;
        let (a, b) = (wallet(), wallet());
        ens.register("miss1.eth", &a);
        ens.register("miss2.eth", &b);
        ens.drop_last
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let names = ["miss1.eth", "miss2.eth"].map(String::from);
        let owners = r.owners_uncached(&names).await;
        assert_eq!(owners["miss1.eth"], Ok(Some(a)));
        assert!(owners["miss2.eth"].is_err());
    }

    #[test]
    fn ens_worlds_need_an_rpc_url_and_a_known_network() {
        let http = Outbound::for_tests;
        let err = |r: anyhow::Result<EnsResolver>| r.err().unwrap().to_string();
        assert!(err(EnsResolver::configured(http(), None, "mainnet")).contains("ETH_RPC_URL"));
        assert!(err(EnsResolver::configured(http(), Some(""), "mainnet")).contains("ETH_RPC_URL"));
        assert!(
            err(EnsResolver::configured(
                http(),
                Some("http://rpc.example"),
                "polygon"
            ))
            .contains("ETH_NETWORK=polygon")
        );
        for network in ["mainnet", "sepolia"] {
            assert!(EnsResolver::configured(http(), Some("http://rpc.example"), network).is_ok());
        }
    }
}
