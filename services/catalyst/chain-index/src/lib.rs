//! The chain-derived readers every domain shares: the collections,
//! marketplace and land-manager subgraph clients (`graph`, `land_graph`),
//! the LAND registries per network (`registries`), Ethereum RPC logs and
//! headers (`rpc`), and the unavailable
//! envelope naming a missing source (`source`). [`ChainIndex`] holds the
//! ONE outbound client, the subgraph URLs and the readers' memos; the shop's
//! `MarketplaceState` builds it, and content, land_authz, worlds, social and
//! builder read through it without depending on the shop.

// The crates this one builds on, under their module names (AGENTS.md).
pub(crate) use catalyst_shared as shared;
pub(crate) use dcl_http as http;

pub mod graph;
pub mod land_graph;
pub mod registries;
pub mod rpc;
pub mod source;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::http::Outbound;

/// The subgraph endpoints and the Ethereum network the readers answer for.
#[derive(Clone, Debug, Default)]
pub struct IndexSettings {
    pub eth_collections_subgraph_url: Option<String>,
    pub matic_collections_subgraph_url: Option<String>,
    pub marketplace_subgraph_url: Option<String>,
    pub land_subgraph_url: Option<String>,
    /// `ETH_NETWORK` (`mainnet` | `sepolia`): picks the LAND registries.
    pub eth_network: String,
}

impl IndexSettings {
    /// Mainnet with no subgraph configured: what the tests start from.
    pub fn for_tests() -> Self {
        Self {
            eth_network: "mainnet".to_string(),
            ..Self::default()
        }
    }
}

pub struct ChainIndex {
    pub http: Outbound,
    pub settings: IndexSettings,
    pub graph: graph::Caches,
    /// The land-manager subgraph reader's memo (`land_graph`).
    pub(crate) land_graph: land_graph::Caches,
    allow_loopback: Arc<AtomicBool>,
}

impl ChainIndex {
    pub fn new(http: Outbound, settings: IndexSettings) -> Self {
        Self {
            http,
            settings,
            graph: graph::Caches::default(),
            land_graph: land_graph::Caches::default(),
            allow_loopback: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The SSRF guard every subgraph call passes: a publicly routable
    /// http(s) URL. Tests may admit a loopback fake upstream.
    pub fn upstream_allowed(&self, url: &str) -> bool {
        if crate::http::is_safe_http_url(url) {
            return true;
        }
        if self.allow_loopback.load(Ordering::Relaxed) {
            return url::Url::parse(url.trim())
                .is_ok_and(|u| u.scheme() == "http" && u.host_str() == Some("127.0.0.1"));
        }
        false
    }

    /// Admit a loopback fake subgraph for unit tests and test-support consumers.
    #[cfg(any(test, feature = "test-support"))]
    pub fn allow_loopback_for_tests(&self) {
        self.allow_loopback.store(true, Ordering::Relaxed);
    }

    /// Tests only: the switch [`Self::allow_loopback_for_tests`] flips, for
    /// a state that wraps this index and keeps its own test hook.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn loopback_switch_for_tests(&self) -> Arc<AtomicBool> {
        self.allow_loopback.clone()
    }
}
