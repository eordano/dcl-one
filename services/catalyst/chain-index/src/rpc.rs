//! Ethereum JSON-RPC logs, headers and bounded range retrieval.

use serde::Deserialize;
use serde_json::{Value, json};

const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const RPC_BODY_CAP: usize = 64 * 1024 * 1024;
pub const HEADER_BATCH: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("RPC request budget exhausted at range ending {upper}")]
    Budget { upper: u64, requests: usize },
    #[error("rpc transport: {0}")]
    Transport(String),
    #[error("rpc returned an error that carries no usable block range: {0}")]
    Rpc(String),
}

#[derive(Debug, Deserialize)]
pub struct RpcLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    #[serde(rename = "blockNumber")]
    pub block_number: String,
    #[serde(rename = "logIndex")]
    pub log_index: String,
    #[serde(rename = "blockTimestamp")]
    pub block_timestamp: Option<String>,
    #[serde(rename = "blockHash", default)]
    pub block_hash: Option<String>,
    #[serde(rename = "transactionHash", default)]
    pub transaction_hash: Option<String>,
}

pub fn hex_to_i64(s: &str) -> Option<i64> {
    i64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

/// The public RPC caps a filtered `eth_getLogs` at ten thousand rows but names
/// a range that would fit; following that hint is what keeps a full history
/// scan to tens of requests instead of thousands of fixed-width windows.
pub fn suggested_upper_bound(message: &str) -> Option<u64> {
    let open = message.rfind('[')?;
    let close = message[open..].find(']')? + open;
    let inner = &message[open + 1..close];
    let (_, hi) = inner.split_once(',')?;
    u64::from_str_radix(hi.trim().trim_start_matches("0x"), 16).ok()
}

/// The other refusal shape the public endpoints answer (observed on
/// rpc.decentraland.org's second backend, 2026-09-25): "range 50000 exceeds
/// limit of 10000" names the widest window it takes, not a block.
pub fn suggested_range_limit(message: &str) -> Option<u64> {
    let (_, tail) = message.split_once("exceeds limit of ")?;
    let digits: String = tail
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u64>().ok().filter(|n| *n > 0)
}

/// A refusal no block range fixes: the filter names more addresses than
/// the endpoint takes ("exceed max addresses or topics per search
/// position"). The caller splits its address list instead of the range.
pub fn is_address_limit(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("max addresses") || m.contains("too many addresses")
}

/// One block header's identity: the reorg check and the time bound of the
/// marketplace indexers read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub number: u64,
    pub hash: String,
    pub timestamp: i64,
}

/// Ethereum logs and headers over the shared outbound client.
pub struct Rpc<'a> {
    http: &'a crate::http::Outbound,
    url: &'a str,
}

impl<'a> Rpc<'a> {
    /// The client over another endpoint, for callers that pass their own
    /// filter to [`Rpc::get_logs_matching`].
    pub fn at(http: &'a crate::http::Outbound, url: &'a str) -> Self {
        Self { http, url }
    }

    fn jsonrpc(&self) -> crate::http::jsonrpc::Rpc<'a> {
        crate::http::jsonrpc::Rpc::new(self.http, self.url)
            .with_timeout(RPC_TIMEOUT)
            .with_body_cap(RPC_BODY_CAP)
    }

    /// A JSON-RPC batch over the same timeout and response body limit.
    pub async fn batch(
        &self,
        requests: &[Value],
    ) -> Result<Vec<Value>, crate::http::jsonrpc::RpcError> {
        self.jsonrpc().batch(requests).await
    }

    /// The whole envelope: the log readers act on the `error` message (a
    /// range hint), so the shared client hands it over unread.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.jsonrpc()
            .envelope(method, params)
            .await
            .map_err(|e| RpcError::Transport(format!("{method}: {e}")))
    }

    pub async fn head_block(&self) -> Result<u64, RpcError> {
        let v = self.call("eth_blockNumber", json!([])).await?;
        v.get("result")
            .and_then(Value::as_str)
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .ok_or_else(|| RpcError::Rpc(format!("eth_blockNumber returned {v}")))
    }

    pub async fn finalized_header(&self) -> Result<BlockHeader, RpcError> {
        let v = self
            .call("eth_getBlockByNumber", json!(["finalized", false]))
            .await?;
        let block = &v["result"];
        let number = block["number"]
            .as_str()
            .and_then(hex_to_i64)
            .filter(|n| *n >= 0)
            .ok_or_else(|| RpcError::Rpc("invalid finalized number".into()))?
            as u64;
        let hash = block["hash"]
            .as_str()
            .filter(|h| {
                h.len() == 66
                    && h.starts_with("0x")
                    && h[2..].bytes().all(|c| c.is_ascii_hexdigit())
            })
            .ok_or_else(|| RpcError::Rpc("invalid finalized hash".into()))?
            .to_ascii_lowercase();
        let timestamp = block["timestamp"]
            .as_str()
            .and_then(hex_to_i64)
            .filter(|t| *t >= 0)
            .ok_or_else(|| RpcError::Rpc("invalid finalized timestamp".into()))?;
        Ok(BlockHeader {
            number,
            hash,
            timestamp,
        })
    }

    /// `eth_getBlockByNumber` without transactions: the hash and time of one block.
    pub async fn block_header(&self, number: u64) -> Result<BlockHeader, RpcError> {
        let v = self
            .call(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        let block = v
            .get("result")
            .filter(|r| !r.is_null())
            .ok_or_else(|| RpcError::Rpc(format!("eth_getBlockByNumber {number} returned {v}")))?;
        if block
            .get("number")
            .and_then(Value::as_str)
            .and_then(hex_to_i64)
            != i64::try_from(number).ok()
        {
            return Err(RpcError::Rpc("block header number mismatch".into()));
        }
        let hash = block
            .get("hash")
            .and_then(Value::as_str)
            .filter(|h| {
                h.len() == 66
                    && h.starts_with("0x")
                    && h[2..].bytes().all(|c| c.is_ascii_hexdigit())
            })
            .map(str::to_ascii_lowercase);
        let timestamp = block
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(hex_to_i64);
        match (hash, timestamp) {
            (Some(hash), Some(timestamp)) => Ok(BlockHeader {
                number,
                hash,
                timestamp,
            }),
            _ => Err(RpcError::Rpc(format!(
                "eth_getBlockByNumber {number}: no hash or timestamp"
            ))),
        }
    }

    /// `eth_getBlockByNumber` for many blocks, in JSON-RPC batches of at
    /// most [`HEADER_BATCH`] (one HTTP request each), keyed by number, and
    /// the requests it took. A batch the endpoint answers without an element
    /// leaves that block to one single call. The marketplace indexes need it:
    /// 4 in 10 of rpc.decentraland.org/polygon's backends omit a log's
    /// `blockTimestamp` (sampled 2026-09-25), and a window can hold a
    /// thousand distinct blocks.
    pub async fn block_headers(
        &self,
        numbers: &[u64],
    ) -> Result<(std::collections::HashMap<u64, BlockHeader>, usize), RpcError> {
        let mut out = std::collections::HashMap::new();
        let mut requests = 0usize;
        for chunk in numbers.chunks(HEADER_BATCH) {
            let body: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    crate::http::jsonrpc::request(
                        i as u64,
                        "eth_getBlockByNumber",
                        json!([format!("0x{n:x}"), false]),
                    )
                })
                .collect();
            let items = self
                .jsonrpc()
                .batch(&body)
                .await
                .map_err(|e| RpcError::Transport(format!("eth_getBlockByNumber batch: {e}")))?;
            requests += 1;
            {
                for item in items {
                    let Some(block) = item.get("result").filter(|r| !r.is_null()) else {
                        continue;
                    };
                    let number = block
                        .get("number")
                        .and_then(Value::as_str)
                        .and_then(hex_to_i64);
                    let hash = block
                        .get("hash")
                        .and_then(Value::as_str)
                        .map(str::to_ascii_lowercase);
                    let timestamp = block
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(hex_to_i64);
                    if let (Some(number), Some(hash), Some(timestamp)) = (number, hash, timestamp) {
                        out.insert(
                            number as u64,
                            BlockHeader {
                                number: number as u64,
                                hash,
                                timestamp,
                            },
                        );
                    }
                }
            }
            for n in chunk {
                if !out.contains_key(n) {
                    let header = self.block_header(*n).await?;
                    requests += 1;
                    out.insert(*n, header);
                }
            }
        }
        Ok((out, requests))
    }

    /// Logs over any filter: `address` and `topics` are the
    /// `eth_getLogs` fields as given. A refusal naming a block
    /// (`[from, hi]`) or a window width ("exceeds limit of N") narrows to
    /// it, one naming neither halves the window, and an address-count
    /// refusal is returned at once for the caller to split its list.
    pub async fn get_logs_matching(
        &self,
        from: u64,
        to: u64,
        address: &Value,
        topics: &Value,
    ) -> Result<(Vec<RpcLog>, u64, usize), RpcError> {
        self.get_logs_bounded(from, to, address, topics, usize::MAX)
            .await
    }

    pub async fn get_logs_bounded(
        &self,
        from: u64,
        to: u64,
        address: &Value,
        topics: &Value,
        budget: usize,
    ) -> Result<(Vec<RpcLog>, u64, usize), RpcError> {
        let mut upper = to;
        let mut requests = 0usize;
        loop {
            if requests >= budget {
                return Err(RpcError::Budget { upper, requests });
            }
            requests += 1;
            let params = json!([{
                "fromBlock": format!("0x{from:x}"),
                "toBlock": format!("0x{upper:x}"),
                "address": address,
                "topics": topics,
            }]);
            let v = self.call("eth_getLogs", params).await?;
            if let Some(result) = v.get("result").filter(|r| !r.is_null()) {
                let logs: Vec<RpcLog> = serde_json::from_value(result.clone())
                    .map_err(|e| RpcError::Rpc(format!("cannot read logs: {e}")))?;
                return Ok((logs, upper, requests));
            }
            let message = v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("no result and no error")
                .to_string();
            if is_address_limit(&message) {
                return Err(RpcError::Rpc(message));
            }
            let by_width = suggested_range_limit(&message).map(|n| from + n.saturating_sub(1));
            match suggested_upper_bound(&message).or(by_width) {
                Some(hi) if hi >= from && hi < upper => upper = hi,
                _ => {
                    let halved = from + (upper - from) / 2;
                    if halved <= from && upper == from {
                        return Err(RpcError::Rpc(message));
                    }
                    upper = halved.max(from);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_upper_bound_the_rpc_suggests() {
        let msg =
            "Log response size exceeded. ... this block range should work: [0x4b7fc2, 0xe95489]";
        assert_eq!(suggested_upper_bound(msg), Some(0xe95489));
        let infura = "query returned more than 10000 results. Try with this block range [0x4B7FC2, 0x4B8000].";
        assert_eq!(suggested_upper_bound(infura), Some(0x4b8000));
    }

    #[test]
    fn a_message_without_a_range_yields_nothing() {
        assert_eq!(suggested_upper_bound("query timeout"), None);
    }

    #[test]
    fn a_window_limit_and_an_address_limit_are_read_from_the_message() {
        assert_eq!(
            suggested_range_limit("range 50000 exceeds limit of 10000"),
            Some(10_000)
        );
        assert_eq!(suggested_range_limit("query timeout"), None);
        assert!(is_address_limit(
            "exceed max addresses or topics per search position"
        ));
        assert!(!is_address_limit("range 50000 exceeds limit of 10000"));
    }

    #[test]
    fn hex_block_numbers_parse() {
        assert_eq!(hex_to_i64("0x589fce"), Some(5_808_078));
        assert_eq!(hex_to_i64("0x0"), Some(0));
    }
}
