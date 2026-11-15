use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{Duration as ChronoDuration, Utc};
use parking_lot::Mutex;

use crate::http::Outbound;
use crate::scene_state::crdt::{CrdtMessage, decode_batch, encode_batch};
use crate::scene_state::delegation::{
    DelegationMinter, DelegationScope, DelegationSlot, Wallet, parse_storage_delegation,
    renewal_loop,
};
use crate::scene_state::jsruntime::{self, StorageCtx, parse_origin};
use crate::scene_state::runtime::RuntimeLimits;
use crate::shared::signed_fetch::build_legacy_payload;
use crate::storage::delegation::{StorageDelegationTarget, verify_storage_delegation};

const AUTHORITATIVE_KEY: &str =
    "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const EPHEMERAL_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const EPHEMERAL2_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

const WORLD: &str = "testworld.dcl.eth";
const SCENE_ID: &str = "bafkreitestscene";
const PARCEL: &str = "1,-2";

fn mint_envelope(
    ephemeral_key: &str,
    world: &str,
    scene_id: &str,
    parcel: &str,
    ttl_secs: i64,
) -> String {
    let authoritative = Wallet::from_hex(AUTHORITATIVE_KEY).unwrap();
    let eph = Wallet::from_hex(ephemeral_key).unwrap();
    let payload = format!(
        "Decentraland Authoritative Storage Delegation\nEphemeral: {}\nWorld: {}\nSceneId: {}\nParcel: {}\nExpiration: {}",
        eph.address(),
        world,
        scene_id,
        parcel,
        (Utc::now() + ChronoDuration::seconds(ttl_secs)).to_rfc3339()
    );
    let signature = authoritative.sign_message(payload.as_bytes()).unwrap();
    let env = serde_json::json!({
        "v": 1,
        "ephemeral": { "privateKey": ephemeral_key, "publicKey": "0x04", "address": eph.address() },
        "scope": { "payload": payload, "signature": signature },
    });
    BASE64.encode(env.to_string())
}

#[derive(Clone)]
struct Req {
    method: String,
    path_and_query: String,
    headers: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct Captured {
    requests: Arc<Mutex<Vec<Req>>>,
    /// Rung after every recorded request.
    arrived: Arc<tokio::sync::Notify>,
}

impl Captured {
    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .iter()
            .map(|r| r.path_and_query.clone())
            .collect()
    }
}

async fn start_storage_server() -> (String, Captured) {
    let captured = Captured::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let cap = captured.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let cap = cap.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path_and_query = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            let headers: Vec<(String, String)> = req
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|v| (k.as_str().to_string(), v.to_string()))
                })
                .collect();
            let path = path_and_query.split('?').next().unwrap_or("").to_string();
            cap.requests.lock().push(Req {
                method,
                path_and_query,
                headers,
            });
            cap.arrived.notify_waiters();
            match path.as_str() {
                "/redirect" => axum::response::Response::builder()
                    .status(302)
                    .header("location", "/target")
                    .body(axum::body::Body::empty())
                    .unwrap(),
                "/big" => axum::response::Response::builder()
                    .status(200)
                    .body(axum::body::Body::from(vec![b'x'; 4096]))
                    .unwrap(),
                _ => axum::response::Response::builder()
                    .status(200)
                    .header("set-cookie", "session=nope")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), captured)
}

/// Re-checks `pred` each time the storage server records a request.
async fn wait_for<F: Fn() -> bool>(pred: F, what: &str, captured: &Captured) {
    let seen = tokio::time::timeout(crate::shared::test_wait::HANG_GUARD, async {
        loop {
            let next = captured.arrived.notified();
            if pred() {
                return;
            }
            next.await;
        }
    })
    .await;
    assert!(
        seen.is_ok(),
        "timed out waiting for {what}; captured: {:?}",
        captured.paths()
    );
}

fn scene_registration() -> &'static str {
    "registerScene({ reservedLocalEntities: 512, networkEntitiesLimit: { serverLimit: 512, clientLimit: 512 } }, function (ev) {});"
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_fetch_pipeline_signs_caps_and_confines_requests() {
    let (base, captured) = start_storage_server().await;

    let delegation =
        parse_storage_delegation(&mint_envelope(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, 3600))
            .unwrap();
    let scope_header = delegation.scope_header.clone();
    let ephemeral_address = delegation.ephemeral_address.clone();
    let slot: DelegationSlot = Arc::new(Mutex::new(Some(delegation)));
    let ctx = StorageCtx {
        origin: parse_origin(&base, true).unwrap(),
        allow_http_loopback: true,
        delegation: slot,
        renew_tx: None,
        http: Outbound::for_tests(),
    };
    let limits = RuntimeLimits {
        fetch_max_response_bytes: 256,
        fetch_max_in_flight: 2,
        fetch_timeout_ms: 5_000,
        ..RuntimeLimits::default()
    };

    let js = format!(
        r#"
        var SF = require('~system/SignedFetch');
        var BASE = '{base}';
        function report(step, r) {{
            return SF.signedFetch({{ url: BASE + '/report?step=' + step + '&status=' + r.status + '&ok=' + r.ok }});
        }}
        module.exports.onStart = async function () {{
            var r1 = await SF.signedFetch({{ url: BASE + '/values/k?x=1', init: {{
                method: 'PUT',
                body: '{{"v":1}}',
                headers: {{
                    'x-identity-metadata': 'evil',
                    'X-Authoritative-Scope': 'evil',
                    'X-Custom': 'yes',
                    'Cookie': 'a=b',
                    'authorization': 'Bearer evil'
                }}
            }} }});
            await report('r1', r1);
            var r2 = await SF.signedFetch({{ url: BASE + '/redirect' }});
            await report('r2', r2);
            var r3 = await SF.signedFetch({{ url: BASE + '/big' }});
            await report('r3', r3);
            var r4 = await SF.signedFetch({{ url: 'https://storage.decentraland.org.evil.com/values/k' }});
            await report('r4', r4);
            var caps = await Promise.all([
                SF.signedFetch({{ url: BASE + '/values/a' }}),
                SF.signedFetch({{ url: BASE + '/values/b' }}),
                SF.signedFetch({{ url: BASE + '/values/c' }})
            ]);
            var rejected = 0;
            for (var i = 0; i < caps.length; i++) if (caps[i].status === 400) rejected++;
            await report('cap', {{ status: rejected, ok: false }});
            await report('done', {{ status: 0, ok: true }});
        }};
        {registration}
        "#,
        base = base,
        registration = scene_registration(),
    );

    let handle = jsruntime::spawn(
        "fetch-scene".into(),
        js,
        "dcl-test".into(),
        limits,
        Vec::new(),
        Some(ctx),
    );

    let cap2 = captured.clone();
    wait_for(
        || cap2.paths().iter().any(|p| p.contains("step=done")),
        "the scene's done report",
        &captured,
    )
    .await;
    handle.shutdown();

    let reqs: Vec<Req> = captured.requests.lock().clone();
    let report = |step: &str| -> String {
        reqs.iter()
            .map(|r| r.path_and_query.clone())
            .find(|p| p.contains(&format!("step={step}&")))
            .unwrap_or_default()
    };

    assert!(
        report("r1").contains("status=200&ok=true"),
        "{}",
        report("r1")
    );
    assert!(
        report("r2").contains("status=302&ok=false"),
        "{}",
        report("r2")
    );
    assert!(
        report("r3").contains("status=500&ok=false"),
        "{}",
        report("r3")
    );
    assert!(
        report("r4").contains("status=400&ok=false"),
        "{}",
        report("r4")
    );
    assert!(
        report("cap").contains("status=1&"),
        "exactly one over-cap fetch must be rejected: {}",
        report("cap")
    );

    assert!(
        !reqs.iter().any(|r| r.path_and_query.starts_with("/target")),
        "redirect must not be followed"
    );
    assert_eq!(
        reqs.iter()
            .filter(|r| r.path_and_query.starts_with("/big"))
            .count(),
        1
    );
    assert_eq!(
        reqs.iter()
            .filter(|r| r.path_and_query.starts_with("/values/k"))
            .count(),
        1,
        "the off-origin lookalike URL must never reach the server"
    );

    let put = reqs
        .iter()
        .find(|r| r.method == "PUT" && r.path_and_query == "/values/k?x=1")
        .expect("the PUT must arrive with its query intact");
    let header = |name: &str| {
        put.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    assert_eq!(header("x-custom").as_deref(), Some("yes"));
    assert!(header("cookie").is_none());
    assert!(header("authorization").is_none());
    assert_eq!(
        header("x-authoritative-scope").as_deref(),
        Some(scope_header.as_str())
    );
    let metadata = header("x-identity-metadata").expect("identity metadata");
    assert_ne!(metadata, "evil");

    let meta: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(meta["realmName"], WORLD);
    assert_eq!(meta["realm"]["serverName"], WORLD);
    assert_eq!(meta["sceneId"], SCENE_ID);
    assert_eq!(meta["parcel"], PARCEL);
    assert_eq!(meta["isGuest"], false);

    let ts = header("x-identity-timestamp").expect("timestamp");
    let link0: serde_json::Value =
        serde_json::from_str(&header("x-identity-auth-chain-0").unwrap()).unwrap();
    let link1: serde_json::Value =
        serde_json::from_str(&header("x-identity-auth-chain-1").unwrap()).unwrap();
    assert_eq!(link0["type"], "SIGNER");
    let signer = link0["payload"].as_str().unwrap().to_string();
    assert_eq!(signer.to_lowercase(), ephemeral_address.to_lowercase());
    let expected_payload = build_legacy_payload("put", "/values/k", &ts, &metadata);
    assert_eq!(link1["payload"].as_str().unwrap(), expected_payload);
    let recovered = crate::auth::recover_address(
        expected_payload.as_bytes(),
        link1["signature"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(recovered.to_lowercase(), ephemeral_address.to_lowercase());

    let authoritative = Wallet::from_hex(AUTHORITATIVE_KEY).unwrap();
    let trusted = vec![authoritative.address()];
    let wire_scope = header("x-authoritative-scope").unwrap();
    let decoded_scope = BASE64.decode(&wire_scope).unwrap();
    assert_eq!(BASE64.encode(&decoded_scope), wire_scope);
    let scope: serde_json::Value = serde_json::from_slice(&decoded_scope).unwrap();
    let expiration = scope["payload"]
        .as_str()
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Expiration: "))
        .unwrap();
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(expiration)
            .unwrap()
            .to_rfc3339(),
        expiration
    );
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
    assert_eq!(
        verify_storage_delegation(
            &wire_scope,
            &target(&signer, WORLD, SCENE_ID, PARCEL, &trusted)
        ),
        Ok(())
    );
    assert!(
        verify_storage_delegation(
            &wire_scope,
            &target(&signer, "other.dcl.eth", SCENE_ID, PARCEL, &trusted)
        )
        .is_err()
    );
    assert!(
        verify_storage_delegation(
            &wire_scope,
            &target(&signer, WORLD, "bafkreiother", PARCEL, &trusted)
        )
        .is_err()
    );
    assert!(
        verify_storage_delegation(
            &wire_scope,
            &target(&signer, WORLD, SCENE_ID, "0,0", &trusted)
        )
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_delegation_fails_closed_without_touching_the_wire() {
    let (base, captured) = start_storage_server().await;
    let delegation =
        parse_storage_delegation(&mint_envelope(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, -60))
            .unwrap();
    let ctx = StorageCtx {
        origin: parse_origin(&base, true).unwrap(),
        allow_http_loopback: true,
        delegation: Arc::new(Mutex::new(Some(delegation))),
        renew_tx: None,
        http: Outbound::for_tests(),
    };

    let expected = encode_batch(&[CrdtMessage::Put {
        entity: 700,
        component_id: 1,
        timestamp: 1,
        data: vec![1],
    }]);
    let unexpected = encode_batch(&[CrdtMessage::Put {
        entity: 701,
        component_id: 1,
        timestamp: 1,
        data: vec![2],
    }]);
    let js = format!(
        r#"
        var SF = require('~system/SignedFetch');
        var EngineApi = require('~system/EngineApi');
        module.exports.onStart = async function () {{
            var r = await SF.signedFetch({{ url: '{base}/values/x' }});
            var bytes = new Uint8Array(r.status === 500 && !r.ok ? {expected:?} : {unexpected:?});
            await EngineApi.crdtSendToRenderer({{ data: bytes }});
        }};
        {registration}
        "#,
        base = base,
        expected = expected,
        unexpected = unexpected,
        registration = scene_registration(),
    );

    let handle = jsruntime::spawn(
        "expired-scene".into(),
        js,
        "dcl-test".into(),
        RuntimeLimits::default(),
        Vec::new(),
        Some(ctx),
    );

    let engine = Arc::clone(&handle.shared.engine);
    assert!(
        crate::shared::test_wait::until(async || engine.lock().component_count() >= 1).await,
        "timed out waiting for the scene to observe the fetch result; captured: {:?}",
        captured.paths()
    );
    let snapshot = handle.shared.snapshot.lock().clone();
    handle.shutdown();

    let msgs = decode_batch(&snapshot);
    assert!(
        msgs.iter()
            .any(|m| matches!(m, CrdtMessage::Put { entity: 700, .. })),
        "the fetch must fail closed with a 500, got {msgs:?}"
    );
    assert!(
        captured.requests.lock().is_empty(),
        "no request may be sent with an expired delegation: {:?}",
        captured.paths()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn near_expiry_delegation_is_renewed_before_the_request() {
    let (base, captured) = start_storage_server().await;

    let fresh = mint_envelope(EPHEMERAL2_KEY, WORLD, SCENE_ID, PARCEL, 3600);
    let minter_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let minter_url = format!("http://{}", minter_listener.local_addr().unwrap());
    let minter_app = axum::Router::new().route(
        "/delegations",
        axum::routing::post(move || {
            let fresh = fresh.clone();
            async move { axum::Json(serde_json::json!({ "delegation": fresh })) }
        }),
    );
    tokio::spawn(async move {
        axum::serve(minter_listener, minter_app).await.unwrap();
    });

    let near_expiry =
        parse_storage_delegation(&mint_envelope(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, 60))
            .unwrap();
    let slot: DelegationSlot = Arc::new(Mutex::new(Some(near_expiry)));
    let (renew_tx, renew_rx) = tokio::sync::mpsc::unbounded_channel();
    let renewal = tokio::spawn(renewal_loop(
        Arc::new(DelegationMinter::Remote {
            http: reqwest::Client::new(),
            url: minter_url,
            token: None,
        }),
        DelegationScope {
            world: WORLD.to_string(),
            scene_id: SCENE_ID.to_string(),
            parcel: PARCEL.to_string(),
        },
        Arc::clone(&slot),
        renew_rx,
    ));
    let ctx = StorageCtx {
        origin: parse_origin(&base, true).unwrap(),
        allow_http_loopback: true,
        delegation: Arc::clone(&slot),
        renew_tx: Some(renew_tx),
        http: Outbound::for_tests(),
    };

    let js = format!(
        r#"
        var SF = require('~system/SignedFetch');
        module.exports.onStart = async function () {{
            await SF.signedFetch({{ url: '{base}/values/renewed' }});
        }};
        {registration}
        "#,
        base = base,
        registration = scene_registration(),
    );
    let handle = jsruntime::spawn(
        "renew-scene".into(),
        js,
        "dcl-test".into(),
        RuntimeLimits::default(),
        Vec::new(),
        Some(ctx),
    );

    let cap2 = captured.clone();
    wait_for(
        || {
            cap2.paths()
                .iter()
                .any(|p| p.starts_with("/values/renewed"))
        },
        "the renewed request",
        &captured,
    )
    .await;
    handle.shutdown();
    renewal.abort();

    let reqs: Vec<Req> = captured.requests.lock().clone();
    let req = reqs
        .iter()
        .find(|r| r.path_and_query.starts_with("/values/renewed"))
        .unwrap();
    let link0: serde_json::Value = serde_json::from_str(
        &req.headers
            .iter()
            .find(|(k, _)| k == "x-identity-auth-chain-0")
            .map(|(_, v)| v.clone())
            .expect("signed request"),
    )
    .unwrap();
    let renewed_signer = link0["payload"].as_str().unwrap().to_lowercase();
    let eph2 = Wallet::from_hex(EPHEMERAL2_KEY).unwrap();
    assert_eq!(
        renewed_signer,
        eph2.address().to_lowercase(),
        "the request must be signed by the RENEWED ephemeral key"
    );
    assert_eq!(
        slot.lock()
            .as_ref()
            .unwrap()
            .ephemeral_address
            .to_lowercase(),
        eph2.address().to_lowercase()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_with_a_query_is_signed_over_the_pathname_and_verifies() {
    let (base, captured) = start_storage_server().await;
    let delegation =
        parse_storage_delegation(&mint_envelope(EPHEMERAL_KEY, WORLD, SCENE_ID, PARCEL, 3600))
            .unwrap();
    let ephemeral_address = delegation.ephemeral_address.clone();
    let ctx = StorageCtx {
        origin: parse_origin(&base, true).unwrap(),
        allow_http_loopback: true,
        delegation: Arc::new(Mutex::new(Some(delegation))),
        renew_tx: None,
        http: Outbound::for_tests(),
    };
    let js = format!(
        r#"
        var SF = require('~system/SignedFetch');
        module.exports.onStart = async function () {{
            await SF.signedFetch({{ url: '{base}/values?prefix=a&limit=10&offset=0' }});
        }};
        {registration}
        "#,
        registration = scene_registration(),
    );
    let handle = jsruntime::spawn(
        "list-scene".into(),
        js,
        "dcl-test".into(),
        RuntimeLimits::default(),
        Vec::new(),
        Some(ctx),
    );
    let cap2 = captured.clone();
    wait_for(
        || cap2.paths().iter().any(|p| p.starts_with("/values?")),
        "the listing",
        &captured,
    )
    .await;
    handle.shutdown();

    let req = captured.requests.lock()[0].clone();
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in &req.headers {
        headers.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    let uri: axum::http::Uri = req.path_and_query.parse().unwrap();
    let paths = crate::storage::signed_paths(&headers, &uri, None);
    let verified = crate::storage::auth::verify_request(&headers, "get", &paths, None)
        .await
        .expect("the storage routes verify the scene host's listing");
    assert_eq!(
        verified.signer,
        crate::shared::identity::Address::parse(&ephemeral_address).unwrap()
    );
    // Signed over the pathname alone, as upstream storage verifies.
    let with_query = vec![req.path_and_query.clone()];
    assert!(
        crate::storage::auth::verify_request(&headers, "get", &with_query, None)
            .await
            .is_err()
    );
}
