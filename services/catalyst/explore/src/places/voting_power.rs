//! The signer's Snapshot voting power, asked for on every favorite and like
//! write and stored as the fact's `user_activity` (upstream places
//! `Snapshot/utils.ts:72-105`, `fetchScore`): the `get_vp` JSON-RPC call
//! against the score API with upstream's strategies, a 10 s timeout, and 0
//! for a non-address, a transport failure or a body without `result.vp`.
//! Only votes with `user_activity >= 100` weigh in `like_rate` and
//! `like_score` (`shared/entityInteractions.ts:20`).
//!
//! The request leaves through the node's guarded outbound client
//! ([`crate::http::Outbound`]), so a private or loopback endpoint is refused
//! before any byte is sent; tests swap in [`Outbound::for_tests`] and a stub.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::auth::is_eth_address;
use crate::http::Outbound;

/// `AbortSignal.timeout(10000)` (`Snapshot/utils.ts:92`).
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The threshold a vote must reach to count in the rates
/// (`shared/entityInteractions.ts:20`, `MIN_USER_ACTIVITY`).
pub const MIN_USER_ACTIVITY: f64 = 100.0;

/// The `snapshot.dcl.eth` strategies, verbatim (`Snapshot/utils.ts:4-70`).
fn strategies() -> Value {
    json!([
        {
            "name": "multichain",
            "network": "1",
            "params": {
                "name": "multichain",
                "graphs": { "137": "subgraph.decentraland.org/blocks-matic-mainnet" },
                "symbol": "MANA",
                "strategies": [
                    {
                        "name": "erc20-balance-of",
                        "params": { "address": "0x0f5d2fb29fb7d3cfee444a200298f468908cc942", "decimals": 18 },
                        "network": "1"
                    },
                    {
                        "name": "erc20-balance-of",
                        "params": { "address": "0xA1c57f48F0Deb89f569dFbE6E2B7f46D33606fD4", "decimals": 18 },
                        "network": "137"
                    }
                ]
            }
        },
        {
            "name": "erc20-balance-of",
            "network": "1",
            "params": { "symbol": "WMANA", "address": "0xfd09cf7cfffa9932e33668311c4777cb9db3c9be", "decimals": 18 }
        },
        {
            "name": "erc721-with-multiplier",
            "network": "1",
            "params": { "symbol": "LAND", "address": "0xf87e31492faf9a91b02ee0deaad50d51d56d5d4d", "multiplier": 2000 }
        },
        {
            "name": "decentraland-estate-size",
            "network": "1",
            "params": { "symbol": "ESTATE", "address": "0x959e104e1a4db6317fa58f8295f586e1a978c297", "multiplier": 2000 }
        },
        {
            "name": "erc721-with-multiplier",
            "network": "1",
            "params": { "symbol": "NAMES", "address": "0x2a187453064356c898cae034eaed119e1663acb8", "multiplier": 100 }
        }
    ])
}

/// The `get_vp` request for `address` (`Snapshot/utils.ts:77-87`).
pub fn score_request(address: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "get_vp",
        "params": {
            "network": "1",
            "address": address.to_lowercase(),
            "strategies": strategies(),
            "space": "snapshot.dcl.eth",
            "delegation": false
        }
    })
}

/// JavaScript's `(x || 0) | 0`: a missing, null or NaN `vp` is 0, anything
/// else truncated toward zero and wrapped to a signed 32-bit integer
/// (`Snapshot/utils.ts:100`).
pub fn to_int32(vp: Option<f64>) -> i32 {
    let v = vp.filter(|v| v.is_finite()).unwrap_or(0.0).trunc();
    let wrapped = (v % 4_294_967_296.0 + 4_294_967_296.0) % 4_294_967_296.0;
    (wrapped as u32) as i32
}

/// `body?.result?.vp` as the stored score.
pub fn score_from_response(body: &Value) -> i32 {
    to_int32(
        body.get("result")
            .and_then(|r| r.get("vp"))
            .and_then(Value::as_f64),
    )
}

/// The score API client: `None` as the endpoint asks nothing and scores
/// every voter 0 (a node without the Snapshot leg). The outbound client is
/// the one [`Client::with_http`] passed (the process's shared guarded client
/// in `serve`); without one, a default is built on the first request, never
/// at construction (review architecture-1 explore-change-8).
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Option<String>,
    http: Arc<OnceLock<Option<Outbound>>>,
}

impl Client {
    pub fn new(endpoint: Option<String>) -> Self {
        Self {
            endpoint: endpoint
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty()),
            http: Arc::new(OnceLock::new()),
        }
    }

    /// The same endpoint through a given outbound client (the process's
    /// shared one in `serve`, a loopback-admitting one in tests).
    pub fn with_http(mut self, http: Outbound) -> Self {
        self.http = Arc::new(OnceLock::from(Some(http)));
        self
    }

    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    fn http(&self) -> Option<&Outbound> {
        self.http
            .get_or_init(
                || match Outbound::new(crate::http::HttpClientCfg::default()) {
                    Ok(http) => Some(http),
                    Err(error) => {
                        tracing::warn!(%error, "Error loading user score: no outbound client");
                        None
                    }
                },
            )
            .as_ref()
    }

    /// `fetchScore(address)`: the signer's voting power, 0 when it cannot be
    /// had (`Snapshot/utils.ts:72-105`).
    pub async fn fetch_score(&self, address: &str) -> f64 {
        // validator.js `isEthereumAddress` (`/^(0x)[0-9a-f]{40}$/i`) also
        // takes a `0X` prefix, which no signer address the callers pass is
        // spelled with.
        if !is_eth_address(address) {
            return 0.0;
        }
        let Some(endpoint) = self.endpoint.as_deref() else {
            return 0.0;
        };
        let Some(http) = self.http() else {
            return 0.0;
        };
        let request = score_request(address);
        let sent = http
            .post(endpoint)
            .timeout(TIMEOUT)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await;
        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Error loading user score");
                return 0.0;
            }
        };
        // `res.json()` regardless of the status; a body that is not JSON is
        // the catch branch (0).
        match crate::http::read_body_capped(response, 1 << 20).await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(body) => f64::from(score_from_response(&body)),
                Err(e) => {
                    tracing::warn!(error = %e, "Error loading user score");
                    0.0
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "Error loading user score");
                0.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_is_upstreams_get_vp_call() {
        let req = score_request("0xABCDEF0123456789abcdef0123456789ABCDEF01");
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["method"], "get_vp");
        assert_eq!(req["params"]["network"], "1");
        assert_eq!(
            req["params"]["address"],
            "0xabcdef0123456789abcdef0123456789abcdef01"
        );
        assert_eq!(req["params"]["space"], "snapshot.dcl.eth");
        assert_eq!(req["params"]["delegation"], false);
        let strategies = req["params"]["strategies"].as_array().unwrap();
        assert_eq!(strategies.len(), 5);
        assert_eq!(strategies[0]["name"], "multichain");
        assert_eq!(strategies[4]["params"]["symbol"], "NAMES");
    }

    #[test]
    fn vp_is_truncated_like_javascripts_bitwise_or() {
        assert_eq!(to_int32(Some(123.9)), 123);
        assert_eq!(to_int32(Some(-0.5)), 0);
        assert_eq!(to_int32(None), 0);
        assert_eq!(to_int32(Some(f64::NAN)), 0);
        assert_eq!(to_int32(Some(4_294_967_296.0 + 7.0)), 7);
        assert_eq!(to_int32(Some(2_147_483_648.0)), i32::MIN);
        assert_eq!(
            score_from_response(&json!({ "jsonrpc": "2.0", "result": { "vp": 250.75 } })),
            250
        );
        assert_eq!(score_from_response(&json!({ "error": "nope" })), 0);
        assert_eq!(score_from_response(&json!({ "result": { "vp": null } })), 0);
    }

    #[tokio::test]
    async fn a_non_address_or_no_endpoint_scores_zero_without_a_request() {
        let client = Client::new(None).with_http(Outbound::for_tests());
        assert_eq!(
            client
                .fetch_score("0x00000000000000000000000000000000000000aa")
                .await,
            0.0
        );
        let client =
            Client::new(Some("http://127.0.0.1:9/".into())).with_http(Outbound::for_tests());
        assert_eq!(client.fetch_score("not-an-address").await, 0.0);
        assert_eq!(client.endpoint(), Some("http://127.0.0.1:9/"));
        assert_eq!(Client::new(Some("  ".into())).endpoint(), None);
    }

    #[tokio::test]
    async fn a_refused_connection_scores_zero() {
        let client =
            Client::new(Some("http://127.0.0.1:9/".into())).with_http(Outbound::for_tests());
        assert_eq!(
            client
                .fetch_score("0x00000000000000000000000000000000000000aa")
                .await,
            0.0
        );
    }
}
