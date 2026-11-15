//! Scoped storage delegations: the capability a hosted scene signs its
//! storage requests with. The authoritative signer (the wallet the storage
//! routes trust, `AUTHORITATIVE_SERVER_ADDRESS`) signs a claim that binds a
//! fresh ephemeral key to exactly one `(world, scene id, parcel)` until an
//! expiration; the host signs every `~system/SignedFetch` with the ephemeral
//! key and sends the claim as `x-authoritative-scope`. A scene therefore
//! holds a capability for its own namespace and nothing else, and a
//! compromised runtime cannot mint one for another scene: the authoritative
//! key never enters the isolate, and with the external minter never enters
//! this process either.
//!
//! Minting: [`DelegationMinter::Local`] signs in-process (the old
//! `catalyrst-deploy-signer --serve-delegations` envelope, byte for byte),
//! [`DelegationMinter::Remote`] asks that minter over HTTP. Either way the
//! answer is parsed and checked against the requested scope before it
//! replaces the slot, so a minter cannot rebind a scene. The renewal loop
//! refreshes a slot `REFRESH_BUFFER_SECS` before expiry and on demand.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

pub const STORAGE_DELEGATION_PREFIX: &str = "Decentraland Authoritative Storage Delegation";

pub const MAX_SCOPE_HEADER_LENGTH: usize = 4096;

pub const REFRESH_BUFFER_SECS: i64 = 5 * 60;

pub const RENEWAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub type DelegationSlot = Arc<Mutex<Option<StorageDelegation>>>;

pub use crate::auth::wallet::{SignError, Wallet, create_simple_auth_chain};

#[derive(Clone)]
pub struct StorageDelegation {
    pub ephemeral: Arc<Wallet>,
    pub ephemeral_address: String,
    /// Lowercase: the storage realm (`world_storage.realm`).
    pub world: String,
    pub scene_id: String,
    pub parcel: String,
    pub expiration: DateTime<Utc>,
    /// The base64 `{payload, signature}` sent as `x-authoritative-scope`.
    pub scope_header: String,
}

impl std::fmt::Debug for StorageDelegation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageDelegation")
            .field("ephemeral_address", &self.ephemeral_address)
            .field("world", &self.world)
            .field("scene_id", &self.scene_id)
            .field("parcel", &self.parcel)
            .field("expiration", &self.expiration)
            .finish_non_exhaustive()
    }
}

impl StorageDelegation {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expiration <= now
    }

    pub fn near_expiry(&self, now: DateTime<Utc>) -> bool {
        self.expiration - chrono::Duration::seconds(REFRESH_BUFFER_SECS) <= now
    }
}

fn claim_field(payload: &str, prefix: &str) -> Option<String> {
    payload
        .lines()
        .find_map(|l| l.strip_prefix(prefix))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The minter's envelope: `{v: 1, ephemeral: {privateKey, publicKey, address},
/// scope: {payload, signature}}`, base64. Every field the host later uses is
/// derived from the signed payload, never from the envelope's own copies.
pub fn parse_storage_delegation(encoded: &str) -> Option<StorageDelegation> {
    let decoded = BASE64.decode(encoded.trim()).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let obj = value.as_object()?;
    if obj.get("v")?.as_u64()? != 1 {
        return None;
    }
    let ephemeral = obj.get("ephemeral")?.as_object()?;
    let private_key = ephemeral.get("privateKey")?.as_str()?;
    ephemeral.get("publicKey")?.as_str()?;
    let address = ephemeral.get("address")?.as_str()?;
    let scope = obj.get("scope")?.as_object()?;
    let payload = scope.get("payload")?.as_str()?;
    let signature = scope.get("signature")?.as_str()?;

    if payload.lines().next() != Some(STORAGE_DELEGATION_PREFIX) {
        return None;
    }
    let world = claim_field(payload, "World:")?.to_lowercase();
    let scene_id = claim_field(payload, "SceneId:")?;
    let parcel = claim_field(payload, "Parcel:")?;
    let expiration = DateTime::parse_from_rfc3339(&claim_field(payload, "Expiration:")?)
        .ok()?
        .with_timezone(&Utc);

    let wallet = Wallet::from_hex(private_key).ok()?;
    let ephemeral_address = wallet.address();
    if !ephemeral_address.eq_ignore_ascii_case(address)
        || !ephemeral_address.eq_ignore_ascii_case(&claim_field(payload, "Ephemeral:")?)
    {
        return None;
    }

    let scope_header = BASE64
        .encode(serde_json::json!({ "payload": payload, "signature": signature }).to_string());
    if scope_header.len() > MAX_SCOPE_HEADER_LENGTH {
        return None;
    }

    Some(StorageDelegation {
        ephemeral: Arc::new(wallet),
        ephemeral_address,
        world,
        scene_id,
        parcel,
        expiration,
        scope_header,
    })
}

/// The claim text the authoritative signer signs.
pub fn claim_payload(
    ephemeral_address: &str,
    world: &str,
    scene_id: &str,
    parcel: &str,
    expiration: DateTime<Utc>,
) -> String {
    format!(
        "{STORAGE_DELEGATION_PREFIX}\nEphemeral: {ephemeral_address}\nWorld: {world}\nSceneId: {scene_id}\nParcel: {parcel}\nExpiration: {}",
        expiration.to_rfc3339()
    )
}

/// The old minter's envelope for `scope`, signed by `authoritative` and
/// valid for `ttl` from `now`.
pub fn mint_envelope(
    authoritative: &Wallet,
    scope: &DelegationScope,
    ttl: std::time::Duration,
    now: DateTime<Utc>,
) -> Result<String> {
    let ephemeral = Wallet::random().context("generating the ephemeral key")?;
    let expiration = now + chrono::Duration::seconds(ttl.as_secs().min(i64::MAX as u64) as i64);
    let payload = claim_payload(
        &ephemeral.address(),
        &scope.world,
        &scope.scene_id,
        &scope.parcel,
        expiration,
    );
    let signature = authoritative
        .sign_message(payload.as_bytes())
        .context("signing the delegation claim")?;
    let envelope = serde_json::json!({
        "v": 1,
        "ephemeral": {
            "privateKey": ephemeral.private_key_hex(),
            "publicKey": ephemeral.public_key_hex(),
            "address": ephemeral.address(),
        },
        "scope": { "payload": payload, "signature": signature },
    });
    Ok(BASE64.encode(envelope.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationScope {
    /// The storage realm, lowercase.
    pub world: String,
    pub scene_id: String,
    pub parcel: String,
}

/// Who mints a scene's delegation.
pub enum DelegationMinter {
    /// The authoritative key is in this process.
    Local {
        authoritative: Arc<Wallet>,
        ttl: std::time::Duration,
    },
    /// `POST {url}/delegations {world, sceneId, parcel}` with an optional
    /// bearer (the old `catalyrst-deploy-signer --serve-delegations`).
    Remote {
        http: reqwest::Client,
        url: String,
        token: Option<String>,
    },
}

impl std::fmt::Debug for DelegationMinter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local { authoritative, ttl } => f
                .debug_struct("Local")
                .field("authoritative", &authoritative.address())
                .field("ttl", ttl)
                .finish(),
            Self::Remote { url, token, .. } => f
                .debug_struct("Remote")
                .field("url", url)
                .field("token", &token.is_some())
                .finish(),
        }
    }
}

impl DelegationMinter {
    /// A delegation for exactly `scope`; an answer for any other scope is
    /// refused (`mint_from_minter_rejects_a_rebind`).
    pub async fn mint(&self, scope: &DelegationScope) -> Result<StorageDelegation> {
        let encoded = match self {
            Self::Local { authoritative, ttl } => {
                mint_envelope(authoritative, scope, *ttl, Utc::now())?
            }
            Self::Remote { http, url, token } => {
                let endpoint = format!("{}/delegations", url.trim_end_matches('/'));
                let mut req = http.post(&endpoint).json(&serde_json::json!({
                    "world": scope.world,
                    "sceneId": scope.scene_id,
                    "parcel": scope.parcel,
                }));
                if let Some(token) = token {
                    req = req.bearer_auth(token);
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| anyhow!("delegation minter unreachable: {}", e.without_url()))?
                    .error_for_status()
                    .map_err(|e| {
                        anyhow!(
                            "delegation minter rejected the request: {}",
                            e.without_url()
                        )
                    })?;
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .context("delegation minter returned a malformed response")?;
                body.get("delegation")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("delegation minter response is missing the delegation"))?
                    .to_string()
            }
        };
        let delegation = parse_storage_delegation(&encoded)
            .ok_or_else(|| anyhow!("delegation minter returned an unparseable delegation"))?;
        if delegation.world != scope.world.to_lowercase()
            || delegation.scene_id != scope.scene_id
            || delegation.parcel != scope.parcel
        {
            bail!("delegation minter answered for a different scene");
        }
        Ok(delegation)
    }
}

/// Keeps `slot` fresh: sleeps until `REFRESH_BUFFER_SECS` before the current
/// expiry (or backs off from 1 s to 5 min while the slot is empty or the
/// minter fails), mints, and answers every waiter that asked for a renewal
/// meanwhile. Ends when the request channel closes (the scene was dropped).
pub async fn renewal_loop(
    minter: Arc<DelegationMinter>,
    scope: DelegationScope,
    slot: DelegationSlot,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        let due = {
            let guard = slot.lock();
            match guard.as_ref() {
                Some(d) => {
                    (d.expiration - chrono::Duration::seconds(REFRESH_BUFFER_SECS) - Utc::now())
                        .to_std()
                        .unwrap_or(std::time::Duration::ZERO)
                }
                None => std::time::Duration::ZERO,
            }
        };
        let sleep_for = if due.is_zero() { backoff } else { due };

        let mut waiters: Vec<tokio::sync::oneshot::Sender<()>> = Vec::new();
        tokio::select! {
            biased;
            req = rx.recv() => match req {
                Some(w) => waiters.push(w),
                None => return,
            },
            _ = tokio::time::sleep(sleep_for) => {}
        }
        while let Ok(w) = rx.try_recv() {
            waiters.push(w);
        }

        let needs = slot
            .lock()
            .as_ref()
            .map(|d| d.near_expiry(Utc::now()))
            .unwrap_or(true);
        if needs {
            match minter.mint(&scope).await {
                Ok(d) => {
                    *slot.lock() = Some(d);
                    backoff = std::time::Duration::from_secs(1);
                    tracing::info!(world = %scope.world, scene = %scope.scene_id, "storage delegation renewed");
                }
                Err(e) => {
                    backoff = (backoff * 2).min(std::time::Duration::from_secs(300));
                    tracing::warn!(world = %scope.world, scene = %scope.scene_id, error = %e, "storage delegation renewal failed");
                }
            }
        }
        for w in waiters {
            let _ = w.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::delegation::{StorageDelegationTarget, verify_storage_delegation};
    use chrono::Duration;

    const AUTHORITATIVE_KEY: &str =
        "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
    const EPHEMERAL_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const EPHEMERAL2_KEY: &str =
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    const WORLD: &str = "myworld.dcl.eth";
    const SCENE_ID: &str = "bafkreigcene";
    const PARCEL: &str = "10,-25";

    fn scope() -> DelegationScope {
        DelegationScope {
            world: WORLD.to_string(),
            scene_id: SCENE_ID.to_string(),
            parcel: PARCEL.to_string(),
        }
    }

    fn envelope_from_payload(ephemeral_key: &str, payload: &str, signature: &str) -> String {
        let eph = Wallet::from_hex(ephemeral_key).unwrap();
        let env = serde_json::json!({
            "v": 1,
            "ephemeral": { "privateKey": ephemeral_key, "publicKey": "0x04", "address": eph.address() },
            "scope": { "payload": payload, "signature": signature },
        });
        BASE64.encode(env.to_string())
    }

    pub(crate) fn mint(
        ephemeral_key: &str,
        world: &str,
        scene_id: &str,
        parcel: &str,
        ttl_secs: i64,
    ) -> String {
        let authoritative = Wallet::from_hex(AUTHORITATIVE_KEY).unwrap();
        let eph = Wallet::from_hex(ephemeral_key).unwrap();
        let payload = claim_payload(
            &eph.address(),
            world,
            scene_id,
            parcel,
            Utc::now() + Duration::seconds(ttl_secs),
        );
        let signature = authoritative.sign_message(payload.as_bytes()).unwrap();
        envelope_from_payload(ephemeral_key, &payload, &signature)
    }

    #[test]
    fn the_wallet_signs_eip191_messages_the_recoverer_accepts() {
        let w = Wallet::from_hex(EPHEMERAL_KEY).unwrap();
        assert_eq!(w.address(), "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
        let sig = w.sign_message(b"hello").unwrap();
        assert_eq!(
            crate::auth::recover_address(b"hello", &sig).unwrap(),
            w.address()
        );
        let chain = create_simple_auth_chain(&w, "get:/x:1:{}").unwrap();
        assert_eq!(chain[0]["payload"], w.address());
        assert_eq!(chain[1]["type"], "ECDSA_SIGNED_ENTITY");
        assert!(Wallet::from_hex("0x1234").is_err());
        assert_ne!(
            Wallet::random().unwrap().address(),
            Wallet::random().unwrap().address()
        );
    }

    #[test]
    fn parses_a_minted_envelope_and_derives_fields_from_the_signed_payload() {
        let d = parse_storage_delegation(&mint(
            EPHEMERAL_KEY,
            "MyWorld.DCL.eth",
            SCENE_ID,
            PARCEL,
            3600,
        ))
        .expect("valid envelope must parse");
        let eph = Wallet::from_hex(EPHEMERAL_KEY).unwrap();
        assert_eq!(d.ephemeral_address, eph.address());
        assert_eq!(d.ephemeral.address(), eph.address());
        assert_eq!(d.world, WORLD, "world is lowercased");
        assert_eq!(d.scene_id, SCENE_ID);
        assert_eq!(d.parcel, PARCEL);
        assert!(!d.is_expired(Utc::now()));
        assert!(!d.near_expiry(Utc::now()));
        assert!(d.near_expiry(Utc::now() + Duration::seconds(3600 - 200)));
        assert!(d.is_expired(Utc::now() + Duration::seconds(3700)));
        assert!(d.scope_header.len() <= MAX_SCOPE_HEADER_LENGTH);
    }

    fn target<'a>(
        signer: &'a str,
        world: &'a str,
        scene_id: &'a str,
        parcel: &'a str,
        trusted: &'a [String],
    ) -> StorageDelegationTarget<'a> {
        StorageDelegationTarget {
            signer,
            world,
            scene_id,
            parcel,
            trusted_signers: trusted,
        }
    }

    #[test]
    fn scope_header_verifies_against_the_storage_side_verifier() {
        let authoritative = Wallet::from_hex(AUTHORITATIVE_KEY).unwrap();
        let trusted = vec![authoritative.address()];
        let d =
            parse_storage_delegation(&mint(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, 3600)).unwrap();
        let signer = d.ephemeral_address.clone();

        assert_eq!(
            verify_storage_delegation(
                &d.scope_header,
                &target(&signer, WORLD, SCENE_ID, PARCEL, &trusted)
            ),
            Ok(())
        );
        assert!(
            verify_storage_delegation(
                &d.scope_header,
                &target(&signer, "other.dcl.eth", SCENE_ID, PARCEL, &trusted)
            )
            .is_err()
        );
        assert!(
            verify_storage_delegation(
                &d.scope_header,
                &target(&signer, WORLD, "bafkreiother", PARCEL, &trusted)
            )
            .is_err()
        );
        assert!(
            verify_storage_delegation(
                &d.scope_header,
                &target(&signer, WORLD, SCENE_ID, "0,0", &trusted)
            )
            .is_err()
        );
        assert!(
            verify_storage_delegation(
                &d.scope_header,
                &target(
                    "0x2222222222222222222222222222222222222222",
                    WORLD,
                    SCENE_ID,
                    PARCEL,
                    &trusted
                )
            )
            .is_err()
        );

        let rogue = Wallet::from_hex(EPHEMERAL2_KEY).unwrap();
        let eph = Wallet::from_hex(EPHEMERAL_KEY).unwrap();
        let payload = claim_payload(
            &eph.address(),
            WORLD,
            SCENE_ID,
            PARCEL,
            Utc::now() + Duration::hours(1),
        );
        let signature = rogue.sign_message(payload.as_bytes()).unwrap();
        let d =
            parse_storage_delegation(&envelope_from_payload(EPHEMERAL_KEY, &payload, &signature))
                .unwrap();
        assert_eq!(
            verify_storage_delegation(
                &d.scope_header,
                &target(&signer, WORLD, SCENE_ID, PARCEL, &trusted)
            ),
            Err("claim not signed by a trusted authoritative address")
        );
    }

    #[test]
    fn the_local_minter_produces_the_old_envelope_scoped_and_verifiable() {
        let authoritative = Wallet::from_hex(AUTHORITATIVE_KEY).unwrap();
        let trusted = vec![authoritative.address()];
        let encoded = mint_envelope(
            &authoritative,
            &scope(),
            std::time::Duration::from_secs(3600),
            Utc::now(),
        )
        .unwrap();
        let decoded = BASE64.decode(&encoded).unwrap();
        assert_eq!(BASE64.encode(&decoded), encoded);
        let raw: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        let expiration = raw["scope"]["payload"]
            .as_str()
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("Expiration: "))
            .unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(expiration)
                .unwrap()
                .to_rfc3339(),
            expiration
        );
        assert_eq!(raw["v"], 1);
        assert!(
            raw["ephemeral"]["publicKey"]
                .as_str()
                .unwrap()
                .starts_with("0x04")
        );
        let d = parse_storage_delegation(&encoded).unwrap();
        assert_eq!(
            BASE64.encode(BASE64.decode(&d.scope_header).unwrap()),
            d.scope_header
        );
        assert_eq!(
            (d.world.as_str(), d.scene_id.as_str(), d.parcel.as_str()),
            (WORLD, SCENE_ID, PARCEL)
        );
        let delta = (d.expiration - Utc::now()).num_seconds();
        assert!(
            (3590..=3600).contains(&delta),
            "ttl must be ~1h, got {delta}"
        );
        assert_eq!(
            verify_storage_delegation(
                &d.scope_header,
                &target(&d.ephemeral_address, WORLD, SCENE_ID, PARCEL, &trusted)
            ),
            Ok(())
        );
        let again = parse_storage_delegation(
            &mint_envelope(
                &authoritative,
                &scope(),
                std::time::Duration::from_secs(60),
                Utc::now(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_ne!(
            again.ephemeral_address, d.ephemeral_address,
            "every mint is a fresh ephemeral key"
        );
    }

    #[test]
    fn expired_envelope_parses_but_fails_the_verifier_and_is_expired() {
        let authoritative = Wallet::from_hex(AUTHORITATIVE_KEY).unwrap();
        let trusted = vec![authoritative.address()];
        let d =
            parse_storage_delegation(&mint(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, -60)).unwrap();
        assert!(d.is_expired(Utc::now()));
        let signer = d.ephemeral_address.clone();
        assert_eq!(
            verify_storage_delegation(
                &d.scope_header,
                &target(&signer, WORLD, SCENE_ID, PARCEL, &trusted)
            ),
            Err("delegation expired")
        );
    }

    #[test]
    fn rejects_malformed_envelopes() {
        assert!(parse_storage_delegation("!!!not-base64!!!").is_none());
        assert!(parse_storage_delegation(&BASE64.encode("not json")).is_none());
        assert!(parse_storage_delegation(&BASE64.encode("[1,2]")).is_none());

        let eph = Wallet::from_hex(EPHEMERAL_KEY).unwrap();
        let authoritative = Wallet::from_hex(AUTHORITATIVE_KEY).unwrap();
        let exp = Utc::now() + Duration::hours(1);
        let payload = claim_payload(&eph.address(), WORLD, SCENE_ID, PARCEL, exp);
        let signature = authoritative.sign_message(payload.as_bytes()).unwrap();

        let cases: Vec<serde_json::Value> = vec![
            serde_json::json!({ "v": 2, "ephemeral": { "privateKey": EPHEMERAL_KEY, "publicKey": "0x04", "address": eph.address() }, "scope": { "payload": payload, "signature": signature } }),
            serde_json::json!({ "ephemeral": { "privateKey": EPHEMERAL_KEY, "publicKey": "0x04", "address": eph.address() }, "scope": { "payload": payload, "signature": signature } }),
            serde_json::json!({ "v": 1, "ephemeral": { "privateKey": 1, "publicKey": "0x04", "address": eph.address() }, "scope": { "payload": payload, "signature": signature } }),
            serde_json::json!({ "v": 1, "ephemeral": { "privateKey": EPHEMERAL_KEY, "publicKey": null, "address": eph.address() }, "scope": { "payload": payload, "signature": signature } }),
            serde_json::json!({ "v": 1, "ephemeral": { "privateKey": EPHEMERAL2_KEY, "publicKey": "0x04", "address": eph.address() }, "scope": { "payload": payload, "signature": signature } }),
            serde_json::json!({ "v": 1, "ephemeral": { "privateKey": EPHEMERAL_KEY, "publicKey": "0x04", "address": eph.address() } }),
            serde_json::json!({ "v": 1, "ephemeral": { "privateKey": EPHEMERAL_KEY, "publicKey": "0x04", "address": eph.address() }, "scope": { "payload": 5, "signature": signature } }),
        ];
        for (i, env) in cases.iter().enumerate() {
            assert!(
                parse_storage_delegation(&BASE64.encode(env.to_string())).is_none(),
                "case {i} must be rejected"
            );
        }

        for bad_payload in [
            format!("wrong prefix\nEphemeral: {}", eph.address()),
            format!(
                "{STORAGE_DELEGATION_PREFIX}\nEphemeral: {}\nWorld: {WORLD}\nSceneId: {SCENE_ID}\nParcel: {PARCEL}",
                eph.address()
            ),
            format!(
                "{STORAGE_DELEGATION_PREFIX}\nEphemeral: {}\nWorld: {WORLD}\nSceneId: {SCENE_ID}\nParcel: {PARCEL}\nExpiration: tomorrow",
                eph.address()
            ),
        ] {
            let sig = authoritative.sign_message(bad_payload.as_bytes()).unwrap();
            assert!(
                parse_storage_delegation(&envelope_from_payload(EPHEMERAL_KEY, &bad_payload, &sig))
                    .is_none(),
                "payload {bad_payload:?} must be rejected"
            );
        }

        let other = Wallet::from_hex(EPHEMERAL2_KEY).unwrap();
        let mismatched = claim_payload(&other.address(), WORLD, SCENE_ID, PARCEL, exp);
        let sig = authoritative.sign_message(mismatched.as_bytes()).unwrap();
        assert!(
            parse_storage_delegation(&envelope_from_payload(EPHEMERAL_KEY, &mismatched, &sig))
                .is_none()
        );
    }

    async fn serve_minter(
        response: serde_json::Value,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits2 = Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/delegations",
            axum::routing::post(move || {
                let hits = Arc::clone(&hits2);
                let response = response.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(response)
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), hits)
    }

    fn remote(url: String) -> Arc<DelegationMinter> {
        Arc::new(DelegationMinter::Remote {
            http: reqwest::Client::new(),
            url,
            token: None,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn renewal_loop_swaps_the_slot_on_demand() {
        let fresh = mint(EPHEMERAL2_KEY, WORLD, SCENE_ID, PARCEL, 3600);
        let (minter, _hits) = serve_minter(serde_json::json!({ "delegation": fresh })).await;

        let near_expiry =
            parse_storage_delegation(&mint(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, 60)).unwrap();
        let old_address = near_expiry.ephemeral_address.clone();
        let slot: DelegationSlot = Arc::new(Mutex::new(Some(near_expiry)));

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(renewal_loop(remote(minter), scope(), Arc::clone(&slot), rx));

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tx.send(done_tx).unwrap();
        tokio::time::timeout(RENEWAL_TIMEOUT, done_rx)
            .await
            .expect("renewal must answer within the timeout")
            .unwrap();

        let renewed = slot.lock().clone().expect("slot must stay filled");
        assert_ne!(
            renewed.ephemeral_address, old_address,
            "slot must be swapped"
        );
        assert!(!renewed.near_expiry(Utc::now()));
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn renewal_keeps_the_slot_when_the_minter_rebinds_or_is_down() {
        let rebind = mint(EPHEMERAL2_KEY, WORLD, "bafkreiother", PARCEL, 3600);
        let (minter, _hits) = serve_minter(serde_json::json!({ "delegation": rebind })).await;

        let current =
            parse_storage_delegation(&mint(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, 60)).unwrap();
        let old_address = current.ephemeral_address.clone();
        let slot: DelegationSlot = Arc::new(Mutex::new(Some(current)));

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(renewal_loop(remote(minter), scope(), Arc::clone(&slot), rx));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tx.send(done_tx).unwrap();
        tokio::time::timeout(RENEWAL_TIMEOUT, done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            slot.lock().clone().unwrap().ephemeral_address,
            old_address,
            "a rebinding minter answer must not replace the slot"
        );
        task.abort();

        let slot2: DelegationSlot = Arc::new(Mutex::new(Some(
            parse_storage_delegation(&mint(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, 60)).unwrap(),
        )));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(renewal_loop(
            remote("http://127.0.0.1:9".to_string()),
            scope(),
            Arc::clone(&slot2),
            rx,
        ));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tx.send(done_tx).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slot2.lock().clone().unwrap().ephemeral_address, old_address);
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mint_from_minter_rejects_a_rebind() {
        let rebind = mint(EPHEMERAL2_KEY, WORLD, SCENE_ID, "0,0", 3600);
        let (minter, _hits) = serve_minter(serde_json::json!({ "delegation": rebind })).await;
        let err = remote(minter).mint(&scope()).await.unwrap_err();
        assert!(err.to_string().contains("different scene"), "{err:#}");
        // The local minter answers its own scope, so it can never rebind.
        let local = DelegationMinter::Local {
            authoritative: Arc::new(Wallet::from_hex(AUTHORITATIVE_KEY).unwrap()),
            ttl: std::time::Duration::from_secs(120),
        };
        let d = local.mint(&scope()).await.unwrap();
        assert_eq!(
            (d.world.as_str(), d.scene_id.as_str(), d.parcel.as_str()),
            (WORLD, SCENE_ID, PARCEL)
        );
    }
}
