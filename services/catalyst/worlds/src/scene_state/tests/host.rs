//! The host against the scratch database and a temporary blob store: the
//! loader reads the active deployment through the resolver and the files
//! from the store (a mirrored scene is refused, a Genesis scene scopes to
//! `main`), the socket admits by policy, and the sweep kicks a revoked
//! client and reloads a redeployed scene. Skips without
//! `CATALYST_TEST_DATABASE_URL`.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

use crate::auth::hash_bytes_v1;
use crate::comms::world_identity::{Parcel, SceneId, WorldName};
use crate::core::{Revocation, RevocationSink};
use crate::policy::{AccessType, PersonalWorldsPolicy, Policy};
use crate::scene_state::crdt::{CrdtMessage, decode_batch, encode_batch};
use crate::scene_state::delegation::{Wallet, create_simple_auth_chain};
use crate::scene_state::protocol::{MessageType, encode_message};
use crate::scene_state::{
    RawSettings, SceneHostState, SceneKey, Settings, SweepSummary, loader, router,
};
use crate::shared::identity::Address;
use crate::shared::signed_fetch::build_payload_v6;
use catalyst_storage::Storage;

const VISITOR_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const STRANGER_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

struct Host {
    state: Arc<SceneHostState>,
    _tmp: std::path::PathBuf,
}

async fn host(js: bool) -> Option<Host> {
    host_with(js, None).await
}

/// A host whose names reader refuses every connection.
async fn host_with_names_down() -> Option<Host> {
    let settings = crate::chain_index::IndexSettings {
        marketplace_subgraph_url: Some("http://127.0.0.1:1/subgraph".to_string()),
        eth_network: "mainnet".to_string(),
        ..Default::default()
    };
    let index = crate::chain_index::ChainIndex::new(crate::http::Outbound::for_tests(), settings);
    index.allow_loopback_for_tests();
    host_with(false, Some(Arc::new(index))).await
}

async fn host_with(js: bool, names: Option<Arc<crate::chain_index::ChainIndex>>) -> Option<Host> {
    host_tuned(js, names, |_| {}).await
}

async fn host_tuned(
    js: bool,
    names: Option<Arc<crate::chain_index::ChainIndex>>,
    tune: impl FnOnce(&mut RawSettings),
) -> Option<Host> {
    let pool = crate::db::scratch::pool(4).await?;
    let tmp = std::env::temp_dir().join(format!("catalyst-scene-host-{}", Uuid::new_v4().simple()));
    let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
    let policy = Arc::new(Policy::new(
        pool.clone(),
        PersonalWorldsPolicy::disabled(),
        names,
    ));
    let mut raw = RawSettings {
        disable_js_runtime: !js,
        auth_timeout_secs: 2,
        ..RawSettings::default()
    };
    tune(&mut raw);
    let settings = Settings::resolve(raw, &crate::http::Outbound::for_tests()).unwrap();
    let state = Arc::new(SceneHostState::new(pool, storage, policy, settings, None).unwrap());
    Some(Host { state, _tmp: tmp })
}

fn world_name() -> WorldName {
    WorldName::parse(&format!(
        "H{}.dcl.eth",
        &Uuid::new_v4().simple().to_string()[..10]
    ))
    .unwrap()
}

async fn store(state: &SceneHostState, bytes: &[u8]) -> String {
    let hash = hash_bytes_v1(bytes);
    state
        .storage
        .store(&hash, bytes::Bytes::copy_from_slice(bytes))
        .await
        .unwrap();
    hash
}

async fn seed_world(pool: &PgPool, world: &WorldName, owner: Option<&Address>, access: AccessType) {
    sqlx::query("INSERT INTO worlds.worlds (name, owner, access_type) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(world.key())
        .bind(owner)
        .bind(access.as_str())
        .execute(pool)
        .await
        .unwrap();
}

#[allow(
    clippy::too_many_arguments,
    reason = "fixture: one argument per seeded scene field"
)]
async fn seed_scene(
    state: &SceneHostState,
    world: &WorldName,
    id: &str,
    code: &str,
    crdt: Option<&[u8]>,
    base: &str,
    parcels: &[&str],
    mirrored: bool,
) {
    let code_hash = store(state, code.as_bytes()).await;
    let mut content = vec![serde_json::json!({ "file": "bin/game.js", "hash": code_hash })];
    if let Some(crdt) = crdt {
        let crdt_hash = store(state, crdt).await;
        content.push(serde_json::json!({ "file": "main.crdt", "hash": crdt_hash }));
    }
    let entity = serde_json::json!({ "metadata": { "main": "bin/game.js", "scene": { "base": base }, "authoritativeMultiplayer": true }, "content": content });
    sqlx::query(
        "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size, mirror_source) \
         VALUES ($1, $2, '[]'::json, $3, $4, $5, 1, $6)",
    )
    .bind(world.key())
    .bind(id)
    .bind(entity)
    .bind(crate::ZERO_ADDRESS)
    .bind(parcels.iter().map(|p| p.to_string()).collect::<Vec<_>>())
    .bind(mirrored.then_some("https://upstream.example"))
    .execute(&state.pool)
    .await
    .unwrap();
    let parcels: Vec<String> = parcels.iter().map(|p| p.to_string()).collect();
    let mut conn = state.pool.acquire().await.unwrap();
    crate::explore::places::scene_identity::assign(&mut conn, world.key(), id, &parcels)
        .await
        .unwrap();
}

fn put(entity: u32, data: &[u8]) -> CrdtMessage {
    CrdtMessage::Put {
        entity,
        component_id: 7,
        timestamp: 1,
        data: data.to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_world_scene_loads_from_the_shared_rows_and_the_blob_store() {
    let Some(h) = host(true).await else { return };
    let (world, owner) = (
        world_name(),
        Address::parse("0x1111111111111111111111111111111111111111").unwrap(),
    );
    seed_world(
        &h.state.pool,
        &world,
        Some(&owner),
        AccessType::Unrestricted,
    )
    .await;
    let seeded = encode_batch(&[put(600, &[42])]);
    seed_scene(
        &h.state,
        &world,
        "bafyHost1",
        "\u{feff}module.exports.onUpdate = function (dt) {};",
        Some(&seeded),
        "1,1",
        &["1,1", "1,2"],
        false,
    )
    .await;

    let key = SceneKey::World(world.clone());
    let resolved = loader::resolve(&h.state, &key)
        .await
        .unwrap()
        .expect("a local active deployment");
    assert_eq!(resolved.deployment.scene_id.as_str(), "bafyHost1");
    assert!(resolved.crdt_hash.is_some());
    h.state.load(&key).await.unwrap();
    let scene = h
        .state
        .scenes
        .get(world.key())
        .expect("hosted under the world key");
    assert_eq!(scene.scene_hash(), "bafyHost1");
    let ns = scene.identity.namespace().unwrap();
    let identity: uuid::Uuid = sqlx::query_scalar(
        "SELECT identity FROM explore.world_scene_place WHERE world_name = $1 AND active",
    )
    .bind(world.key())
    .fetch_one(&h.state.pool)
    .await
    .unwrap();
    assert_eq!(
        (ns.realm(), ns.scene_namespace()),
        (world.key(), identity.to_string()),
        "the scene's storage identity"
    );
    assert_eq!(
        h.state.scenes.loaded(),
        vec![format!("{}:bafyHost1", world.key())]
    );
    // The static main.crdt seeds the engine (the JS runtime is on).
    let live: Vec<CrdtMessage> = decode_batch(&scene.snapshot())
        .into_iter()
        .filter(|m| matches!(m, CrdtMessage::Put { .. }))
        .collect();
    assert_eq!(live, vec![put(600, &[42])]);

    // A reload replaces the hosted scene and never counts against the cap.
    h.state.load(&key).await.unwrap();
    assert_eq!(h.state.scenes.len(), 1);
    assert_eq!(h.state.unload(&key), 0);
    assert!(h.state.scenes.is_empty());

    // A world without a scene, and a missing blob, are refusals with a reason.
    let empty = world_name();
    let err = h
        .state
        .load(&SceneKey::World(empty.clone()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no scene published"), "{err}");
    seed_world(&h.state.pool, &empty, None, AccessType::Unrestricted).await;
    sqlx::query(
        "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size) \
         VALUES ($1, 'bafyNoBlob', '[]'::json, $2, $3, '{\"0,0\"}', 1)",
    )
    .bind(empty.key())
    .bind(serde_json::json!({ "metadata": { "main": "game.js" }, "content": [{ "file": "game.js", "hash": "bafkreie4eisvkzyjuqrcendydk6vikqs2vco5lmib4nlzsxtjzofiqy2pa" }] }))
    .bind(crate::ZERO_ADDRESS)
    .execute(&h.state.pool)
    .await
    .unwrap();
    let err = h.state.load(&SceneKey::World(empty)).await.unwrap_err();
    assert!(err.to_string().contains("not in the blob store"), "{err}");
}

#[tokio::test]
async fn a_mirrored_scene_is_not_hosted() {
    let Some(h) = host(false).await else { return };
    let world = world_name();
    seed_world(&h.state.pool, &world, None, AccessType::Unrestricted).await;
    seed_scene(
        &h.state,
        &world,
        "bafyMirror",
        "module.exports = {};",
        None,
        "0,0",
        &["0,0"],
        true,
    )
    .await;
    let key = SceneKey::World(world.clone());
    assert_eq!(
        loader::resolve(&h.state, &key).await.unwrap(),
        None,
        "a mirrored deployment grants no host authority"
    );
    assert_eq!(
        loader::current_scene_id(&h.state, &key).await.unwrap(),
        None
    );
    let err = h.state.load(&key).await.unwrap_err();
    assert!(err.to_string().contains("mirrored"), "{err}");
    assert!(h.state.scenes.is_empty());
}

#[tokio::test]
async fn a_genesis_scene_loads_by_id_and_by_parcel_into_the_main_scope() {
    let Some(h) = host(false).await else { return };
    let id = format!("bafyG{}", Uuid::new_v4().simple());
    let x = 30_000 + (Uuid::new_v4().as_u128() % 10_000) as i32;
    let (p1, p2) = (format!("{x},7"), format!("{x},8"));
    let code_hash = store(&h.state, b"module.exports.onUpdate = function () {};").await;
    let dep_id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
         VALUES ('0x0', 'v3', 'scene', $1, $2::json, timezone('UTC', now()), $3, timezone('UTC', now()), '[]'::json) RETURNING id",
    )
    .bind(&id)
    .bind(serde_json::json!({ "main": "bin/game.js", "scene": { "base": p2 } }))
    .bind(vec![p1.clone(), p2.clone()])
    .fetch_one(&h.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, 'bin/game.js')")
        .bind(dep_id)
        .bind(&code_hash)
        .execute(&h.state.pool)
        .await
        .unwrap();
    for p in [&p1, &p2] {
        sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id) VALUES ($1, $2)")
            .bind(p)
            .bind(&id)
            .execute(&h.state.pool)
            .await
            .unwrap();
    }

    let by_id = SceneKey::Genesis(SceneId::parse(&id).unwrap());
    h.state.load(&by_id).await.unwrap();
    let scene = h.state.scenes.get(&id).unwrap();
    let ns = scene.identity.namespace().unwrap();
    assert_eq!((ns.realm(), ns.scene_namespace()), ("main", p2.clone()));
    let at = SceneKey::GenesisParcel(Parcel::parse(&p1).unwrap());
    h.state.load(&at).await.unwrap();
    assert!(
        h.state.scenes.get(&p1).is_none(),
        "a parcel is not its own host"
    );
    let same = h.state.hosted(&at).await.unwrap();
    assert_eq!(
        same.identity.deployment, scene.identity.deployment,
        "one deployment, two keys, one scope"
    );
    assert_eq!(
        h.state.scenes.len(),
        1,
        "every parcel shares the scene's host"
    );

    sqlx::query("DELETE FROM content.active_pointers WHERE entity_id = $1")
        .bind(&id)
        .execute(&h.state.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.content_files WHERE deployment = $1")
        .bind(dep_id)
        .execute(&h.state.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.deployments WHERE id = $1")
        .bind(dep_id)
        .execute(&h.state.pool)
        .await
        .unwrap();
    // Gone from the shared rows: the sweep unloads it.
    let summary = h.state.sweep().await;
    assert_eq!(
        summary,
        SweepSummary {
            unloaded: 1,
            ..Default::default()
        }
    );
    assert!(h.state.scenes.is_empty());
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn serve(state: Arc<SceneHostState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().nest(crate::scene_state::ROUTE_PREFIX, router(state));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn auth_frame(key: &str, path: &str, metadata: &str) -> Vec<u8> {
    let wallet = Wallet::from_hex(key).unwrap();
    let ts = chrono::Utc::now().timestamp_millis().to_string();
    let payload = build_payload_v6("get", path, &ts, metadata);
    let chain = create_simple_auth_chain(&wallet, &payload).unwrap();
    let mut obj = serde_json::Map::new();
    obj.insert("x-identity-timestamp".into(), ts.into());
    obj.insert("x-identity-metadata".into(), metadata.into());
    for (i, link) in chain.as_array().unwrap().iter().enumerate() {
        obj.insert(
            format!("x-identity-auth-chain-{i}"),
            link.to_string().into(),
        );
    }
    encode_message(
        MessageType::Auth,
        serde_json::Value::Object(obj).to_string().as_bytes(),
    )
}

/// Connects, authenticates and returns the socket with the Init frame, or
/// `None` when the host closed it instead.
async fn join(
    addr: std::net::SocketAddr,
    world: &WorldName,
    key: &str,
    metadata: &str,
) -> Option<(Ws, Vec<u8>)> {
    join_name(addr, world.display(), key, metadata).await
}

async fn join_name(
    addr: std::net::SocketAddr,
    name: &str,
    key: &str,
    metadata: &str,
) -> Option<(Ws, Vec<u8>)> {
    let path = format!("/scene-state/ws/{name}");
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}{path}"))
        .await
        .unwrap();
    ws.send(WsMessage::Binary(auth_frame(key, &path, metadata).into()))
        .await
        .unwrap();
    match tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("an answer within 5 s")
    {
        Some(Ok(WsMessage::Binary(b))) if b.first() == Some(&(MessageType::Init as u8)) => {
            Some((ws, b.to_vec()))
        }
        Some(Ok(WsMessage::Close(_))) | None => None,
        other => panic!("unexpected first frame: {other:?}"),
    }
}

/// Whether the host closes the socket within 5 s; relayed frames still in
/// flight (a departed neighbour's reclaim) are drained on the way.
async fn closed(ws: &mut Ws) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => return true,
            Ok(Some(Ok(_))) => continue,
            Err(_) => return false,
        }
    }
}

/// With the name authority down, re-admission keeps a client whose answer
/// depends on it (the stored owner) and still removes a refused stranger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_authority_outage_keeps_undetermined_clients_through_the_sweep() {
    let Some(h) = host_with_names_down().await else {
        return;
    };
    let key = || format!("0x{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let (owner_key, stranger_key) = (key(), key());
    let owner = Address::parse(&Wallet::from_hex(&owner_key).unwrap().address()).unwrap();
    let world = world_name();
    seed_world(
        &h.state.pool,
        &world,
        Some(&owner),
        AccessType::Unrestricted,
    )
    .await;
    seed_scene(
        &h.state,
        &world,
        "bafyOut1",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    let addr = serve(h.state.clone()).await;
    let (_owner_ws, _) = join(addr, &world, &owner_key, "{}").await.expect("open");
    let (mut stranger_ws, _) = join(addr, &world, &stranger_key, "{}").await.expect("open");
    let scene = h.state.scenes.get(world.key()).unwrap();
    assert_eq!(scene.clients().len(), 2);

    sqlx::query("UPDATE worlds.worlds SET access_type = 'allow-list' WHERE name = $1")
        .bind(world.key())
        .execute(&h.state.pool)
        .await
        .unwrap();
    assert_eq!(
        h.state.sweep().await,
        SweepSummary {
            kicked: 1,
            errors: 1,
            ..Default::default()
        }
    );
    assert!(closed(&mut stranger_ws).await);
    let left: Vec<Address> = scene
        .clients()
        .iter()
        .map(|c| Address::parse(&c.address).unwrap())
        .collect();
    assert_eq!(left, vec![owner]);
    assert_eq!(h.state.readmit_world(&world, true).await, 0);
    assert_eq!(scene.clients().len(), 1);
}

/// With the name authority down, a client admitted by the shared secret is
/// carried by its grant, and a new holder connected as owner (`worlds.owner`
/// still the previous holder) is undetermined; a rotated secret still voids
/// the secret grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_authority_outage_keeps_secret_and_owner_grants_through_the_sweep() {
    let Some(h) = host_with_names_down().await else {
        return;
    };
    let visitor_key = format!("0x{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let previous = Address::parse("0x3333333333333333333333333333333333333333").unwrap();
    let holder = Address::parse("0x4444444444444444444444444444444444444444").unwrap();
    let world = world_name();
    let hash = bcrypt::hash("open sesame", 4).unwrap();
    sqlx::query("INSERT INTO worlds.worlds (name, owner, access_type, access_secret_hash) VALUES ($1, $2, 'shared-secret', $3)")
        .bind(world.key())
        .bind(&previous)
        .bind(&hash)
        .execute(&h.state.pool)
        .await
        .unwrap();
    seed_scene(
        &h.state,
        &world,
        "bafyOut2",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    let addr = serve(h.state.clone()).await;
    let (mut visitor_ws, _) = join(addr, &world, &visitor_key, r#"{"secret":"open sesame"}"#)
        .await
        .expect("the secret admits without asking the authority");
    let scene = h.state.scenes.get(world.key()).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    scene.add_client_admitted(
        holder.as_str().to_string(),
        tx,
        Some(crate::policy::AccessGrant::Owner),
    );
    assert_eq!(scene.clients().len(), 2);

    assert_eq!(
        h.state.sweep().await,
        SweepSummary {
            errors: 1,
            ..Default::default()
        },
        "the secret client is carried; the owner-admitted holder is undetermined"
    );
    assert_eq!(scene.clients().len(), 2);
    assert_eq!(h.state.readmit_world(&world, false).await, 0);
    assert_eq!(h.state.readmit_world(&world, true).await, 1);
    assert!(closed(&mut visitor_ws).await);
    let left: Vec<String> = scene.clients().iter().map(|c| c.address.clone()).collect();
    assert_eq!(left, vec![holder.as_str().to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_owner_can_join_with_a_valid_secret_during_an_authority_outage() {
    let Some(h) = host_with_names_down().await else {
        return;
    };
    let visitor_key = format!("0x{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let previous = Address::parse(&Wallet::from_hex(&visitor_key).unwrap().address()).unwrap();
    let world = world_name();
    let hash = bcrypt::hash("open sesame", 4).unwrap();
    sqlx::query("INSERT INTO worlds.worlds (name, owner, access_type, access_secret_hash) VALUES ($1, $2, 'shared-secret', $3)")
        .bind(world.key())
        .bind(&previous)
        .bind(&hash)
        .execute(&h.state.pool)
        .await
        .unwrap();
    seed_scene(
        &h.state,
        &world,
        "bafyOwnerOutage",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    let addr = serve(h.state.clone()).await;
    // The stored owner cannot establish independent ownership during the outage.
    // A wrong secret remains denied; a valid one retries the full policy under budget.
    assert!(
        join(addr, &world, &visitor_key, r#"{"secret":"wrong"}"#)
            .await
            .is_none()
    );
    // Authority errors may refund a reservation. Fill the caller's budget
    // explicitly, then prove this fallback cannot bypass it with a valid secret.
    let throttle = crate::comms::gatekeeper::admission::world_mint::SecretThrottle {
        pool: &h.state.pool,
        max_attempts: h.state.secret_max_attempts,
        world: world.key(),
        subject: previous.as_str(),
    };
    let now = chrono::Utc::now().naive_utc();
    for _ in 0..h.state.secret_max_attempts {
        let _ = throttle.reserve(now).await.unwrap();
    }
    assert!(throttle.reserve(now).await.unwrap().is_none());
    assert!(
        join(addr, &world, &visitor_key, r#"{"secret":"open sesame"}"#)
            .await
            .is_none(),
        "a valid secret cannot bypass the exhausted caller budget during an authority outage"
    );
    assert!(
        h.state.scenes.get(world.key()).is_none(),
        "a refused client loads nothing"
    );
    sqlx::query("UPDATE social.world_secret_attempts SET window_start=timezone('UTC',now())-interval '61 seconds' WHERE world_name=$1 AND subject=$2")
        .bind(world.key())
        .bind(previous.as_str())
        .execute(&h.state.pool)
        .await
        .unwrap();
    let (_visitor_ws, _) = join(addr, &world, &visitor_key, r#"{"secret":"open sesame"}"#)
        .await
        .expect("the secret admits without asking the authority");
    assert_eq!(h.state.scenes.get(world.key()).unwrap().clients().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_admits_by_policy_and_the_sweep_enforces_revocation_and_redeploy() {
    let Some(h) = host(false).await else { return };
    // Its own key: the block below is database-wide, and the other host
    // tests admit VISITOR_KEY.
    let visitor_key = format!("0x{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let visitor = Address::parse(&Wallet::from_hex(&visitor_key).unwrap().address()).unwrap();
    let owner = Address::parse("0x2222222222222222222222222222222222222222").unwrap();
    let world = world_name();
    seed_world(&h.state.pool, &world, Some(&owner), AccessType::AllowList).await;
    sqlx::query("INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'access', $2)")
        .bind(world.key())
        .bind(&visitor)
        .execute(&h.state.pool)
        .await
        .unwrap();
    seed_scene(&h.state, &world, "bafyV1", "", None, "0,0", &["0,0"], false).await;
    let addr = serve(h.state.clone()).await;

    // Autoload on first contact, then Init for the allow-listed visitor.
    assert!(h.state.scenes.is_empty());
    let (mut visitor_ws, init) = join(addr, &world, &visitor_key, "{}")
        .await
        .expect("the allow-listed visitor is admitted");
    assert_eq!(
        &init[1..5],
        &1024u32.to_be_bytes(),
        "client 0 owns [1024, 1536)"
    );
    let scene = h.state.scenes.get(world.key()).expect("autoloaded");
    assert_eq!(scene.scene_hash(), "bafyV1");
    assert_eq!(scene.clients().len(), 1);

    // A stranger is refused before any Init frame; presence proves nothing.
    assert!(
        join(addr, &world, STRANGER_KEY, "{}").await.is_none(),
        "a wallet outside the allow-list is refused"
    );
    assert_eq!(scene.clients().len(), 1);

    // A relayed CRDT batch reaches the other admitted client.
    let (mut second, _) = {
        sqlx::query("INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'access', $2)")
            .bind(world.key())
            .bind(Address::parse(&Wallet::from_hex(STRANGER_KEY).unwrap().address()).unwrap())
            .execute(&h.state.pool)
            .await
            .unwrap();
        join(addr, &world, STRANGER_KEY, "{}")
            .await
            .expect("listed now")
    };
    let batch = encode_batch(&[put(1100, &[1, 2, 3])]);
    visitor_ws
        .send(WsMessage::Binary(
            encode_message(MessageType::Crdt, &batch).into(),
        ))
        .await
        .unwrap();
    let relayed = tokio::time::timeout(Duration::from_secs(5), second.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WsMessage::Binary(relayed) = relayed else {
        panic!("expected a CRDT frame, got {relayed:?}")
    };
    assert_eq!(relayed[0], MessageType::Crdt as u8);
    assert_eq!(decode_batch(&relayed[1..]), vec![put(1100, &[1, 2, 3])]);

    // Revocation: a block lands in the database; the sweep closes the socket.
    sqlx::query("INSERT INTO worlds.blocked_wallets (wallet) VALUES ($1)")
        .bind(&visitor)
        .execute(&h.state.pool)
        .await
        .unwrap();
    let summary = h.state.sweep().await;
    assert_eq!(
        summary,
        SweepSummary {
            kicked: 1,
            ..Default::default()
        }
    );
    assert!(
        closed(&mut visitor_ws).await,
        "the revoked client's socket must close"
    );
    assert_eq!(scene.clients().len(), 1);
    assert!(
        join(addr, &world, &visitor_key, "{}").await.is_none(),
        "and it cannot come back"
    );

    // Redeploy: a newer scene row makes the sweep reload; the survivors are kicked.
    seed_scene(&h.state, &world, "bafyV2", "", None, "0,0", &["0,0"], false).await;
    let summary = h.state.sweep().await;
    assert_eq!(
        summary,
        SweepSummary {
            reloaded: 1,
            ..Default::default()
        }
    );
    assert!(closed(&mut second).await);
    assert_eq!(
        h.state.scenes.get(world.key()).unwrap().scene_hash(),
        "bafyV2"
    );

    // The worlds core's revocation hook is the immediate path: the
    // allow-list removal closes the socket without any sweep.
    let (mut third, _) = join(addr, &world, STRANGER_KEY, "{}")
        .await
        .expect("still listed");
    let stranger = Address::parse(&Wallet::from_hex(STRANGER_KEY).unwrap().address()).unwrap();
    let other_world = world_name();
    assert_eq!(
        h.state.kick_from_world(&other_world, &stranger),
        0,
        "another world's revocation touches nothing"
    );
    sqlx::query("DELETE FROM worlds.world_permissions WHERE world_name = $1 AND permission = 'access' AND address = $2")
        .bind(world.key())
        .bind(&stranger)
        .execute(&h.state.pool)
        .await
        .unwrap();
    h.state
        .revoke(&Revocation::Address {
            world: world.clone(),
            address: stranger.clone(),
        })
        .await;
    assert!(closed(&mut third).await);
    assert!(join(addr, &world, STRANGER_KEY, "{}").await.is_none());
    // A platform block through the hook closes the wallet's sockets everywhere.
    sqlx::query("INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'access', $2)")
        .bind(world.key())
        .bind(&stranger)
        .execute(&h.state.pool)
        .await
        .unwrap();
    let (mut fourth, _) = join(addr, &world, STRANGER_KEY, "{}")
        .await
        .expect("listed again");
    h.state
        .revoke(&Revocation::Platform {
            address: stranger.clone(),
        })
        .await;
    assert!(closed(&mut fourth).await);

    // /status reports the hosted scene; /ws on an unknown world is 404.
    let status: serde_json::Value = reqwest::get(format!("http://{addr}/scene-state/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["loadedScenes"],
        serde_json::json!([format!("{}:bafyV2", world.key())])
    );
    let err = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/scene-state/ws/{}",
        world_name().display()
    ))
    .await
    .unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 404),
        other => panic!("expected a 404 handshake refusal, got {other:?}"),
    }
}

fn fresh_key() -> (String, Address) {
    let key = format!("0x{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let address = Address::parse(&Wallet::from_hex(&key).unwrap().address()).unwrap();
    (key, address)
}

async fn seed_community(pool: &PgPool, world: &WorldName, members: &[&Address]) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO social.communities (id, name, owner_address) VALUES ($1, 'c', $2)")
        .bind(id)
        .bind(crate::ZERO_ADDRESS)
        .execute(pool)
        .await
        .unwrap();
    for m in members {
        sqlx::query("INSERT INTO social.community_members (community_id, member_address, role) VALUES ($1, $2, 'member')")
            .bind(id)
            .bind(m)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO worlds.world_access_communities (world_name, community_id) VALUES ($1, $2)",
    )
    .bind(world.key())
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn community_members_are_admitted_and_kicked_once_no_longer_admitted() {
    let Some(h) = host(false).await else { return };
    let core = crate::core::CoreState::new(
        h.state.pool.clone(),
        h.state.storage.clone(),
        h.state.policy.clone(),
        crate::core::Settings::for_tests(None),
        None,
    );
    core.add_revocation_sink(h.state.clone());
    let world = world_name();
    seed_world(&h.state.pool, &world, None, AccessType::AllowList).await;
    seed_scene(&h.state, &world, "bafyC", "", None, "0,0", &["0,0"], false).await;
    let ((member_key, member), (listed_key, listed), (other_key, other)) =
        (fresh_key(), fresh_key(), fresh_key());
    sqlx::query("INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'access', $2)")
        .bind(world.key())
        .bind(&listed)
        .execute(&h.state.pool)
        .await
        .unwrap();
    let community = seed_community(&h.state.pool, &world, &[&member, &listed]).await;
    let deleted = seed_community(&h.state.pool, &world, &[&other]).await;
    let addr = serve(h.state.clone()).await;

    let (mut member_ws, _) = join(addr, &world, &member_key, "{}")
        .await
        .expect("a community member is admitted");
    let (_listed_ws, _) = join(addr, &world, &listed_key, "{}").await.unwrap();
    let (mut other_ws, _) = join(addr, &world, &other_key, "{}").await.unwrap();
    assert!(
        join(addr, &world, &fresh_key().0, "{}").await.is_none(),
        "a non-member is refused"
    );
    let scene = h.state.scenes.get(world.key()).unwrap();
    assert_eq!(scene.clients().len(), 3);

    // Both leave the community: only the wallet nothing else admits is kicked.
    sqlx::query("DELETE FROM social.community_members WHERE community_id = $1")
        .bind(community)
        .execute(&h.state.pool)
        .await
        .unwrap();
    core.community_member_removed(community, &listed).await;
    assert_eq!(scene.clients().len(), 3, "the allow-listed wallet stays");
    core.community_member_removed(community, &member).await;
    assert!(closed(&mut member_ws).await);
    assert_eq!(scene.clients().len(), 2);

    // A deleted community re-decides every participant of the world.
    sqlx::query("UPDATE social.communities SET active = FALSE WHERE id = $1")
        .bind(deleted)
        .execute(&h.state.pool)
        .await
        .unwrap();
    core.community_closed(deleted).await;
    assert!(closed(&mut other_ws).await);
    assert_eq!(scene.clients().len(), 1, "the allow-listed wallet stays");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_secret_world_reads_the_secret_from_the_signed_metadata() {
    let Some(h) = host(false).await else { return };
    let world = world_name();
    let hash = bcrypt::hash("open sesame", 4).unwrap();
    sqlx::query("INSERT INTO worlds.worlds (name, access_type, access_secret_hash) VALUES ($1, 'shared-secret', $2)")
        .bind(world.key())
        .bind(&hash)
        .execute(&h.state.pool)
        .await
        .unwrap();
    seed_scene(
        &h.state,
        &world,
        "bafySecret",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    let addr = serve(h.state.clone()).await;
    assert!(
        join(addr, &world, VISITOR_KEY, "{}").await.is_none(),
        "no secret, no entry"
    );
    assert!(
        join(addr, &world, VISITOR_KEY, r#"{"secret":"wrong"}"#)
            .await
            .is_none()
    );
    let (mut ws, _) = join(addr, &world, VISITOR_KEY, r#"{"secret":"open sesame"}"#)
        .await
        .expect("the right secret admits");
    // The sweep re-checks without the secret and keeps a client admitted by it.
    assert_eq!(h.state.sweep().await, SweepSummary::default());
    // A rotated secret reaches the running room through the access-setting
    // hook: the prior grant is void, the client must present the new secret.
    let rotated = bcrypt::hash("new words", 4).unwrap();
    sqlx::query("UPDATE worlds.worlds SET access_secret_hash = $2 WHERE name = $1")
        .bind(world.key())
        .bind(&rotated)
        .execute(&h.state.pool)
        .await
        .unwrap();
    h.state
        .revoke(&Revocation::AccessSetting {
            world: world.clone(),
        })
        .await;
    assert!(
        closed(&mut ws).await,
        "the socket admitted under the old secret must close"
    );
    assert!(
        join(addr, &world, VISITOR_KEY, r#"{"secret":"open sesame"}"#)
            .await
            .is_none()
    );
    assert!(
        join(addr, &world, VISITOR_KEY, r#"{"secret":"new words"}"#)
            .await
            .is_none(),
        "three wrong guesses share the caller budget even after a correct secret"
    );
    sqlx::query("UPDATE social.world_secret_attempts SET window_start=timezone('UTC',now())-interval '61 seconds' WHERE world_name=$1")
        .bind(world.key()).execute(&h.state.pool).await.unwrap();
    let (mut ws, _) = join(addr, &world, VISITOR_KEY, r#"{"secret":"new words"}"#)
        .await
        .expect("the new secret admits after the charged window expires");
    assert_eq!(
        h.state.sweep().await,
        SweepSummary::default(),
        "the sweep still carries a secret-admitted client"
    );
    // Until the access type changes underneath it.
    sqlx::query("UPDATE worlds.worlds SET access_type = 'nft-ownership', access_secret_hash = NULL, access_nft = 'urn:x' WHERE name = $1")
        .bind(world.key())
        .execute(&h.state.pool)
        .await
        .unwrap();
    assert_eq!(
        h.state.sweep().await,
        SweepSummary {
            kicked: 1,
            ..Default::default()
        }
    );
    assert!(closed(&mut ws).await);
}

async fn connect(addr: std::net::SocketAddr, world: &WorldName) -> Option<Ws> {
    let path = format!("/scene-state/ws/{}", world.display());
    tokio_tungstenite::connect_async(format!("ws://{addr}{path}"))
        .await
        .ok()
        .map(|(ws, _)| ws)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_an_admitted_client_loads_a_scene_and_only_one_that_opts_in() {
    let Some(h) = host(false).await else { return };
    let world = world_name();
    seed_world(&h.state.pool, &world, None, AccessType::Unrestricted).await;
    seed_scene(
        &h.state,
        &world,
        "bafyOptIn",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    let addr = serve(h.state.clone()).await;

    let mut silent = connect(addr, &world)
        .await
        .expect("the upgrade is answered");
    assert!(closed(&mut silent).await, "no auth frame, no session");
    assert!(
        h.state.scenes.is_empty(),
        "an unauthenticated socket loads nothing"
    );
    let (_ws, _) = join(addr, &world, VISITOR_KEY, "{}")
        .await
        .expect("an admitted client loads the scene");
    assert!(h.state.scenes.get(world.key()).is_some());

    let other = world_name();
    seed_world(&h.state.pool, &other, None, AccessType::Unrestricted).await;
    seed_scene(
        &h.state,
        &other,
        "bafyOptOut",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    sqlx::query("UPDATE worlds.world_scenes SET entity = (entity::jsonb #- '{metadata,authoritativeMultiplayer}')::json WHERE world_name = $1")
        .bind(other.key())
        .execute(&h.state.pool)
        .await
        .unwrap();
    assert!(
        connect(addr, &other).await.is_none(),
        "a scene without authoritativeMultiplayer is not autoloaded"
    );
    assert!(h.state.scenes.get(other.key()).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweep_unloads_scenes_left_without_clients() {
    let Some(h) = host_tuned(false, None, |raw| raw.idle_unload_secs = 1).await else {
        return;
    };
    let (busy, idle) = (world_name(), world_name());
    for (world, id) in [(&busy, "bafyBusy"), (&idle, "bafyIdle")] {
        seed_world(&h.state.pool, world, None, AccessType::Unrestricted).await;
        seed_scene(&h.state, world, id, "", None, "0,0", &["0,0"], false).await;
    }
    let addr = serve(h.state.clone()).await;
    let (_ws, _) = join(addr, &busy, VISITOR_KEY, "{}")
        .await
        .expect("admitted");
    h.state.load(&SceneKey::World(idle.clone())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        h.state.sweep().await,
        SweepSummary {
            unloaded: 1,
            ..Default::default()
        }
    );
    assert!(
        h.state.scenes.get(idle.key()).is_none(),
        "an empty scene is unloaded"
    );
    assert!(
        h.state.scenes.get(busy.key()).is_some(),
        "a scene with a client stays"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_sources_failed_secrets_never_refuse_a_correct_one() {
    let Some(h) = host(false).await else { return };
    let world = world_name();
    let hash = bcrypt::hash("open sesame", 4).unwrap();
    sqlx::query("INSERT INTO worlds.worlds (name, access_type, access_secret_hash) VALUES ($1, 'shared-secret', $2)")
        .bind(world.key())
        .bind(&hash)
        .execute(&h.state.pool)
        .await
        .unwrap();
    seed_scene(
        &h.state,
        &world,
        "bafySourceSecret",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    let addr = serve(h.state.clone()).await;
    let now = chrono::Utc::now().naive_utc();
    for source in ["203.0.113.7", "2001:db8::1"] {
        let other = crate::comms::gatekeeper::admission::world_mint::SecretThrottle {
            pool: &h.state.pool,
            max_attempts: h.state.secret_max_attempts,
            world: world.key(),
            subject: source,
        };
        while other.reserve(now).await.unwrap().is_some() {}
    }
    // Same /64 as an exhausted source: the shared source key limits it too.
    let neighbour = crate::comms::gatekeeper::admission::world_mint::SecretThrottle {
        pool: &h.state.pool,
        max_attempts: h.state.secret_max_attempts,
        world: world.key(),
        subject: "2001:db8::ffff",
    };
    assert!(neighbour.reserve(now).await.unwrap().is_none());
    assert!(
        join(addr, &world, VISITOR_KEY, r#"{"secret":"open sesame"}"#)
            .await
            .is_some(),
        "a source with budget left is admitted whatever other sources spent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_admitted_across_an_idle_unload_joins_a_hosted_scene() {
    let Some(h) = host_tuned(false, None, |raw| raw.idle_unload_secs = 1).await else {
        return;
    };
    let world = world_name();
    seed_world(&h.state.pool, &world, None, AccessType::Unrestricted).await;
    seed_scene(
        &h.state,
        &world,
        "bafyIdleRace",
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    h.state.load(&SceneKey::World(world.clone())).await.unwrap();
    let before = h.state.scenes.get(world.key()).unwrap();
    let addr = serve(h.state.clone()).await;

    // Upgraded while hosted; the scene is unloaded before the auth frame lands.
    let path = format!("/scene-state/ws/{}", world.display());
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}{path}"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(h.state.unload_idle().await, 1);
    ws.send(WsMessage::Binary(
        auth_frame(VISITOR_KEY, &path, "{}").into(),
    ))
    .await
    .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("an answer within 5 s");
    assert!(
        matches!(first, Some(Ok(WsMessage::Binary(ref b))) if b.first() == Some(&(MessageType::Init as u8))),
        "{first:?}"
    );
    let hosted = h.state.scenes.get(world.key()).expect("hosted again");
    assert!(
        !Arc::ptr_eq(&before, &hosted),
        "the unloaded scene is not reused"
    );
    assert_eq!(
        hosted.clients().len(),
        1,
        "the client is in the hosted scene"
    );
}

/// A Genesis deployment on `parcels`, with `authoritativeMultiplayer` when `opt_in`.
async fn seed_genesis(h: &Host, parcels: &[String], opt_in: bool) -> String {
    let id = format!("bafyG{}", Uuid::new_v4().simple());
    let code_hash = store(&h.state, b"").await;
    let mut metadata =
        serde_json::json!({ "main": "bin/game.js", "scene": { "base": parcels[0] } });
    if opt_in {
        metadata["authoritativeMultiplayer"] = true.into();
    }
    let dep_id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
         VALUES ('0x0', 'v3', 'scene', $1, $2::json, timezone('UTC', now()), $3, timezone('UTC', now()), '[]'::json) RETURNING id",
    )
    .bind(&id)
    .bind(metadata)
    .bind(parcels.to_vec())
    .fetch_one(&h.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, 'bin/game.js')")
        .bind(dep_id)
        .bind(&code_hash)
        .execute(&h.state.pool)
        .await
        .unwrap();
    for p in parcels {
        sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id) VALUES ($1, $2)")
            .bind(p)
            .bind(&id)
            .execute(&h.state.pool)
            .await
            .unwrap();
    }
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_genesis_parcel_autoloads_its_scene_only_when_it_opts_in() {
    let Some(h) = host(false).await else { return };
    let x = 40_000 + (Uuid::new_v4().as_u128() % 10_000) as i32;
    let (on, off) = (
        vec![format!("{x},3"), format!("{x},4")],
        vec![format!("{x},5")],
    );
    let id = seed_genesis(&h, &on, true).await;
    seed_genesis(&h, &off, false).await;
    let addr = serve(h.state.clone()).await;

    let _first = join_name(addr, &on[1], VISITOR_KEY, "{}")
        .await
        .expect("an opted-in Genesis scene autoloads by parcel");
    let _second = join_name(addr, &on[0], VISITOR_KEY, "{}")
        .await
        .expect("another parcel of it joins the same host");
    assert_eq!(h.state.scenes.len(), 1);
    assert_eq!(h.state.scenes.get(&id).unwrap().clients().len(), 2);

    let path = format!("/scene-state/ws/{}", off[0]);
    let refused = tokio_tungstenite::connect_async(format!("ws://{addr}{path}")).await;
    assert!(
        matches!(&refused, Err(tokio_tungstenite::tungstenite::Error::Http(r)) if r.status() == 404),
        "a Genesis scene without the flag is not autoloaded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_host_answers_503_before_the_upgrade() {
    let Some(h) = host_tuned(false, None, |raw| raw.max_scenes = 1).await else {
        return;
    };
    let (first, second) = (world_name(), world_name());
    for (world, id) in [(&first, "bafyFull1"), (&second, "bafyFull2")] {
        seed_world(&h.state.pool, world, None, AccessType::Unrestricted).await;
        seed_scene(&h.state, world, id, "", None, "0,0", &["0,0"], false).await;
    }
    let addr = serve(h.state.clone()).await;
    let (_ws, _) = join(addr, &first, VISITOR_KEY, "{}")
        .await
        .expect("the first scene fits");
    let path = format!("/scene-state/ws/{}", second.display());
    let refused = tokio_tungstenite::connect_async(format!("ws://{addr}{path}")).await;
    assert!(
        matches!(&refused, Err(tokio_tungstenite::tungstenite::Error::Http(r)) if r.status() == 503),
        "{refused:?}"
    );
    assert!(h.state.scenes.get(second.key()).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_load_in_progress_does_not_hold_up_joins_to_other_scenes() {
    let Some(h) = host(false).await else { return };
    let (slow, hosted) = (world_name(), world_name());
    for (world, id) in [(&slow, "bafySlowLoad"), (&hosted, "bafyHostedJoin")] {
        seed_world(&h.state.pool, world, None, AccessType::Unrestricted).await;
        seed_scene(&h.state, world, id, "", None, "0,0", &["0,0"], false).await;
    }
    h.state
        .load(&SceneKey::World(hosted.clone()))
        .await
        .unwrap();
    let addr = serve(h.state.clone()).await;

    // Hold the slow scene's load lock, as a long load would.
    let held = h
        .state
        .load_lock(&SceneKey::World(slow.clone()))
        .lock_owned()
        .await;
    let waiting = {
        let slow = slow.clone();
        tokio::spawn(async move { join(addr, &slow, VISITOR_KEY, "{}").await.is_some() })
    };
    let (_ws, _) = tokio::time::timeout(
        Duration::from_secs(3),
        join(addr, &hosted, VISITOR_KEY, "{}"),
    )
    .await
    .expect("a hosted scene's join does not wait on another key's load")
    .expect("admitted");
    assert!(
        !waiting.is_finished(),
        "the slow key's join waits for its load"
    );
    drop(held);
    assert!(
        waiting.await.unwrap(),
        "the slow key's join completes after its load"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_at_the_cap_keeps_its_slot() {
    let Some(h) = host_tuned(false, None, |raw| raw.max_scenes = 1).await else {
        return;
    };
    let (first, second) = (world_name(), world_name());
    for (world, id) in [(&first, "bafyCapReload"), (&second, "bafyCapOther")] {
        seed_world(&h.state.pool, world, None, AccessType::Unrestricted).await;
        seed_scene(&h.state, world, id, "", None, "0,0", &["0,0"], false).await;
    }
    let (first_key, second_key) = (SceneKey::World(first.clone()), SceneKey::World(second));
    h.state.load(&first_key).await.unwrap();

    // Mid-reload: the slot is reserved and the old scene is already down.
    let reload = h
        .state
        .reserve_slot(&first_key)
        .expect("a reload at the cap is allowed");
    h.state.remove_hosted(&first_key.room_key());
    assert!(h.state.scenes.is_empty());
    assert!(
        h.state.reserve_slot(&second_key).is_err(),
        "another scene cannot take the reloading scene's slot"
    );
    drop(reload);
    assert!(h.state.reserve_slot(&second_key).is_ok());
}

/// A Genesis deployment on `parcels`, its pointers active.
async fn seed_parked_genesis(h: &Host, id: &str, parcels: &[String]) {
    let code_hash = store(&h.state, b"").await;
    let dep_id: i32 = sqlx::query_scalar(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
         VALUES ('0x0', 'v3', 'scene', $1, $2::json, timezone('UTC', now()), $3, timezone('UTC', now()), '[]'::json) RETURNING id",
    )
    .bind(id)
    .bind(serde_json::json!({ "main": "bin/game.js", "scene": { "base": parcels[0] } }))
    .bind(parcels.to_vec())
    .fetch_one(&h.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, 'bin/game.js')")
        .bind(dep_id)
        .bind(&code_hash)
        .execute(&h.state.pool)
        .await
        .unwrap();
    for p in parcels {
        sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id) VALUES ($1, $2) ON CONFLICT (pointer) DO UPDATE SET entity_id = EXCLUDED.entity_id")
            .bind(p)
            .bind(id)
            .execute(&h.state.pool)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scene_parked_by_an_oom_is_retired_and_stays_refused() {
    let Some(h) = host(false).await else { return };
    let world = world_name();
    seed_world(&h.state.pool, &world, None, AccessType::Unrestricted).await;
    let world_id = format!("bafyParkedW{}", Uuid::new_v4().simple());
    seed_scene(
        &h.state,
        &world,
        &world_id,
        "",
        None,
        "0,0",
        &["0,0"],
        false,
    )
    .await;
    let world_key = SceneKey::World(world.clone());
    h.state.load(&world_key).await.unwrap();

    let x = 50_000 + (Uuid::new_v4().as_u128() % 10_000) as i32;
    let (first, second) = (
        format!("bafyParkedG{}", Uuid::new_v4().simple()),
        format!("bafyParkedH{}", Uuid::new_v4().simple()),
    );
    seed_parked_genesis(&h, &first, &[format!("{x},1"), format!("{x},2")]).await;
    let genesis_key = SceneKey::Genesis(SceneId::parse(&first).unwrap());
    h.state.load(&genesis_key).await.unwrap();

    for scene in [h.state.scenes.get(world.key()), h.state.scenes.get(&first)] {
        scene.unwrap().runtime.mark_parked_for_tests();
    }
    assert_eq!(h.state.retire_parked(), 2);
    assert!(h.state.scenes.get(world.key()).is_none());
    assert!(h.state.scenes.get(&first).is_none());
    assert_eq!(
        h.state.parked_count(),
        2,
        "parked isolates keep their slots"
    );

    let err = h.state.load(&world_key).await.unwrap_err();
    assert!(err.to_string().contains("ran out of memory"), "{err}");
    // A redeploy under a new id on one of the parked scene's parcels.
    seed_parked_genesis(&h, &second, &[format!("{x},2"), format!("{x},3")]).await;
    let err = h
        .state
        .load(&SceneKey::Genesis(SceneId::parse(&second).unwrap()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ran out of memory"), "{err}");
    assert!(h.state.scenes.get(&second).is_none());
}
