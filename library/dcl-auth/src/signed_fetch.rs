//! Signed fetch, the `x-identity-*` header bag: an auth chain over
//! `method:path:timestamp:metadata`. One implementation for every surface
//! that verifies one (the node's routes and sockets, the SDK's preview
//! server), in separate steps so each surface keeps its own stage order,
//! gates and wire answers:
//!
//! - [`extract_auth_chain`] reads the chain and checks its shape;
//! - [`check_timestamp_window`] bounds the timestamp to a window around now
//!   (the future side capped at [`MAX_FUTURE_SKEW_SECS`]),
//!   [`check_past_window`] only how old it may be;
//! - [`parse_metadata`] reads the metadata object, over which
//!   [`field_is_canonical`] and [`is_refused_signer`] are upstream's `signer`
//!   gates and [`names_scene_signer`] the scene check alone;
//! - [`candidate_paths`] is the path rule ([`SignedPath`]);
//! - [`verify_signature`] tries the payloads: the crypto-middleware 6.x one
//!   (method and path folded, the metadata bytes bound to the signature) over
//!   every candidate path, then the pre-6.0.0 fully folded one as far as the
//!   surface's [`LegacyPayload`] allows. Only an invalid-signature verdict
//!   ([`is_invalid_signature`]) moves on to the next payload.

use axum::http::{HeaderMap, Uri};
use serde_json::{Map, Value};

use crate::auth_chain::{AuthChain, AuthLink, AuthLinkType, MAX_AUTH_CHAIN_LINKS};
use crate::eip1654::Eip1654Validator;
use crate::error::AuthError;
use crate::verify::verify_auth_chain_async;

pub const AUTH_CHAIN_HEADER_PREFIX: &str = "x-identity-auth-chain-";
pub const AUTH_TIMESTAMP_HEADER: &str = "x-identity-timestamp";
pub const AUTH_METADATA_HEADER: &str = "x-identity-metadata";
pub const ORIGINAL_PATH_HEADER: &str = "x-original-path";

/// Upstream's freshness window.
pub const FIVE_MINUTES: i64 = 5 * 60;

/// How far ahead of now a signed timestamp may be, whatever the window.
pub const MAX_FUTURE_SKEW_SECS: i64 = 60;

/// The `signer` an explorer sets on an auth chain it signed on a scene's behalf.
pub const SCENE_SIGNER: &str = "decentraland-kernel-scene";

pub fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Why the headers carry no well-formed auth chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainShapeError {
    /// Link `index` is not an auth link; `detail` is the JSON parser's message.
    Unparseable { index: usize, detail: String },
    /// A SIGNER link at `index`, past the first.
    SignerNotFirst(usize),
    /// The first link is not the SIGNER.
    FirstNotSigner,
    /// Link `index`, past the SIGNER, carries no signature.
    MissingSignature(usize),
    /// A link at index [`MAX_AUTH_CHAIN_LINKS`].
    TooLong,
    /// Fewer than two links.
    InsufficientLinks,
}

/// What extraction does with a link past the SIGNER that carries no signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsignedLinks {
    /// Refused as [`ChainShapeError::MissingSignature`], as upstream's
    /// middleware refuses it.
    Malformed,
    /// Left to [`verify_signature`], which refuses it as a settled verdict.
    LeftToVerify,
}

/// The chain in `x-identity-auth-chain-0..` as `get` reads each header (a
/// header map, or a socket frame's JSON object).
pub fn extract_auth_chain_with<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    unsigned: UnsignedLinks,
) -> Result<AuthChain, ChainShapeError> {
    let mut links: AuthChain = Vec::new();
    for index in 0..MAX_AUTH_CHAIN_LINKS {
        let Some(raw) = get(&format!("{AUTH_CHAIN_HEADER_PREFIX}{index}")) else {
            break;
        };
        let link: AuthLink =
            serde_json::from_str(raw).map_err(|e| ChainShapeError::Unparseable {
                index,
                detail: e.to_string(),
            })?;
        match (link.link_type == AuthLinkType::SIGNER, index) {
            (true, 0) => {}
            (true, _) => return Err(ChainShapeError::SignerNotFirst(index)),
            (false, 0) => return Err(ChainShapeError::FirstNotSigner),
            (false, _) => {
                if unsigned == UnsignedLinks::Malformed
                    && link.signature.as_deref().unwrap_or("").is_empty()
                {
                    return Err(ChainShapeError::MissingSignature(index));
                }
            }
        }
        links.push(link);
    }
    if get(&format!("{AUTH_CHAIN_HEADER_PREFIX}{MAX_AUTH_CHAIN_LINKS}")).is_some() {
        return Err(ChainShapeError::TooLong);
    }
    if links.len() < 2 {
        return Err(ChainShapeError::InsufficientLinks);
    }
    Ok(links)
}

/// [`extract_auth_chain_with`] over request headers.
pub fn extract_auth_chain(
    headers: &HeaderMap,
    unsigned: UnsignedLinks,
) -> Result<AuthChain, ChainShapeError> {
    extract_auth_chain_with(|name| header_str(headers, name), unsigned)
}

/// The timestamp header as upstream reads it (`Number(raw || '0')`): absent
/// or empty is 0, which expires rather than skipping the window.
pub fn timestamp_or_zero(raw: Option<&str>) -> &str {
    raw.filter(|raw| !raw.is_empty()).unwrap_or("0")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkewError {
    /// Not an integer count of milliseconds; the timestamp as received.
    InvalidTimestamp(String),
    /// Outside the window; instants in seconds.
    Expired {
        signed_at: i64,
        now: i64,
        window_secs: i64,
    },
}

/// A millisecond `timestamp` is fresh when it is at most `window_secs` old and
/// at most `min(window_secs, MAX_FUTURE_SKEW_SECS)` ahead, both in seconds; a
/// non-numeric one is refused rather than skipping the window.
pub fn check_timestamp_window(
    timestamp: &str,
    now: i64,
    window_secs: i64,
) -> Result<(), SkewError> {
    let signed_at = timestamp
        .parse::<i64>()
        .map_err(|_| SkewError::InvalidTimestamp(timestamp.to_string()))?
        / 1000;
    let age = now.saturating_sub(signed_at);
    if age > window_secs || -age > window_secs.min(MAX_FUTURE_SKEW_SECS) {
        return Err(SkewError::Expired {
            signed_at,
            now,
            window_secs,
        });
    }
    Ok(())
}

/// dcl-crypto-middleware-rs's expiration: `timestamp` is an unsigned
/// integer count of milliseconds, stale once more than `max_age_ms` before
/// `now_ms`; a timestamp ahead of now is never stale.
pub fn check_past_window(timestamp: &str, now_ms: u128, max_age_ms: u128) -> Result<(), SkewError> {
    let signed_at = timestamp
        .parse::<u128>()
        .map_err(|_| SkewError::InvalidTimestamp(timestamp.to_string()))?;
    if signed_at.saturating_add(max_age_ms) < now_ms {
        let secs = |ms: u128| i64::try_from(ms / 1000).unwrap_or(i64::MAX);
        return Err(SkewError::Expired {
            signed_at: secs(signed_at),
            now: secs(now_ms),
            window_secs: secs(max_age_ms),
        });
    }
    Ok(())
}

/// Upstream's `verifyMetadata`: the metadata object, a JSON `null` read as
/// an empty one; `None` for anything else, refused before anything verifies.
pub fn parse_metadata(raw: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(raw).ok()? {
        Value::Null => Some(Value::Object(Map::new())),
        parsed @ Value::Object(_) => Some(parsed),
        _ => None,
    }
}

/// Upstream folds with `String.prototype.trim`, whose WhiteSpace set includes
/// ZWNBSP (U+FEFF); Rust's `str::trim` follows the Unicode `White_Space`
/// property, which does not.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

fn is_canonical(value: &str) -> bool {
    value == js_trim(value).to_lowercase().as_str()
}

/// Whether any key of `object` folds to `field` without being spelled exactly
/// that: refused rather than read as absent, so `{"Signer": ...}` can never
/// read as "not a scene".
fn has_folded_variant(object: &Map<String, Value>, field: &str) -> bool {
    let folded = field.to_lowercase();
    object
        .keys()
        .any(|key| key != field && key.to_lowercase() == folded)
}

/// Absent passes; a present value passes only as a canonical string
/// (upstream's `canonicalField(field)`).
pub fn field_is_canonical(metadata: &Value, field: &str) -> bool {
    let Some(object) = metadata.as_object() else {
        return true;
    };
    if has_folded_variant(object, field) {
        return false;
    }
    match object.get(field) {
        None => true,
        Some(Value::String(value)) => is_canonical(value),
        Some(_) => false,
    }
}

/// Mirrors upstream `rejectIfSigner(SCENE_SIGNER)` (#492, #493): metadata
/// declaring no `signer` passes; a canonical one passes unless it names the
/// scene signer; a non-canonical spelling of the key or the value, and a
/// present non-string, are refused rather than compared.
pub fn is_refused_signer(metadata: &Value) -> bool {
    if !field_is_canonical(metadata, "signer") {
        return true;
    }
    matches!(metadata.get("signer"), Some(Value::String(value)) if value == SCENE_SIGNER)
}

/// Whether `metadata` names the scene signer under any spelling of the
/// `signer` key or its value (case folded, trimmed as upstream trims). Only
/// the scene signer is looked for: other keys and values, canonical or not,
/// pass, so a surface that reads nothing else from the metadata refuses a
/// scene's request without refusing an ordinary one.
pub fn names_scene_signer(metadata: &Value) -> bool {
    metadata.as_object().is_some_and(|object| {
        object.iter().any(|(key, value)| {
            key.to_lowercase() == "signer"
                && matches!(value, Value::String(v) if js_trim(v).to_lowercase() == SCENE_SIGNER)
        })
    })
}

/// The crypto-middleware < 6.0.0 payload: everything, metadata included, lowercased.
pub fn build_legacy_payload(method: &str, path: &str, timestamp: &str, metadata: &str) -> String {
    format!("{method}:{path}:{timestamp}:{metadata}").to_lowercase()
}

/// The crypto-middleware >= 6.0.0 payload: only method and path folded, so the
/// metadata bytes are bound to the signature instead of being re-spellable
/// after signing.
pub fn build_payload_v6(method: &str, path: &str, timestamp: &str, metadata: &str) -> String {
    format!(
        "{}:{}:{}:{}",
        method.to_lowercase(),
        path.to_lowercase(),
        timestamp,
        metadata
    )
}

/// The path a request arrived on (`request`: the mounted path, its gateway
/// prefix included, `OriginalUri`) and the route-local one (`local`: the
/// route as the module registers it; equal when the router is mounted at
/// the root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedPath {
    pub request: String,
    pub local: String,
}

impl SignedPath {
    pub fn new(request: impl Into<String>, local: impl Into<String>) -> Self {
        Self {
            request: request.into(),
            local: local.into(),
        }
    }

    /// A route mounted at the root: the request path is the route path.
    pub fn local(local: impl Into<String>) -> Self {
        let local = local.into();
        Self {
            request: local.clone(),
            local,
        }
    }

    /// The request's `OriginalUri` and its nest-stripped `Uri`.
    pub fn from_uris(original: &Uri, local: &Uri) -> Self {
        Self::new(original.path(), local.path())
    }

    /// The request's `OriginalUri` and the route the handler serves.
    pub fn at(original: &Uri, local: impl Into<String>) -> Self {
        Self::new(original.path(), local)
    }
}

/// The path rule: the paths a request's signature may be over are the path
/// it arrived on, and the proxy-forwarded `x-original-path` (query stripped)
/// when it ends with the route. The header is client-controlled without the
/// proxy, so a prefix in front of this route is the same authority and any
/// other path is refused: a signature captured for one route never opens
/// another. The bare route-local path of a nested mount is not a candidate,
/// so a chain signed over `/v1/x` does not open `/social-api/v1/x` unless the
/// proxy says that is what the client asked.
pub fn candidate_paths<'a>(headers: &'a HeaderMap, path: &'a SignedPath) -> Vec<&'a str> {
    let mut paths = Vec::with_capacity(2);
    paths.push(path.request.as_str());
    if let Some(original) = header_str(headers, ORIGINAL_PATH_HEADER)
        .map(|raw| raw.split('?').next().unwrap_or(raw))
        .filter(|stripped| stripped.ends_with(path.local.as_str()) && *stripped != path.request)
    {
        paths.push(original);
    }
    paths
}

/// The verdicts a payload signed over another path, or in the other shape,
/// can still overturn: every other one is the chain's own (an expired or
/// malformed delegation, an EIP-1654 check that could not run) and settles
/// the request.
pub fn is_invalid_signature(err: &AuthError) -> bool {
    matches!(
        err,
        AuthError::RecoveryFailed(_)
            | AuthError::SignerMismatch { .. }
            | AuthError::FinalAuthorityMismatch { .. }
            | AuthError::Eip1654Rejected { .. }
    )
}

/// Whether a surface still accepts the pre-6.0.0 payload once every 6.x one
/// failed as an invalid signature. Wherever the two shapes coincide the
/// folded one is not tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyPayload {
    /// 6.x only.
    Refused,
    /// The folded payload too.
    Accepted,
    /// The folded payload only when the surface's guard over the metadata
    /// held (`keys_canonical`); otherwise
    /// [`SignatureRefusal::LegacyKeysNotCanonical`].
    Guarded { keys_canonical: bool },
}

/// What a signed fetch signed: `method:path:timestamp:metadata` for one of
/// `paths`, tried in order.
#[derive(Debug, Clone, Copy)]
pub struct SignedRequest<'a> {
    pub method: &'a str,
    pub paths: &'a [&'a str],
    pub timestamp: &'a str,
    /// The metadata header's bytes, as signed.
    pub metadata: &'a str,
}

#[derive(Debug)]
pub enum SignatureRefusal {
    /// Every payload tried failed as an invalid signature: the last verdict,
    /// `None` when there was no payload to try.
    Invalid(Option<AuthError>),
    /// A verdict no other payload can overturn ([`is_invalid_signature`]).
    Settled(AuthError),
    /// [`LegacyPayload::Guarded`] whose guard failed, once the 6.x payloads had.
    LegacyKeysNotCanonical,
}

/// The first of `payloads` that verifies settles it; only an invalid
/// signature tries the next.
async fn first_verifying(
    chain: &AuthChain,
    payloads: &[String],
    now_secs: i64,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<(), SignatureRefusal> {
    let mut last = None;
    for payload in payloads {
        match verify_auth_chain_async(chain, payload, Some(now_secs * 1000), validator).await {
            Ok(()) => return Ok(()),
            Err(e) if is_invalid_signature(&e) => last = Some(e),
            Err(e) => return Err(SignatureRefusal::Settled(e)),
        }
    }
    Err(SignatureRefusal::Invalid(last))
}

/// `chain`'s signature over `request` at `now_secs`: the 6.x payload over
/// every candidate path, then the folded one as `legacy` allows.
pub async fn verify_signature(
    chain: &AuthChain,
    request: &SignedRequest<'_>,
    now_secs: i64,
    legacy: LegacyPayload,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<(), SignatureRefusal> {
    let SignedRequest {
        method,
        paths,
        timestamp,
        metadata,
    } = *request;
    let v6: Vec<String> = paths
        .iter()
        .map(|p| build_payload_v6(method, p, timestamp, metadata))
        .collect();
    let refusal = match first_verifying(chain, &v6, now_secs, validator).await {
        Err(SignatureRefusal::Invalid(last)) => last,
        settled => return settled,
    };
    match legacy {
        LegacyPayload::Refused => return Err(SignatureRefusal::Invalid(refusal)),
        LegacyPayload::Guarded {
            keys_canonical: false,
        } => return Err(SignatureRefusal::LegacyKeysNotCanonical),
        LegacyPayload::Accepted
        | LegacyPayload::Guarded {
            keys_canonical: true,
        } => {}
    }
    let folded: Vec<String> = paths
        .iter()
        .map(|p| build_legacy_payload(method, p, timestamp, metadata))
        .filter(|payload| !v6.contains(payload))
        .collect();
    if folded.is_empty() {
        return Err(SignatureRefusal::Invalid(refusal));
    }
    first_verifying(chain, &folded, now_secs, validator).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::{Wallet, create_simple_auth_chain};

    fn wallet() -> Wallet {
        Wallet::from_hex("4c0883a69102937d6231471b5dbb6204fe512961708279f2e3e8a5d4b8e3e3e3")
            .unwrap()
    }

    fn chain_signing(payload: &str) -> AuthChain {
        serde_json::from_value(create_simple_auth_chain(&wallet(), payload).unwrap()).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::try_from(*name).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    const SIGNER: &str = r#"{"type":"SIGNER","payload":"0xabc","signature":""}"#;
    const UNSIGNED: &str = r#"{"type":"ECDSA_SIGNED_ENTITY","payload":"x"}"#;

    #[test]
    fn extraction_checks_the_shape() {
        let two = |second: &str| {
            headers(&[
                ("x-identity-auth-chain-0", SIGNER),
                ("x-identity-auth-chain-1", second),
            ])
        };
        assert_eq!(
            extract_auth_chain(
                &headers(&[("x-identity-auth-chain-0", SIGNER)]),
                UnsignedLinks::Malformed
            ),
            Err(ChainShapeError::InsufficientLinks)
        );
        assert_eq!(
            extract_auth_chain(&two(SIGNER), UnsignedLinks::Malformed),
            Err(ChainShapeError::SignerNotFirst(1))
        );
        assert_eq!(
            extract_auth_chain(
                &headers(&[("x-identity-auth-chain-0", UNSIGNED)]),
                UnsignedLinks::Malformed
            ),
            Err(ChainShapeError::FirstNotSigner)
        );
        assert!(matches!(
            extract_auth_chain(&two("{"), UnsignedLinks::Malformed),
            Err(ChainShapeError::Unparseable { index: 1, .. })
        ));
        assert_eq!(
            extract_auth_chain(&two(UNSIGNED), UnsignedLinks::Malformed),
            Err(ChainShapeError::MissingSignature(1))
        );
        assert_eq!(
            extract_auth_chain(&two(UNSIGNED), UnsignedLinks::LeftToVerify)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn the_window_caps_the_future_side_and_refuses_garbage() {
        assert_eq!(check_timestamp_window("1000000", 1000, 0), Ok(()));
        assert_eq!(check_timestamp_window("700000", 1000, 300), Ok(()));
        assert!(check_timestamp_window("699000", 1000, 300).is_err());
        assert_eq!(check_timestamp_window("1060000", 1000, 300), Ok(()));
        assert!(check_timestamp_window("1061000", 1000, 300).is_err());
        assert_eq!(check_timestamp_window("1030000", 1000, 30), Ok(()));
        assert!(check_timestamp_window("1031000", 1000, 30).is_err());
        assert!(matches!(
            check_timestamp_window("1301000", 1000, 300),
            Err(SkewError::Expired {
                signed_at: 1301,
                ..
            })
        ));
        assert_eq!(
            check_timestamp_window("soon", 1000, 300),
            Err(SkewError::InvalidTimestamp("soon".into()))
        );
        assert_eq!(timestamp_or_zero(Some("")), "0");
        assert_eq!(timestamp_or_zero(None), "0");
    }

    #[test]
    fn the_past_window_bounds_only_the_age() {
        assert_eq!(check_past_window("100000", 160_000, 60_000), Ok(()));
        assert!(matches!(
            check_past_window("100000", 160_001, 60_000),
            Err(SkewError::Expired {
                signed_at: 100,
                now: 160,
                window_secs: 60
            })
        ));
        assert_eq!(check_past_window("999999999999999", 1, 60_000), Ok(()));
        for garbage in ["", "-1", "1.5", "soon"] {
            assert_eq!(
                check_past_window(garbage, 1, 60_000),
                Err(SkewError::InvalidTimestamp(garbage.into()))
            );
        }
    }

    #[test]
    fn the_scene_signer_is_found_under_any_spelling() {
        let scene = |raw: &str| names_scene_signer(&serde_json::from_str(raw).unwrap());
        assert!(scene(r#"{"signer":"decentraland-kernel-scene"}"#));
        assert!(scene(r#"{"Signer":" Decentraland-Kernel-Scene\ufeff"}"#));
        assert!(!scene("{}"));
        assert!(!scene(r#"{"Signer":"dcl:explorer"}"#));
        assert!(!scene(r#"{"signer":1}"#));
        assert!(!scene(r#""decentraland-kernel-scene""#));
    }

    #[test]
    fn metadata_is_an_object_or_null() {
        assert_eq!(parse_metadata("null"), Some(serde_json::json!({})));
        assert_eq!(
            parse_metadata(r#"{"a":1}"#),
            Some(serde_json::json!({"a": 1}))
        );
        assert_eq!(parse_metadata("[]"), None);
        assert_eq!(parse_metadata("{"), None);
    }

    #[test]
    fn the_scene_signer_and_its_respellings_are_refused() {
        let refused = |raw: &str| is_refused_signer(&serde_json::from_str(raw).unwrap());
        assert!(!refused("{}"));
        assert!(!refused(r#"{"signer":"dcl:explorer"}"#));
        assert!(refused(r#"{"signer":"decentraland-kernel-scene"}"#));
        assert!(refused(r#"{"signer":"Decentraland-Kernel-Scene"}"#));
        assert!(refused(r#"{"signer":"dcl:explorer\ufeff"}"#));
        assert!(refused(r#"{"Signer":"dcl:explorer"}"#));
        assert!(refused(r#"{"signer":1}"#));
    }

    async fn verdict(
        signed: &str,
        paths: &[&str],
        legacy: LegacyPayload,
    ) -> Result<(), SignatureRefusal> {
        let chain = chain_signing(signed);
        let request = SignedRequest {
            method: "POST",
            paths,
            timestamp: "1000",
            metadata: r#"{"Intent":"x"}"#,
        };
        verify_signature(&chain, &request, 1, legacy, None).await
    }

    #[tokio::test]
    async fn the_6x_payload_verifies_over_any_candidate_path() {
        let signed = build_payload_v6("POST", "/b", "1000", r#"{"Intent":"x"}"#);
        assert!(
            verdict(&signed, &["/a", "/b"], LegacyPayload::Refused)
                .await
                .is_ok()
        );
        assert!(matches!(
            verdict(&signed, &["/a"], LegacyPayload::Accepted).await,
            Err(SignatureRefusal::Invalid(Some(_)))
        ));
        assert!(matches!(
            verdict(&signed, &[], LegacyPayload::Accepted).await,
            Err(SignatureRefusal::Invalid(None))
        ));
    }

    #[tokio::test]
    async fn the_folded_payload_verifies_only_as_the_surface_allows() {
        let signed = build_legacy_payload("POST", "/b", "1000", r#"{"Intent":"x"}"#);
        let paths = ["/a", "/b"];
        assert!(matches!(
            verdict(&signed, &paths, LegacyPayload::Refused).await,
            Err(SignatureRefusal::Invalid(Some(_)))
        ));
        assert!(
            verdict(&signed, &paths, LegacyPayload::Accepted)
                .await
                .is_ok()
        );
        assert!(
            verdict(
                &signed,
                &paths,
                LegacyPayload::Guarded {
                    keys_canonical: true
                }
            )
            .await
            .is_ok()
        );
        assert!(matches!(
            verdict(
                &signed,
                &paths,
                LegacyPayload::Guarded {
                    keys_canonical: false
                }
            )
            .await,
            Err(SignatureRefusal::LegacyKeysNotCanonical)
        ));
    }

    #[tokio::test]
    async fn an_expired_delegation_settles_without_trying_more_payloads() {
        let ephemeral = Wallet::random().unwrap();
        let signed = build_payload_v6("POST", "/a", "1000", "{}");
        let delegation = format!(
            "Decentraland Login\nEphemeral address: {}\nExpiration: 2000-01-01T00:00:00.000Z",
            ephemeral.address()
        );
        let link = |link_type, payload: &str, signature: String| AuthLink {
            link_type,
            payload: payload.to_string(),
            signature: Some(signature),
        };
        let chain = vec![
            link(AuthLinkType::SIGNER, &wallet().address(), String::new()),
            link(
                AuthLinkType::EcdsaEphemeral,
                &delegation,
                wallet().sign_message(delegation.as_bytes()).unwrap(),
            ),
            link(
                AuthLinkType::EcdsaSignedEntity,
                &signed,
                ephemeral.sign_message(signed.as_bytes()).unwrap(),
            ),
        ];
        let request = SignedRequest {
            method: "POST",
            paths: &["/a"],
            timestamp: "1000",
            metadata: "{}",
        };
        assert!(matches!(
            verify_signature(
                &chain,
                &request,
                1_000_000_000,
                LegacyPayload::Accepted,
                None
            )
            .await,
            Err(SignatureRefusal::Settled(
                AuthError::EphemeralExpired { .. }
            ))
        ));
    }
}
