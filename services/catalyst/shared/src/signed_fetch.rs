//! Signed-fetch verification for the explore routes: the `x-identity-*`
//! headers carry an auth chain over `method:path:timestamp:metadata`. The
//! viewer's address decides what a read shows (favorites, likes, attendance,
//! own pending events), so a chain that does not verify reads as anonymous on
//! the optional path and as 401 on the required one. The steps are
//! `dcl_auth::signed_fetch`'s; this module is the node's posture over
//! them and its wire answers.
//!
//! There is no nonce and no replay cache on this path: the +/- 5 minute window
//! is the only bound on how long a captured request stays usable, so it is a
//! replay-tolerance budget, not a clock-skew allowance. The payload shapes a
//! surface accepts are its [`PayloadPolicy`]: the crypto-middleware 6.x one
//! (method and path folded, the metadata bytes bound to the signature) always,
//! then the legacy one (everything lowercased) only where the old service
//! declared that fallback and only when the metadata spells its `signer` key
//! canonically, which keeps the fold from re-spelling a gated value. The
//! signature binds the route, under ONE path rule for every signed surface
//! ([`SignedPath`], [`candidate_paths`]): the path the request arrived on,
//! its gateway prefix included (`OriginalUri`), is what a client of this
//! listener signs; the route-local path is accepted only through the
//! proxy's `x-original-path` (the deploy tree sets it to the client's
//! request URI on every node location, so a root alias nginx rewrites to
//! its prefixed twin still verifies), and only when that header ends with
//! the route. A request a scene
//! runtime signed on a visiting player's behalf (`{"signer":
//! "decentraland-kernel-scene"}`) is refused rather than served as that player,
//! as upstream decentraland-gatsby does on every authed route.
//!
//! EIP-1654 (contract wallet) links verify through the [`Eip1654Validator`]
//! the caller passes; with `None` they fail closed and the optional path reads
//! them as anonymous.
//!
//! All of the above is the posture of [`PayloadPolicy`] callers, the profile
//! [`VerificationProfile::node`]. A surface whose upstream verified with
//! another middleware states its rules as a [`VerificationProfile`] instead
//! ([`VerificationProfile::camera_reel_read`],
//! [`VerificationProfile::camera_reel_write`]) and calls [`require_signer_with`];
//! the chain shape, the path rule, the payload builders and the signature
//! steps stay this module's.

use axum::http::HeaderMap;
use thiserror::Error;

use crate::auth::eip1654::Eip1654Validator;
use crate::auth::signed_fetch::{
    self as core, ChainShapeError, LegacyPayload, SignatureRefusal, SignedRequest, SkewError,
    UnsignedLinks,
};
use crate::auth::{AuthChain, AuthError, MAX_AUTH_CHAIN_LINKS};

pub use crate::auth::signed_fetch::{
    AUTH_CHAIN_HEADER_PREFIX, AUTH_METADATA_HEADER, AUTH_TIMESTAMP_HEADER, FIVE_MINUTES,
    ORIGINAL_PATH_HEADER, SCENE_SIGNER, SignedPath, build_legacy_payload, build_payload_v6,
    candidate_paths, field_is_canonical, header_str, is_refused_signer, names_scene_signer,
};

/// dcl-crypto-middleware-rs's default expiration: a signature older than
/// this is stale.
pub const SIXTY_SECONDS_MS: u64 = 60 * 1000;

const DETAIL_MAX_CHARS: usize = 64;

#[derive(Debug, Error)]
pub enum SignedFetchError {
    #[error("Invalid Auth Chain")]
    MalformedChain { detail: String },
    #[error("Invalid Auth Chain")]
    InsufficientLinks,
    #[error("Invalid timestamp")]
    InvalidTimestamp(String),
    #[error("Expired signature")]
    Expired {
        signed_at: i64,
        now: i64,
        window_secs: i64,
    },
    #[error("Invalid signature")]
    InvalidSignature(String),
    #[error("EIP-1654 chains need an RPC validator")]
    EipNotImplemented,
    #[error("Catalyst unavailable: {0}")]
    CatalystUnavailable(String),
}

impl SignedFetchError {
    /// A malformed request answers 400 with its detail; every other refusal is
    /// 401 Unauthenticated, as upstream answers them.
    pub fn http_status_and_message(&self) -> (u16, String) {
        match self {
            Self::MalformedChain { detail } => (400, format!("Invalid Auth Chain: {detail}")),
            Self::InsufficientLinks => (400, "Invalid Auth Chain".to_string()),
            Self::InvalidTimestamp(v) => (400, format!("Invalid timestamp: {v}")),
            Self::Expired { .. } => (401, "Unauthenticated: Expired signature".to_string()),
            Self::InvalidSignature(_) | Self::EipNotImplemented => {
                (401, "Unauthenticated: Invalid signature".to_string())
            }
            Self::CatalystUnavailable(d) => (503, format!("Catalyst unavailable: {d}")),
        }
    }
}

impl From<ChainShapeError> for SignedFetchError {
    fn from(e: ChainShapeError) -> Self {
        let detail = match e {
            ChainShapeError::InsufficientLinks => return Self::InsufficientLinks,
            ChainShapeError::Unparseable { detail, .. } => truncate_detail(&detail),
            ChainShapeError::SignerNotFirst(i) => format!("SIGNER link at non-zero index {i}"),
            ChainShapeError::FirstNotSigner => "first link must be SIGNER".to_string(),
            ChainShapeError::MissingSignature(i) => format!("missing signature on link {i}"),
            ChainShapeError::TooLong => format!("exceeds max length of {MAX_AUTH_CHAIN_LINKS}"),
        };
        Self::MalformedChain { detail }
    }
}

impl From<SkewError> for SignedFetchError {
    fn from(e: SkewError) -> Self {
        match e {
            SkewError::InvalidTimestamp(timestamp) => Self::InvalidTimestamp(timestamp),
            SkewError::Expired {
                signed_at,
                now,
                window_secs,
            } => Self::Expired {
                signed_at,
                now,
                window_secs,
            },
        }
    }
}

/// A verified signer: the lowercased address of the chain's SIGNER link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer(String);

impl Signer {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// `value` cut to its first 64 characters (`...` marks the cut): a refusal's
/// detail never echoes a whole client-sent header.
pub fn truncate_detail(value: &str) -> String {
    if value.chars().count() > DETAIL_MAX_CHARS {
        let head: String = value.chars().take(DETAIL_MAX_CHARS).collect();
        format!("{head}...")
    } else {
        value.to_string()
    }
}

/// The first link's address as the headers claim it, unverified: the old
/// federation writes compared it with the envelope signer.
pub fn auth_chain_claimed_address(headers: &HeaderMap) -> Option<String> {
    let raw = header_str(headers, &format!("{AUTH_CHAIN_HEADER_PREFIX}0"))?;
    let link: serde_json::Value = serde_json::from_str(raw).ok()?;
    let addr = link.get("payload").and_then(|p| p.as_str())?;
    crate::auth::is_eth_address(addr).then(|| addr.to_lowercase())
}

pub fn extract_auth_chain(headers: &HeaderMap) -> Result<AuthChain, SignedFetchError> {
    Ok(core::extract_auth_chain(headers, UnsignedLinks::Malformed)?)
}

/// A millisecond timestamp header is fresh when it is at most `window_secs`
/// old and at most `min(window_secs, MAX_FUTURE_SKEW_SECS)` ahead; a
/// non-numeric one is rejected rather than skipping the window.
pub fn check_timestamp_window(
    timestamp: &str,
    now: i64,
    window_secs: i64,
) -> Result<(), SignedFetchError> {
    Ok(core::check_timestamp_window(timestamp, now, window_secs)?)
}

/// Unparseable or non-object metadata is refused before anything is verified;
/// an explicit JSON `null` reads as an empty object.
pub fn parse_metadata(raw: &str) -> Result<serde_json::Value, SignedFetchError> {
    core::parse_metadata(raw).ok_or_else(|| SignedFetchError::MalformedChain {
        detail: format!("invalid chain metadata: \"{}\"", truncate_detail(raw)),
    })
}

/// An auth-chain verification failure as this module's refusal, which
/// [`SignedFetchError::http_status_and_message`] answers as 400, 401 or 503.
pub fn map_auth_error(err: AuthError) -> SignedFetchError {
    match err {
        AuthError::MalformedChain(d) => SignedFetchError::MalformedChain { detail: d },
        AuthError::MissingSignature { .. } => SignedFetchError::MalformedChain {
            detail: err.to_string(),
        },
        AuthError::RecoveryFailed(d) => SignedFetchError::InvalidSignature(d),
        AuthError::SignerMismatch { .. } | AuthError::FinalAuthorityMismatch { .. } => {
            SignedFetchError::InvalidSignature(err.to_string())
        }
        AuthError::EphemeralExpired {
            expiration_ms,
            now_ms,
        } => SignedFetchError::Expired {
            signed_at: expiration_ms / 1000,
            now: now_ms / 1000,
            window_secs: 0,
        },
        AuthError::InvalidEphemeralPayload(d) => SignedFetchError::MalformedChain { detail: d },
        AuthError::Eip1654NotImplemented => SignedFetchError::EipNotImplemented,
        AuthError::Eip1654Rejected { .. } => SignedFetchError::InvalidSignature(err.to_string()),
        AuthError::Eip1654ValidationFailed(d) => SignedFetchError::CatalystUnavailable(d),
    }
}

/// Which payload shapes a surface accepts: the crypto-middleware 6.x one
/// alone (the old social-service verified "the 6.x payload only"), or 6.x
/// first and then the pre-6.0.0 folded one behind the canonical-`signer`
/// guard (the explore reads and `PUT /notifications/read`, whose upstreams
/// still declare the legacy fallback; the other notifications routes take 6.x only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadPolicy {
    V6Only,
    V6ThenLegacy,
}

/// How fresh the `x-identity-timestamp` must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimestampWindow {
    /// At most this many seconds old and at most `min(secs,
    /// MAX_FUTURE_SKEW_SECS)` ahead; an absent or empty header reads as 0 and
    /// expires ([`check_timestamp_window`]).
    AroundNow { secs: i64 },
    /// Required, an unsigned integer, and no more than this many milliseconds
    /// old; any future timestamp is fresh ([`crate::auth::signed_fetch::check_past_window`]).
    PastOnly { max_age_ms: u64 },
}

/// What the `x-identity-metadata` header must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataHeader {
    /// Absent reads as `{}`; present, it must be a JSON object (or `null`).
    OptionalJsonObject,
    /// Present, any string: the bytes are signed, never required to parse.
    RequiredRaw,
}

/// What a request a scene runtime signed (`{"signer":
/// "decentraland-kernel-scene"}`) gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SceneSignerRule {
    /// Upstream gatsby's gate ([`is_refused_signer`]): the scene signer and
    /// any non-canonical spelling of the `signer` key or value are refused,
    /// and so is metadata that is not an object.
    Gated,
    /// The scene signer under any spelling is refused: metadata whose
    /// object names it ([`names_scene_signer`]), and any metadata whose
    /// lowercased bytes contain it. The second test is the one that holds
    /// against a replay: the legacy payload folds case, so a scene-signed
    /// header replayed with a literal uppercased (`false` -> `FALSE`) no
    /// longer parses yet still verifies, and serde keeps the last of
    /// duplicate `signer` keys; neither hides the signer from the bytes.
    Refused,
    /// Served as the chain's signer.
    Allowed,
}

/// Whether the pre-6.0.0 folded payload is tried once the 6.x ones failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyFallback {
    Refused,
    /// Only when the metadata is an object spelling its `signer` key and
    /// value canonically ([`field_is_canonical`]).
    GuardedSigner,
    /// With no guard: for a surface that authorizes nothing on the metadata
    /// (dcl-crypto-middleware-rs `accept_legacy_payload(&[])`).
    Unguarded,
}

/// A surface's signed-fetch rules: the stages [`require_signer_with`] runs
/// over the shared chain shape, path rule and signature steps. Only the
/// constructors below build one, so only their combinations exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationProfile {
    window: TimestampWindow,
    metadata: MetadataHeader,
    scene_signer: SceneSignerRule,
    legacy: LegacyFallback,
}

impl VerificationProfile {
    /// @dcl/platform-crypto-middleware as the Node services use it: +/- 5
    /// minutes, optional object metadata, the scene signer gated, and the
    /// legacy payload as `policy` declares it behind the canonical-`signer`
    /// guard. What [`require_signer_at`] runs.
    pub const fn node(policy: PayloadPolicy) -> Self {
        Self {
            window: TimestampWindow::AroundNow { secs: FIVE_MINUTES },
            metadata: MetadataHeader::OptionalJsonObject,
            scene_signer: SceneSignerRule::Gated,
            legacy: match policy {
                PayloadPolicy::V6Only => LegacyFallback::Refused,
                PayloadPolicy::V6ThenLegacy => LegacyFallback::GuardedSigner,
            },
        }
    }

    /// camera-reel-service's dcl-crypto-middleware-rs 0.3.0
    /// (`VerificationOptions::default().accept_legacy_payload(&[])`) for its
    /// reads: a signature at most 60 s old and never too new, the metadata
    /// header required but unread, the legacy payload unguarded, and a
    /// scene's request served as its signer, as upstream serves it.
    pub const fn camera_reel_read() -> Self {
        Self::camera_reel(SceneSignerRule::Allowed)
    }

    /// [`Self::camera_reel_read`] for the writes, with the scene signer
    /// refused (upstream never checks it).
    pub const fn camera_reel_write() -> Self {
        Self::camera_reel(SceneSignerRule::Refused)
    }

    const fn camera_reel(scene_signer: SceneSignerRule) -> Self {
        Self {
            window: TimestampWindow::PastOnly {
                max_age_ms: SIXTY_SECONDS_MS,
            },
            metadata: MetadataHeader::RequiredRaw,
            scene_signer,
            legacy: LegacyFallback::Unguarded,
        }
    }
}

/// [`require_signer_at`] for a route mounted at the root (`path` is both
/// the request's and the route's).
pub async fn require_signer(
    headers: &HeaderMap,
    method: &str,
    path: &str,
    policy: PayloadPolicy,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<Signer, SignedFetchError> {
    require_signer_at(headers, method, &SignedPath::local(path), policy, validator).await
}

/// [`require_signer_with`] under [`VerificationProfile::node`]`(policy)`.
pub async fn require_signer_at(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    policy: PayloadPolicy,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<Signer, SignedFetchError> {
    require_signer_with(
        headers,
        method,
        path,
        &VerificationProfile::node(policy),
        validator,
    )
    .await
}

/// Stage order matches upstream `verify()`: chain extraction, timestamp,
/// expiration, metadata, scene gate, signature (6.x first, then the legacy
/// one as `profile` allows) over the [`candidate_paths`] of `path`.
pub async fn require_signer_with(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    profile: &VerificationProfile,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<Signer, SignedFetchError> {
    let chain = extract_auth_chain(headers)?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now = now_ms.div_euclid(1000);
    let ts = match profile.window {
        TimestampWindow::AroundNow { secs } => {
            let ts = core::timestamp_or_zero(header_str(headers, AUTH_TIMESTAMP_HEADER));
            check_timestamp_window(ts, now, secs)?;
            ts
        }
        TimestampWindow::PastOnly { max_age_ms } => {
            let ts = header_str(headers, AUTH_TIMESTAMP_HEADER)
                .ok_or_else(|| SignedFetchError::InvalidTimestamp(String::new()))?;
            core::check_past_window(ts, now_ms.max(0) as u128, u128::from(max_age_ms))?;
            ts
        }
    };

    let (metadata_raw, metadata) = match profile.metadata {
        MetadataHeader::OptionalJsonObject => {
            let raw = header_str(headers, AUTH_METADATA_HEADER).unwrap_or("{}");
            (raw, Some(parse_metadata(raw)?))
        }
        MetadataHeader::RequiredRaw => {
            let raw = header_str(headers, AUTH_METADATA_HEADER).ok_or_else(|| {
                SignedFetchError::MalformedChain {
                    detail: format!("missing {AUTH_METADATA_HEADER}"),
                }
            })?;
            (raw, core::parse_metadata(raw))
        }
    };
    let scene = match profile.scene_signer {
        SceneSignerRule::Gated => metadata.as_ref().is_none_or(is_refused_signer),
        SceneSignerRule::Refused => {
            metadata.as_ref().is_some_and(names_scene_signer)
                || metadata_raw.to_lowercase().contains(SCENE_SIGNER)
        }
        SceneSignerRule::Allowed => false,
    };
    if scene {
        return Err(SignedFetchError::MalformedChain {
            detail: format!(
                "invalid metadata content: \"{}\"",
                truncate_detail(metadata_raw)
            ),
        });
    }

    let legacy = match profile.legacy {
        LegacyFallback::Refused => LegacyPayload::Refused,
        LegacyFallback::GuardedSigner => LegacyPayload::Guarded {
            keys_canonical: metadata
                .as_ref()
                .is_some_and(|m| field_is_canonical(m, "signer")),
        },
        LegacyFallback::Unguarded => LegacyPayload::Accepted,
    };
    let request = SignedRequest {
        method,
        paths: &candidate_paths(headers, path),
        timestamp: ts,
        metadata: metadata_raw,
    };
    core::verify_signature(&chain, &request, now, legacy, validator)
        .await
        .map_err(|refusal| match refusal {
            SignatureRefusal::Invalid(Some(e)) | SignatureRefusal::Settled(e) => map_auth_error(e),
            SignatureRefusal::Invalid(None) => {
                SignedFetchError::InvalidSignature("no payload".into())
            }
            SignatureRefusal::LegacyKeysNotCanonical => SignedFetchError::MalformedChain {
                detail: "non-canonical metadata key: signer".to_string(),
            },
        })?;
    Ok(Signer(chain[0].payload.to_lowercase()))
}

/// [`optional_signer_at`] for a route mounted at the root.
pub async fn optional_signer(
    headers: &HeaderMap,
    method: &str,
    path: &str,
    policy: PayloadPolicy,
    validator: Option<&dyn Eip1654Validator>,
) -> Option<Signer> {
    optional_signer_at(headers, method, &SignedPath::local(path), policy, validator).await
}

/// Upstream's optional path swallows every refusal to ANONYMOUS, never to an
/// authenticated caller. A request without a chain is anonymous at no cost.
pub async fn optional_signer_at(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    policy: PayloadPolicy,
    validator: Option<&dyn Eip1654Validator>,
) -> Option<Signer> {
    headers.get(format!("{AUTH_CHAIN_HEADER_PREFIX}0"))?;
    match require_signer_at(headers, method, path, policy, validator).await {
        Ok(signer) => Some(signer),
        Err(e) => {
            tracing::debug!(error = %e, "optional signed-fetch verification failed");
            None
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use crate::auth::{AuthLink, AuthLinkType};
    use alloy::signers::{Signer as _, local::PrivateKeySigner};
    use axum::http::{HeaderName, HeaderValue};

    pub const KEY: &str = "0x4c0883a69102937d6231471b5dbb6204fe512961708279f2e3e8a5d4b8e3e3e3";

    pub fn wallet() -> PrivateKeySigner {
        KEY.parse().unwrap()
    }

    pub fn address() -> String {
        format!("{:#x}", wallet().address())
    }

    /// Headers a client signs for `method path` now, with `metadata`, in the
    /// 6.x or the legacy payload shape.
    pub async fn signed_headers(method: &str, path: &str, metadata: &str, v6: bool) -> HeaderMap {
        let timestamp = chrono::Utc::now().timestamp_millis().to_string();
        signed_headers_at(method, path, metadata, v6, &timestamp).await
    }

    pub async fn signed_headers_at(
        method: &str,
        path: &str,
        metadata: &str,
        v6: bool,
        timestamp: &str,
    ) -> HeaderMap {
        signed_headers_by(&wallet(), method, path, metadata, v6, timestamp).await
    }

    /// As [`signed_headers_at`], signed by `wallet`.
    pub async fn signed_headers_by(
        wallet: &PrivateKeySigner,
        method: &str,
        path: &str,
        metadata: &str,
        v6: bool,
        timestamp: &str,
    ) -> HeaderMap {
        let payload = if v6 {
            build_payload_v6(method, path, timestamp, metadata)
        } else {
            build_legacy_payload(method, path, timestamp, metadata)
        };
        let signature = wallet
            .sign_message(payload.as_bytes())
            .await
            .unwrap()
            .to_string();
        let chain = [
            AuthLink {
                link_type: AuthLinkType::SIGNER,
                payload: format!("{:#x}", wallet.address()),
                signature: None,
            },
            AuthLink {
                link_type: AuthLinkType::EcdsaSignedEntity,
                payload,
                signature: Some(signature),
            },
        ];
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTH_TIMESTAMP_HEADER,
            HeaderValue::from_str(timestamp).unwrap(),
        );
        headers.insert(
            AUTH_METADATA_HEADER,
            HeaderValue::from_str(metadata).unwrap(),
        );
        for (i, link) in chain.iter().enumerate() {
            headers.insert(
                HeaderName::from_bytes(format!("{AUTH_CHAIN_HEADER_PREFIX}{i}").as_bytes())
                    .unwrap(),
                HeaderValue::from_str(&serde_json::to_string(link).unwrap()).unwrap(),
            );
        }
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use axum::http::HeaderValue;

    const PATH: &str = "/api/places/1/likes";

    #[tokio::test]
    async fn an_ordinary_signer_verifies_under_either_payload_shape() {
        let metadata = r#"{"signer":"dcl:explorer","intent":"dcl:places:like"}"#;
        for v6 in [true, false] {
            let headers = signed_headers("patch", PATH, metadata, v6).await;
            let signer = require_signer(&headers, "patch", PATH, PayloadPolicy::V6ThenLegacy, None)
                .await
                .unwrap_or_else(|e| panic!("v6={v6}: {e}"));
            assert_eq!(signer.as_str(), address().to_lowercase());
            assert_eq!(
                optional_signer(&headers, "patch", PATH, PayloadPolicy::V6ThenLegacy, None)
                    .await
                    .map(|s| s.into_string()),
                Some(address().to_lowercase())
            );
        }
    }

    #[tokio::test]
    async fn a_scene_signed_request_is_refused_and_reads_as_anonymous() {
        for metadata in [
            r#"{"signer":"decentraland-kernel-scene"}"#,
            r#"{"Signer":"decentraland-kernel-scene"}"#,
        ] {
            for v6 in [true, false] {
                let headers = signed_headers("patch", PATH, metadata, v6).await;
                let err =
                    require_signer(&headers, "patch", PATH, PayloadPolicy::V6ThenLegacy, None)
                        .await
                        .expect_err("a scene-signed request must not be served");
                assert_eq!(err.http_status_and_message().0, 400, "v6={v6}: {err}");
                assert!(
                    optional_signer(&headers, "patch", PATH, PayloadPolicy::V6ThenLegacy, None)
                        .await
                        .is_none()
                );
            }
        }
    }

    #[tokio::test]
    async fn a_wrong_method_or_path_is_an_invalid_signature() {
        let headers = signed_headers("get", PATH, "{}", true).await;
        let err = require_signer(&headers, "post", PATH, PayloadPolicy::V6ThenLegacy, None)
            .await
            .unwrap_err();
        assert_eq!(err.http_status_and_message().0, 401);
        let err = require_signer(
            &headers,
            "get",
            "/api/places/2/likes",
            PayloadPolicy::V6ThenLegacy,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.http_status_and_message().0, 401);
        assert!(
            optional_signer(&headers, "post", PATH, PayloadPolicy::V6ThenLegacy, None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_public_path_a_proxy_forwards_is_accepted_over_the_route_path() {
        let mut headers = signed_headers("get", "/places/api/places", "{}", true).await;
        headers.insert(
            ORIGINAL_PATH_HEADER,
            HeaderValue::from_static("/places/api/places"),
        );
        assert!(
            require_signer(
                &headers,
                "get",
                "/api/places",
                PayloadPolicy::V6ThenLegacy,
                None
            )
            .await
            .is_ok()
        );
        let headers = signed_headers("get", "/api/places", "{}", false).await;
        assert!(
            require_signer(
                &headers,
                "get",
                "/api/places",
                PayloadPolicy::V6ThenLegacy,
                None
            )
            .await
            .is_ok()
        );
    }

    /// A signature captured for one route must not open another: the header
    /// is client-controlled, so only a prefix in front of the route is tried.
    #[tokio::test]
    async fn a_forged_x_original_path_does_not_rebind_the_signature_to_another_route() {
        let mut headers = signed_headers("post", "/v1/mutes", "{}", true).await;
        headers.insert(ORIGINAL_PATH_HEADER, HeaderValue::from_static("/v1/mutes"));
        let err = require_signer(
            &headers,
            "post",
            "/v1/communities/x/posts",
            PayloadPolicy::V6ThenLegacy,
            None,
        )
        .await
        .expect_err("a chain over /v1/mutes must not authorize a post");
        assert_eq!(err.http_status_and_message().0, 401, "{err}");
        for forged in [
            "/v1/mutes?x=/v1/communities/x/posts",
            "/v1/communities/x/posts/../../mutes",
            "",
        ] {
            headers.insert(ORIGINAL_PATH_HEADER, HeaderValue::from_str(forged).unwrap());
            assert!(
                require_signer(
                    &headers,
                    "post",
                    "/v1/communities/x/posts",
                    PayloadPolicy::V6ThenLegacy,
                    None
                )
                .await
                .is_err(),
                "{forged:?}"
            );
        }
        let headers = signed_headers("get", "/social-api/v1/mutes?page=1", "{}", true).await;
        let mut with_query = headers.clone();
        with_query.insert(
            ORIGINAL_PATH_HEADER,
            HeaderValue::from_static("/social-api/v1/mutes?page=1"),
        );
        assert!(
            require_signer(
                &with_query,
                "get",
                "/v1/mutes",
                PayloadPolicy::V6ThenLegacy,
                None
            )
            .await
            .is_err(),
            "the query is stripped from the candidate, so a signature over it does not verify"
        );
        let headers = signed_headers("get", "/social-api/v1/mutes", "{}", true).await;
        let mut with_query = headers.clone();
        with_query.insert(
            ORIGINAL_PATH_HEADER,
            HeaderValue::from_static("/social-api/v1/mutes?page=1"),
        );
        assert!(
            require_signer(
                &with_query,
                "get",
                "/v1/mutes",
                PayloadPolicy::V6ThenLegacy,
                None
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn the_v6_only_policy_refuses_a_legacy_only_chain() {
        let metadata = r#"{"signer":"dcl:explorer","intent":"Probe-Legacy"}"#;
        let headers = signed_headers("get", PATH, metadata, false).await;
        assert!(
            require_signer(&headers, "get", PATH, PayloadPolicy::V6ThenLegacy, None)
                .await
                .is_ok()
        );
        let err = require_signer(&headers, "get", PATH, PayloadPolicy::V6Only, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SignedFetchError::InvalidSignature(_)),
            "{err}"
        );
        assert_eq!(err.http_status_and_message().0, 401);
        assert!(
            optional_signer(&headers, "get", PATH, PayloadPolicy::V6Only, None)
                .await
                .is_none()
        );
        let headers = signed_headers("get", PATH, metadata, true).await;
        assert!(
            require_signer(&headers, "get", PATH, PayloadPolicy::V6Only, None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn stale_and_unparseable_timestamps_are_refused() {
        let stale = (chrono::Utc::now().timestamp_millis() - 10 * 60 * 1000).to_string();
        let headers = signed_headers_at("get", PATH, "{}", true, &stale).await;
        let err = require_signer(&headers, "get", PATH, PayloadPolicy::V6ThenLegacy, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SignedFetchError::Expired { .. }), "{err}");
        assert_eq!(err.http_status_and_message().0, 401);

        let mut headers = signed_headers("get", PATH, "{}", true).await;
        headers.insert(AUTH_TIMESTAMP_HEADER, HeaderValue::from_static("yesterday"));
        let err = require_signer(&headers, "get", PATH, PayloadPolicy::V6ThenLegacy, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SignedFetchError::InvalidTimestamp(_)),
            "{err}"
        );

        let mut headers = signed_headers("get", PATH, "{}", true).await;
        headers.remove(AUTH_TIMESTAMP_HEADER);
        assert!(matches!(
            require_signer(&headers, "get", PATH, PayloadPolicy::V6ThenLegacy, None)
                .await
                .unwrap_err(),
            SignedFetchError::Expired { .. }
        ));
    }

    #[tokio::test]
    async fn a_request_without_a_chain_is_anonymous_and_a_short_chain_is_malformed() {
        assert!(
            optional_signer(
                &HeaderMap::new(),
                "get",
                PATH,
                PayloadPolicy::V6ThenLegacy,
                None
            )
            .await
            .is_none()
        );
        assert!(matches!(
            require_signer(
                &HeaderMap::new(),
                "get",
                PATH,
                PayloadPolicy::V6ThenLegacy,
                None
            )
            .await
            .unwrap_err(),
            SignedFetchError::InsufficientLinks
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-identity-auth-chain-0",
            HeaderValue::from_static(
                r#"{"type":"SIGNER","payload":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"}"#,
            ),
        );
        assert_eq!(
            auth_chain_claimed_address(&headers).as_deref(),
            Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
        );
        assert!(
            optional_signer(&headers, "get", PATH, PayloadPolicy::V6ThenLegacy, None)
                .await
                .is_none()
        );
        headers.insert(
            "x-identity-auth-chain-0",
            HeaderValue::from_static(
                r#"{"type":"ECDSA_SIGNED_ENTITY","payload":"x","signature":"0x00"}"#,
            ),
        );
        assert!(matches!(
            extract_auth_chain(&headers).unwrap_err(),
            SignedFetchError::MalformedChain { .. }
        ));
    }

    fn ms_from_now(offset_ms: i64) -> String {
        (chrono::Utc::now().timestamp_millis() + offset_ms).to_string()
    }

    async fn camera_reel(
        headers: &HeaderMap,
        method: &str,
        scene: SceneSignerRule,
    ) -> Result<Signer, SignedFetchError> {
        require_signer_with(
            headers,
            method,
            &SignedPath::local(PATH),
            &VerificationProfile::camera_reel(scene),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn the_camera_reel_window_bounds_only_the_age() {
        let at = |offset_ms| async move {
            let headers = signed_headers_at("get", PATH, "{}", true, &ms_from_now(offset_ms)).await;
            (
                camera_reel(&headers, "get", SceneSignerRule::Refused).await,
                require_signer(&headers, "get", PATH, PayloadPolicy::V6ThenLegacy, None).await,
            )
        };
        let (reel, _) = at(-55_000).await;
        assert!(reel.is_ok(), "{reel:?}");
        let (reel, node) = at(-65_000).await;
        assert!(
            matches!(reel, Err(SignedFetchError::Expired { .. })),
            "{reel:?}"
        );
        assert!(node.is_ok(), "the node window is still five minutes");
        let (reel, node) = at(3_600_000).await;
        assert!(reel.is_ok(), "a future signature is fresh: {reel:?}");
        assert!(
            matches!(node, Err(SignedFetchError::Expired { .. })),
            "the node window still bounds the future: {node:?}"
        );

        for timestamp in [None, Some(""), Some("-1"), Some("yesterday")] {
            let mut headers = signed_headers("get", PATH, "{}", true).await;
            match timestamp {
                None => headers.remove(AUTH_TIMESTAMP_HEADER),
                Some(raw) => headers.insert(AUTH_TIMESTAMP_HEADER, HeaderValue::from_static(raw)),
            };
            assert!(
                matches!(
                    camera_reel(&headers, "get", SceneSignerRule::Allowed).await,
                    Err(SignedFetchError::InvalidTimestamp(_))
                ),
                "{timestamp:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_camera_reel_metadata_is_required_but_never_parsed() {
        let mut headers = signed_headers("get", PATH, "{}", true).await;
        headers.remove(AUTH_METADATA_HEADER);
        assert!(
            camera_reel(&headers, "get", SceneSignerRule::Allowed)
                .await
                .is_err()
        );
        assert!(
            require_signer(&headers, "get", PATH, PayloadPolicy::V6ThenLegacy, None)
                .await
                .is_ok(),
            "the node profile reads an absent header as {{}}"
        );

        for metadata in ["not json", "[1,2]", "", r#"{"Signer":"Dcl:Explorer"}"#] {
            let headers = signed_headers("get", PATH, metadata, true).await;
            for scene in [SceneSignerRule::Allowed, SceneSignerRule::Refused] {
                let signer = camera_reel(&headers, "get", scene)
                    .await
                    .unwrap_or_else(|e| panic!("{metadata:?} {scene:?}: {e}"));
                assert_eq!(signer.as_str(), address().to_lowercase());
            }
        }
    }

    #[tokio::test]
    async fn the_camera_reel_legacy_payload_has_no_canonical_key_guard() {
        let metadata = r#"{"Signer":"Dcl:Explorer","Intent":"Upload"}"#;
        let headers = signed_headers("post", PATH, metadata, false).await;
        assert!(
            camera_reel(&headers, "post", SceneSignerRule::Refused)
                .await
                .is_ok()
        );
        assert!(matches!(
            require_signer(&headers, "post", PATH, PayloadPolicy::V6ThenLegacy, None)
                .await
                .unwrap_err(),
            SignedFetchError::MalformedChain { .. }
        ));
    }

    #[tokio::test]
    async fn the_camera_reel_scene_signer_is_served_on_reads_and_refused_on_writes() {
        for metadata in [
            r#"{"signer":"decentraland-kernel-scene"}"#,
            r#"{"Signer":"Decentraland-Kernel-Scene"}"#,
        ] {
            for v6 in [true, false] {
                let headers = signed_headers("post", PATH, metadata, v6).await;
                let signer = camera_reel(&headers, "post", SceneSignerRule::Allowed)
                    .await
                    .unwrap_or_else(|e| panic!("{metadata} v6={v6}: {e}"));
                assert_eq!(signer.as_str(), address().to_lowercase());
                assert!(
                    camera_reel(&headers, "post", SceneSignerRule::Refused)
                        .await
                        .is_err(),
                    "{metadata} v6={v6}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_replayed_scene_signature_stays_refused_on_writes_whatever_the_metadata_parses_to() {
        // A legacy-signed scene request whose replay uppercases a literal: the
        // metadata no longer parses, the folded payload still verifies.
        let signed = r#"{"signer":"decentraland-kernel-scene","isGuest":false}"#;
        let mut headers = signed_headers("patch", PATH, signed, false).await;
        headers.insert(
            AUTH_METADATA_HEADER,
            HeaderValue::from_static(r#"{"signer":"decentraland-kernel-scene","isGuest":FALSE}"#),
        );
        assert!(
            camera_reel(&headers, "patch", SceneSignerRule::Allowed)
                .await
                .is_ok(),
            "the replay verifies"
        );
        assert!(matches!(
            camera_reel(&headers, "patch", SceneSignerRule::Refused).await,
            Err(SignedFetchError::MalformedChain { .. })
        ));

        // serde keeps the last of duplicate keys.
        for v6 in [true, false] {
            let headers = signed_headers(
                "patch",
                PATH,
                r#"{"signer":"decentraland-kernel-scene","signer":"dcl:explorer"}"#,
                v6,
            )
            .await;
            assert!(
                camera_reel(&headers, "patch", SceneSignerRule::Refused)
                    .await
                    .is_err(),
                "duplicate signer keys, v6={v6}"
            );
        }
    }

    #[test]
    fn the_node_profile_is_the_payload_policy_posture() {
        for (policy, legacy) in [
            (PayloadPolicy::V6Only, LegacyFallback::Refused),
            (PayloadPolicy::V6ThenLegacy, LegacyFallback::GuardedSigner),
        ] {
            assert_eq!(
                VerificationProfile::node(policy),
                VerificationProfile {
                    window: TimestampWindow::AroundNow { secs: FIVE_MINUTES },
                    metadata: MetadataHeader::OptionalJsonObject,
                    scene_signer: SceneSignerRule::Gated,
                    legacy,
                }
            );
        }
    }

    #[test]
    fn metadata_gate_refuses_non_canonical_spellings_and_non_objects() {
        assert!(!is_refused_signer(&serde_json::json!({})));
        assert!(!is_refused_signer(
            &serde_json::json!({"signer": "dcl:explorer"})
        ));
        assert!(is_refused_signer(
            &serde_json::json!({"signer": "decentraland-kernel-scene"})
        ));
        assert!(is_refused_signer(
            &serde_json::json!({"signer": "Decentraland-Kernel-Scene"})
        ));
        assert!(is_refused_signer(&serde_json::json!({"Signer": "x"})));
        assert!(is_refused_signer(
            &serde_json::json!({"signer": "\u{FEFF}x"})
        ));
        assert!(is_refused_signer(&serde_json::json!({"signer": 1})));
        assert!(parse_metadata("[]").is_err());
        assert!(parse_metadata("null").unwrap().is_object());
        assert!(parse_metadata("{").is_err());
    }
}
