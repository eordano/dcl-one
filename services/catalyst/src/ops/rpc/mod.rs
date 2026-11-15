//! rpc.decentraland.org: a method-filtered, read-only EVM JSON-RPC relay
//! (`POST /{network}` and the WebSocket upgrade on the same path, the
//! unity-explorer `DappWeb3Authenticator` path) in front of operator
//! configured upstreams. No table: the read-only method allowlist and the
//! per-network upstream map are seeded from `RPC_UPSTREAM_<NETWORK>` and
//! edited in memory by `/admin/rpc/*` behind `CATALYRST_RPC_ADMIN_TOKEN`
//! (timing-safe compared; unset fails closed with 403); a restart re-seeds
//! from the environment, as the old binary did. Every edit records one
//! `ops.admin_audit` row after the effect (process state, so the row is
//! best effort), with the upstream's host
//! only: a gateway key in a configured URL never reaches the table or a
//! log line.
//!
//! Upstream calls go through the ONE `http::Outbound` (DNS-pinned SSRF
//! guard) unless the operator opted a self-hosted stack in with
//! `CATALYRST_RPC_ALLOW_PRIVATE_UPSTREAM` (a compose service name such as
//! `http://geth:8545` has to stay reachable); then the pooled client sends
//! as is and only the host's own stack, the reserved ranges and the cloud
//! metadata endpoint are refused (`admin::blocks_local_upstream_only`), with
//! the host read the way the HTTP client reads it. The old vhost answered at `rpc.<domain>/`; the
//! router is mounted under `/rpc` and its `GET /` lists the configured
//! networks the way the old nginx `= /` location did.

pub mod admin;
pub mod relay;
pub mod routes;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use serde_json::Value;
use sqlx::PgPool;

/// The networks the old binary read an upstream for, with the variable
/// each came from; a network with no variable set is unsupported (never a
/// default that would relay to production).
pub const NETWORK_ENV: &[(&str, &str)] = &[
    ("mainnet", "RPC_UPSTREAM_MAINNET"),
    ("ethereum", "RPC_UPSTREAM_ETHEREUM"),
    ("sepolia", "RPC_UPSTREAM_SEPOLIA"),
    ("polygon", "RPC_UPSTREAM_POLYGON"),
    ("matic", "RPC_UPSTREAM_MATIC"),
    ("amoy", "RPC_UPSTREAM_AMOY"),
    ("mumbai", "RPC_UPSTREAM_MUMBAI"),
    ("arbitrum", "RPC_UPSTREAM_ARBITRUM"),
    ("optimism", "RPC_UPSTREAM_OPTIMISM"),
    ("avalanche", "RPC_UPSTREAM_AVALANCHE"),
    ("binance", "RPC_UPSTREAM_BINANCE"),
    ("fantom", "RPC_UPSTREAM_FANTOM"),
];

pub fn chain_id_for(network: &str) -> Option<u64> {
    match network.to_ascii_lowercase().as_str() {
        "mainnet" | "ethereum" => Some(1),
        "sepolia" => Some(11155111),
        "polygon" | "matic" => Some(137),
        "amoy" => Some(80002),
        "mumbai" => Some(80001),
        "arbitrum" => Some(42161),
        "optimism" => Some(10),
        "avalanche" => Some(43114),
        "binance" => Some(56),
        "fantom" => Some(250),
        _ => None,
    }
}

pub const READ_ONLY_METHODS: &[&str] = &[
    "eth_getTransactionReceipt",
    "eth_estimateGas",
    "eth_call",
    "eth_getBalance",
    "eth_getStorageAt",
    "eth_blockNumber",
    "eth_gasPrice",
    "eth_protocolVersion",
    "net_version",
    "web3_sha3",
    "web3_clientVersion",
    "eth_getTransactionCount",
    "eth_getBlockByNumber",
    "eth_getCode",
];

/// The read methods the relay may ever forward, by name...
const READ_METHODS: &[&str] = &[
    "eth_call",
    "eth_estimateGas",
    "eth_chainId",
    "eth_blockNumber",
    "eth_gasPrice",
    "eth_feeHistory",
    "eth_maxPriorityFeePerGas",
    "eth_syncing",
    "eth_protocolVersion",
];

/// ...and by namespace (`eth_getBalance`, `net_version`, `web3_sha3`).
const READ_PREFIXES: &[&str] = &["eth_get", "net_", "web3_"];

/// Whether `method` only reads. The relay is read-only by design: neither
/// the seed nor `/admin/rpc/methods` may admit anything else (a send, a
/// signature, a filter or subscription the upstream would keep, a node
/// reconfiguration), and a request for it is refused even if it got in.
pub fn is_read_method(method: &str) -> bool {
    READ_METHODS.contains(&method)
        || READ_PREFIXES
            .iter()
            .any(|prefix| method.len() > prefix.len() && method.starts_with(prefix))
}

/// The upstream request budget of the old client.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

const MEMO_MAX_ENTRIES: usize = 256;

/// Short-lived memo of successful responses for param-less methods.
#[derive(Default)]
pub struct RpcMemo {
    entries: Mutex<HashMap<String, (Instant, Value)>>,
}

impl RpcMemo {
    pub fn ttl_for(method: &str) -> Option<Duration> {
        match method {
            "eth_blockNumber" => Some(Duration::from_secs(1)),
            "net_version" | "web3_clientVersion" => Some(Duration::from_secs(2)),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        let map = self.entries.lock().expect("memo lock poisoned");
        let (until, body) = map.get(key)?;
        (*until > Instant::now()).then(|| body.clone())
    }

    pub fn put(&self, key: String, ttl: Duration, body: &Value) {
        if body.get("result").is_none() {
            return;
        }
        let mut map = self.entries.lock().expect("memo lock poisoned");
        let now = Instant::now();
        if map.len() >= MEMO_MAX_ENTRIES {
            map.retain(|_, (until, _)| *until > now);
            if map.len() >= MEMO_MAX_ENTRIES {
                map.clear();
            }
        }
        map.insert(key, (now + ttl, body.clone()));
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// network (lowercase) -> upstream URL, the seed of the editable map.
    pub upstreams: BTreeMap<String, String>,
    /// `CATALYRST_RPC_ADMIN_TOKEN`; unset: every `/admin/rpc/*` answers 403.
    pub admin_token: Option<String>,
    /// `CATALYRST_RPC_ALLOW_PRIVATE_UPSTREAM`: self-hosted upstreams by
    /// internal name; the narrow local deny-list still applies.
    pub allow_private_upstream: bool,
    /// What `GET /` lists the networks under (`https://rpc.<domain>` or
    /// `<PUBLIC_URL>/rpc`), trailing slash stripped.
    pub public_base: String,
}

impl Settings {
    /// `pairs` are `(network, value)` for the twelve `NETWORK_ENV` entries
    /// (a blank value is unset). Every configured URL must be http(s); a
    /// private, internal or single-label host is refused at startup naming
    /// the variable (never the URL) unless `allow_private_upstream`, which
    /// still refuses the host's own stack and the metadata endpoint.
    pub fn parse<'a>(
        pairs: impl IntoIterator<Item = (&'a str, Option<String>)>,
        admin_token: Option<String>,
        allow_private_upstream: bool,
        public_base: &str,
    ) -> anyhow::Result<Self> {
        let mut upstreams = BTreeMap::new();
        for (network, value) in pairs {
            let Some(url) = value.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
                continue;
            };
            let var = NETWORK_ENV
                .iter()
                .find(|(n, _)| *n == network)
                .map(|(_, v)| *v)
                .unwrap_or("RPC_UPSTREAM_<NETWORK>");
            if chain_id_for(network).is_none() {
                anyhow::bail!("{var}: {network} is not a network this relay knows");
            }
            if let Some(reason) = admin::upstream_refusal(url, allow_private_upstream) {
                anyhow::bail!("{var}: {reason}");
            }
            upstreams.insert(network.to_ascii_lowercase(), url.to_string());
        }
        Ok(Self {
            upstreams,
            admin_token: admin_token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            allow_private_upstream,
            public_base: public_base.trim().trim_end_matches('/').to_string(),
        })
    }

    pub fn for_tests(networks: &[(&str, &str)]) -> Self {
        Self {
            upstreams: networks
                .iter()
                .map(|(n, u)| (n.to_string(), u.to_string()))
                .collect(),
            admin_token: None,
            allow_private_upstream: true,
            public_base: "http://localhost/rpc".into(),
        }
    }
}

pub struct State {
    pub settings: Settings,
    pub http: crate::http::Outbound,
    pub pool: PgPool,
    pub allowed_methods: RwLock<BTreeSet<String>>,
    pub upstreams: RwLock<BTreeMap<String, String>>,
    pub memo: RpcMemo,
}

pub type Shared = Arc<State>;

impl State {
    pub fn new(settings: Settings, http: crate::http::Outbound, pool: PgPool) -> Shared {
        if settings.admin_token.is_none() {
            tracing::warn!(
                "CATALYRST_RPC_ADMIN_TOKEN unset; /admin/rpc/* routes fail closed (403)"
            );
        }
        for network in settings.upstreams.keys() {
            if chain_id_for(network).is_none() {
                tracing::warn!(%network, "no known chain id for configured network");
            }
        }
        tracing::info!(
            networks = ?settings.upstreams.keys().collect::<Vec<_>>(),
            methods = READ_ONLY_METHODS.len(),
            private_upstreams = settings.allow_private_upstream,
            "rpc relay"
        );
        Arc::new(Self {
            allowed_methods: RwLock::new(READ_ONLY_METHODS.iter().map(|m| m.to_string()).collect()),
            upstreams: RwLock::new(settings.upstreams.clone()),
            settings,
            http,
            pool,
            memo: RpcMemo::default(),
        })
    }

    pub fn methods_snapshot(&self) -> Vec<String> {
        self.allowed_methods
            .read()
            .expect("allowed_methods lock poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn is_method_allowed(&self, method: &str) -> bool {
        is_read_method(method)
            && self
                .allowed_methods
                .read()
                .expect("allowed_methods lock poisoned")
                .contains(method)
    }

    pub fn upstreams_snapshot(&self) -> BTreeMap<String, String> {
        self.upstreams
            .read()
            .expect("upstreams lock poisoned")
            .clone()
    }

    pub fn upstream_for(&self, network: &str) -> Option<String> {
        self.upstreams
            .read()
            .expect("upstreams lock poisoned")
            .get(&network.to_ascii_lowercase())
            .cloned()
    }
}

/// `GET /` (the networks index), `GET /health`, `/admin/rpc/*` and the
/// `POST` + WebSocket relay at `/{network}`; mounted under `/rpc` (a
/// per-vhost proxy forwards `rpc.<domain>/x` there).
pub fn router(state: Shared) -> Router {
    routes::routes().merge(admin::routes()).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPPORTED: &[(&str, u64)] = &[
        ("mainnet", 1),
        ("ethereum", 1),
        ("sepolia", 11155111),
        ("polygon", 137),
        ("matic", 137),
        ("amoy", 80002),
        ("mumbai", 80001),
        ("arbitrum", 42161),
        ("optimism", 10),
        ("avalanche", 43114),
        ("binance", 56),
        ("fantom", 250),
    ];

    #[test]
    fn chain_id_covers_full_client_network_set() {
        for (net, want) in SUPPORTED {
            assert_eq!(
                chain_id_for(net),
                Some(*want),
                "network {net} should map to chain id {want}"
            );
        }
        assert_eq!(NETWORK_ENV.len(), SUPPORTED.len());
        for (net, var) in NETWORK_ENV {
            assert!(SUPPORTED.iter().any(|(n, _)| n == net), "{net}");
            assert_eq!(*var, format!("RPC_UPSTREAM_{}", net.to_uppercase()));
        }
    }

    #[test]
    fn chain_id_is_case_insensitive() {
        assert_eq!(chain_id_for("MAINNET"), Some(1));
        assert_eq!(chain_id_for("Polygon"), Some(137));
        assert_eq!(chain_id_for("MATIC"), Some(137));
    }

    #[test]
    fn unknown_network_has_no_chain_id() {
        assert_eq!(chain_id_for("solana"), None);
        assert_eq!(chain_id_for("goerli"), None);
        assert_eq!(chain_id_for(""), None);
        assert_eq!(chain_id_for("mainet"), None);
    }

    #[test]
    fn upstreams_come_only_from_configuration() {
        let bare = Settings::parse(
            NETWORK_ENV.iter().map(|(n, _)| (*n, None)),
            None,
            false,
            "https://rpc.example",
        )
        .unwrap();
        for (net, _) in SUPPORTED {
            assert!(
                !bare.upstreams.contains_key(*net),
                "{net} must have no upstream until one is configured; a default here would relay to production"
            );
        }
        let configured = Settings::parse(
            [
                (
                    "mainnet",
                    Some("https://rpc.example.com/v3/KEY ".to_string()),
                ),
                ("polygon", Some("  ".to_string())),
            ],
            Some(" t ".into()),
            false,
            "https://rpc.example/",
        )
        .unwrap();
        assert_eq!(
            configured.upstreams.get("mainnet").map(String::as_str),
            Some("https://rpc.example.com/v3/KEY")
        );
        assert!(!configured.upstreams.contains_key("polygon"));
        assert_eq!(configured.admin_token.as_deref(), Some("t"));
        assert_eq!(configured.public_base, "https://rpc.example");
    }

    #[test]
    fn a_private_upstream_is_refused_at_startup_naming_the_variable_only() {
        let err = Settings::parse(
            [("mainnet", Some("http://127.0.0.1:8545/SECRET".to_string()))],
            None,
            false,
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("RPC_UPSTREAM_MAINNET"), "{err}");
        assert!(!err.contains("SECRET"), "{err}");
        let err = Settings::parse(
            [("mainnet", Some("ftp://rpc.example".to_string()))],
            None,
            false,
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("http(s)"), "{err}");
        let err = Settings::parse(
            [("solana", Some("https://rpc.example".to_string()))],
            None,
            false,
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("solana"), "{err}");
        let opted_in = Settings::parse(
            [("mainnet", Some("http://geth:8545".to_string()))],
            None,
            true,
            "",
        )
        .unwrap();
        assert_eq!(
            opted_in.upstreams.get("mainnet").map(String::as_str),
            Some("http://geth:8545")
        );
        assert!(
            Settings::parse(
                [("mainnet", Some("http://localhost:8545".to_string()))],
                None,
                true,
                ""
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn upstream_lookup_is_case_insensitive_and_rejects_unknown() {
        let state = State::new(
            Settings::for_tests(&[("polygon", "https://example/polygon")]),
            crate::http::Outbound::for_tests(),
            lazy_pool(),
        );
        assert_eq!(
            state.upstream_for("POLYGON").as_deref(),
            Some("https://example/polygon")
        );
        assert_eq!(state.upstream_for("does-not-exist"), None);
        assert!(state.is_method_allowed("eth_call"));
        assert!(!state.is_method_allowed("eth_sendRawTransaction"));
    }

    /// Every method of the default list is a read the relay forwards; what
    /// sends, signs, keeps upstream state or reconfigures the node is not.
    #[tokio::test]
    async fn the_default_list_is_all_reads_and_nothing_else_is_admitted() {
        let state = State::new(
            Settings::for_tests(&[("polygon", "https://example/polygon")]),
            crate::http::Outbound::for_tests(),
            lazy_pool(),
        );
        for method in READ_ONLY_METHODS {
            assert!(is_read_method(method), "{method}");
            assert!(state.is_method_allowed(method), "{method}");
        }
        for method in [
            "eth_sendRawTransaction",
            "eth_sendTransaction",
            "eth_sign",
            "eth_signTypedData_v4",
            "eth_submitWork",
            "eth_newFilter",
            "eth_subscribe",
            "eth_uninstallFilter",
            "eth_accounts",
            "eth_get",
            "ETH_GETBALANCE",
            "personal_unlockAccount",
            "admin_addPeer",
            "debug_setHead",
            "txpool_content",
            "anvil_setBalance",
            "hardhat_impersonateAccount",
        ] {
            assert!(!is_read_method(method), "{method}");
            state
                .allowed_methods
                .write()
                .unwrap()
                .insert(method.to_string());
            assert!(!state.is_method_allowed(method), "even listed: {method}");
        }
    }

    pub(super) fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
            .unwrap()
    }
}
