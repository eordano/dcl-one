//! The JSON-RPC 2.0 wire: request body, envelope and error shapes, no
//! transport. Edition-2021 clean: dcl-scene-cli compiles it by path.

use std::time::Duration;

use serde_json::{Value, json};

/// Receipts of a busy block carry hundreds of logs; four MiB covers them.
pub const DEFAULT_BODY_CAP: usize = 4 * 1024 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The guarded client refused the endpoint before any byte left.
    Refused(String),
    /// The request or the body read failed (connect, timeout, cap).
    Transport(String),
    /// A non-2xx status whose body is not a JSON-RPC answer.
    Status(u16),
    /// A 2xx body that is not JSON, or not a JSON-RPC envelope.
    Malformed(String),
    /// The node answered with a JSON-RPC error object.
    Response { code: i64, message: String },
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Refused(m) => write!(f, "endpoint refused: {m}"),
            RpcError::Transport(m) => write!(f, "rpc unavailable: {m}"),
            RpcError::Status(s) => write!(f, "rpc responded with status {s}"),
            RpcError::Malformed(m) => write!(f, "malformed rpc body: {m}"),
            RpcError::Response { code, message } => write!(f, "rpc error {code}: {message}"),
        }
    }
}

impl std::error::Error for RpcError {}

impl RpcError {
    /// Whether the node itself was unreachable or unusable, as opposed to
    /// answering the call with an error.
    pub fn is_transport(&self) -> bool {
        matches!(
            self,
            RpcError::Refused(_)
                | RpcError::Transport(_)
                | RpcError::Status(_)
                | RpcError::Malformed(_)
        )
    }

    /// Whether the node answered that the call itself reverted: geth's code
    /// 3, the "execution reverted" of hosted providers, Nethermind's "VM
    /// execution error".
    pub fn is_revert(&self) -> bool {
        matches!(self, RpcError::Response { code, message }
        if *code == 3 || {
            let m = message.to_ascii_lowercase();
            m.contains("revert") || m.contains("vm execution error")
        })
    }
}

/// The request body of one call.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// The answer as the node sent it, whatever its status: a JSON-RPC envelope
/// (one carrying `result` or `error`) is accepted from any status; any other
/// body is a transport failure when the status is not 2xx and malformed when
/// it is.
pub fn parse_envelope(status: u16, bytes: &[u8]) -> Result<Value, RpcError> {
    let ok = (200..300).contains(&status);
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) if v.get("result").is_some() || v.get("error").is_some() => Ok(v),
        Ok(_) | Err(_) if !ok => Err(RpcError::Status(status)),
        Ok(v) => Ok(v),
        Err(e) => Err(RpcError::Malformed(e.to_string())),
    }
}

/// The `result` of an envelope (`null` when the node answered none: an
/// unknown transaction, a missing block), or its `error`.
pub fn result_of(mut envelope: Value) -> Result<Value, RpcError> {
    if let Some(err) = envelope.get("error").filter(|e| !e.is_null()) {
        return Err(RpcError::Response {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(envelope
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// A `0x` hex string result (quantities and data).
pub fn hex_of(result: Value) -> Result<String, RpcError> {
    match result {
        Value::String(s) => Ok(s),
        other => Err(RpcError::Malformed(format!("non-string result: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverts_are_told_from_other_provider_errors() {
        let response = |code, message: &str| RpcError::Response {
            code,
            message: message.to_string(),
        };
        for revert in [
            response(3, "execution reverted"),
            response(-32000, "execution reverted"),
            response(-32000, "Execution reverted: ERC721: invalid token ID"),
            response(-32603, "VM Exception while processing transaction: revert"),
            response(-32015, "VM execution error."),
        ] {
            assert!(revert.is_revert(), "{revert}");
        }
        for failure in [
            response(-32000, "header not found"),
            response(
                -32000,
                "missing trie node 1a2b (path ) state is not available",
            ),
            response(-32005, "limit exceeded"),
            response(-32603, "request timed out"),
            response(429, "Too Many Requests"),
            RpcError::Status(502),
            RpcError::Transport("connection reset".into()),
            RpcError::Refused("private address".into()),
        ] {
            assert!(!failure.is_revert(), "{failure}");
        }
    }
}
