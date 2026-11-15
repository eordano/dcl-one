use crate::auth::verify_auth_chain;
use crate::auth::{AuthChain, AuthLink, AuthLinkType, MAX_AUTH_CHAIN_LINKS};

use crate::protocol::pulse::decentraland::pulse::HandshakeRequest;

pub const MAX_TIMESTAMP_SKEW_MS: i64 = 60_000;

use crate::shared::signed_fetch::{
    AUTH_CHAIN_HEADER_PREFIX, AUTH_METADATA_HEADER as METADATA_HEADER,
    AUTH_TIMESTAMP_HEADER as TIMESTAMP_HEADER,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    InvalidJson,

    NoAuthChain,

    StaleTimestamp,

    InvalidAuthChain(String),
}

impl HandshakeError {
    pub fn message(&self) -> String {
        match self {
            HandshakeError::InvalidJson => "Invalid auth chain JSON".to_string(),
            HandshakeError::NoAuthChain => "No x-identity-auth-chain-* headers found.".to_string(),
            HandshakeError::StaleTimestamp => {
                format!("timestamp outside \u{B1}{MAX_TIMESTAMP_SKEW_MS}ms skew window")
            }
            HandshakeError::InvalidAuthChain(_) => "Invalid auth chain".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedHandshake {
    pub user_address: String,

    /// The identity that actually signed this connection: the last delegated ephemeral address,
    /// or the wallet itself when the chain never delegates. Two devices on one wallet therefore
    /// carry different sessions, which is what makes a takeover distinguishable from a reconnect.
    pub session: String,

    pub timestamp: String,
}

pub use crate::shared::signed_fetch::build_payload_v6 as build_signed_fetch_payload;

fn obj_str<'a>(obj: &'a serde_json::Map<String, serde_json::Value>, key: &str) -> Option<&'a str> {
    obj.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
}

/// The `x-identity-auth-chain-N` entries of a signed-fetch header bag as an auth chain, with the
/// old crypto crate's shape rules: SIGNER first and only first, a signature on every later link,
/// at most `MAX_AUTH_CHAIN_LINKS` links and at least two.
fn extract_from_object(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<AuthChain, String> {
    let mut links = Vec::new();
    for i in 0..MAX_AUTH_CHAIN_LINKS {
        let name = format!("{AUTH_CHAIN_HEADER_PREFIX}{i}");
        let Some(raw) = obj_str(obj, &name) else {
            break;
        };
        let link: AuthLink = serde_json::from_str(raw).map_err(|e| {
            let mut detail = e.to_string();
            detail.truncate(detail.floor_char_boundary(64));
            format!("invalid auth-chain link {i}: {detail}")
        })?;
        match link.link_type {
            AuthLinkType::SIGNER => {
                if i != 0 {
                    return Err(format!(
                        "invalid auth-chain link {i}: SIGNER link at non-zero index"
                    ));
                }
            }
            _ => {
                if i == 0 {
                    return Err("invalid auth-chain link 0: first link must be SIGNER".to_string());
                }
                if link.signature.as_deref().unwrap_or("").is_empty() {
                    return Err(format!("invalid auth-chain link {i}: missing signature"));
                }
            }
        }
        links.push(link);
    }
    let overflow = format!("{AUTH_CHAIN_HEADER_PREFIX}{MAX_AUTH_CHAIN_LINKS}");
    if obj_str(obj, &overflow).is_some() {
        return Err(format!(
            "invalid auth-chain link {MAX_AUTH_CHAIN_LINKS}: exceeds max length {MAX_AUTH_CHAIN_LINKS}"
        ));
    }
    if links.len() < 2 {
        return Err("auth-chain shorter than 2 links".to_string());
    }
    Ok(links)
}

fn has_auth_chain_header(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    obj.keys()
        .any(|k| k.to_lowercase().starts_with(AUTH_CHAIN_HEADER_PREFIX))
}

/// The final delegate in the chain. Only the last one counts: a chain may delegate more than
/// once, and every earlier ephemeral is a link in the path to it, not the signer of the payload.
fn session_address(chain: &AuthChain) -> Option<String> {
    chain
        .iter()
        .rev()
        .find(|l| l.link_type == AuthLinkType::EcdsaEphemeral)
        .and_then(|l| {
            crate::auth::auth_chain::parse_ephemeral_payload(&l.payload)
                .ok()
                .map(|(_, ephemeral, _)| ephemeral.trim().to_lowercase())
        })
}

fn signer_address(chain: &AuthChain) -> Option<String> {
    chain
        .first()
        .filter(|l| l.link_type == AuthLinkType::SIGNER)
        .map(|l| l.payload.trim().to_lowercase())
}

pub fn verify_handshake(
    request: &HandshakeRequest,
    now_ms: i64,
) -> Result<VerifiedHandshake, HandshakeError> {
    verify_handshake_bytes(&request.auth_chain, now_ms)
}

/// Shared by the player and scene-listener handshake paths, which sign the identical
/// `"connect"/"/"` payload.
pub fn verify_handshake_bytes(
    auth_chain: &[u8],
    now_ms: i64,
) -> Result<VerifiedHandshake, HandshakeError> {
    let json = std::str::from_utf8(auth_chain).map_err(|_| HandshakeError::InvalidJson)?;
    let obj: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|_| HandshakeError::InvalidJson)?;

    if !has_auth_chain_header(&obj) {
        return Err(HandshakeError::NoAuthChain);
    }

    let timestamp = obj_str(&obj, TIMESTAMP_HEADER).unwrap_or("").to_string();
    let metadata = obj_str(&obj, METADATA_HEADER).unwrap_or("").to_string();

    let ts_ms: i64 = timestamp
        .parse()
        .map_err(|_| HandshakeError::StaleTimestamp)?;
    if now_ms.abs_diff(ts_ms) > MAX_TIMESTAMP_SKEW_MS as u64 {
        return Err(HandshakeError::StaleTimestamp);
    }

    let chain: AuthChain = extract_from_object(&obj).map_err(HandshakeError::InvalidAuthChain)?;

    let expected_payload = build_signed_fetch_payload("connect", "/", &timestamp, &metadata);

    let final_payload = chain
        .last()
        .map(|l| l.payload.clone())
        .ok_or(HandshakeError::NoAuthChain)?;
    if final_payload != expected_payload {
        return Err(HandshakeError::InvalidAuthChain(
            "Final link rejected by policy: expected connect payload".into(),
        ));
    }

    verify_auth_chain(&chain, &expected_payload, Some(now_ms))
        .map_err(|e| HandshakeError::InvalidAuthChain(e.to_string()))?;

    let user_address = signer_address(&chain)
        .ok_or_else(|| HandshakeError::InvalidAuthChain("First link must be SIGNER".into()))?;

    let session = session_address(&chain).unwrap_or_else(|| user_address.clone());

    Ok(VerifiedHandshake {
        user_address,
        session,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::pulse::decentraland::pulse::HandshakeRequest;

    #[test]
    fn public_handshake_errors_never_echo_invalid_chain_details() {
        const CANARY: &str = "pulse-invalid-chain-private-material-canary";
        let timestamp = "100000";
        let metadata = "{}";
        let expected = build_signed_fetch_payload("connect", "/", timestamp, metadata);
        let signer = AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: "0x0000000000000000000000000000000000000001".into(),
            signature: None,
        };
        let ephemeral = AuthLink {
            link_type: AuthLinkType::EcdsaEphemeral,
            payload: format!(
                "Decentraland Login\nEphemeral address: 0x0000000000000000000000000000000000000002\nExpiration: {CANARY}"
            ),
            signature: Some("0x00".into()),
        };
        let signed = AuthLink {
            link_type: AuthLinkType::EcdsaSignedEntity,
            payload: expected,
            signature: Some("0x00".into()),
        };
        let body = header_bag_json(
            &[(0, &signer), (1, &ephemeral), (2, &signed)],
            timestamp,
            metadata,
        );
        let error = verify_handshake_bytes(body.as_bytes(), 100000).unwrap_err();
        assert!(
            matches!(&error, HandshakeError::InvalidAuthChain(detail) if detail.contains(CANARY))
        );
        assert!(!error.message().contains(CANARY));
        assert_eq!(error.message(), "Invalid auth chain");
    }

    #[test]
    fn a_long_invalid_link_detail_is_cut_on_a_character_boundary() {
        // The parser's message echoes the link type, so byte 64 falls inside `é`.
        let link_type = format!("{}é", "a".repeat(46));
        let raw = format!(r#"{{"type":"{link_type}","payload":"x"}}"#);
        let mut obj = serde_json::Map::new();
        obj.insert(
            format!("{AUTH_CHAIN_HEADER_PREFIX}0"),
            serde_json::Value::String(raw),
        );
        let error = extract_from_object(&obj).unwrap_err();
        assert_eq!(
            error,
            format!(
                "invalid auth-chain link 0: unknown variant `{}",
                "a".repeat(46)
            )
        );
    }

    fn header_bag_json(links: &[(usize, &AuthLink)], ts: &str, metadata: &str) -> String {
        let mut map = serde_json::Map::new();
        for (i, link) in links {
            map.insert(
                format!("{AUTH_CHAIN_HEADER_PREFIX}{i}"),
                serde_json::Value::String(serde_json::to_string(link).unwrap()),
            );
        }
        map.insert(
            TIMESTAMP_HEADER.into(),
            serde_json::Value::String(ts.into()),
        );
        map.insert(
            METADATA_HEADER.into(),
            serde_json::Value::String(metadata.into()),
        );
        serde_json::to_string(&serde_json::Value::Object(map)).unwrap()
    }

    #[test]
    fn signed_fetch_payload_is_lowercased() {
        assert_eq!(
            build_signed_fetch_payload("CONNECT", "/", "123", "{}"),
            "connect:/:123:{}"
        );
    }

    #[test]
    fn rejects_non_utf8_or_bad_json() {
        let req = HandshakeRequest {
            auth_chain: vec![0xFF, 0xFE],
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };
        assert_eq!(
            verify_handshake(&req, 1000),
            Err(HandshakeError::InvalidJson)
        );

        let req = HandshakeRequest {
            auth_chain: b"not json".to_vec(),
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };
        assert_eq!(
            verify_handshake(&req, 1000),
            Err(HandshakeError::InvalidJson)
        );
    }

    #[test]
    fn rejects_missing_auth_chain_headers() {
        let json =
            serde_json::json!({ TIMESTAMP_HEADER: "1000", METADATA_HEADER: "{}" }).to_string();
        let req = HandshakeRequest {
            auth_chain: json.into_bytes(),
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };
        assert_eq!(
            verify_handshake(&req, 1000),
            Err(HandshakeError::NoAuthChain)
        );
    }

    #[test]
    fn rejects_stale_timestamp() {
        let signer = AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: "0xabc".into(),
            signature: None,
        };
        let json = header_bag_json(&[(0, &signer)], "1000", "{}");
        let req = HandshakeRequest {
            auth_chain: json.into_bytes(),
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };

        assert_eq!(
            verify_handshake(&req, 1000 + MAX_TIMESTAMP_SKEW_MS + 1),
            Err(HandshakeError::StaleTimestamp)
        );
    }

    #[test]
    fn extreme_timestamps_are_rejected_without_overflow() {
        let signer = AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: "0xabc".into(),
            signature: None,
        };
        for (timestamp, now) in [(i64::MIN, 1), (i64::MAX, -1), (0, i64::MIN)] {
            let body = header_bag_json(&[(0, &signer)], &timestamp.to_string(), "{}");
            assert_eq!(
                verify_handshake_bytes(body.as_bytes(), now),
                Err(HandshakeError::StaleTimestamp)
            );
        }
    }

    #[test]
    fn rejects_final_payload_not_connect() {
        let signer = AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: "0xabc".into(),
            signature: None,
        };
        let json = header_bag_json(&[(0, &signer)], "100000", "meta");
        let req = HandshakeRequest {
            auth_chain: json.into_bytes(),
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };
        let err = verify_handshake(&req, 100000).unwrap_err();
        assert!(matches!(err, HandshakeError::InvalidAuthChain(_)));
    }

    #[test]
    fn rejects_single_signer_only_chain() {
        let ts = "100000";
        let metadata = "{}";
        let connect_payload = build_signed_fetch_payload("connect", "/", ts, metadata);
        let signer = AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: connect_payload,
            signature: Some(String::new()),
        };
        let json = header_bag_json(&[(0, &signer)], ts, metadata);
        let req = HandshakeRequest {
            auth_chain: json.into_bytes(),
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };
        let now_ms: i64 = ts.parse().unwrap();
        let err = verify_handshake(&req, now_ms).unwrap_err();
        assert!(
            matches!(err, HandshakeError::InvalidAuthChain(_)),
            "single SIGNER-only chain must be rejected, got {err:?}"
        );
    }

    #[tokio::test]
    async fn accepts_real_signed_chain() {
        use crate::auth::AuthChain as Chain;
        use alloy::signers::{Signer, local::PrivateKeySigner};

        let root: PrivateKeySigner =
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                .parse()
                .unwrap();
        let root_addr = format!("{:#x}", root.address());
        let ephemeral: PrivateKeySigner =
            "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
                .parse()
                .unwrap();
        let eph_addr = format!("{:#x}", ephemeral.address());

        let ts = "1700000000000";
        let metadata = "{\"signer\":\"dcl:explorer\"}";
        let connect_payload = build_signed_fetch_payload("connect", "/", ts, metadata);

        let eph_payload = format!(
            "Decentraland Login\nEphemeral address: {eph_addr}\nExpiration: 2099-01-01T00:00:00.000Z"
        );
        let eph_sig = root
            .sign_message(eph_payload.as_bytes())
            .await
            .unwrap()
            .to_string();
        let final_sig = ephemeral
            .sign_message(connect_payload.as_bytes())
            .await
            .unwrap()
            .to_string();

        let chain: Chain = vec![
            AuthLink {
                link_type: AuthLinkType::SIGNER,
                payload: root_addr.clone(),
                signature: None,
            },
            AuthLink {
                link_type: AuthLinkType::EcdsaEphemeral,
                payload: eph_payload,
                signature: Some(eph_sig),
            },
            AuthLink {
                link_type: AuthLinkType::EcdsaSignedEntity,
                payload: connect_payload,
                signature: Some(final_sig),
            },
        ];

        let links: Vec<(usize, &AuthLink)> = chain.iter().enumerate().collect();
        let json = header_bag_json(&links, ts, metadata);
        let req = HandshakeRequest {
            auth_chain: json.into_bytes(),
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };

        let now_ms: i64 = ts.parse().unwrap();
        let ok = verify_handshake(&req, now_ms).expect("real chain must verify");
        assert_eq!(ok.user_address, root_addr.to_lowercase());
        assert_eq!(ok.session, eph_addr.to_lowercase());
        assert_ne!(ok.session, ok.user_address);
        assert_eq!(ok.timestamp, ts);

        let bad_metadata = "{\"signer\":\"dcl:other\"}";
        let bad_connect = build_signed_fetch_payload("connect", "/", ts, bad_metadata);
        let mut bad_chain = chain.clone();

        bad_chain[2].payload = bad_connect;
        let bad_links: Vec<(usize, &AuthLink)> = bad_chain.iter().enumerate().collect();
        let bad_json = header_bag_json(&bad_links, ts, metadata);
        let bad_req = HandshakeRequest {
            auth_chain: bad_json.into_bytes(),
            profile_version: 0,
            initial_state: None,
            protocol_features: 0,
        };
        assert!(matches!(
            verify_handshake(&bad_req, now_ms).unwrap_err(),
            HandshakeError::InvalidAuthChain(_)
        ));
    }
}
