//! The gatekeeper's three credentials, each answering with the caller it
//! established or exactly why it could not:
//!
//! - a signed-fetch auth chain (`x-identity-*`) over `method:path:timestamp:
//!   metadata`, verified through `crate::auth` (the same chain verifier the
//!   content write path uses) over the shared path rule
//!   (`shared::signed_fetch::candidate_paths`: the path the request arrived
//!   on, `/comms-gatekeeper/x` behind the gateway, and the proxy's
//!   `x-original-path`) with the comms-gatekeeper posture: a 60 s
//!   freshness window on the explorer/scene routes, the `signer` allow-list
//!   the route declares, the 6.x payload first and the legacy (folded) one
//!   only when every key in [`CANONICAL_METADATA_KEYS`] is delivered in its
//!   declared spelling;
//! - the platform moderator credential (`MODERATOR_TOKEN` bearer, or a
//!   signed fetch by a `PLATFORM_USER_MODERATORS` wallet);
//! - the platform service credential (`COMMS_GATEKEEPER_AUTH_TOKEN` bearer)
//!   in front of the voice routes and the world ban-status route.
//!
//! No refusal echoes the metadata or the signature detail.

use axum::http::HeaderMap;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::admin_bearer::Refusal;
use crate::auth::eip1654::Eip1654Validator;
use crate::auth::signed_fetch::{
    LegacyPayload, SignatureRefusal, SignedRequest, parse_metadata, timestamp_or_zero,
    verify_signature,
};
use crate::auth::{AuthChain, AuthError, AuthLinkType};
use crate::shared::signed_fetch::{
    self, AUTH_METADATA_HEADER, AUTH_TIMESTAMP_HEADER, FIVE_MINUTES,
    SignedFetchError as ChainError, SignedPath, candidate_paths, check_timestamp_window,
    header_str,
};
use crate::{Address, AuthorityNotEstablished};

use super::errors::ApiError;

/// The comms-gatekeeper freshness window of every signed-fetch route.
pub const DEFAULT_EXPIRATION_SECS: i64 = 60;

pub const SCENE_SIGNER: &str = "decentraland-kernel-scene";
pub const SERVER_SIGNER: &str = "dcl:authoritative-server";
pub const EXPLORER_SIGNER: &str = "dcl:explorer";

/// The metadata fields a legacy-payload request may still be authorized on,
/// as upstream comms-gatekeeper declares them (`src/logic/utils.ts`
/// `CANONICAL_METADATA_KEYS`), plus `displayName` consumed by scene tokens.
/// The pre-6.0.0 fold left key casing outside the
/// signature; naming the fields is what makes accepting that shape safe.
pub const CANONICAL_METADATA_KEYS: &[&str] = &[
    "signer",
    "intent",
    "sceneId",
    "parcel",
    "realmName",
    "displayName",
    "deviceIdentifier",
    "realm.hostname",
    "realm.serverName",
];

/// The moderator gate authorizes on `signer` only.
const MODERATOR_METADATA_KEYS: &[&str] = &["signer"];

#[derive(Debug)]
pub struct SignedFetchError {
    pub status: u16,
    pub message: String,
}

impl SignedFetchError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// Every route that verifies with `?` sits behind upstream's crypto
/// middleware, whose refusal is `{ok: false, message}`; the adapter routes
/// map the error themselves.
impl From<SignedFetchError> for ApiError {
    fn from(e: SignedFetchError) -> Self {
        ApiError::middleware_refusal(e.status, e.message)
    }
}

/// A verified request: who signed it, the metadata it signed, and whether the
/// chain carried no ephemeral delegation (a guest session).
pub struct SignedFetch {
    pub signer: Address,
    pub metadata: Value,
    pub is_guest: bool,
}

pub fn chain_is_guest(chain: &AuthChain) -> bool {
    !chain.iter().any(|link| {
        matches!(
            link.link_type,
            AuthLinkType::EcdsaEphemeral | AuthLinkType::EcdsaEip1654Ephemeral
        )
    })
}

pub use crate::shared::signed_fetch::field_is_canonical;

fn own_string<'a>(metadata: &'a Value, field: &str) -> Option<&'a str> {
    metadata.as_object()?.get(field)?.as_str()
}

/// Upstream's `rejectIfSigner`: a canonical `signer` naming one of `rejected`
/// is refused; a re-spelled key is refused rather than read as absent.
fn signer_gate_permits(metadata: &Value, rejected: &[&str]) -> bool {
    field_is_canonical(metadata, "signer")
        && !own_string(metadata, "signer").is_some_and(|s| rejected.contains(&s))
}

/// Only legacy-signed metadata requires canonical declared key spelling.
fn legacy_metadata_keys_are_canonical(metadata: &Value, canonical_keys: &[&str]) -> bool {
    crate::signed_fetch::first_metadata_key_mismatch(metadata, canonical_keys).is_none()
}

enum Verdict {
    Signer(Address),
    InvalidSignature,
    Other(SignedFetchError),
}

/// The comms-gatekeeper answer to a signature verdict: an invalid signature
/// is the caller's 401, a malformed or expired delegation and an EIP-1654
/// check that could not run answer as upstream does.
fn verdict(outcome: Result<(), SignatureRefusal>, chain: &AuthChain) -> Verdict {
    let settled = match outcome {
        Ok(()) => {
            return match Address::parse(&chain[0].payload) {
                Ok(address) => Verdict::Signer(address),
                Err(_) => Verdict::Other(SignedFetchError::new(400, "Invalid chain format")),
            };
        }
        Err(SignatureRefusal::Invalid(_)) => return Verdict::InvalidSignature,
        Err(SignatureRefusal::LegacyKeysNotCanonical) => {
            return Verdict::Other(SignedFetchError::new(400, "Invalid chain format"));
        }
        Err(SignatureRefusal::Settled(e)) => e,
    };
    match settled {
        AuthError::RecoveryFailed(_)
        | AuthError::SignerMismatch { .. }
        | AuthError::FinalAuthorityMismatch { .. }
        | AuthError::Eip1654Rejected { .. } => Verdict::InvalidSignature,
        AuthError::EphemeralExpired { .. } => {
            Verdict::Other(SignedFetchError::new(401, "Expired signature"))
        }
        AuthError::Eip1654NotImplemented => Verdict::Other(SignedFetchError::new(
            503,
            "EIP-1654 validation unavailable",
        )),
        AuthError::Eip1654ValidationFailed(_) => {
            Verdict::Other(SignedFetchError::new(503, "Error connecting to catalyst"))
        }
        AuthError::MalformedChain(_)
        | AuthError::MissingSignature { .. }
        | AuthError::InvalidEphemeralPayload(_) => {
            Verdict::Other(SignedFetchError::new(400, "Invalid chain format"))
        }
    }
}

fn map_chain_error(e: ChainError) -> SignedFetchError {
    match e {
        ChainError::MalformedChain { .. } => SignedFetchError::new(400, "Invalid chain format"),
        ChainError::InsufficientLinks => SignedFetchError::new(400, "Invalid Auth Chain"),
        ChainError::InvalidTimestamp(_) => SignedFetchError::new(400, "Invalid timestamp"),
        ChainError::Expired { .. } => SignedFetchError::new(401, "Expired signature"),
        ChainError::InvalidSignature(_) => SignedFetchError::new(401, "Invalid signature"),
        ChainError::EipNotImplemented => {
            SignedFetchError::new(503, "EIP-1654 validation unavailable")
        }
        ChainError::CatalystUnavailable(_) => {
            SignedFetchError::new(503, "Error connecting to catalyst")
        }
    }
}

/// Stage order as upstream `verify()`: chain, timestamp freshness, metadata
/// parse, the gates, then the signature (6.x over every candidate path, the
/// legacy shape only behind the key guard and only when the two differ).
#[allow(
    clippy::too_many_arguments,
    reason = "the request and the per-route signer policy, kept flat to mirror the v1 verifier"
)]
async fn verify_with_keys(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    allowed_signers: &[&str],
    rejected_signers: &[&str],
    canonical_metadata_keys: &[&str],
    tolerance_secs: i64,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<SignedFetch, SignedFetchError> {
    let chain = signed_fetch::extract_auth_chain(headers).map_err(map_chain_error)?;
    let ts = timestamp_or_zero(header_str(headers, AUTH_TIMESTAMP_HEADER));
    let now = chrono::Utc::now().timestamp();
    check_timestamp_window(ts, now, tolerance_secs).map_err(map_chain_error)?;

    // Upstream's `verifyMetadata`: unparseable or non-object refused, `null`
    // reads as an empty object.
    let raw_metadata = header_str(headers, AUTH_METADATA_HEADER).unwrap_or("{}");
    let metadata = parse_metadata(raw_metadata)
        .ok_or_else(|| SignedFetchError::new(400, "Invalid chain metadata"))?;
    if !rejected_signers.is_empty() && !signer_gate_permits(&metadata, rejected_signers) {
        return Err(SignedFetchError::new(400, "Invalid metadata content"));
    }
    if !allowed_signers.is_empty() {
        if !field_is_canonical(&metadata, "signer") {
            return Err(SignedFetchError::new(400, "Invalid metadata content"));
        }
        let signer_field = own_string(&metadata, "signer").unwrap_or("");
        if !allowed_signers.contains(&signer_field) {
            return Err(SignedFetchError::new(400, "Invalid metadata content"));
        }
    }

    let legacy = match canonical_metadata_keys {
        [] => LegacyPayload::Refused,
        keys => LegacyPayload::Guarded {
            keys_canonical: legacy_metadata_keys_are_canonical(&metadata, keys),
        },
    };
    let request = SignedRequest {
        method,
        paths: &candidate_paths(headers, path),
        timestamp: ts,
        metadata: raw_metadata,
    };
    let outcome = verify_signature(&chain, &request, now, legacy, validator).await;
    let signer = match verdict(outcome, &chain) {
        Verdict::Signer(signer) => signer,
        Verdict::InvalidSignature => {
            tracing::warn!(reason = "invalid_signature", %method, "signed-fetch rejected");
            return Err(SignedFetchError::new(401, "Invalid signature"));
        }
        Verdict::Other(e) => {
            tracing::warn!(status = e.status, %method, "signed-fetch rejected");
            return Err(e);
        }
    };
    let is_guest = chain_is_guest(&chain);
    Ok(SignedFetch {
        signer,
        metadata,
        is_guest,
    })
}

/// Upstream's `requireSigner` middlewares (`auth`, `authSceneOrServer`,
/// `authExplorer`): the 60 s window, the route's `signer` allow-list (an empty
/// list allows any), and the legacy payload behind the key guard.
pub async fn verify_signed_fetch(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    allowed_signers: &[&str],
    validator: Option<&dyn Eip1654Validator>,
) -> Result<SignedFetch, SignedFetchError> {
    verify_with_keys(
        headers,
        method,
        path,
        allowed_signers,
        &[],
        CANONICAL_METADATA_KEYS,
        DEFAULT_EXPIRATION_SECS,
        validator,
    )
    .await
}

/// A wallet's own signed fetch (the player-report routes): any `signer`
/// but the scene's (a re-spelled key refused too, 400), so a scene-signed
/// chain is never filed as the wallet that signed it.
pub async fn verify_wallet_signed_fetch(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<SignedFetch, SignedFetchError> {
    verify_with_keys(
        headers,
        method,
        path,
        &[],
        &[SCENE_SIGNER],
        CANONICAL_METADATA_KEYS,
        DEFAULT_EXPIRATION_SECS,
        validator,
    )
    .await
}

/// Authoritative server context, verified with its existing five-minute window.
pub async fn verify_server_signed_fetch(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<SignedFetch, SignedFetchError> {
    verify_with_keys(
        headers,
        method,
        path,
        &[],
        &[],
        CANONICAL_METADATA_KEYS,
        FIVE_MINUTES,
        validator,
    )
    .await
}

/// The bare signer of a request (the authoritative server's route): the five
/// minute window, either payload shape, no metadata gate.
pub async fn require_signer(
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<Address, SignedFetchError> {
    let chain = signed_fetch::extract_auth_chain(headers).map_err(map_chain_error)?;
    let ts = timestamp_or_zero(header_str(headers, AUTH_TIMESTAMP_HEADER));
    let now = chrono::Utc::now().timestamp();
    check_timestamp_window(ts, now, FIVE_MINUTES).map_err(map_chain_error)?;
    let request = SignedRequest {
        method,
        paths: &candidate_paths(headers, path),
        timestamp: ts,
        metadata: header_str(headers, AUTH_METADATA_HEADER).unwrap_or("{}"),
    };
    let outcome = verify_signature(&chain, &request, now, LegacyPayload::Accepted, validator).await;
    match verdict(outcome, &chain) {
        Verdict::Signer(signer) => Ok(signer),
        Verdict::InvalidSignature => Err(SignedFetchError::new(401, "Invalid signature")),
        Verdict::Other(e) => Err(e),
    }
}

/// The platform service token in front of the voice routes and the world
/// ban-status route: unset is a misconfiguration (503), a missing or wrong
/// bearer a 401.
pub fn require_service_token(
    expected: Option<&str>,
    headers: &HeaderMap,
) -> Result<(), AuthorityNotEstablished> {
    match crate::auth::admin_bearer::check(expected, headers) {
        Ok(()) => Ok(()),
        Err(Refusal::NotConfigured) => Err(AuthorityNotEstablished::CredentialNotConfigured {
            environment_variable: "COMMS_GATEKEEPER_AUTH_TOKEN",
        }),
        Err(Refusal::Mismatch) => Err(AuthorityNotEstablished::PresentedSharedSecretDidNotMatch),
        Err(Refusal::NoBearer) => Err(AuthorityNotEstablished::AuthenticationMissingOrInvalid {
            detail: "no bearer token",
        }),
    }
}

/// The wire text of every service-token refusal (the old gate answered 401
/// "Authentication required" for both the missing and the wrong bearer).
pub fn service_token_error(why: AuthorityNotEstablished) -> ApiError {
    match why {
        AuthorityNotEstablished::CredentialNotConfigured { .. } => ApiError::http(
            503,
            "This route is unavailable: COMMS_GATEKEEPER_AUTH_TOKEN is not configured, so the platform service token cannot be verified",
        ),
        _ => ApiError::http(401, "Authentication required"),
    }
}

const MAX_MODERATOR_NAME_LENGTH: usize = 100;

fn sanitize_moderator_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_MODERATOR_NAME_LENGTH {
        return None;
    }
    trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-' || c == '.')
        .then(|| trimmed.to_string())
}

pub enum ModeratorMode {
    Read,
    Write,
}

/// What the moderator gate needs from the settings.
pub struct ModeratorCredentials<'a> {
    pub token: Option<&'a str>,
    pub addresses: &'a [Address],
}

/// Upstream's user-moderation gate: the `MODERATOR_TOKEN` bearer (a write
/// names its operator in `?moderator=`), else a signed fetch by a wallet in
/// `PLATFORM_USER_MODERATORS` (legacy payload only over `signer`, the scene
/// signer refused). Every refusal is the same 401.
pub async fn authorize_moderator(
    creds: &ModeratorCredentials<'_>,
    headers: &HeaderMap,
    method: &str,
    path: &SignedPath,
    mode: ModeratorMode,
    moderator_query: Option<&str>,
    validator: Option<&dyn Eip1654Validator>,
) -> Result<String, ApiError> {
    if crate::auth::admin_bearer::matches(creds.token, headers) {
        return match mode {
            ModeratorMode::Write => {
                let raw = moderator_query
                    .ok_or_else(|| ApiError::bad_request("Missing moderator query parameter"))?;
                sanitize_moderator_name(raw).ok_or_else(|| {
                    ApiError::bad_request(
                        "Invalid moderator query parameter. Must be alphanumeric (spaces, hyphens, underscores, and dots allowed) and at most 100 characters",
                    )
                })
            }
            ModeratorMode::Read => Ok("moderator-token".to_string()),
        };
    }
    let unauthorized = || ApiError::http(401, "You are not authorized to access this resource");
    let sf = verify_with_keys(
        headers,
        method,
        path,
        &[],
        &[SCENE_SIGNER],
        MODERATOR_METADATA_KEYS,
        FIVE_MINUTES,
        validator,
    )
    .await
    .map_err(|_| unauthorized())?;
    if creds.addresses.contains(&sf.signer) {
        Ok(sf.signer.into_string())
    } else {
        Err(unauthorized())
    }
}

pub fn get_request_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn device_identifier(metadata: &Value) -> Option<String> {
    metadata
        .get("deviceIdentifier")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub trait SchemaValidate {
    fn schema_validate(value: &Value) -> Result<(), String>;
}

pub fn validate_body<T>(content_type: Option<&str>, bytes: &[u8]) -> Result<T, ApiError>
where
    T: DeserializeOwned + SchemaValidate,
{
    if !crate::content::validation::is_json_content_type(content_type) {
        return Err(ApiError::http(415, "Content-Type must be application/json"));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| ApiError::http(400, e.to_string()))?;
    if let Err(detail) = T::schema_validate(&value) {
        return Err(ApiError::http(400, format!("Invalid JSON body: {detail}")));
    }
    serde_json::from_value(value).map_err(|e| ApiError::http(400, e.to_string()))
}

#[cfg(test)]
pub(crate) use crate::shared::test_keys as test_support;

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::shared::signed_fetch::{
        ORIGINAL_PATH_HEADER, build_legacy_payload, build_payload_v6,
    };
    use axum::http::HeaderValue;

    const PATH: &str = "/scene-bans";
    const METADATA: &str = r#"{"signer":"decentraland-kernel-scene","intent":"dcl:scene:ban","isGuest":false,"realmName":"LocalPreview","sceneId":"bafkreiAbC123"}"#;
    const CREDENTIAL_CANARY: &str = "server-signed-metadata-private-canary";

    fn failure(err: SignedFetchError) -> (u16, String) {
        (err.status, err.message)
    }

    #[test]
    fn guest_detection_reads_the_ephemeral_links() {
        let link = |t: AuthLinkType| crate::auth::AuthLink {
            link_type: t,
            payload: String::new(),
            signature: None,
        };
        assert!(chain_is_guest(&vec![
            link(AuthLinkType::SIGNER),
            link(AuthLinkType::EcdsaSignedEntity)
        ]));
        assert!(!chain_is_guest(&vec![
            link(AuthLinkType::SIGNER),
            link(AuthLinkType::EcdsaEphemeral),
            link(AuthLinkType::EcdsaSignedEntity)
        ]));
        assert!(!chain_is_guest(&vec![
            link(AuthLinkType::SIGNER),
            link(AuthLinkType::EcdsaEip1654Ephemeral),
            link(AuthLinkType::EcdsaEip1654SignedEntity)
        ]));
    }

    #[test]
    fn the_fixture_metadata_makes_the_two_shapes_differ() {
        assert_ne!(
            build_legacy_payload("post", PATH, "1", METADATA),
            build_payload_v6("post", PATH, "1", METADATA)
        );
    }

    #[tokio::test]
    async fn either_shape_with_mixed_case_metadata_verifies_through_the_allowlist() {
        for shape in SHAPES {
            let headers = signed_as(KEY, shape, "post", PATH, METADATA, METADATA).await;
            let sf = verify_signed_fetch(
                &headers,
                "post",
                &SignedPath::local(PATH),
                &[SCENE_SIGNER],
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("{shape:?}: {} {}", e.status, e.message));
            assert_eq!(sf.signer.as_str(), address_of(KEY));
            assert_eq!(sf.metadata["sceneId"], serde_json::json!("bafkreiAbC123"));
            assert!(sf.is_guest);
        }
    }

    #[tokio::test]
    async fn the_signer_allowlist_refuses_missing_respelled_and_unlisted_signers() {
        let canonical = r#"{"signer":"decentraland-kernel-scene"}"#;
        assert!(
            verify_signed_fetch(
                &signed(KEY, "post", "/scene-admin", canonical).await,
                "post",
                &SignedPath::local("/scene-admin"),
                &[SCENE_SIGNER],
                None
            )
            .await
            .is_ok()
        );
        for metadata in [
            r#"{"signer":"decentraland-kernel-scene","Signer":"x"}"#,
            r#"{"Signer":"decentraland-kernel-scene"}"#,
            r#"{"signer":"dcl:explorer"}"#,
            "{}",
        ] {
            let err = verify_signed_fetch(
                &signed(KEY, "post", "/scene-admin", metadata).await,
                "post",
                &SignedPath::local("/scene-admin"),
                &[SCENE_SIGNER],
                None,
            )
            .await
            .map(|_| ())
            .unwrap_err();
            assert_eq!(
                failure(err),
                (400, "Invalid metadata content".to_string()),
                "{metadata}"
            );
        }
        assert!(
            verify_signed_fetch(
                &signed(KEY, "post", "/get-scene-adapter", "{}").await,
                "post",
                &SignedPath::local("/get-scene-adapter"),
                &[],
                None
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn a_present_but_empty_timestamp_expires_and_a_stale_one_is_refused() {
        let mut headers = signed_as(KEY, Shape::V6, "post", PATH, METADATA, METADATA).await;
        headers.insert(AUTH_TIMESTAMP_HEADER, HeaderValue::from_static(""));
        let err = verify_signed_fetch(
            &headers,
            "post",
            &SignedPath::local(PATH),
            &[SCENE_SIGNER],
            None,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert_eq!(failure(err), (401, "Expired signature".to_string()));

        let stale = (chrono::Utc::now().timestamp_millis() - 120 * 1000).to_string();
        let headers = signed_at(KEY, Shape::V6, "post", PATH, METADATA, METADATA, &stale).await;
        let err = verify_signed_fetch(
            &headers,
            "post",
            &SignedPath::local(PATH),
            &[SCENE_SIGNER],
            None,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert_eq!(
            failure(err),
            (401, "Expired signature".to_string()),
            "the gatekeeper window is 60 s"
        );
        assert!(
            require_signer(&headers, "post", &SignedPath::local(PATH), None)
                .await
                .is_ok(),
            "the bare signer route keeps the five minute window"
        );
    }

    #[tokio::test]
    async fn a_bad_signature_still_answers_401_invalid_signature_under_either_shape() {
        for shape in SHAPES {
            let wrong_path =
                signed_as(KEY, shape, "post", "/scene-admin", METADATA, METADATA).await;
            let err = verify_signed_fetch(
                &wrong_path,
                "post",
                &SignedPath::local(PATH),
                &[SCENE_SIGNER],
                None,
            )
            .await
            .map(|_| ())
            .unwrap_err();
            assert_eq!(
                failure(err),
                (401, "Invalid signature".to_string()),
                "{shape:?}"
            );

            let tampered = METADATA.replace("bafkreiAbC123", "bafkreiXyZ999");
            let tampered_metadata = signed_as(KEY, shape, "post", PATH, METADATA, &tampered).await;
            let err = verify_signed_fetch(
                &tampered_metadata,
                "post",
                &SignedPath::local(PATH),
                &[SCENE_SIGNER],
                None,
            )
            .await
            .map(|_| ())
            .unwrap_err();
            assert_eq!(
                failure(err),
                (401, "Invalid signature".to_string()),
                "{shape:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_legacy_signed_request_with_a_respelled_declared_key_is_refused() {
        let recased = METADATA.replace("\"sceneId\"", "\"sceneid\"");
        let headers = signed_as(KEY, Shape::Legacy, "post", PATH, &recased, &recased).await;
        let err = verify_signed_fetch(
            &headers,
            "post",
            &SignedPath::local(PATH),
            &[SCENE_SIGNER],
            None,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert_eq!(err.status, 400);
        let nested = r#"{"signer":"decentraland-kernel-scene","sceneId":"bafkreiAbC","realm":{"servername":"main"}}"#;
        let headers = signed_as(KEY, Shape::Legacy, "post", PATH, nested, nested).await;
        assert_eq!(
            verify_signed_fetch(
                &headers,
                "post",
                &SignedPath::local(PATH),
                &[SCENE_SIGNER],
                None
            )
            .await
            .map(|_| ())
            .unwrap_err()
            .status,
            400
        );
        let all_lowercase =
            r#"{"signer":"decentraland-kernel-scene","realm":{"servername":"main"}}"#;
        let headers = signed_as(
            KEY,
            Shape::Legacy,
            "post",
            PATH,
            all_lowercase,
            all_lowercase,
        )
        .await;
        assert!(
            verify_signed_fetch(
                &headers,
                "post",
                &SignedPath::local(PATH),
                &[SCENE_SIGNER],
                None
            )
            .await
            .is_ok(),
            "identical payloads settle on the 6.x attempt"
        );
        let canonical_nested = r#"{"signer":"decentraland-kernel-scene","sceneId":"bafkreiAbC","realm":{"serverName":"main"}}"#;
        let headers = signed_as(
            KEY,
            Shape::Legacy,
            "post",
            PATH,
            canonical_nested,
            canonical_nested,
        )
        .await;
        assert!(
            verify_signed_fetch(
                &headers,
                "post",
                &SignedPath::local(PATH),
                &[SCENE_SIGNER],
                None
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn malformed_metadata_never_echoes_and_a_missing_chain_is_a_400() {
        let malformed = format!(r#"{{"private":"{CREDENTIAL_CANARY}""#);
        let err = verify_signed_fetch(
            &signed_as(KEY, Shape::V6, "post", PATH, &malformed, &malformed).await,
            "post",
            &SignedPath::local(PATH),
            &[SCENE_SIGNER],
            None,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert_eq!(failure(err), (400, "Invalid chain metadata".to_string()));
        let err = verify_signed_fetch(
            &HeaderMap::new(),
            "post",
            &SignedPath::local(PATH),
            &[],
            None,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert_eq!(failure(err), (400, "Invalid Auth Chain".to_string()));
    }

    #[tokio::test]
    async fn the_public_path_a_proxy_forwards_is_accepted_only_when_it_ends_with_the_route() {
        let mut headers = signed(KEY, "get", "/comms-gatekeeper/scene-admin", "{}").await;
        headers.insert(
            ORIGINAL_PATH_HEADER,
            HeaderValue::from_static("/comms-gatekeeper/scene-admin?place_id=x"),
        );
        assert!(
            verify_signed_fetch(
                &headers,
                "get",
                &SignedPath::local("/scene-admin"),
                &[],
                None
            )
            .await
            .is_ok()
        );
        let mut headers = signed(KEY, "get", "/elsewhere", "{}").await;
        headers.insert(ORIGINAL_PATH_HEADER, HeaderValue::from_static("/elsewhere"));
        assert_eq!(
            verify_signed_fetch(
                &headers,
                "get",
                &SignedPath::local("/scene-admin"),
                &[],
                None
            )
            .await
            .map(|_| ())
            .unwrap_err()
            .status,
            401
        );
    }

    #[test]
    fn the_declared_keys_include_upstream_names_and_the_signed_display_name() {
        assert_eq!(
            CANONICAL_METADATA_KEYS,
            &[
                "signer",
                "intent",
                "sceneId",
                "parcel",
                "realmName",
                "displayName",
                "deviceIdentifier",
                "realm.hostname",
                "realm.serverName"
            ]
        );
    }

    #[test]
    fn the_service_token_gate_distinguishes_unset_missing_and_wrong() {
        let mut headers = HeaderMap::new();
        assert!(matches!(
            require_service_token(None, &headers),
            Err(AuthorityNotEstablished::CredentialNotConfigured { .. })
        ));
        assert!(matches!(
            require_service_token(Some("t"), &headers),
            Err(AuthorityNotEstablished::AuthenticationMissingOrInvalid { .. })
        ));
        headers.insert("authorization", HeaderValue::from_static("Bearer nope"));
        assert!(matches!(
            require_service_token(Some("t"), &headers),
            Err(AuthorityNotEstablished::PresentedSharedSecretDidNotMatch)
        ));
        headers.insert("authorization", HeaderValue::from_static("Bearer t"));
        assert!(require_service_token(Some("t"), &headers).is_ok());
        assert_eq!(
            service_token_error(AuthorityNotEstablished::PresentedSharedSecretDidNotMatch).code,
            401
        );
        assert_eq!(
            service_token_error(AuthorityNotEstablished::CredentialNotConfigured {
                environment_variable: "X"
            })
            .code,
            503
        );
    }

    #[tokio::test]
    async fn the_moderator_gate_accepts_the_token_or_a_listed_wallet_and_nothing_else() {
        let listed = Address::parse(&address_of(KEY)).unwrap();
        let creds = ModeratorCredentials {
            token: Some("mod-token"),
            addresses: std::slice::from_ref(&listed),
        };
        let mut bearer = HeaderMap::new();
        bearer.insert(
            "authorization",
            HeaderValue::from_static("Bearer mod-token"),
        );
        assert_eq!(
            authorize_moderator(
                &creds,
                &bearer,
                "get",
                &SignedPath::local("/bans"),
                ModeratorMode::Read,
                None,
                None
            )
            .await
            .unwrap(),
            "moderator-token"
        );
        assert_eq!(
            authorize_moderator(
                &creds,
                &bearer,
                "post",
                &SignedPath::local("/users/0xa/bans"),
                ModeratorMode::Write,
                Some(" alice.dcl "),
                None
            )
            .await
            .unwrap(),
            "alice.dcl"
        );
        assert_eq!(
            authorize_moderator(
                &creds,
                &bearer,
                "post",
                &SignedPath::local("/x"),
                ModeratorMode::Write,
                None,
                None
            )
            .await
            .unwrap_err()
            .code,
            400
        );
        assert_eq!(
            authorize_moderator(
                &creds,
                &bearer,
                "post",
                &SignedPath::local("/x"),
                ModeratorMode::Write,
                Some("bad;name"),
                None
            )
            .await
            .unwrap_err()
            .code,
            400
        );

        let signed_listed = signed(KEY, "get", "/bans", "{}").await;
        assert_eq!(
            authorize_moderator(
                &creds,
                &signed_listed,
                "get",
                &SignedPath::local("/bans"),
                ModeratorMode::Read,
                None,
                None
            )
            .await
            .unwrap(),
            listed.as_str()
        );
        let signed_outsider = signed(OUTSIDER_KEY, "get", "/bans", "{}").await;
        assert_eq!(
            authorize_moderator(
                &creds,
                &signed_outsider,
                "get",
                &SignedPath::local("/bans"),
                ModeratorMode::Read,
                None,
                None
            )
            .await
            .unwrap_err()
            .code,
            401
        );
        let scene_signed = signed(
            KEY,
            "get",
            "/bans",
            r#"{"signer":"decentraland-kernel-scene"}"#,
        )
        .await;
        assert_eq!(
            authorize_moderator(
                &creds,
                &scene_signed,
                "get",
                &SignedPath::local("/bans"),
                ModeratorMode::Read,
                None,
                None
            )
            .await
            .unwrap_err()
            .code,
            401
        );
        let respelled = signed_as(
            KEY,
            Shape::Legacy,
            "get",
            "/bans",
            r#"{"Signer":"decentraland-kernel-scene"}"#,
            r#"{"Signer":"decentraland-kernel-scene"}"#,
        )
        .await;
        assert_eq!(
            authorize_moderator(
                &creds,
                &respelled,
                "get",
                &SignedPath::local("/bans"),
                ModeratorMode::Read,
                None,
                None
            )
            .await
            .unwrap_err()
            .code,
            401
        );
        let mut wrong_bearer = HeaderMap::new();
        wrong_bearer.insert("authorization", HeaderValue::from_static("Bearer other"));
        assert_eq!(
            authorize_moderator(
                &creds,
                &wrong_bearer,
                "get",
                &SignedPath::local("/bans"),
                ModeratorMode::Read,
                None,
                None
            )
            .await
            .unwrap_err()
            .code,
            401
        );
    }

    #[test]
    fn body_validation_requires_json_and_the_schema() {
        #[derive(Debug, serde::Deserialize)]
        struct Body {
            reason: String,
        }
        impl SchemaValidate for Body {
            fn schema_validate(value: &Value) -> Result<(), String> {
                value
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|_| ())
                    .ok_or_else(|| "reason must be a non-empty string".into())
            }
        }
        assert_eq!(
            validate_body::<Body>(Some("text/plain"), b"{}")
                .unwrap_err()
                .code,
            415
        );
        assert_eq!(
            validate_body::<Body>(Some("application/json"), b"{")
                .unwrap_err()
                .code,
            400
        );
        assert_eq!(
            validate_body::<Body>(Some("application/json; charset=utf-8"), br#"{"reason":""}"#)
                .unwrap_err()
                .code,
            400
        );
        assert_eq!(
            validate_body::<Body>(Some("application/vnd.api+json"), br#"{"reason":"spam"}"#)
                .unwrap()
                .reason,
            "spam"
        );
        assert_eq!(get_request_ip(&HeaderMap::new()), None);
        assert_eq!(
            device_identifier(&serde_json::json!({"deviceIdentifier": "d1"})).as_deref(),
            Some("d1")
        );
        assert_eq!(
            device_identifier(&serde_json::json!({"deviceIdentifier": ""})),
            None
        );
    }
}
