//! The local write path through the content router, the way `serve` wires
//! it: the multipart deploy with the content validator attached (its access
//! checks against a chain on the loopback), the durable denylist on deploy
//! and on the reads, and the signed unpublish (its LAND check over the
//! `land_authz` index); the gates in front of both routes.
//!
//! The scene round trip runs on a database of its own (the LAND index's
//! cursor row is global), created from `CATALYST_TEST_DATABASE_URL` and
//! dropped at the end; the profile contract runs on the process's scratch
//! database. Both skip without the variable.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::signers::{Signer as _, local::PrivateKeySigner};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode, header};
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;

use super::tests::support::{TestNode, require_test_db};
use super::write_deployer::WriteDeployer;
use super::{ContentState, Settings, SharedState, SyncState, build_router, writes};
use crate::auth::{AuthLink, AuthLinkType, hash_bytes_v1};
use crate::http::Outbound;
use crate::land_authz::{LandAuthzState, events::AuthzEvent, indexer};
use crate::shared::signed_fetch::{
    AUTH_CHAIN_HEADER_PREFIX, AUTH_METADATA_HEADER, AUTH_TIMESTAMP_HEADER, build_payload_v6,
};
use crate::storage::Storage;
use crate::validator::fake_chain::{self, FakeChain, Query, Reply};

/// Hardhat accounts #0 and #1: the LAND owner and a stranger.
const OWNER_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const STRANGER_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

fn key(hex: &str) -> PrivateKeySigner {
    hex.parse().unwrap()
}

fn address(signer: &PrivateKeySigner) -> String {
    format!("{:#x}", signer.address())
}

/// An entity file signed by `signer`: its bytes, id and auth chain.
async fn signed_entity(signer: &PrivateKeySigner, entity: Value) -> (Bytes, String, Value) {
    let bytes = Bytes::from(serde_json::to_vec(&entity).unwrap());
    signed_entity_bytes(signer, bytes).await
}

async fn signed_entity_bytes(signer: &PrivateKeySigner, bytes: Bytes) -> (Bytes, String, Value) {
    let id = hash_bytes_v1(&bytes);
    let signature = signer
        .sign_message(id.as_bytes())
        .await
        .unwrap()
        .to_string();
    let chain = json!([
        {"type": "SIGNER", "payload": address(signer), "signature": ""},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": id, "signature": signature}
    ]);
    (bytes, id, chain)
}

fn scene(pointers: &[&str], files: &[(&str, &str)]) -> Value {
    json!({
        "version": "v3",
        "type": "scene",
        "pointers": pointers,
        "timestamp": chrono::Utc::now().timestamp_millis(),
        "content": files.iter().map(|(file, hash)| json!({"file": file, "hash": hash})).collect::<Vec<_>>(),
        "metadata": {
            "display": {"title": "write path e2e"},
            "main": "bin/index.js",
            "scene": {"base": pointers[0], "parcels": pointers},
        },
    })
}

/// The multipart form the SDK sends: `entityId`, `authChain` as JSON, and
/// every file as a part named by its hash.
/// A chain of the right shape for `entity_id` whose signature checks fail.
fn unsigned_chain(entity_id: &str) -> Value {
    json!([
        {"type": "SIGNER", "payload": "0x0000000000000000000000000000000000000001", "signature": ""},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": entity_id, "signature": "0x01"}
    ])
}

fn deploy_request(
    path: &str,
    entity_id: &str,
    chain: &Value,
    files: &[(&str, &Bytes)],
) -> Request<Body> {
    let boundary = "catalyst-write-path-boundary";
    let mut body: Vec<u8> = Vec::new();
    let mut field = |name: &str, value: &[u8], filename: Option<&str>| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
            ),
            None => body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes()),
        }
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    };
    field("entityId", entity_id.as_bytes(), None);
    field("authChain", chain.to_string().as_bytes(), None);
    for (hash, bytes) in files {
        field(hash, bytes, Some(hash));
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::post(path)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// `DELETE path` signed by `signer` over the 6.x payload.
async fn signed_delete(signer: &PrivateKeySigner, path: &str) -> Request<Body> {
    signed_delete_over(signer, path, path).await
}

/// `DELETE path` carrying a chain `signer` signed over `signed_path`.
async fn signed_delete_over(
    signer: &PrivateKeySigner,
    signed_path: &str,
    path: &str,
) -> Request<Body> {
    let timestamp = chrono::Utc::now().timestamp_millis().to_string();
    let metadata = r#"{"signer":"dcl:explorer"}"#;
    let payload = build_payload_v6("delete", signed_path, &timestamp, metadata);
    let signature = signer
        .sign_message(payload.as_bytes())
        .await
        .unwrap()
        .to_string();
    let chain = [
        AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: address(signer),
            signature: None,
        },
        AuthLink {
            link_type: AuthLinkType::EcdsaSignedEntity,
            payload,
            signature: Some(signature),
        },
    ];
    let mut req = Request::delete(path).body(Body::empty()).unwrap();
    let headers: &mut HeaderMap = req.headers_mut();
    headers.insert(
        AUTH_TIMESTAMP_HEADER,
        HeaderValue::from_str(&timestamp).unwrap(),
    );
    headers.insert(
        AUTH_METADATA_HEADER,
        HeaderValue::from_static(r#"{"signer":"dcl:explorer"}"#),
    );
    for (i, link) in chain.iter().enumerate() {
        headers.insert(
            HeaderName::from_bytes(format!("{AUTH_CHAIN_HEADER_PREFIX}{i}").as_bytes()).unwrap(),
            HeaderValue::from_str(&serde_json::to_string(link).unwrap()).unwrap(),
        );
    }
    req
}

async fn call(router: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    (
        status,
        headers,
        to_bytes(resp.into_body(), 1 << 26).await.unwrap(),
    )
}

fn json_of(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

async fn active_by_pointers(router: &Router, pointers: &[&str]) -> Value {
    let req = Request::post("/content/entities/active")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"pointers": pointers}).to_string()))
        .unwrap();
    let (status, _, body) = call(router, req).await;
    assert_eq!(status, StatusCode::OK);
    json_of(&body)
}

/// Attaches a deployer over the node's pool and store: the validator over
/// `chain` (none: chain checks undecidable), the unpublish LAND check over
/// `land`, the node's denylist.
fn attach(
    state: &SharedState,
    enabled: bool,
    land: Option<Arc<LandAuthzState>>,
    chain: Option<&FakeChain>,
) {
    let sources = super::validator::Sources::new(
        state.pool.clone(),
        state.storage.clone(),
        Outbound::for_tests(),
        None,
        super::validator::Settings {
            eth_network: "mainnet".into(),
            l1_rpc_url: chain.map(|c| c.url.clone()),
            l2_rpc_url: chain.map(|c| c.url.clone()),
            ..Default::default()
        },
    );
    let deployer = WriteDeployer::new(
        state.pool.clone(),
        state.storage.clone(),
        None,
        Arc::new(sources),
    )
    .with_land_authz(land)
    .with_denylist(state.denylist.clone());
    assert!(state.attach_writes(enabled, Arc::new(deployer)));
}

#[tokio::test]
async fn the_write_routes_answer_their_gates_in_order() {
    let node = TestNode::without_db("write-gates").await;
    let router = node.router();
    let not_multipart = || {
        Request::post("/content/entities")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"hello":"world"}"#))
            .unwrap()
    };
    let (status, _, _) = call(&router, not_multipart()).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the gates answer before the body is looked at"
    );

    let chain = json!([{"type": "SIGNER", "payload": "0xabc"}]);
    for path in ["/content/entities", "/entities"] {
        let (status, headers, body) =
            call(&router, deploy_request(path, "bafy", &chain, &[])).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(headers[header::RETRY_AFTER], "3600");
        assert_eq!(json_of(&body), json!({"error": writes::DISABLED_MESSAGE}));
    }
    let (status, _, _) = call(
        &router,
        Request::delete("/content/scenes/0,0")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "unpublish is gated by the same switch"
    );

    let disabled = TestNode::without_db("write-gates-off").await;
    attach(&disabled.state, false, None, None);
    let (status, _, _) = call(
        &disabled.router(),
        deploy_request("/entities", "bafy", &chain, &[]),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "an attached but disabled write path is still off"
    );

    // Read-only answers as upstream's `READ_ONLY` node, which has no write
    // route: the router's not-found, ahead of the switch.
    disabled.state.set_read_only(true);
    let (status, _, body) = call(
        &disabled.router(),
        deploy_request("/entities", "bafy", &chain, &[]),
    )
    .await;
    assert_eq!(
        (status, &body[..]),
        (StatusCode::NOT_FOUND, &b"Not found"[..])
    );
    attach(&node.state, true, None, None);
    node.state.set_read_only(true);
    let unrouted = call(
        &router,
        Request::post("/content/no-such-route")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, headers, body) = call(
        &router,
        deploy_request("/content/entities", "bafy", &chain, &[]),
    )
    .await;
    assert_eq!(
        (status, &body[..]),
        (StatusCode::NOT_FOUND, &b"Not found"[..])
    );
    assert_eq!(
        (status, headers[header::CONTENT_TYPE].clone(), body),
        (
            unrouted.0,
            unrouted.1[header::CONTENT_TYPE].clone(),
            unrouted.2
        ),
        "the same answer as a path the router does not serve"
    );
    let (status, _, body) = call(
        &router,
        Request::delete("/scenes/0,0").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        (status, &body[..]),
        (StatusCode::NOT_FOUND, &b"Not found"[..])
    );
    let other_method = |method: &str| {
        Request::builder()
            .method(method)
            .uri("/content/entities")
            .body(Body::empty())
            .unwrap()
    };
    for method in ["GET", "PUT", "DELETE"] {
        let (status, _, body) = call(&router, other_method(method)).await;
        assert_eq!(
            (status, &body[..]),
            (StatusCode::NOT_FOUND, &b"Not found"[..]),
            "{method} while read-only"
        );
    }
    node.state.set_read_only(false);
    for method in ["GET", "PUT", "DELETE"] {
        let (status, headers, _) = call(&router, other_method(method)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(headers[header::ALLOW], "POST");
    }

    for req in [
        deploy_request("/content/entities", "bafy", &chain, &[]),
        not_multipart(),
    ] {
        let (status, headers, body) = call(&router, req).await;
        assert_eq!(
            (status, json_of(&body)),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": writes::BOOTSTRAPPING_MESSAGE})
            )
        );
        assert_eq!(headers[header::RETRY_AFTER], "60");
    }

    node.state.sync.set_state(SyncState::Syncing);
    let (status, headers, body) = call(&router, not_multipart()).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "Invalid request: expected a multipart/form-data body"})
        )
    );
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let (status, _, body) = call(
        &router,
        deploy_request("/content/entities", "bafy", &json!([]), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    let (status, _, body) = call(
        &router,
        Request::delete("/content/scenes/0,0")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "Invalid Auth Chain"})
        ),
        "an unsigned unpublish"
    );
}

/// `SERVE_UNSYNCED` forges `Syncing` for the read contracts. While
/// `SYNC_SOURCE` names a peer (its default does) the node is an unsynced copy
/// of that peer and keeps the bootstrapping refusal; with no source at all (an
/// island or a journey node) it is its own authority and a deploy passes.
#[tokio::test]
async fn serve_unsynced_takes_deploys_only_with_no_sync_source() {
    for (i, (source, takes_deploys)) in [
        (None, false),
        (Some("https://peer.example/content"), false),
        (Some(""), true),
        (Some(" , "), true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut argv = vec!["x", "--database-url", "postgres:///c", "--serve-unsynced"];
        argv.extend(source.map(|s| ["--sync-source", s]).into_iter().flatten());
        let config = crate::config::Config::parse_for_tests(&argv).unwrap();
        let node = TestNode::without_db(&format!("serve-unsynced-{i}")).await;
        attach(&node.state, true, None, None);
        crate::sync::worker::serve_unsynced(&config, &node.state.sync);
        assert_eq!(node.state.sync.state_name(), "Syncing", "{source:?}");

        let (status, _, body) = call(
            &node.router(),
            deploy_request("/content/entities", "bafy", &json!([]), &[]),
        )
        .await;
        if takes_deploys {
            assert_eq!(status, StatusCode::BAD_REQUEST, "{source:?}: {body:?}");
        } else {
            assert_eq!(
                (status, json_of(&body)),
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"error": writes::BOOTSTRAPPING_MESSAGE})
                ),
                "{source:?}"
            );
        }
    }
}

/// `SERVE_UNSYNCED_DEPLOY_TYPES` opens an unsynced copy to the types it
/// names (a development stack's own profiles): such a deploy passes the gate
/// to the deployer's validation, any other type keeps the bootstrapping 503.
#[tokio::test]
async fn an_unsynced_copy_takes_the_deploy_types_it_names() {
    let config = crate::config::Config::parse_for_tests(&[
        "x",
        "--database-url",
        "postgres:///c",
        "--serve-unsynced",
        "--sync-source",
        "https://peer.example/content",
        "--serve-unsynced-deploy-types",
        " Profile ,",
    ])
    .unwrap();
    let node = TestNode::without_db("serve-unsynced-types").await;
    attach(&node.state, true, None, None);
    crate::sync::worker::serve_unsynced(&config, &node.state.sync);
    assert!(!node.state.sync.takes_deploys());
    assert!(node.state.sync.takes_deploys_of("profile"));

    let entity = |kind: &str| {
        Bytes::from(
            json!({"version": "v3", "type": kind, "pointers": ["0x00000000000000000000000000000000000000aa"],
                   "timestamp": chrono::Utc::now().timestamp_millis(), "content": [], "metadata": {}})
            .to_string(),
        )
    };
    for (kind, taken) in [("profile", true), ("scene", false)] {
        let file = entity(kind);
        let id = crate::auth::hash_bytes_v1(&file);
        let (status, _, body) = call(
            &node.router(),
            deploy_request(
                "/content/entities",
                &id,
                &unsigned_chain(&id),
                &[(&id, &file)],
            ),
        )
        .await;
        let refused = (status, json_of(&body))
            == (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": writes::BOOTSTRAPPING_MESSAGE}),
            );
        assert_eq!(!refused, taken, "{kind}: {status} {body:?}");
        if taken {
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "the unsigned profile is validated: {body:?}"
            );
        }
    }
    let other = Bytes::from_static(b"{}");
    let (status, _, body) = call(
        &node.router(),
        deploy_request(
            "/content/entities",
            "bafynotthefile",
            &unsigned_chain("bafynotthefile"),
            &[("x", &other)],
        ),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"errors": ["The entity file was not part of the uploaded files or has no type."]})
        ),
        "no entity file, no type: refused"
    );
}

/// Pointers are lowercased as the entity file is read, so two that differ
/// only by case are the same pointer twice.
#[tokio::test]
async fn pointers_differing_only_by_case_are_repeated() {
    let node = TestNode::without_db("case-pointers").await;
    attach(&node.state, true, None, None);
    node.state.sync.set_state(SyncState::Syncing);
    let pointer =
        "urn:decentraland:matic:collections-v2:0xAbC0000000000000000000000000000000000001:0";
    // Signed: the signature is checked before the validator's rules.
    let (file, id, chain) = signed_entity(
        &PrivateKeySigner::random(),
        json!({"version": "v3", "type": "wearable", "pointers": [pointer, pointer.to_lowercase()],
               "timestamp": chrono::Utc::now().timestamp_millis(), "content": [], "metadata": {}}),
    )
    .await;
    let (status, _, body) = call(
        &node.router(),
        deploy_request("/content/entities", &id, &chain, &[(&id, &file)]),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"errors": ["There are repeated pointers in your request."]})
        )
    );
}

/// A switch that is on still waits for the restore stamps: a stale backup
/// stamp answers the content 503 naming the stamp and its age, on the
/// deploy and the unpublish alike, before the read-only and bootstrapping
/// gates; a fresh one lets the request on to them.
#[tokio::test]
async fn a_stale_restore_stamp_holds_content_writes_and_names_it() {
    use crate::shared::writes::restore::{
        lab::{HOUR, StampLab},
        scoped,
    };
    let node = TestNode::without_db("write-gates-held").await;
    let router = node.router();
    attach(&node.state, true, None, None);
    let chain = json!([{"type": "SIGNER", "payload": "0xabc"}]);
    let lab = StampLab::fresh();
    scoped(lab.stamps.clone(), async {
        let (_, _, body) = call(
            &router,
            deploy_request("/content/entities", "bafy", &chain, &[]),
        )
        .await;
        assert_eq!(
            json_of(&body),
            json!({"error": writes::BOOTSTRAPPING_MESSAGE}),
            "fresh stamps pass the request on to the next gate"
        );
        lab.advance(30 * HOUR);
        let held = "content writes are held on this node: the last complete backup (backup.json) is 31 h old, past WRITES_BACKUP_MAX_AGE_HOURS=26";
        let (status, headers, body) = call(
            &router,
            deploy_request("/content/entities", "bafy", &chain, &[]),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers[header::RETRY_AFTER], "3600");
        assert_eq!(json_of(&body), json!({"error": held}));
        let (status, _, body) = call(
            &router,
            Request::delete("/content/scenes/0,0")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            (status, json_of(&body)),
            (StatusCode::SERVICE_UNAVAILABLE, json!({"error": held}))
        );
    })
    .await;
}

/// Content stores blobs: a backup that left them out (`--no-blobs`) holds
/// its writes, naming the stamp.
#[tokio::test]
async fn a_backup_without_blobs_holds_content_writes() {
    use crate::shared::writes::restore::{Stamp, lab::StampLab, scoped};
    let node = TestNode::without_db("write-gates-no-blobs").await;
    let router = node.router();
    attach(&node.state, true, None, None);
    let chain = json!([{"type": "SIGNER", "payload": "0xabc"}]);
    let lab = StampLab::fresh();
    lab.write_json(
        Stamp::Backup,
        json!({
            "kind": "backup",
            "completed_at": (lab.now() - chrono::Duration::hours(1)).to_rfc3339(),
            "snapshot": "20261002T020000Z",
            "manifest_sha256": "ab",
            "blobs": { "included": false },
        }),
    );
    scoped(lab.stamps.clone(), async {
        let (status, _, body) = call(
            &router,
            deploy_request("/content/entities", "bafy", &chain, &[]),
        )
        .await;
        assert_eq!(
            (status, json_of(&body)),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": "content writes are held on this node: the last complete backup (backup.json) left the blobs out (backup.sh --no-blobs)"})
            )
        );
    })
    .await;
}

/// A database of its own, created from `CATALYST_TEST_DATABASE_URL` with
/// schema.sql applied; dropped by [`OwnDb::drop_now`].
struct OwnDb {
    admin: PgPool,
    name: String,
    pool: PgPool,
}

impl OwnDb {
    async fn create() -> Option<Self> {
        let base = std::env::var("CATALYST_TEST_DATABASE_URL").ok()?;
        let opts: PgConnectOptions = base.parse().unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.database("postgres"))
            .await
            .unwrap();
        let pool = crate::db::scratch::isolated_pool(6).await?;
        let name = pool.connect_options().get_database().unwrap().to_string();
        Some(Self { admin, name, pool })
    }

    async fn drop_now(self) {
        self.pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
    }
}

/// The decimal token id of LAND (x, y), as the registry packs it.
fn land_token(x: i32, y: i32) -> String {
    use alloy_primitives::U256;
    let value: U256 = (U256::from(x as i128 as u128) << 128usize) | U256::from(y as i128 as u128);
    value.to_string()
}

/// Folds `owner`'s LAND at `parcels` into the index and publishes the fold.
async fn index_land(pool: &PgPool, owner: &str, parcels: &[(i32, i32)]) {
    let grants: Vec<_> = parcels
        .iter()
        .map(|&p| (crate::land_authz::KIND_TRANSFER, p, owner))
        .collect();
    index_land_events(pool, &grants).await;
}

/// Folds LAND registry events `(kind, parcel, address)` into the index and
/// publishes the fold: a transfer's address is the new owner, an update
/// operator's the operator.
async fn index_land_events(pool: &PgPool, grants: &[(&'static str, (i32, i32), &str)]) {
    let settings = crate::land_authz::Settings::for_tests();
    let start = settings.start_block as i64;
    indexer::ensure_cursor(pool, &settings).await.unwrap();
    let events: Vec<AuthzEvent> = grants
        .iter()
        .enumerate()
        .map(|(i, &(kind, (x, y), address))| {
            let transfer = kind == crate::land_authz::KIND_TRANSFER;
            AuthzEvent {
                block_number: start + 10,
                log_index: i as i32,
                block_time: (start + 10) * 12,
                token_address: crate::land_authz::LAND_REGISTRY_MAINNET.to_string(),
                kind,
                token_id: Some(land_token(x, y)),
                estate_id: None,
                account: transfer.then(|| address.to_string()),
                operator: (!transfer).then(|| address.to_string()),
                approved: None,
            }
        })
        .collect();
    let mut conn = pool.acquire().await.unwrap();
    indexer::insert_events(&mut conn, &events).await.unwrap();
    indexer::set_cursor(&mut conn, &settings, start + 100)
        .await
        .unwrap();
    drop(conn);
    indexer::fold(pool, &settings, None).await.unwrap();
    assert!(
        indexer::mark_folded(pool, &settings, (start + 100) as u64)
            .await
            .unwrap()
    );
}

/// The old publish round trip, through the routes: the LAND owner deploys a
/// signed scene (`CONTENT_WRITES_ENABLED`, the validator's LAND leg over the
/// index), `/entities/active` returns it and `/contents/{hash}` serves its
/// files, `/audit` shows the local provenance; a stranger is refused on the
/// same parcel; the denylist hides and refuses; the stranger cannot
/// unpublish; the owner can, once, and the parcel is empty afterwards.
#[tokio::test]
async fn a_signed_scene_deploys_serves_and_unpublishes_end_to_end() {
    signed_scene_roundtrip(false).await;
}

#[tokio::test]
async fn a_signed_positional_scene_keeps_metadata_and_immutable_bytes() {
    signed_scene_roundtrip(true).await;
}

async fn signed_scene_roundtrip(positional: bool) {
    require_test_db!(Some(db) = OwnDb::create().await);
    let (owner, stranger) = (key(OWNER_KEY), key(STRANGER_KEY));
    let (x, y) = (37, -11);
    let parcel = format!("{x},{y}");
    index_land(&db.pool, &address(&owner), &[(x, y), (x + 1, y)]).await;

    let tmp = std::env::temp_dir().join(format!(
        "catalyst-write-path-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
    let state = Arc::new(ContentState::new(
        db.pool.clone(),
        Settings::for_tests(),
        storage.clone(),
    ));
    state.sync.set_state(SyncState::Syncing);
    let land = Arc::new(LandAuthzState::new(
        db.pool.clone(),
        Outbound::for_tests(),
        crate::land_authz::Settings::for_tests(),
        None,
    ));
    let chain = fake_chain::serve(fake_chain::land_owners(vec![
        (address(&owner), x.into(), y.into()),
        (address(&owner), (x + 1).into(), y.into()),
    ]))
    .await;
    attach(&state, true, Some(land), Some(&chain));
    let router = build_router(state.clone());

    let code = Bytes::from_static(b"export function onUpdate() {}");
    let code_hash = hash_bytes_v1(&code);
    let metadata = format!(
        r#"{{"display":{{"title":"write path e2e"}},"main":"bin/index.js","scene":{{"base":"{parcel}","parcels":["{parcel}"]}},"extra":{{"z":9007199254740993,"a":18446744073709551615,"nested":{{"z":"é","a":"quoted\\\""}}}}}}"#
    );
    let timestamp = chrono::Utc::now().timestamp_millis();
    let raw = if positional {
        format!(
            r#"["untrusted-document-id","scene",["{parcel}"],{timestamp},[{{"file":"bin/index.js","hash":"{code_hash}"}}],"v3",{metadata}]"#
        )
    } else {
        format!(
            r#"{{"id":"untrusted-document-id","version":"v3","type":"scene","pointers":["{parcel}"],"timestamp":{timestamp},"content":[{{"file":"bin/index.js","hash":"{code_hash}"}}],"metadata":{metadata},"unused":1e400}}"#
        )
    };
    let semantic: crate::entity::Entity = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        semantic.metadata,
        Some(serde_json::from_str(&metadata).unwrap())
    );
    let (entity_bytes, entity_id, chain) = signed_entity_bytes(&owner, Bytes::from(raw)).await;
    state
        .deployments_cache
        .insert("stale".into(), Bytes::from_static(b"[]"));
    let (status, _, body) = call(
        &router,
        deploy_request(
            "/content/entities",
            &entity_id,
            &chain,
            &[(&entity_id, &entity_bytes), (&code_hash, &code)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", json_of(&body));
    let stored: String = sqlx::query_scalar(
        "SELECT entity_metadata::text FROM content.deployments WHERE entity_id=$1",
    )
    .bind(&entity_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(stored, format!(r#"{{"v":{metadata}}}"#));
    eprintln!("validated_local_sql={stored}");
    let selected = crate::queries::active_entity_docs_with_ordered_metadata_by_pointers(
        &db.pool,
        std::slice::from_ref(&parcel),
    )
    .await
    .unwrap();
    assert_eq!(selected.len(), 1);
    let selected_text = std::str::from_utf8(&selected[0].json).unwrap();
    assert!(selected_text.contains(&format!(r#""metadata":{metadata}"#)));
    let ordinary =
        crate::queries::active_entity_docs_by_pointers(&db.pool, std::slice::from_ref(&parcel))
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&selected[0].json).unwrap(),
        serde_json::from_slice::<Value>(&ordinary[0].json).unwrap()
    );
    let created = json_of(&body)["creationTimestamp"].as_i64().unwrap();
    assert!((created - chrono::Utc::now().timestamp_millis()).abs() < 60_000);
    assert!(
        state.deployments_cache.is_empty(),
        "a committed deployment clears the history memo"
    );

    let active = active_by_pointers(&router, &[&parcel]).await;
    assert_eq!(active.as_array().unwrap().len(), 1, "{active}");
    assert_eq!(active[0]["id"], entity_id);
    assert_eq!(active[0]["pointers"], json!([parcel]));
    assert_eq!(
        active[0]["content"],
        json!([{"file": "bin/index.js", "hash": code_hash}])
    );
    assert_eq!(active[0]["metadata"]["scene"]["base"], parcel);
    for (hash, bytes) in [(&code_hash, &code), (&entity_id, &entity_bytes)] {
        let (status, _, served) = call(
            &router,
            Request::get(format!("/content/contents/{hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!((status, &served), (StatusCode::OK, bytes), "{hash}");
    }
    let (status, _, audit) = call(
        &router,
        Request::get(format!("/content/audit/scene/{entity_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let audit = json_of(&audit);
    assert_eq!(audit["localProvenance"]["status"], "active");
    assert_eq!(audit["localProvenance"]["signer"], address(&owner));
    // The audit sends the stored double with its sub-ms fraction; the
    // creationTimestamp is it truncated, as the history routes report it.
    assert_eq!(
        audit["localTimestamp"].as_f64().map(|ms| ms.floor() as i64),
        Some(created),
        "creationTimestamp is the stored localTimestamp"
    );

    // A replay answers the stored timestamp before any check: inside the
    // pointer's rate-limit window, with no files and a chain that would not
    // verify.
    let deployer = state.deployer().unwrap().clone();
    assert!(deployer.is_rate_limited("scene", &parcel));
    for (replay_chain, files) in [
        (chain.clone(), vec![(entity_id.as_str(), &entity_bytes)]),
        (
            json!([{"type": "SIGNER", "payload": "0xabc", "signature": ""}]),
            vec![],
        ),
    ] {
        let (status, _, body) = call(
            &router,
            deploy_request("/entities", &entity_id, &replay_chain, &files),
        )
        .await;
        assert_eq!(
            (status, json_of(&body)),
            (StatusCode::OK, json!({"creationTimestamp": created}))
        );
    }
    // An otherwise valid ordinary scene string reaches the database expression index.
    // Failure leaves immutable bytes available but commits no deployment state.
    state.deployer().unwrap().clear_rate_limits();
    let nul_raw = String::from_utf8(entity_bytes.to_vec())
        .unwrap()
        .replace("write path e2e", r#"write\u0000path"#);
    let nul_raw = nul_raw.replace(&timestamp.to_string(), &(timestamp + 1).to_string());
    let (nul_bytes, nul_id, nul_chain) = signed_entity_bytes(&owner, Bytes::from(nul_raw)).await;
    let (nul_status, _, nul_body) = call(
        &router,
        deploy_request(
            "/content/entities",
            &nul_id,
            &nul_chain,
            &[(&nul_id, &nul_bytes), (&code_hash, &code)],
        ),
    )
    .await;
    assert_eq!(
        nul_status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        json_of(&nul_body)
    );
    assert!(String::from_utf8_lossy(&nul_body).contains("There was an error deploying the entity"));
    let nul_rows: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM content.deployments WHERE entity_id=$1),(SELECT count(*) FROM content.local_entities WHERE entity_id=$1),(SELECT count(*) FROM content.forward_outbox WHERE entity_id=$1)").bind(&nul_id).fetch_one(&db.pool).await.unwrap();
    assert_eq!(nul_rows, (0, 0, 0));
    assert_eq!(storage.retrieve(&nul_id).await.unwrap(), Some(nul_bytes));
    deployer.clear_rate_limits();
    assert_eq!(
        deployer
            .deploy_entity(
                vec![entity_bytes.clone()],
                &entity_id,
                chain.clone(),
                "LOCAL"
            )
            .await
            .ok(),
        Some(created),
        "the write itself answers a deployed entity's stored timestamp"
    );
    deployer.clear_rate_limits();
    let (bytes2, id2, chain2) = signed_entity(&stranger, scene(&[&parcel], &[])).await;
    let (status, headers, body) = call(
        &router,
        deploy_request("/entities", &id2, &chain2, &[(&id2, &bytes2)]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", json_of(&body));
    assert!(headers.get(header::RETRY_AFTER).is_none());
    let errors = json_of(&body)["errors"].clone();
    assert!(
        errors.to_string().contains("does not have access"),
        "{errors}"
    );
    assert!(
        !deployer.is_rate_limited("scene", &parcel),
        "a rejected deploy does not hold the pointer"
    );

    // The denylist: a listed hash is not served and cannot be deployed
    // again; a listed entity drops out of the reads.
    state.denylist.operator_insert(&code_hash);
    let (status, _, _) = call(
        &router,
        Request::get(format!("/contents/{code_hash}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let neighbour = format!("{},{y}", x + 1);
    let (bytes3, id3, chain3) = signed_entity(
        &owner,
        scene(&[&neighbour], &[("bin/index.js", &code_hash)]),
    )
    .await;
    let (status, _, body) = call(
        &router,
        deploy_request("/content/entities", &id3, &chain3, &[(&id3, &bytes3)]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_of(&body),
        json!({"errors": [format!("These content files are denylisted on this node: {code_hash}")]})
    );
    state.denylist.operator_remove(&code_hash);
    state.denylist.operator_insert(&entity_id);
    assert_eq!(active_by_pointers(&router, &[&parcel]).await, json!([]));
    state.denylist.operator_remove(&entity_id);
    assert_eq!(
        active_by_pointers(&router, &[&parcel])
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Unpublish: the stranger holds no LAND there; the owner does, once.
    let path = format!("/content/scenes/{parcel}");
    let (status, _, body) = call(&router, signed_delete(&stranger, &path).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", json_of(&body));
    assert_eq!(
        json_of(&body)["error"],
        format!("The provided Eth Address does not have access to the following parcel: ({x},{y})")
    );
    let (status, _, body) = call(
        &router,
        signed_delete_over(&owner, "/content/scenes/0,0", &path).await,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a chain signed for another path: {}",
        json_of(&body)
    );

    state
        .deployments_cache
        .insert("stale".into(), Bytes::from_static(b"[]"));
    let (status, _, body) = call(&router, signed_delete(&owner, &path).await).await;
    assert_eq!(status, StatusCode::OK, "{}", json_of(&body));
    assert_eq!(
        json_of(&body),
        json!({"entityId": entity_id, "unpublished": true, "parcels": [parcel]})
    );
    assert!(state.deployments_cache.is_empty());
    assert_eq!(active_by_pointers(&router, &[&parcel]).await, json!([]));
    let (_, _, audit) = call(
        &router,
        Request::get(format!("/audit/scene/{entity_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(json_of(&audit)["localProvenance"]["status"], "unpublished");
    let (status, _, body) = call(&router, signed_delete(&owner, &path).await).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::NOT_FOUND,
            json!({"error": format!("No locally published scene at {x},{y}.")})
        )
    );

    // The owner publishes again over the tombstone.
    deployer.clear_rate_limits();
    let (bytes4, id4, chain4) = signed_entity(&owner, scene(&[&parcel], &[])).await;
    let (status, _, body) = call(
        &router,
        deploy_request("/entities", &id4, &chain4, &[(&id4, &bytes4)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", json_of(&body));
    assert_eq!(active_by_pointers(&router, &[&parcel]).await[0]["id"], id4);

    drop((router, deployer, state));
    let _ = std::fs::remove_dir_all(&tmp);
    db.drop_now().await;
}

/// A scene on two parcels goes only for a signer with LAND rights on both:
/// the holder of one of them is refused and the scene stays; the holder of
/// both takes it down, and both parcels are left empty although an older
/// scene occupied one of them before.
#[tokio::test]
async fn unpublishing_a_scene_needs_rights_on_every_parcel_and_empties_them() {
    require_test_db!(Some(db) = OwnDb::create().await);
    let (owner, neighbour) = (key(OWNER_KEY), key(STRANGER_KEY));
    let (x, y) = (41, -13);
    let (a, b) = (format!("{x},{y}"), format!("{},{y}", x + 1));
    // The owner holds (x, y) and operates (x + 1, y), which the neighbour owns.
    index_land_events(
        &db.pool,
        &[
            (crate::land_authz::KIND_TRANSFER, (x, y), &address(&owner)),
            (
                crate::land_authz::KIND_TRANSFER,
                (x + 1, y),
                &address(&neighbour),
            ),
            (
                crate::land_authz::KIND_UPDATE_OPERATOR,
                (x + 1, y),
                &address(&owner),
            ),
        ],
    )
    .await;

    let tmp = std::env::temp_dir().join(format!(
        "catalyst-write-path-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
    let state = Arc::new(ContentState::new(
        db.pool.clone(),
        Settings::for_tests(),
        storage.clone(),
    ));
    state.sync.set_state(SyncState::Syncing);
    let land = Arc::new(LandAuthzState::new(
        db.pool.clone(),
        Outbound::for_tests(),
        crate::land_authz::Settings::for_tests(),
        None,
    ));
    // Deploys ask the chain; the unpublish route reads the LAND index.
    let fake = fake_chain::serve(fake_chain::land_owners(vec![
        (address(&neighbour), (x + 1).into(), y.into()),
        (address(&owner), x.into(), y.into()),
        (address(&owner), (x + 1).into(), y.into()),
    ]))
    .await;
    attach(&state, true, Some(land), Some(&fake));
    let router = build_router(state.clone());
    let deployer = state.deployer().unwrap().clone();

    // The neighbour's older scene on (x + 1, y), then the owner's over both.
    let timestamp = chrono::Utc::now().timestamp_millis();
    let mut previous = scene(&[&b], &[]);
    previous["timestamp"] = json!(timestamp - 1);
    let (old_bytes, old_id, old_chain) = signed_entity(&neighbour, previous).await;
    let (status, _, body) = call(
        &router,
        deploy_request("/entities", &old_id, &old_chain, &[(&old_id, &old_bytes)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", json_of(&body));
    deployer.clear_rate_limits();
    let mut replacement = scene(&[&a, &b], &[]);
    replacement["timestamp"] = json!(timestamp);
    let (bytes, entity_id, chain) = signed_entity(&owner, replacement).await;
    let (status, _, body) = call(
        &router,
        deploy_request("/entities", &entity_id, &chain, &[(&entity_id, &bytes)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", json_of(&body));
    let active = active_by_pointers(&router, &[&a, &b]).await;
    assert_eq!(active.as_array().unwrap().len(), 1, "{active}");
    assert_eq!(active[0]["id"], entity_id);

    // The neighbour holds (x + 1, y) only: refused on either coordinate.
    for coord in [&a, &b] {
        let path = format!("/content/scenes/{coord}");
        let (status, _, body) = call(&router, signed_delete(&neighbour, &path).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", json_of(&body));
        assert_eq!(
            json_of(&body),
            json!({"error": format!("The provided Eth Address does not have access to the following parcel: ({x},{y})")})
        );
    }
    let active = active_by_pointers(&router, &[&a, &b]).await;
    assert_eq!(active.as_array().unwrap().len(), 1, "{active}");
    assert_eq!(active[0]["id"], entity_id, "the scene is still active");

    // The owner holds both: the scene goes and neither parcel has a scene.
    let path = format!("/content/scenes/{b}");
    let (status, _, body) = call(&router, signed_delete(&owner, &path).await).await;
    assert_eq!(status, StatusCode::OK, "{}", json_of(&body));
    let mut parcels = vec![a.clone(), b.clone()];
    parcels.sort();
    assert_eq!(
        json_of(&body),
        json!({"entityId": entity_id, "unpublished": true, "parcels": parcels})
    );
    assert_eq!(active_by_pointers(&router, &[&a, &b]).await, json!([]));
    let (_, _, audit) = call(
        &router,
        Request::get(format!("/audit/scene/{old_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        json_of(&audit)["localProvenance"]["status"],
        "superseded",
        "the older scene is not restored"
    );

    drop((router, deployer, state));
    let _ = std::fs::remove_dir_all(&tmp);
    db.drop_now().await;
}

/// What the LAND check's subgraph does before it answers, standing in for a
/// write that lands between the route's lookup and its unpublish.
enum Race {
    /// A peer scene replaces the local one on the parcel.
    PeerReplaces {
        local: String,
        peer: String,
        pointer: String,
    },
    /// A concurrent DELETE takes the local scene down first.
    Unpublished { local: String, pointer: String },
}

/// The marketplace subgraph on the loopback: `owner` owns every parcel asked
/// for, and each request first runs the armed [`Race`] against `pool`.
async fn serve_racing_land_subgraph(
    pool: PgPool,
    owner: String,
    race: Arc<parking_lot::Mutex<Option<Race>>>,
) -> String {
    use axum::extract::State;
    type Shared = (PgPool, String, Arc<parking_lot::Mutex<Option<Race>>>);
    async fn graphql(
        State(shared): State<Arc<Shared>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        let (pool, owner, race) = &*shared;
        let armed = race.lock().take();
        match armed {
            Some(Race::PeerReplaces {
                local,
                peer,
                pointer,
            }) => {
                let id: i32 = sqlx::query_scalar(
                    "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id,
                         entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain)
                     VALUES ('0xpeer', 'v3', 'scene', $1, NULL, timezone('UTC', now()), ARRAY[$2],
                             timezone('UTC', now()), '[]'::json)
                     RETURNING id",
                )
                .bind(&peer)
                .bind(&pointer)
                .fetch_one(pool)
                .await
                .unwrap();
                sqlx::query(
                    "UPDATE content.deployments SET deleter_deployment = $2 WHERE entity_id = $1",
                )
                .bind(&local)
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
                sqlx::query("UPDATE content.active_pointers SET entity_id = $2 WHERE pointer = $1")
                    .bind(&pointer)
                    .bind(&peer)
                    .execute(pool)
                    .await
                    .unwrap();
            }
            Some(Race::Unpublished { local, pointer }) => {
                super::land_publish::unpublish_local_scene(pool, &pointer, &local)
                    .await
                    .unwrap();
            }
            None => {}
        }
        let xs: Vec<String> = serde_json::from_value(body["variables"]["xs"].clone()).unwrap();
        let ys: Vec<String> = serde_json::from_value(body["variables"]["ys"].clone()).unwrap();
        let nfts: Vec<Value> = xs
            .iter()
            .flat_map(|x| ys.iter().map(move |y| (x, y)))
            .map(|(x, y)| {
                json!({"id": format!("0xland-{x}-{y}"), "searchParcelX": x, "searchParcelY": y,
                       "owner": {"id": owner}, "parcel": {"estate": null}})
            })
            .collect();
        axum::Json(json!({"data": {"nfts": nfts}}))
    }
    let app = Router::new()
        .route("/", axum::routing::post(graphql))
        .with_state(Arc::new((pool, owner, race)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/")
}

/// A local scene on `pointer`, as the deploy function leaves it.
async fn seed_local_scene(pool: &PgPool, entity_id: &str, pointer: &str) {
    sqlx::query(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id,
             entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain)
         VALUES ('0xowner', 'v3', 'scene', $1, NULL, timezone('UTC', now()) - interval '1 day',
                 ARRAY[$2], timezone('UTC', now()), '[]'::json)",
    )
    .bind(entity_id)
    .bind(pointer)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'scene')")
        .bind(pointer)
        .bind(entity_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO content.local_entities (entity_id, signer) VALUES ($1, '0xowner')")
        .bind(entity_id)
        .execute(pool)
        .await
        .unwrap();
}

/// A write between the route's lookup and its unpublish: a peer scene that
/// replaced the local one answers 409 and stays; a concurrent DELETE that took
/// the scene down first leaves this one a 404.
#[tokio::test]
async fn an_unpublish_raced_by_another_write_answers_409_or_404() {
    require_test_db!(Some(db) = OwnDb::create().await);
    let owner = key(OWNER_KEY);
    let race = Arc::new(parking_lot::Mutex::new(None));
    let subgraph = serve_racing_land_subgraph(db.pool.clone(), address(&owner), race.clone()).await;
    let market = Arc::new(crate::chain_index::ChainIndex::new(
        Outbound::for_tests(),
        crate::chain_index::IndexSettings {
            marketplace_subgraph_url: Some(subgraph),
            ..crate::chain_index::IndexSettings::for_tests()
        },
    ));
    market.allow_loopback_for_tests();

    let tmp = std::env::temp_dir().join(format!(
        "catalyst-write-path-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
    let state = Arc::new(ContentState::new(
        db.pool.clone(),
        Settings::for_tests(),
        storage.clone(),
    ));
    state.sync.set_state(SyncState::Syncing);
    let land = Arc::new(LandAuthzState::new(
        db.pool.clone(),
        Outbound::for_tests(),
        crate::land_authz::Settings::for_tests(),
        Some(market),
    ));
    attach(&state, true, Some(land), None);
    let router = build_router(state.clone());

    let (replaced, gone) = ("23,-7", "29,-7");
    seed_local_scene(&db.pool, "bafylocalreplaced", replaced).await;
    seed_local_scene(&db.pool, "bafylocalgone", gone).await;

    *race.lock() = Some(Race::PeerReplaces {
        local: "bafylocalreplaced".into(),
        peer: "bafypeerreplacing".into(),
        pointer: replaced.into(),
    });
    let path = format!("/content/scenes/{replaced}");
    let (status, _, body) = call(&router, signed_delete(&owner, &path).await).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", json_of(&body));
    assert!(
        race.lock().is_none(),
        "the race ran during the rights check"
    );
    let tombstoned: Option<chrono::NaiveDateTime> = sqlx::query_scalar(
        "SELECT tombstoned_at FROM content.local_entities WHERE entity_id = 'bafylocalreplaced'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        tombstoned, None,
        "the replaced local scene is not tombstoned"
    );
    let holder: String =
        sqlx::query_scalar("SELECT entity_id FROM content.active_pointers WHERE pointer = $1")
            .bind(replaced)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(holder, "bafypeerreplacing");

    *race.lock() = Some(Race::Unpublished {
        local: "bafylocalgone".into(),
        pointer: gone.into(),
    });
    let path = format!("/content/scenes/{gone}");
    let (status, _, body) = call(&router, signed_delete(&owner, &path).await).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::NOT_FOUND,
            json!({"error": format!("No locally published scene at {gone}.")})
        )
    );

    drop((router, state));
    let _ = std::fs::remove_dir_all(&tmp);
    db.drop_now().await;
}

fn profile(owner: &str, wearables: &[&str]) -> Value {
    json!({
        "version": "v3",
        "type": "profile",
        "pointers": [owner],
        "timestamp": chrono::Utc::now().timestamp_millis(),
        "content": [],
        "metadata": {"avatars": [{
            "name": "writepath", "hasClaimedName": false, "description": "",
            "ethAddress": owner, "version": 1, "tutorialStep": 0,
            "avatar": {
                "bodyShape": "urn:decentraland:off-chain:base-avatars:BaseMale",
                "eyes": {"color": {"r": 0.1, "g": 0.2, "b": 0.3}},
                "hair": {"color": {"r": 0.1, "g": 0.2, "b": 0.3}},
                "skin": {"color": {"r": 0.1, "g": 0.2, "b": 0.3}},
                "wearables": wearables,
                "emotes": [{"slot": 0, "urn": "urn:decentraland:off-chain:base-emotes:wave"}]
            }
        }]},
    })
}

const PROFILE_CLIENT_IP: &str = "203.0.113.30";

#[tokio::test]
async fn a_profile_with_nul_metadata_preserves_the_write_error_and_active_profile() {
    let node = TestNode::with_db("profile-metadata-validity")
        .await
        .expect("test database required");
    node.state.sync.set_state(SyncState::Syncing);
    attach(&node.state, true, None, None);
    let owner = PrivateKeySigner::random();
    let pointer = address(&owner);
    let mut entity = profile(&pointer, &[]);
    entity["metadata"]["avatars"][0]["description"] = json!(r"literal \u0000 is valid");
    let (bytes, accepted, chain) = signed_entity(&owner, entity.clone()).await;
    let (status, _, body) = call(
        &node.router(),
        deploy_request(
            "/content/entities",
            &accepted,
            &chain,
            &[(&accepted, &bytes)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", json_of(&body));
    let before: String = sqlx::query_scalar(
        "SELECT entity_metadata::text FROM content.deployments WHERE entity_id=$1",
    )
    .bind(&accepted)
    .fetch_one(&node.state.pool)
    .await
    .unwrap();
    // The accepted profile reserves this pointer's rate window. Clear that independent
    // gate so the schema-valid successor reaches persistence rather than an early 400.
    let deployer = node.state.deployer().unwrap();
    deployer.clear_rate_limits();
    assert!(!deployer.is_rate_limited("profile", &pointer));
    entity["timestamp"] = json!(entity["timestamp"].as_i64().unwrap() + 1);
    entity["metadata"]["avatars"][0]["version"] = json!(2);
    entity["metadata"]["avatars"][0]["description"] = json!("escaped \0 is invalid");
    let (bytes, rejected, chain) = signed_entity(&owner, entity).await;
    assert!(
        node.state
            .storage
            .retrieve(&rejected)
            .await
            .unwrap()
            .is_none()
    );
    let (status, headers, body) = call(
        &node.router(),
        deploy_request(
            "/content/entities",
            &rejected,
            &chain,
            &[(&rejected, &bytes)],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        json_of(&body)
    );
    assert_eq!(
        headers[header::RETRY_AFTER],
        writes::UNAVAILABLE_RETRY_AFTER_SECS
    );
    assert_eq!(
        json_of(&body),
        json!({"errors":[super::write_deployer::DEPLOY_ERROR_MESSAGE]})
    );
    assert_eq!(
        node.state.storage.retrieve(&rejected).await.unwrap(),
        Some(bytes),
        "the validator must pass and immutable bytes must reach persistence before JSONB refuses metadata"
    );
    let after: String = sqlx::query_scalar(
        "SELECT entity_metadata::text FROM content.deployments WHERE entity_id=$1",
    )
    .bind(&accepted)
    .fetch_one(&node.state.pool)
    .await
    .unwrap();
    assert_eq!(after, before);
    assert_eq!(
        active_by_pointers(&node.router(), &[&pointer]).await[0]["id"],
        accepted
    );
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM content.deployments WHERE entity_id=$1)")
            .bind(&rejected)
            .fetch_one(&node.state.pool)
            .await
            .unwrap();
    assert!(
        !present,
        "invalid metadata must not reach peers or JSONB readers"
    );
}

/// The old `profile_batch_no_overlay` contract on the write path: with no
/// escrow lease anywhere, a worn item counts for the wallet that holds that
/// token and for no one else (alice holds token 7 of item 1, bob token 9 of
/// item 2; a profile deployed now names each item with its token id, the
/// item's `ownerOf`), another token of an item one holds is not
/// ownership, and a profile wearing only base items asks the chain
/// nothing.
#[tokio::test]
async fn a_profile_deploy_keeps_owner_scope_without_usage_grants() {
    require_test_db!(Some(pool) = crate::db::scratch::pool(4).await);
    let (alice, bob) = (PrivateKeySigner::random(), PrivateKeySigner::random());
    let contract =
        format!("0x{}", &uuid::Uuid::new_v4().simple().to_string()[..32]).to_string() + "00000000";
    let (item1, item2) = (
        format!("urn:decentraland:matic:collections-v2:{contract}:1"),
        format!("urn:decentraland:matic:collections-v2:{contract}:2"),
    );
    let (alices, bobs) = (format!("{item1}:7"), format!("{item2}:9"));
    let holders = [
        (contract.clone(), 7u64, address(&alice)),
        (contract.clone(), 9, address(&bob)),
    ];
    let chain = fake_chain::serve(move |q, _| match q {
        Query::OwnerOf { contract, token } => Reply::Owner(
            holders
                .iter()
                .find(|(c, t, _)| {
                    c.parse::<alloy_primitives::Address>().ok() == Some(*contract)
                        && alloy_primitives::U256::from(*t) == *token
                })
                .map_or(alloy_primitives::Address::ZERO, |(_, _, who)| {
                    who.parse().unwrap()
                }),
        ),
        other => panic!("{other:?}"),
    })
    .await;
    let granted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM shop.usage_grants WHERE grantee_address = ANY($1)",
    )
    .bind(vec![address(&alice), address(&bob)])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(granted, 0, "no overlay rows for either wallet");

    let tmp = std::env::temp_dir().join(format!(
        "catalyst-write-profile-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
    let state = Arc::new(ContentState::new(
        pool.clone(),
        Settings::for_tests(),
        storage,
    ));
    state.sync.set_state(SyncState::Syncing);
    attach(&state, true, None, Some(&chain));
    let router = build_router(state.clone());

    let deployer = state.deployer().unwrap().clone();
    let deploy = |signer: PrivateKeySigner, wearables: Vec<String>| {
        let router = router.clone();
        let deployer = deployer.clone();
        async move {
            deployer.clear_rate_limits();
            let refs: Vec<&str> = wearables.iter().map(String::as_str).collect();
            let (bytes, id, chain) =
                signed_entity(&signer, profile(&address(&signer), &refs)).await;
            let mut req = deploy_request("/content/entities", &id, &chain, &[(&id, &bytes)]);
            req.headers_mut()
                .insert("x-real-ip", HeaderValue::from_static(PROFILE_CLIENT_IP));
            let (status, _, body) = call(&router, req).await;
            (status, json_of(&body), id)
        }
    };

    let (status, body, _) = deploy(alice.clone(), vec![alices.clone(), bobs.clone()]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"],
        json!([format!(
            "The following items ({bobs}) are not owned by the address {}.",
            address(&alice)
        )])
    );
    let (status, body, _) = deploy(bob.clone(), vec![alices.clone(), bobs.clone()]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"],
        json!([format!(
            "The following items ({alices}) are not owned by the address {}.",
            address(&bob)
        )])
    );
    let other_token = format!("{item1}:8");
    let (status, body, _) = deploy(alice.clone(), vec![other_token.clone()]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"],
        json!([format!(
            "The following items ({other_token}) are not owned by the address {}.",
            address(&alice)
        )])
    );

    let (status, body, alice_id) = deploy(alice.clone(), vec![alices.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, bob_id) = deploy(bob.clone(), vec![bobs.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    chain.clear();
    let carol = PrivateKeySigner::random();
    let (status, body, carol_id) = deploy(
        carol.clone(),
        vec!["urn:decentraland:off-chain:base-avatars:eyes_00".to_string()],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(chain.calls().is_empty(), "base items are never asked about");
    assert_eq!(
        state.deploy_quota.counted_for(PROFILE_CLIENT_IP),
        6,
        "the 400s and 200s all count"
    );

    let mut scope: HashMap<String, String> = HashMap::new();
    for (id, who) in [(&alice_id, &alice), (&bob_id, &bob), (&carol_id, &carol)] {
        scope.insert(id.clone(), address(who));
    }
    for (id, who) in &scope {
        let active = active_by_pointers(&router, &[who]).await;
        assert_eq!(active[0]["id"], json!(id));
        assert!(
            super::land_publish::local_provenance(&pool, id)
                .await
                .unwrap()
                .is_none(),
            "only a local scene carries provenance"
        );
    }
    for (id, who) in scope {
        sqlx::query("DELETE FROM content.active_pointers WHERE pointer = $1")
            .bind(&who)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM content.deployments WHERE entity_id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let _ = std::fs::remove_dir_all(&tmp);
    pool.close().await;
}

/// A deploy form of the given text fields from `ip`, as the gateway stamps it.
fn form_request(ip: &str, fields: &[(String, String)]) -> Request<Body> {
    let boundary = "catalyst-admission-boundary";
    let mut body = String::new();
    for (name, value) in fields {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    Request::post("/content/entities")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::CONTENT_LENGTH, body.len())
        .header("x-real-ip", ip)
        .body(Body::from(body))
        .unwrap()
}

/// A form that passes admission and is then refused by the deployer (400):
/// a well-formed chain, no entity file.
fn entity_form() -> Vec<(String, String)> {
    vec![
        ("entityId".to_string(), "bafy".to_string()),
        (
            "authChain".to_string(),
            json!([
                {"type": "SIGNER", "payload": "0x0000000000000000000000000000000000000001", "signature": ""},
                {"type": "ECDSA_SIGNED_ENTITY", "payload": "bafy", "signature": "0x01"}
            ])
            .to_string(),
        ),
    ]
}

async fn deploying_node(tag: &str, settings: Settings) -> TestNode {
    let node = TestNode::without_db_settings(tag, settings).await;
    attach(&node.state, true, None, None);
    node.state.sync.set_state(SyncState::Syncing);
    node
}

#[tokio::test]
async fn a_form_over_its_field_limits_is_refused() {
    let node = deploying_node("field-limits", Settings::for_tests()).await;
    let router = node.router();

    let mut many = entity_form();
    many.extend((0..31).map(|i| (format!("extra{i}"), "x".to_string())));
    let (status, _, body) = call(&router, form_request("203.0.113.1", &many)).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error": "Too many form fields in the request. The maximum allowed is 32."})
        )
    );

    let mut large = entity_form();
    large.push(("big".to_string(), "x".repeat(32 * 1024 + 1)));
    let (status, _, body) = call(&router, form_request("203.0.113.1", &large)).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error": "Field 'big' is too large. The maximum allowed size per field is 32768 bytes."})
        )
    );

    let mut fits = entity_form();
    fits.push(("big".to_string(), "x".repeat(32 * 1024)));
    let (status, _, body) = call(&router, form_request("203.0.113.1", &fits)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_of(&body),
        json!({"errors": ["Failed to find the entity file."]}),
        "a form at the limits reaches the deployer"
    );
}

#[tokio::test]
async fn a_client_past_its_daily_quota_is_turned_away() {
    let settings = Settings {
        post_entities_daily_quota_max: 2,
        ..Settings::for_tests()
    };
    let node = deploying_node("daily-quota", settings).await;
    let router = node.router();
    for _ in 0..2 {
        let (status, _, _) = call(&router, form_request("203.0.113.7", &entity_form())).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "refused deploys still count"
        );
    }
    let (status, headers, body) = call(&router, form_request("203.0.113.7", &entity_form())).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "Too many requests"})
        )
    );
    let retry: u64 = headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry > 0 && retry <= 24 * 60 * 60, "{retry}");

    let (status, _, _) = call(&router, form_request("203.0.113.8", &entity_form())).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "another address has its own quota"
    );
}

/// Upstream's burst limit: every request of a client address counts, a
/// refused one included, and past it the answer is the JSON 429 with the
/// seconds left in the window; the daily quota is not touched by it.
#[tokio::test]
async fn a_client_past_its_burst_limit_is_turned_away() {
    let settings = Settings {
        post_entities_rate_limit_max: 2,
        ..Settings::for_tests()
    };
    let node = deploying_node("burst-limit", settings).await;
    let router = node.router();
    for _ in 0..2 {
        let (status, _, _) = call(&router, form_request("203.0.113.20", &entity_form())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, headers, body) = call(&router, form_request("203.0.113.20", &entity_form())).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "Too many requests"})
        )
    );
    let retry: u64 = headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry > 0 && retry <= 60, "{retry}");
    assert_eq!(node.state.deploy_quota.counted_for("203.0.113.20"), 2);
    let (status, _, _) = call(&router, form_request("203.0.113.21", &entity_form())).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "another address has its own"
    );
}

/// A client reaching the API directly is counted by its socket: a spoofed
/// `X-Real-IP` neither escapes its burst limit nor spends another address's.
/// Only a socket peer in `HTTP_TRUSTED_PROXY_CIDRS` names the client.
#[tokio::test]
async fn the_burst_limit_counts_a_direct_socket_not_its_header() {
    let settings = Settings {
        post_entities_rate_limit_max: 2,
        trusted_proxy_cidrs: vec!["10.0.0.0/8".parse().unwrap()],
        ..Settings::for_tests()
    };
    let node = deploying_node("burst-direct-socket", settings).await;
    let router = node.router();
    let from =
        |peer: [u8; 4], ip: &str| {
            let mut request = form_request(ip, &entity_form());
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from((peer, 40000)),
            ));
            request
        };
    // A direct client rotating its header: one budget, its socket's.
    for ip in ["203.0.113.30", "203.0.113.31"] {
        let (status, _, _) = call(&router, from([198, 51, 100, 7], ip)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _, _) = call(&router, from([198, 51, 100, 7], "203.0.113.32")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(node.state.deploy_quota.counted_for("198.51.100.7"), 2);
    assert_eq!(node.state.deploy_quota.counted_for("203.0.113.30"), 0);
    // Behind a trusted proxy, its X-Real-IP is the client.
    for _ in 0..2 {
        let (status, _, _) = call(&router, from([10, 0, 0, 5], "203.0.113.33")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _, _) = call(&router, from([10, 0, 0, 5], "203.0.113.33")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let (status, _, _) = call(&router, from([10, 0, 0, 5], "203.0.113.34")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "another client behind the proxy"
    );
}

#[tokio::test]
async fn a_body_past_the_upload_budget_is_shed_with_retry_after() {
    let capacity = super::create_entity::MAX_DEPLOYMENT_BYTES as u64;
    let settings = Settings {
        max_in_memory_deployment_bytes: capacity,
        ..Settings::for_tests()
    };
    let node = deploying_node("upload-budget", settings).await;
    let router = node.router();

    let held = node.state.upload_budget.try_reserve(capacity).unwrap();
    let (status, headers, body) = call(&router, form_request("203.0.113.9", &entity_form())).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": super::upload_admission::BUDGET_EXHAUSTED_MESSAGE})
        )
    );
    assert_eq!(headers[header::RETRY_AFTER], "30");

    drop(held);
    let (status, _, _) = call(&router, form_request("203.0.113.9", &entity_form())).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "admitted once the budget frees"
    );
    assert!(
        node.state.upload_budget.try_reserve(capacity).is_some(),
        "a finished request returns its reservation"
    );
}

/// Upstream's multipart limits (`multipart.ts`), each a 413 with its text:
/// 3000 files, 100 MiB per file (named by the part's file name), and a
/// declared body past the total plus framing, refused unread.
#[tokio::test]
async fn a_form_past_upstreams_multipart_limits_is_413() {
    let node = deploying_node("multipart-limits", Settings::for_tests()).await;
    let router = node.router();
    let one = Bytes::from_static(b"x");
    let names: Vec<String> = (0..=super::create_entity::MAX_DEPLOY_FILES)
        .map(|i| format!("bafyfile{i}"))
        .collect();
    let files: Vec<(&str, &Bytes)> = names.iter().map(|n| (n.as_str(), &one)).collect();
    let (status, _, body) = call(
        &router,
        deploy_request("/content/entities", "bafy", &json!([]), &files),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error": "Too many files in the request. The maximum allowed is 3000."})
        )
    );

    let big = Bytes::from(vec![0u8; super::create_entity::MAX_DEPLOY_FILE_BYTES + 1]);
    let (status, _, body) = call(
        &router,
        deploy_request(
            "/content/entities",
            "bafy",
            &json!([]),
            &[("big.glb", &big)],
        ),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error": "File 'big.glb' is too large. The maximum allowed size per file is 104857600 bytes."})
        )
    );
    drop(big);

    let max_body = super::create_entity::max_deployment_body_bytes(32);
    let mut declared = form_request("203.0.113.15", &entity_form());
    declared
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(max_body + 1));
    let (status, _, body) = call(&router, declared).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error": format!(
                "The request body is too large. The maximum allowed total upload size is 209715200 bytes plus multipart framing ({max_body} bytes in all)."
            )})
        )
    );
}

/// A form the parser cannot read answers upstream's fixed 400, never the
/// parser's own text.
#[tokio::test]
async fn a_malformed_form_answers_without_parser_detail() {
    let node = deploying_node("multipart-malformed", Settings::for_tests()).await;
    let truncated = Request::post("/content/entities")
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=catalyst-admission-boundary",
        )
        .body(Body::from(
            "--catalyst-admission-boundary\r\nContent-Disposition: form-data; name=\"entityId\"\r\n\r\nbafy",
        ))
        .unwrap();
    let (status, _, body) = call(&node.router(), truncated).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "Invalid multipart/form-data request"})
        )
    );
}

#[tokio::test]
async fn a_repeated_form_name_is_refused() {
    let node = deploying_node("field-repeats", Settings::for_tests()).await;
    let mut repeated = entity_form();
    repeated.extend((0..30).map(|_| ("extra".to_string(), "x".to_string())));
    let (status, _, body) = call(&node.router(), form_request("203.0.113.1", &repeated)).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "Duplicate form field 'extra'"})
        )
    );
    let file = Bytes::from_static(b"same");
    let (status, _, body) = call(
        &node.router(),
        deploy_request(
            "/content/entities",
            "bafy",
            &json!([]),
            &[("bafyfile", &file), ("bafyfile", &file)],
        ),
    )
    .await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "Duplicate form field 'bafyfile'"})
        ),
        "file parts too"
    );
}

/// Upstream counts a deploy once its form is read (`withDeploymentQuota`):
/// forms the parser or its limits refuse are not counted, a form refused
/// after it (no `entityId`, a bad chain) is.
#[tokio::test]
async fn only_forms_that_were_read_count_toward_the_quota() {
    let settings = Settings {
        post_entities_daily_quota_max: 2,
        ..Settings::for_tests()
    };
    let node = deploying_node("quota-parse-failures", settings).await;
    let router = node.router();
    let mut large = entity_form();
    large.push(("big".to_string(), "x".repeat(32 * 1024 + 1)));
    for _ in 0..3 {
        let (status, _, _) = call(&router, form_request("203.0.113.10", &large)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
    assert_eq!(node.state.deploy_quota.counted_for("203.0.113.10"), 0);
    let (status, _, body) = call(&router, form_request("203.0.113.10", &entity_form()[1..])).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "Missing required field: entityId"})
        )
    );
    let (status, _, _) = call(&router, form_request("203.0.113.10", &entity_form())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = call(&router, form_request("203.0.113.10", &large)).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a spent source is turned away before its body"
    );
}

/// Upstream's order before the body: a spent quota answers 429 ahead of a
/// full upload budget.
#[tokio::test]
async fn a_spent_quota_is_answered_before_the_upload_budget() {
    let capacity = super::create_entity::MAX_DEPLOYMENT_BYTES as u64;
    let settings = Settings {
        post_entities_daily_quota_max: 1,
        max_in_memory_deployment_bytes: capacity,
        ..Settings::for_tests()
    };
    let node = deploying_node("quota-before-budget", settings).await;
    let router = node.router();
    let (status, _, _) = call(&router, form_request("203.0.113.16", &entity_form())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let _held = node.state.upload_budget.try_reserve(capacity).unwrap();
    let (status, _, _) = call(&router, form_request("203.0.113.16", &entity_form())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let (status, _, _) = call(&router, form_request("203.0.113.17", &entity_form())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// Upstream's per-source admission (`source-upload-admission.ts`): a client
/// with its uploads or bytes in flight gets 429 with `Retry-After: 5`
/// before its body is read; another client is admitted.
#[tokio::test]
async fn a_source_past_its_uploads_in_flight_is_turned_away() {
    let settings = Settings {
        max_concurrent_uploads_per_source: 1,
        ..Settings::for_tests()
    };
    let node = deploying_node("source-uploads", settings).await;
    let router = node.router();
    let held = node
        .state
        .source_uploads
        .try_acquire("203.0.113.18", 1)
        .unwrap();
    let (status, headers, body) = call(&router, form_request("203.0.113.18", &entity_form())).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "Too many uploads in progress from this client, please retry shortly."})
        )
    );
    assert_eq!(headers[header::RETRY_AFTER], "5");
    let (status, _, _) = call(&router, form_request("203.0.113.19", &entity_form())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    drop(held);
    let (status, _, _) = call(&router, form_request("203.0.113.18", &entity_form())).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "admitted once its upload ends"
    );
}

/// A body still arriving at the upload timeout answers upstream's 408 with
/// what arrived (`multipart.ts`).
#[tokio::test]
async fn a_body_past_the_upload_timeout_is_408_with_what_arrived() {
    use futures::StreamExt;
    let settings = Settings {
        multipart_upload_timeout: std::time::Duration::from_millis(50),
        ..Settings::for_tests()
    };
    let node = deploying_node("upload-timeout", settings).await;
    let first = "--catalyst-admission-boundary\r\nContent-Disposition: form-data; name=\"entityId\"\r\n\r\nbafy\r\n";
    let stalled = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
        first.as_bytes(),
    ))])
    .chain(futures::stream::pending());
    let request = Request::post("/content/entities")
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=catalyst-admission-boundary",
        )
        .header(header::CONTENT_LENGTH, 1000)
        .header("x-real-ip", "203.0.113.22")
        .body(Body::from_stream(stalled))
        .unwrap();
    let (status, _, body) = call(&node.router(), request).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    let kib_per_s = first.len() as f64 * 1000.0 / 50.0 / 1024.0;
    assert_eq!(
        json_of(&body),
        json!({"error": format!(
            "The upload did not finish within 0.05 s: received {} of 1000 bytes ({} KiB/s). Retry on a faster connection or send smaller batches.",
            first.len(),
            format!("{kib_per_s:.2}").trim_end_matches('0').trim_end_matches('.')
        )})
    );
    assert_eq!(
        node.state.deploy_quota.counted_for("203.0.113.22"),
        0,
        "an unread form is not counted"
    );
}

/// An unsynced copy taking only scenes: a form with no typed entity file is
/// 400 and a profile 503, and both count, since the client chose them.
#[tokio::test]
async fn an_unsynced_copys_refusals_count_toward_the_quota() {
    let settings = Settings {
        post_entities_daily_quota_max: 2,
        ..Settings::for_tests()
    };
    let node = deploying_node("quota-copy", settings).await;
    node.state.sync.set_unsynced_copy(true);
    node.state
        .sync
        .set_copy_deploy_types(vec!["scene".to_string()]);
    let router = node.router();
    let (status, _, body) = call(&router, form_request("203.0.113.11", &entity_form())).await;
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::BAD_REQUEST,
            json!({"errors": ["The entity file was not part of the uploaded files or has no type."]})
        )
    );
    let file = Bytes::from(
        json!({"version": "v3", "type": "profile", "pointers": ["0x00000000000000000000000000000000000000aa"],
               "timestamp": chrono::Utc::now().timestamp_millis(), "content": [], "metadata": {}})
        .to_string(),
    );
    let id = hash_bytes_v1(&file);
    let profile = || {
        let mut req = deploy_request(
            "/content/entities",
            &id,
            &unsigned_chain(&id),
            &[(&id, &file)],
        );
        req.headers_mut()
            .insert("x-real-ip", HeaderValue::from_static("203.0.113.11"));
        req
    };
    let (status, _, _) = call(&router, profile()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _, _) = call(&router, profile()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// A contract-wallet chain this node cannot check (no `ETH_RPC_URL`) is a 503
/// the client chose: it counts toward the quota.
#[tokio::test]
async fn deploys_with_an_unverifiable_signature_count_toward_the_quota() {
    let settings = Settings {
        post_entities_daily_quota_max: 2,
        ..Settings::for_tests()
    };
    let node = deploying_node("quota-unverifiable", settings).await;
    let router = node.router();
    let file = Bytes::from(profile("0x0000000000000000000000000000000000000002", &[]).to_string());
    let id = hash_bytes_v1(&file);
    let chain = serde_json::to_value(
        crate::sync_test_support::signed_chain_behind_contract(
            &id,
            &(chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339(),
        )
        .await,
    )
    .unwrap();
    for _ in 0..2 {
        node.state.deployer().unwrap().clear_rate_limits();
        let mut req = deploy_request("/content/entities", &id, &chain, &[(&id, &file)]);
        req.headers_mut()
            .insert("x-real-ip", HeaderValue::from_static("203.0.113.13"));
        let (status, headers, body) = call(&router, req).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
        assert!(json_of(&body)["errors"].is_array(), "{body:?}");
        assert!(headers.contains_key(header::RETRY_AFTER));
    }
    assert_eq!(node.state.deploy_quota.counted_for("203.0.113.13"), 2);
    let (status, _, _) = call(&router, form_request("203.0.113.13", &entity_form())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// The deployer's 503 for this node's own state (here no LAND rights source)
/// is given back to the client's quota.
#[tokio::test]
async fn deploys_the_node_cannot_decide_are_given_back() {
    let settings = Settings {
        post_entities_daily_quota_max: 1,
        ..Settings::for_tests()
    };
    let node = deploying_node("quota-refund", settings).await;
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let (file, id, chain) = signed_entity(&signer, scene(&["0,0"], &[])).await;
    for _ in 0..3 {
        node.state.deployer().unwrap().clear_rate_limits();
        let mut req = deploy_request("/content/entities", &id, &chain, &[(&id, &file)]);
        req.headers_mut()
            .insert("x-real-ip", HeaderValue::from_static("203.0.113.14"));
        let (status, headers, body) = call(&router, req).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
        assert!(
            json_of(&body)["errors"][0]
                .as_str()
                .is_some_and(|e| e == crate::validator::checker::PARCEL_ACCESS_UNDECIDED),
            "{body:?}"
        );
        assert!(headers.contains_key(header::RETRY_AFTER));
    }
    assert_eq!(node.state.deploy_quota.counted_for("203.0.113.14"), 0);
    let (status, _, _) = call(&router, form_request("203.0.113.14", &entity_form())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the quota is untouched");
    let (status, _, _) = call(&router, form_request("203.0.113.14", &entity_form())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "a 400 is kept");
}

/// An undecided deploy asking more than one access batch keeps its quota
/// charge: the request itself can drive the check past its deadline. One that
/// fits the batch exactly is given back.
#[tokio::test]
async fn an_undecided_oversized_deploy_keeps_its_quota_charge() {
    let settings = Settings {
        post_entities_daily_quota_max: 1,
        ..Settings::for_tests()
    };
    let node = deploying_node("quota-oversized", settings).await;
    let router = node.router();
    let signer = PrivateKeySigner::random();
    let parcels: Vec<String> = (0..101).map(|x| format!("{x},0")).collect();
    let parcels: Vec<&str> = parcels.iter().map(String::as_str).collect();
    let (file, id, chain) = signed_entity(&signer, scene(&parcels, &[])).await;
    let mut req = deploy_request("/content/entities", &id, &chain, &[(&id, &file)]);
    req.headers_mut()
        .insert("x-real-ip", HeaderValue::from_static("203.0.113.15"));
    let (status, _, body) = call(&router, req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(node.state.deploy_quota.counted_for("203.0.113.15"), 1);
    let (status, _, _) = call(&router, form_request("203.0.113.15", &entity_form())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let parcels: Vec<String> = (0..100).map(|x| format!("{x},1")).collect();
    let parcels: Vec<&str> = parcels.iter().map(String::as_str).collect();
    let (file, id, chain) = signed_entity(&signer, scene(&parcels, &[])).await;
    let mut req = deploy_request("/content/entities", &id, &chain, &[(&id, &file)]);
    req.headers_mut()
        .insert("x-real-ip", HeaderValue::from_static("203.0.113.16"));
    let (status, _, body) = call(&router, req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(node.state.deploy_quota.counted_for("203.0.113.16"), 0);
}

#[tokio::test]
async fn a_request_without_a_client_address_skips_the_quota() {
    let settings = Settings {
        post_entities_daily_quota_max: 1,
        ..Settings::for_tests()
    };
    let node = deploying_node("quota-no-address", settings).await;
    let router = node.router();
    for _ in 0..3 {
        let mut req = form_request("", &entity_form());
        req.headers_mut().remove("x-real-ip");
        let (status, _, _) = call(&router, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn a_chunked_body_reserves_the_whole_body_cap() {
    let cap = super::create_entity::MAX_DEPLOYMENT_BYTES as u64;
    let settings = Settings {
        max_in_memory_deployment_bytes: 2 * cap,
        ..Settings::for_tests()
    };
    let node = deploying_node("upload-budget-chunked", settings).await;
    let router = node.router();
    let _held = node.state.upload_budget.try_reserve(cap + 1).unwrap();

    let mut chunked = form_request("203.0.113.12", &entity_form());
    chunked.headers_mut().remove(header::CONTENT_LENGTH);
    let (status, _, _) = call(&router, chunked).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the cap does not fit"
    );

    let (status, _, _) = call(&router, form_request("203.0.113.12", &entity_form())).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a declared small body fits"
    );
}

#[tokio::test]
async fn a_cancelled_request_returns_its_reservation_and_source_share() {
    let cap = super::create_entity::MAX_DEPLOYMENT_BYTES as u64;
    let settings = Settings {
        max_in_memory_deployment_bytes: cap,
        max_concurrent_uploads_per_source: 1,
        ..Settings::for_tests()
    };
    let node = deploying_node("upload-budget-cancel", settings).await;
    let router = node.router();

    let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let boundary = "catalyst-admission-boundary";
    tx.try_send(Ok(Bytes::from(format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"entityId\"\r\n\r\nba"
    ))))
    .unwrap();
    let req = Request::post("/content/entities")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("x-real-ip", "203.0.113.13")
        .body(Body::from_stream(rx))
        .unwrap();
    let pending = tokio::spawn(router.clone().oneshot(req));

    let budget = &node.state.upload_budget;
    let mut waited = 0;
    while budget.try_reserve(1).is_some() {
        assert!(waited < 500, "the request never reserved");
        waited += 1;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let sources = &node.state.source_uploads;
    assert!(
        sources.try_acquire("203.0.113.13", 1).is_err(),
        "the request holds its source's only upload"
    );
    pending.abort();
    let _ = pending.await;
    drop(tx);
    assert!(
        budget.try_reserve(cap).is_some(),
        "the aborted request gave its reservation back"
    );
    assert!(
        matches!(sources.try_acquire("203.0.113.13", 1), Ok(Some(_))),
        "the aborted request gave its source's upload back"
    );
}

/// A database that drops its first `fail_first` connections (a lookup that
/// fails at once and is passed over) and holds every later one unanswered.
async fn stalling_pool(fail_first: usize) -> (PgPool, tokio::task::JoinHandle<()>) {
    let (pool, server, _) = counted_stalling_pool(fail_first).await;
    (pool, server)
}

/// [`stalling_pool`] and how many connections it accepted.
async fn counted_stalling_pool(
    fail_first: usize,
) -> (
    PgPool,
    tokio::task::JoinHandle<()>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepted_count = Arc::clone(&count);
    let server = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            let accepted = accepted_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if accepted > fail_first {
                held.push(socket);
            }
        }
    });
    let pool = PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(60))
        .connect_lazy(&format!("postgres://nobody@{addr}/nowhere"))
        .unwrap();
    (pool, server, count)
}

async fn stalled_deploying_node(
    tag: &str,
    fail_first: usize,
    settings: Settings,
) -> (TestNode, tokio::task::JoinHandle<()>) {
    let (pool, server) = stalling_pool(fail_first).await;
    let node = TestNode::with_pool_settings(tag, pool, settings).await;
    attach(&node.state, true, None, None);
    node.state.sync.set_state(SyncState::Syncing);
    (node, server)
}

/// `POST /entities` skips the whole-request timeout, which still answers
/// any other route's stall with 408; a read form not decided within
/// `DEPLOYMENT_PROCESSING_TIMEOUT_MS` answers 503 with `Retry-After`.
#[tokio::test]
async fn a_deploy_skips_the_request_timeout_and_answers_503_at_its_processing_deadline() {
    let settings = Settings {
        request_timeout: Some(std::time::Duration::from_millis(200)),
        deployment_processing_timeout: std::time::Duration::from_millis(800),
        ..Settings::for_tests()
    };
    let (node, server) = stalled_deploying_node("processing-deadline", 0, settings).await;
    let router = node.router();

    let active = Request::post("/content/entities/active")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"pointers": ["0,0"]}).to_string()))
        .unwrap();
    let (status, _, _) = call(&router, active).await;
    assert_eq!(
        status,
        StatusCode::REQUEST_TIMEOUT,
        "another route over the stalled database"
    );

    let started = std::time::Instant::now();
    let (status, headers, body) = call(&router, form_request("203.0.113.40", &entity_form())).await;
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(800),
        "answered at {:?}, before its processing deadline",
        started.elapsed()
    );
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"errors": ["Deployment processing exceeded the 800ms deadline."]})
        )
    );
    assert_eq!(headers[header::RETRY_AFTER], "30");
    server.abort();
}

/// A deploy stalled before validation (the newer-entity lookup on a database
/// that never answers) holds its upload budget and its source's share until
/// its processing deadline, then lets both go; it never holds its pointer.
#[tokio::test]
async fn a_deploy_stalled_before_validation_holds_no_pointer_and_releases_its_budget_and_share() {
    let cap = super::create_entity::MAX_DEPLOYMENT_BYTES as u64;
    let settings = Settings {
        max_in_memory_deployment_bytes: cap,
        max_concurrent_uploads_per_source: 1,
        deployment_processing_timeout: std::time::Duration::from_secs(2),
        ..Settings::for_tests()
    };
    // The replay lookup's connection fails at once; the next one stalls.
    let (pool, server, accepted) = counted_stalling_pool(1).await;
    let node = TestNode::with_pool_settings("processing-stall", pool, settings).await;
    attach(&node.state, true, None, None);
    node.state.sync.set_state(SyncState::Syncing);
    let router = node.router();
    let parcel = "71,-71";
    let (bytes, id, chain) = signed_entity(&key(OWNER_KEY), scene(&[parcel], &[])).await;
    let mut req = deploy_request("/content/entities", &id, &chain, &[(&id, &bytes)]);
    req.headers_mut()
        .insert("x-real-ip", HeaderValue::from_static("203.0.113.41"));
    let pending = {
        let router = router.clone();
        tokio::spawn(async move { call(&router, req).await })
    };

    let deployer = node.state.deployer().unwrap().clone();
    let (budget, sources) = (&node.state.upload_budget, &node.state.source_uploads);
    let mut waited = 0;
    // The second connection is the newer-entity lookup, which stalls.
    while accepted.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        assert!(
            waited < 150,
            "the deploy never reached its newer-entity lookup"
        );
        waited += 1;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        budget.try_reserve(cap).is_none(),
        "the deploy holds its reservation"
    );
    assert!(
        !deployer.holds_pointer("scene", parcel),
        "an unvalidated deploy holds no pointer"
    );
    assert!(
        sources.try_acquire("203.0.113.41", 1).is_err(),
        "the deploy holds its source's only upload"
    );

    let (status, headers, body) = pending.await.unwrap();
    assert_eq!(
        (status, json_of(&body)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"errors": ["Deployment processing exceeded the 2000ms deadline."]})
        )
    );
    assert_eq!(headers[header::RETRY_AFTER], "30");
    assert!(
        !deployer.holds_pointer("scene", parcel),
        "the claim is released"
    );
    assert!(
        budget.try_reserve(cap).is_some(),
        "the reservation is released"
    );
    assert!(
        matches!(sources.try_acquire("203.0.113.41", 1), Ok(Some(_))),
        "the source's upload is released"
    );
    server.abort();
}
