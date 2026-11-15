//! The worlds content server's signed-fetch verifier, shared by the worlds
//! routes (`worlds::core::auth` re-exports it) and the world comms mints:
//! [`require_verified`] runs the 60-second window, a per-route list of
//! metadata keys that switches the folded-payload fallback on (empty = the
//! 6.x payload only), the same scene-signer gate, and refuses with a
//! [`SignedFetchRefusal`]: the shared cause, which decides the status, and
//! the `@dcl/crypto-middleware` 6.3.0 message, which the routes answer as
//! upstream's `onError` does ([`map_auth_error_api`]). The steps it runs (the
//! chain, the timestamp window, the metadata parse, the one path rule) are
//! `shared::signed_fetch`'s.

use axum::http::HeaderMap;
use serde_json::{Map, Value};

use crate::auth::eip1654::Eip1654Validator;
use crate::auth::signed_fetch::{ChainShapeError, UnsignedLinks};
use crate::auth::{AuthChain, AuthError, MAX_AUTH_CHAIN_LINKS};
use crate::shared::identity::Address;
use crate::shared::signed_fetch::{
    AUTH_CHAIN_HEADER_PREFIX, AUTH_METADATA_HEADER, AUTH_TIMESTAMP_HEADER, SignedFetchError,
    SignedPath, build_legacy_payload, build_payload_v6, candidate_paths, check_timestamp_window,
    header_str, is_refused_signer, map_auth_error, parse_metadata,
};
use crate::worlds_envelope::ApiError;

/// Upstream's `setupRouter` passes no `expiration` to `wellKnownComponents`, so every
/// route on this surface runs on crypto-middleware's `DEFAULT_EXPIRATION` of `1000 * 60`
/// (core-libs/libs/crypto-middleware/src/types.ts). The world-storage surface already
/// reads the same default.
pub const SIGNED_FETCH_EXPIRATION_SECS: i64 = 60;

/// The explorer comms handshakes. The js explorers still sign the folded payload with
/// camelCase metadata while bevy-explorer mints the 6.x payload, so the fallback stays
/// on behind exactly the keys upstream pins. The comms adapter checks signer and intent
/// after verification; re-spelled keys are refused on the same requests upstream refuses. Empty
/// the list once every explorer signs the 6.x payload.
pub const EXPLORER_METADATA_KEYS: &[&str] = &["signer", "intent", "secret"];

#[derive(Debug, Clone)]
pub struct VerifiedAuth {
    pub signer: Address,
    pub metadata: Value,
}

impl VerifiedAuth {
    pub fn secret(&self) -> Option<String> {
        self.metadata
            .get("secret")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

/// A refused signed fetch: `cause` decides the status (400, 401 or 503, as
/// `SignedFetchError::http_status_and_message` splits them) and `message` is
/// the text `@dcl/crypto-middleware` 6.3.0 `verify()` throws for it.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SignedFetchRefusal {
    pub cause: SignedFetchError,
    pub message: String,
}

impl SignedFetchRefusal {
    fn new(cause: SignedFetchError, message: impl Into<String>) -> Self {
        Self {
            cause,
            message: message.into(),
        }
    }

    pub fn status(&self) -> u16 {
        self.cause.http_status_and_message().0
    }
}

/// crypto-middleware's `safe()`: a fragment echoed into a message, cut to 64
/// characters with `…`.
fn safe(value: &str) -> String {
    const MAX: usize = 64;
    if value.chars().count() > MAX {
        let head: String = value.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        value.to_string()
    }
}

/// `JSON.stringify(JSON.parse(raw))` for the metadata upstream echoes: the
/// delivered bytes without the whitespace between tokens, keys in delivered order.
fn compact_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in raw.chars() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// `extractAuthChain`'s refusals over the shared chain shape.
fn chain_refusal(headers: &HeaderMap, e: ChainShapeError) -> SignedFetchRefusal {
    let message = match &e {
        ChainShapeError::InsufficientLinks => "Invalid Auth Chain".to_string(),
        ChainShapeError::TooLong => {
            format!("Auth chain exceeds maximum length of {MAX_AUTH_CHAIN_LINKS}")
        }
        // Valid JSON that is not an auth link, or not JSON at all.
        ChainShapeError::Unparseable { index, detail } => {
            let raw = header_str(headers, &format!("{AUTH_CHAIN_HEADER_PREFIX}{index}"));
            if raw.is_some_and(|raw| serde_json::from_str::<Value>(raw).is_ok()) {
                format!("Invalid chain format: malformed auth link at position {index}")
            } else {
                format!("Invalid chain format: {}", safe(detail))
            }
        }
        // Upstream lets these through to the signature check; the text names the link.
        ChainShapeError::SignerNotFirst(index) | ChainShapeError::MissingSignature(index) => {
            format!("Invalid chain format: malformed auth link at position {index}")
        }
        ChainShapeError::FirstNotSigner => {
            "Invalid chain format: malformed auth link at position 0".to_string()
        }
    };
    SignedFetchRefusal::new(e.into(), message)
}

/// `verifySign`'s refusals: `Invalid signature: <reason>` for a personal-sign
/// chain, the bare `Invalid signature` and the catalyst 503 for EIP-1654.
fn signature_refusal(e: AuthError) -> SignedFetchRefusal {
    let message = match &e {
        AuthError::Eip1654Rejected { .. } | AuthError::Eip1654NotImplemented => {
            "Invalid signature".to_string()
        }
        AuthError::Eip1654ValidationFailed(detail) => {
            format!("Error connecting to catalyst: {detail}")
        }
        other => format!("Invalid signature: {other}"),
    };
    SignedFetchRefusal::new(map_auth_error(e), message)
}

fn objects_to_inspect<'a>(value: &'a Value, out: &mut Vec<&'a Map<String, Value>>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| objects_to_inspect(item, out)),
        Value::Object(object) => out.push(object),
        _ => {}
    }
}

/// The first mismatch in declared-path, segment and array order. Values and
/// undeclared keys are not inspected for spelling.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MetadataKeyMismatch<'a> {
    Spelling {
        expected: &'a str,
        delivered: &'a str,
    },
    Collision {
        expected: &'a str,
        count: usize,
    },
}

pub(crate) fn first_metadata_key_mismatch<'a>(
    metadata: &'a Value,
    canonical_keys: &[&'a str],
) -> Option<MetadataKeyMismatch<'a>> {
    for declared_path in canonical_keys {
        let mut containers: Vec<&Value> = vec![metadata];
        for segment in declared_path.split('.') {
            let folded = segment.to_lowercase();
            let mut objects: Vec<&Map<String, Value>> = Vec::new();
            for container in &containers {
                objects_to_inspect(container, &mut objects);
            }
            let mut next: Vec<&Value> = Vec::new();
            for object in objects {
                let delivered: Vec<&String> = object
                    .keys()
                    .filter(|key| key.to_lowercase() == folded)
                    .collect();
                match delivered.as_slice() {
                    [] => continue,
                    [one] if one.as_str() == segment => {}
                    [one] => {
                        return Some(MetadataKeyMismatch::Spelling {
                            expected: segment,
                            delivered: one,
                        });
                    }
                    many => {
                        return Some(MetadataKeyMismatch::Collision {
                            expected: segment,
                            count: many.len(),
                        });
                    }
                }
                if let Some(value) = object.get(segment) {
                    next.push(value);
                }
            }
            if next.is_empty() {
                break;
            }
            containers = next;
        }
    }
    None
}

/// Render the signed-fetch refusal without exposing untrusted key contents.
fn first_respelled_key(metadata: &Value, canonical_keys: &[&str]) -> Option<String> {
    first_metadata_key_mismatch(metadata, canonical_keys).map(|mismatch| match mismatch {
        MetadataKeyMismatch::Spelling {
            expected,
            delivered,
        } => format!(
            "Invalid chain metadata: expected \"{}\", got \"{}\"",
            safe(expected),
            safe(delivered)
        ),
        MetadataKeyMismatch::Collision { expected, count } => format!(
            "Invalid chain metadata: \"{}\" delivered under {} spellings",
            safe(expected),
            count
        ),
    })
}

async fn validate_over_paths(
    chain: &AuthChain,
    payloads: &[String],
    now: i64,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<Address, SignedFetchRefusal> {
    let mut verdict = None;
    for payload in payloads {
        match crate::auth::verify_auth_chain_async(chain, payload, Some(now * 1000), validator)
            .await
        {
            Ok(()) => {
                return Address::parse(&chain[0].payload).map_err(|e| {
                    SignedFetchRefusal::new(
                        SignedFetchError::MalformedChain {
                            detail: format!("signer: {e}"),
                        },
                        "Invalid chain format: malformed auth link at position 0",
                    )
                });
            }
            Err(e) => match signature_refusal(e) {
                refusal @ SignedFetchRefusal {
                    cause: SignedFetchError::InvalidSignature(_),
                    ..
                } => verdict = Some(refusal),
                other => return Err(other),
            },
        }
    }
    Err(verdict.unwrap_or_else(|| {
        SignedFetchRefusal::new(
            SignedFetchError::InvalidSignature("no payload".into()),
            "Invalid signature: no payload",
        )
    }))
}

/// `canonical_metadata_keys` is the switch for the folded-payload fallback: a route
/// naming no keys verifies the 6.x payload only. The owner-only routes (scene delete,
/// undeploy, settings, per-address permissions) name none, as upstream keeps them
/// strict; the gate still answers there. Stage order as upstream `verify()`:
/// chain, timestamp (`Number(raw || '0')`, so an absent header expires like an
/// empty one), metadata parse, the scene gate, then the signature.
/// `signed_path` names the route as mounted, so a nested request verifies a
/// signature over its gateway-forwarded bare path (`SignedPath::from_uris`).
pub async fn require_verified(
    headers: &HeaderMap,
    method: &str,
    signed_path: &SignedPath,
    canonical_metadata_keys: &[&str],
    validator: Option<&dyn Eip1654Validator>,
) -> Result<VerifiedAuth, SignedFetchRefusal> {
    let chain = crate::auth::signed_fetch::extract_auth_chain(headers, UnsignedLinks::Malformed)
        .map_err(|e| chain_refusal(headers, e))?;
    let ts = header_str(headers, AUTH_TIMESTAMP_HEADER)
        .filter(|raw| !raw.is_empty())
        .unwrap_or("0")
        .to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now = now_ms.div_euclid(1000);
    check_timestamp_window(&ts, now, SIGNED_FETCH_EXPIRATION_SECS).map_err(|cause| {
        let message = match (&cause, ts.parse::<i64>()) {
            (SignedFetchError::Expired { .. }, Ok(signed)) if signed > now_ms => format!(
                "Signature timestamp is too far in the future: signature timestamp: {signed}, local timestamp: {now_ms}"
            ),
            (SignedFetchError::Expired { .. }, Ok(signed)) => format!(
                "Expired signature: signature timestamp: {signed}, timestamp expiration: {}, local timestamp: {now_ms}",
                signed.saturating_add(SIGNED_FETCH_EXPIRATION_SECS * 1000)
            ),
            _ => format!("Invalid chain timestamp: {}", safe(&ts)),
        };
        SignedFetchRefusal::new(cause, message)
    })?;

    let raw_metadata = header_str(headers, AUTH_METADATA_HEADER)
        .unwrap_or("{}")
        .to_string();
    let metadata = parse_metadata(&raw_metadata).map_err(|cause| {
        SignedFetchRefusal::new(
            cause,
            format!("Invalid chain metadata: \"{}\"", safe(&raw_metadata)),
        )
    })?;
    if is_refused_signer(&metadata) {
        let content = safe(&compact_json(&raw_metadata));
        return Err(SignedFetchRefusal::new(
            SignedFetchError::MalformedChain {
                detail: format!("invalid metadata content: {content}"),
            },
            format!("Invalid metadata content: {content}"),
        ));
    }

    let paths = candidate_paths(headers, signed_path);
    let v6: Vec<String> = paths
        .iter()
        .map(|p| build_payload_v6(method, p, &ts, &raw_metadata))
        .collect();
    let signer = match validate_over_paths(&chain, &v6, now, validator).await {
        Err(
            refusal @ SignedFetchRefusal {
                cause: SignedFetchError::InvalidSignature(_),
                ..
            },
        ) if !canonical_metadata_keys.is_empty() => {
            if let Some(message) = first_respelled_key(&metadata, canonical_metadata_keys) {
                return Err(SignedFetchRefusal::new(
                    SignedFetchError::MalformedChain {
                        detail: message.clone(),
                    },
                    message,
                ));
            }
            let legacy: Vec<String> = paths
                .iter()
                .map(|p| build_legacy_payload(method, p, &ts, &raw_metadata))
                .collect();
            if legacy == v6 {
                return Err(refusal);
            }
            validate_over_paths(&chain, &legacy, now, validator).await?
        }
        settled => settled?,
    };
    Ok(VerifiedAuth { signer, metadata })
}

/// Upstream's signed-fetch `onError`: the middleware's message in `error`, the
/// ADR-44 sentence in `message`, at the middleware's status.
pub fn map_auth_error_api(e: SignedFetchRefusal) -> ApiError {
    ApiError::signed_fetch(e.status(), e.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_traversal_keeps_path_array_and_segment_precedence() {
        let metadata = serde_json::json!({
            "rows": [[null, {"nested": [{"SceneId": "value"}]}], {"Nested": {"sceneId": "value"}}],
            "Other": true
        });
        // Every container at a segment is checked before descending further.
        assert_eq!(
            first_respelled_key(&metadata, &["rows.nested.sceneId", "other"]).as_deref(),
            Some("Invalid chain metadata: expected \"nested\", got \"Nested\"")
        );
        assert_eq!(
            first_respelled_key(&metadata, &["other", "rows.nested.sceneId"]).as_deref(),
            Some("Invalid chain metadata: expected \"other\", got \"Other\"")
        );
        let metadata = serde_json::json!({"rows": [[{"SceneId": "first"}], {"SCENEID": "second"}]});
        assert_eq!(
            first_respelled_key(&metadata, &["rows.sceneId"]).as_deref(),
            Some("Invalid chain metadata: expected \"sceneId\", got \"SceneId\"")
        );
    }

    #[test]
    fn metadata_traversal_preserves_unicode_collisions_and_absent_paths() {
        let metadata = serde_json::json!({"rows": [[{"Ä": 1, "ä": 2}]]});
        assert_eq!(
            first_respelled_key(&metadata, &["rows.ä"]).as_deref(),
            Some("Invalid chain metadata: \"ä\" delivered under 2 spellings")
        );
        for metadata in [
            serde_json::json!({"rows": [null, 1, [], {"other": "SceneId"}]}),
            serde_json::json!({"rows": [{"sceneId": "UPPERCASE VALUE"}]}),
            serde_json::json!(null),
        ] {
            assert_eq!(
                first_metadata_key_mismatch(&metadata, &["rows.sceneId"]),
                None
            );
        }
        let metadata = serde_json::json!({"İ": true});
        assert!(matches!(
            first_metadata_key_mismatch(&metadata, &["i\u{307}"]),
            Some(MetadataKeyMismatch::Spelling {
                delivered: "İ", ..
            })
        ));
    }

    #[test]
    fn echoed_fragments_are_cut_like_crypto_middleware() {
        assert_eq!(safe("short"), "short");
        let long = "x".repeat(70);
        assert_eq!(safe(&long), format!("{}…", "x".repeat(64)));
    }

    #[test]
    fn metadata_is_echoed_as_json_stringify_writes_it() {
        assert_eq!(
            compact_json("{ \"signer\" : \"a b\",\n \"Signer\": \"x\\\" y\" }"),
            r#"{"signer":"a b","Signer":"x\" y"}"#
        );
    }

    #[test]
    fn the_key_guard_names_the_delivered_spelling() {
        let metadata = serde_json::json!({ "Secret": "x" });
        assert_eq!(
            first_respelled_key(&metadata, EXPLORER_METADATA_KEYS).as_deref(),
            Some(r#"Invalid chain metadata: expected "secret", got "Secret""#)
        );
        let metadata = serde_json::json!({ "intent": "a", "INTENT": "b" });
        assert_eq!(
            first_respelled_key(&metadata, EXPLORER_METADATA_KEYS).as_deref(),
            Some(r#"Invalid chain metadata: "intent" delivered under 2 spellings"#)
        );
    }
}
