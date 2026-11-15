//! The write surface's contracts against the real schema, DB-backed through
//! `db::scratch` (skipped without `CATALYST_TEST_DATABASE_URL`); worlds and
//! wallets are uuid-scoped, so the tests share one database without seeing
//! one another. The name authority is a marketplace subgraph stub on the
//! loopback (the old tests seeded squid's `ens`/`nft` tables).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::auth::hash_bytes_v1;
use crate::chain_index::ChainIndex;
use crate::chain_index::graph::test_support::{Names, index_with_names};
use crate::comms::world_identity::{Parcel, ParcelSet, SceneId, WorldName};
use crate::content::forward;
use crate::policy::{Deploy, Permission, PersonalWorldsPolicy, Policy};
use crate::rooms;
use crate::shared::identity::Address;
use crate::shared::signed_fetch::ORIGINAL_PATH_HEADER;
use catalyst_storage::Storage;

use super::auth::test_support::{Wallet, signed_headers};
use super::deploy_form::test_support::{chain_fields, multipart_body};
use super::store::{
    self, AccessSetting, AllowListEdit, AllowListEditOutcome, OrderDirection, SceneDeployment,
    WorldSettingsUpdate, WorldsListFilters, WorldsListOptions, WorldsOrderBy,
};
use super::{CoreState, Settings, SharedState, router};

const ADMIN_TOKEN: &str = "cg-worlds-admin";
const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

struct Fixture {
    pool: PgPool,
    storage: Arc<Storage>,
    state: SharedState,
    app: Router,
    names: Names,
    /// The name authority behind `names`; its 30 s memo is cleared by the
    /// tests that move a NAME.
    market: Arc<ChainIndex>,
    /// The storage allowances (unknown wallets: unlimited).
    allowance: Arc<super::quota::test_support::FixedAllowance>,
    /// The user notices the routes wrote.
    social: Arc<crate::comms::interfaces::test_support::StubDirectory>,
}

async fn wait_for_lock(pool: &PgPool, blocker: i32, count: i64) {
    assert!(crate::shared::test_wait::until(async || {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid))")
            .bind(blocker).fetch_one(pool).await.unwrap() >= count
    }).await, "the expected transactions did not reach the held lock");
}

/// Personal worlds are not published here: the policy's cap counts every
/// personal world in the shared scratch database, so a route test that
/// leaves one behind would fail `policy::tests`; the cap and the personal
/// owner rule are covered there.
/// `forward_to`: the publishes are queued for that (loopback) upstream.
async fn fixture_with(personal: PersonalWorldsPolicy, forward_to: Option<&str>) -> Option<Fixture> {
    fixture_full(personal, forward_to, None, test_settings()).await
}

fn test_settings() -> Settings {
    Settings::for_tests(Some(ADMIN_TOKEN))
}

/// A fixture whose access allow-lists hold at most `wallets` wallets and
/// `communities` communities (`ACCESS_MAX_WALLETS`, `ACCESS_MAX_COMMUNITIES`).
async fn fixture_with_access_caps(wallets: u32, communities: u32) -> Option<Fixture> {
    fixture_full(
        PersonalWorldsPolicy::disabled(),
        None,
        None,
        test_settings().with_access_caps(wallets, communities),
    )
    .await
}

/// ENS worlds on, over a fake chain with an ENS cap of `ens_cap` bytes.
async fn fixture_with_ens(ens_cap: i64) -> Option<(Fixture, crate::ens::test_support::FakeEns)> {
    let (resolver, ens) = crate::ens::test_support::resolver().await;
    let f = fixture_full(
        PersonalWorldsPolicy::disabled(),
        None,
        Some((resolver, ens_cap)),
        test_settings(),
    )
    .await?;
    Some((f, ens))
}

/// Held across a collector run: the run lock is one per database and the lib tests share
/// the scratch database, so a concurrent run answers 503.
static GC_RUNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fixture_full(
    personal: PersonalWorldsPolicy,
    forward_to: Option<&str>,
    ens: Option<(Arc<crate::ens::EnsResolver>, i64)>,
    settings: Settings,
) -> Option<Fixture> {
    let pool = crate::db::scratch::pool(8).await?;
    Some(fixture_on_pool(pool, personal, forward_to, ens, settings, "").await)
}

/// A fixture on its own scratch database whose name deny list blocks
/// `blocked_world_names` (`BLOCKED_WORLD_NAMES`).
async fn fixture_blocking(blocked_world_names: &str) -> Option<Fixture> {
    let pool = crate::db::scratch::pool(8).await?;
    Some(
        fixture_on_pool(
            pool,
            PersonalWorldsPolicy::disabled(),
            None,
            None,
            test_settings(),
            blocked_world_names,
        )
        .await,
    )
}

/// `blocked_world_names`: `BLOCKED_WORLD_NAMES` of the one name deny list the
/// policy and the core share.
async fn fixture_on_pool(
    pool: PgPool,
    personal: PersonalWorldsPolicy,
    forward_to: Option<&str>,
    ens: Option<(Arc<crate::ens::EnsResolver>, i64)>,
    settings: Settings,
    blocked_world_names: &str,
) -> Fixture {
    let (market, names) = index_with_names().await;
    let dir = std::env::temp_dir().join(format!(
        "catalyst-worlds-core-{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&dir).await.unwrap());
    let mut policy = Policy::new(pool.clone(), personal, Some(market.clone())).with_name_deny_list(
        Arc::new(super::names::NameDenyList::new(blocked_world_names)),
    );
    if let Some((resolver, cap)) = ens {
        policy = policy.with_ens(Some(resolver), cap);
    }
    let forwarder = forward_to.map(|target| {
        forward::test_support::forwarder(
            pool.clone(),
            storage.clone(),
            super::forwarding::LANE,
            target,
            3,
        )
    });
    let allowance = Arc::new(super::quota::test_support::FixedAllowance::default());
    let social = Arc::new(crate::comms::interfaces::test_support::StubDirectory::default());
    let state = Arc::new(
        CoreState::new(
            pool.clone(),
            storage.clone(),
            Arc::new(policy),
            settings,
            None,
        )
        .with_forwarder(forwarder)
        .with_allowance(allowance.clone())
        .with_notices(super::notices::Notices::new(
            Some(social.clone()),
            "https://builder.example",
        )),
    );
    let app = router(state.clone());
    Fixture {
        pool,
        storage,
        state,
        app,
        names,
        market,
        allowance,
        social,
    }
}

async fn fixture() -> Option<Fixture> {
    fixture_with(PersonalWorldsPolicy::disabled(), None).await
}

fn world_name() -> WorldName {
    WorldName::parse(&format!(
        "w{}.dcl.eth",
        &Uuid::new_v4().simple().to_string()[..12]
    ))
    .unwrap()
}

struct Case {
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<(Vec<u8>, String)>,
}

impl Case {
    fn new(method: &'static str, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            headers: Vec::new(),
            body: None,
        }
    }
    async fn signed(self, w: &Wallet) -> Self {
        self.signed_meta(w, "{}").await
    }
    async fn signed_meta(self, w: &Wallet, metadata: &str) -> Self {
        let path = self.path.split('?').next().unwrap().to_string();
        self.signed_over(w, &path, metadata).await
    }
    /// Signed over `path`, whatever path the request is sent to.
    async fn signed_over(mut self, w: &Wallet, path: &str, metadata: &str) -> Self {
        let headers = signed_headers(w, self.method, path, metadata).await;
        for (k, v) in headers.iter() {
            self.headers
                .push((k.to_string(), v.to_str().unwrap().to_string()));
        }
        self
    }
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    fn bearer(mut self, token: &str) -> Self {
        self.headers
            .push(("authorization".to_string(), format!("Bearer {token}")));
        self
    }
    fn json(mut self, v: &Value) -> Self {
        self.body = Some((
            serde_json::to_vec(v).unwrap(),
            "application/json".to_string(),
        ));
        self
    }
    fn body(mut self, bytes: Vec<u8>, content_type: &str) -> Self {
        self.body = Some((bytes, content_type.to_string()));
        self
    }
}

async fn hit(app: &Router, case: Case) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(case.method.to_uppercase().as_str())
        .uri(&case.path);
    for (k, v) in &case.headers {
        builder = builder.header(k, v);
    }
    let request = match case.body {
        Some((bytes, ct)) => builder
            .header(header::CONTENT_TYPE, ct)
            .body(Body::from(bytes))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

async fn expect(app: &Router, case: Case, status: u16) -> Value {
    let path = case.path.clone();
    let method = case.method;
    let (got, body) = hit(app, case).await;
    assert_eq!(got.as_u16(), status, "{method} {path}: {body}");
    body
}

struct Deployment {
    body: Vec<u8>,
    content_type: String,
    entity_id: String,
    thumb_hash: String,
}

async fn deployment_with(
    wallet: &Wallet,
    world: &WorldName,
    title: &str,
    base: &str,
    pointers: &[&str],
    nav_thumb: &str,
    thumb: &[u8],
) -> Deployment {
    let now = chrono::Utc::now().timestamp_millis();
    deployment_at(wallet, world, title, base, pointers, nav_thumb, thumb, now).await
}

#[allow(clippy::too_many_arguments)]
async fn deployment_at(
    wallet: &Wallet,
    world: &WorldName,
    title: &str,
    base: &str,
    pointers: &[&str],
    nav_thumb: &str,
    thumb: &[u8],
    timestamp: i64,
) -> Deployment {
    let thumb_hash = hash_bytes_v1(thumb);
    let entity = json!({
        "version": "v3",
        "type": "scene",
        "timestamp": timestamp,
        "pointers": pointers,
        "content": [{ "file": nav_thumb, "hash": thumb_hash }],
        "metadata": {
            "main": "bin/index.js",
            "display": { "title": title, "navmapThumbnail": nav_thumb },
            "worldConfiguration": { "name": world.display() },
            "scene": { "base": base, "parcels": pointers }
        }
    });
    let entity_bytes = serde_json::to_vec(&entity).unwrap();
    let entity_id = hash_bytes_v1(&entity_bytes);
    let chain = chain_fields(&wallet.chain(&entity_id).await);
    let mut parts: Vec<(&str, Option<&str>, &[u8])> =
        vec![("entityId", None, entity_id.as_bytes())];
    parts.extend(chain.iter().map(|(k, v)| (k.as_str(), None, v.as_bytes())));
    parts.push((&entity_id, Some(&entity_id), &entity_bytes));
    parts.push((&thumb_hash, Some(&thumb_hash), thumb));
    let (body, content_type) = multipart_body(&parts);
    Deployment {
        body,
        content_type,
        entity_id,
        thumb_hash,
    }
}

async fn deployment(
    wallet: &Wallet,
    world: &WorldName,
    title: &str,
    base: &str,
    pointers: &[&str],
) -> Deployment {
    let thumb = format!("{title} thumb").into_bytes();
    deployment_with(wallet, world, title, base, pointers, "thumb.png", &thumb).await
}

async fn post_entity(app: &Router, d: &Deployment, status: u16) -> Value {
    expect(
        app,
        Case::new("post", "/entities").body(d.body.clone(), &d.content_type),
        status,
    )
    .await
}

async fn stored_owner(pool: &PgPool, world: &WorldName) -> Option<String> {
    sqlx::query_scalar("SELECT owner FROM worlds.worlds WHERE name = $1")
        .bind(world.key())
        .fetch_optional(pool)
        .await
        .unwrap()
        .flatten()
}

async fn deployed_entity_ids(pool: &PgPool, world: &WorldName) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT entity_id FROM worlds.world_scenes WHERE world_name = $1 ORDER BY entity_id",
    )
    .bind(world.key())
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A parcel-scoped `deployment` grant, written the way the permissions write
/// surface writes it: one grant row plus one parcel row per parcel.
async fn grant_parcels(pool: &PgPool, world: &WorldName, address: &Address, parcels: &[&str]) {
    sqlx::query("INSERT INTO worlds.worlds (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
        .bind(world.key())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'deployment', $2)
         ON CONFLICT (world_name, permission, address) DO UPDATE SET updated_at = timezone('UTC', now())",
    )
    .bind(world.key())
    .bind(address)
    .execute(pool)
    .await
    .unwrap();
    for parcel in parcels {
        sqlx::query("INSERT INTO worlds.world_permission_parcels (world_name, permission, address, parcel) VALUES ($1, 'deployment', $2, $3) ON CONFLICT DO NOTHING")
            .bind(world.key())
            .bind(address)
            .bind(*parcel)
            .execute(pool)
            .await
            .unwrap();
    }
}

fn no_permission(world: &str) -> String {
    format!(
        "Deployment failed: Deployment failed: Your wallet has no permission to publish this scene because it does not have permission to deploy under \"{world}\". Check scene.json to select a name that either you own or you were given permission to deploy."
    )
}

fn parcels(raw: &[&str]) -> ParcelSet {
    ParcelSet::parse(raw).unwrap()
}

fn scene_entity(
    world: &WorldName,
    title: &str,
    thumb: &str,
    base: &str,
    parcels: &[&str],
) -> Value {
    json!({
        "type": "scene",
        "timestamp": 1000,
        "pointers": parcels,
        "content": [{ "file": "thumb.png", "hash": thumb }],
        "metadata": {
            "display": { "title": title, "description": "A test world", "navmapThumbnail": "thumb.png" },
            "worldConfiguration": {
                "name": world.display(),
                "skyboxConfig": { "fixedTime": 36000 },
                "fixedAdapter": "offline:offline"
            },
            "scene": { "base": base, "parcels": parcels },
            "tags": ["art", "game"],
            "rating": "E"
        }
    })
}

/// A store-level deploy (the old `WorldsComponent::deploy_scene` call shape).
async fn deploy(
    f: &Fixture,
    world: &WorldName,
    owner: &Address,
    entity_id: &str,
    entity: &Value,
    footprint: &[&str],
    replacement: &Deploy,
) -> Result<(), super::http::ApiError> {
    // deploy_scene requires the entity blob; seed it for canonical ids.
    if crate::auth::is_canonical_cid(entity_id) {
        f.storage
            .store(entity_id, bytes::Bytes::from(entity.to_string()))
            .await
            .unwrap();
    }
    store::deploy_scene(
        &f.pool,
        &f.storage,
        SceneDeployment {
            world,
            name_owner: Some(owner),
            entity_id,
            deployer: owner,
            deployment_auth_chain: &json!([{ "type": "SIGNER", "payload": owner.as_str() }]),
            entity,
            parcels: &parcels(footprint),
            size: 1,
            replacement,
            forward_to: None,
            quota: None,
        },
    )
    .await
    .map(drop)
}

async fn entity_ids(f: &Fixture, world: &WorldName) -> Vec<String> {
    let mut ids: Vec<String> = store::list_scenes(&f.pool, world)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn acl_deployer_cannot_take_over_an_existing_world_owner() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let name_owner = Wallet::random();
    let acl_deployer = Wallet::random();
    f.names.set(&name_owner.address, world.label());

    let d = deployment(&name_owner, &world, "Owner World", "0,0", &["0,0", "0,1"]).await;
    post_entity(&f.app, &d, 200).await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(name_owner.address.as_str())
    );

    let path = format!("/world/{}/permissions/deployment", world.display());
    let meta = json!({ "type": "allow-list", "wallets": [acl_deployer.display()] }).to_string();
    expect(
        &f.app,
        Case::new("post", &path)
            .signed_meta(&name_owner, &meta)
            .await,
        204,
    )
    .await;

    let d = deployment(&acl_deployer, &world, "ACL World", "0,0", &["0,0", "0,1"]).await;
    post_entity(&f.app, &d, 200).await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(name_owner.address.as_str()),
        "an ACL deployer rewrote worlds.owner to {}",
        acl_deployer.address.as_str()
    );

    let permissions = expect(
        &f.app,
        Case::new("get", format!("/world/{}/permissions", world.display())),
        200,
    )
    .await;
    assert_eq!(
        permissions["owner"].as_str(),
        Some(name_owner.address.as_str())
    );
    assert_eq!(
        permissions["permissions"]["deployment"]["wallets"],
        json!([acl_deployer.address.as_str()])
    );

    expect(
        &f.app,
        Case::new("post", &path)
            .signed_meta(&acl_deployer, &meta)
            .await,
        401,
    )
    .await;
}

#[tokio::test]
async fn first_deploy_seeds_the_resolved_name_owner() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let name_owner = Wallet::random();
    f.names.set(&name_owner.address, world.label());
    let d = deployment(&name_owner, &world, "Seeded World", "0,0", &["0,0", "0,1"]).await;
    post_entity(&f.app, &d, 200).await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(name_owner.address.as_str())
    );
    // A stranger (no NAME) is refused with the old message, and nothing lands.
    let stranger = Wallet::random();
    let other = world_name();
    let d = deployment(&stranger, &other, "Nope", "0,0", &["0,0"]).await;
    let body = post_entity(&f.app, &d, 400).await;
    assert_eq!(body["message"], no_permission(other.display()));
    assert!(deployed_entity_ids(&f.pool, &other).await.is_empty());
}

/// A grantee's publish refreshes `worlds.owner` to the live NAME holder, and
/// is refused when nobody holds the NAME.
#[tokio::test]
async fn a_grantee_publish_refreshes_the_owner_to_the_live_name_holder() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let (old, new, grantee) = (Wallet::random(), Wallet::random(), Wallet::random());
    f.names.set(&old.address, world.label());
    post_entity(
        &f.app,
        &deployment(&old, &world, "Before", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    let path = format!("/world/{}/permissions/deployment", world.display());
    let meta = json!({ "type": "allow-list", "wallets": [grantee.display()] }).to_string();
    expect(
        &f.app,
        Case::new("post", &path).signed_meta(&old, &meta).await,
        204,
    )
    .await;

    f.names.forget(&old.address);
    f.market.graph.name_owner.clear();
    let body = post_entity(
        &f.app,
        &deployment(&grantee, &world, "Orphan", "0,0", &["0,0"]).await,
        400,
    )
    .await;
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .ends_with("owner address could not be resolved."),
        "{body}"
    );

    f.names.set(&new.address, world.label());
    f.market.graph.name_owner.clear();
    post_entity(
        &f.app,
        &deployment(&grantee, &world, "Grantee", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(new.address.as_str())
    );
}

/// A NAME transfer: the previous holder loses the publish and every owner
/// route at once (the authority is asked, `worlds.owner` is only its cache),
/// the new holder's publish rewrites the cache.
#[tokio::test]
async fn a_name_transfer_moves_ownership_on_the_next_publish() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let (old, new) = (Wallet::random(), Wallet::random());
    f.names.set(&old.address, world.label());
    post_entity(
        &f.app,
        &deployment(&old, &world, "Before", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(old.address.as_str())
    );

    f.names.forget(&old.address);
    f.names.set(&new.address, world.label());
    f.market.graph.names.clear();

    let body = post_entity(
        &f.app,
        &deployment(&old, &world, "Stale", "0,0", &["0,0"]).await,
        400,
    )
    .await;
    assert_eq!(body["message"], no_permission(world.display()));
    let path = format!("/world/{}/permissions/deployment", world.display());
    let meta = json!({ "type": "allow-list", "wallets": [] }).to_string();
    expect(
        &f.app,
        Case::new("post", &path).signed_meta(&old, &meta).await,
        401,
    )
    .await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(old.address.as_str()),
        "a refused publish rewrites nothing"
    );

    post_entity(
        &f.app,
        &deployment(&new, &world, "After", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(new.address.as_str())
    );
    expect(
        &f.app,
        Case::new("post", &path).signed_meta(&new, &meta).await,
        204,
    )
    .await;
    let permissions = expect(
        &f.app,
        Case::new("get", format!("/world/{}/permissions", world.display())),
        200,
    )
    .await;
    assert_eq!(permissions["owner"].as_str(), Some(new.address.as_str()));
}

/// A world this node only mirrored becomes local on its owner's publish: the
/// overlapping mirrored scene goes, the new scene is local and the world
/// row's `mirror_source` is cleared in the same transaction (the mirror then
/// refuses to write it, `worlds::mirror`); a stranger is refused as before.
#[tokio::test]
async fn an_owner_publish_converts_a_mirrored_world_to_local() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let (owner, stranger) = (Wallet::random(), Wallet::random());
    f.names.set(&owner.address, world.label());
    sqlx::query(
        "INSERT INTO worlds.worlds (name, mirror_source) VALUES ($1, 'https://upstream.example')",
    )
    .bind(world.key())
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size, mirror_source) \
         VALUES ($1, 'bafymirrored', '[]'::json, '{}'::jsonb, $2, ARRAY['0,0'], 1, 'https://upstream.example')",
    )
    .bind(world.key())
    .bind(crate::ZERO_ADDRESS)
    .execute(&f.pool)
    .await
    .unwrap();

    post_entity(
        &f.app,
        &deployment(&stranger, &world, "Nope", "0,0", &["0,0"]).await,
        400,
    )
    .await;
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT entity_id, mirror_source FROM worlds.world_scenes WHERE world_name = $1",
    )
    .bind(world.key())
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(
            "bafymirrored".to_string(),
            Some("https://upstream.example".to_string())
        )]
    );

    post_entity(
        &f.app,
        &deployment(&owner, &world, "Mine now", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT entity_id, mirror_source FROM worlds.world_scenes WHERE world_name = $1",
    )
    .bind(world.key())
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].1.is_none(),
        "the local scene replaced the mirrored one"
    );
    let (stored, mirror): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT owner, mirror_source FROM worlds.worlds WHERE name = $1")
            .bind(world.key())
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        (stored.as_deref(), mirror),
        (Some(owner.address.as_str()), None),
        "the world row says local"
    );
}

/// The mirror never refreshes a local world, so the publish that converts a
/// mirrored world takes every mirrored scene with it, the ones away from its
/// parcels too: none is left behind, stale, beside the local scene.
#[tokio::test]
async fn a_local_publish_leaves_no_mirrored_scene_in_the_world() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    sqlx::query(
        "INSERT INTO worlds.worlds (name, mirror_source) VALUES ($1, 'https://upstream.example')",
    )
    .bind(world.key())
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size, mirror_source) \
         VALUES ($1, 'bafyover', '[]'::json, '{}'::jsonb, $2, ARRAY['0,0'], 1, 'https://upstream.example'), \
                ($1, 'bafyaway', '[]'::json, '{}'::jsonb, $2, ARRAY['5,5'], 1, 'https://upstream.example')",
    )
    .bind(world.key())
    .bind(crate::ZERO_ADDRESS)
    .execute(&f.pool)
    .await
    .unwrap();

    post_entity(
        &f.app,
        &deployment(&owner, &world, "Mine now", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    let mirrored: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM worlds.world_scenes WHERE world_name = $1 AND mirror_source IS NOT NULL",
    )
    .bind(world.key())
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(mirrored, Vec::<String>::new());
    let local: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worlds.world_scenes WHERE world_name = $1 AND mirror_source IS NULL",
    )
    .bind(world.key())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(local, 1);
}

#[tokio::test]
async fn a_parcel_scoped_grant_authorizes_only_its_own_parcels() {
    let Some(f) = fixture().await else { return };
    let (world, owner, grantee) = (world_name(), Wallet::random(), Wallet::random());
    f.names.set(&owner.address, world.label());

    let seed = deployment(&owner, &world, "Seed", "9,9", &["9,9"]).await;
    post_entity(&f.app, &seed, 200).await;

    grant_parcels(&f.pool, &world, &grantee.address, &["0,0"]).await;

    let outside = deployment(&grantee, &world, "Outside", "0,0", &["0,0", "0,1"]).await;
    post_entity(&f.app, &outside, 400).await;
    assert!(
        !deployed_entity_ids(&f.pool, &world)
            .await
            .contains(&outside.entity_id),
        "a parcel-scoped grant must not authorize parcels outside it"
    );

    let inside = deployment(&grantee, &world, "Inside", "0,0", &["0,0"]).await;
    post_entity(&f.app, &inside, 200).await;
    assert!(
        deployed_entity_ids(&f.pool, &world)
            .await
            .contains(&inside.entity_id)
    );
}

#[tokio::test]
async fn replacing_a_scene_requires_authority_over_its_whole_footprint() {
    let Some(f) = fixture().await else { return };
    let (world, owner, grantee) = (world_name(), Wallet::random(), Wallet::random());
    f.names.set(&owner.address, world.label());

    let existing = deployment(&owner, &world, "Existing", "0,0", &["0,0", "0,1"]).await;
    post_entity(&f.app, &existing, 200).await;
    let untouched = deployment(&owner, &world, "Untouched", "7,7", &["7,7"]).await;
    post_entity(&f.app, &untouched, 200).await;

    grant_parcels(&f.pool, &world, &grantee.address, &["0,0"]).await;

    let partial = deployment(&grantee, &world, "Partial", "0,0", &["0,0"]).await;
    post_entity(&f.app, &partial, 400).await;
    let mut ids = deployed_entity_ids(&f.pool, &world).await;
    ids.sort();
    let mut expected = vec![existing.entity_id.clone(), untouched.entity_id.clone()];
    expected.sort();
    assert_eq!(ids, expected, "the 403 must leave both scenes in place");

    grant_parcels(&f.pool, &world, &grantee.address, &["0,0", "0,1"]).await;
    let full = deployment(&grantee, &world, "Full", "0,0", &["0,0"]).await;
    post_entity(&f.app, &full, 200).await;

    let mut ids = deployed_entity_ids(&f.pool, &world).await;
    ids.sort();
    let mut expected = vec![full.entity_id.clone(), untouched.entity_id.clone()];
    expected.sort();
    assert_eq!(
        ids, expected,
        "only the authorized scene identity may be replaced"
    );
}

#[tokio::test]
async fn a_scene_may_not_declare_a_base_parcel_outside_itself() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random());
    f.names.set(&owner.address, world.label());

    let liar = deployment(&owner, &world, "Liar", "50,50", &["0,0", "0,1"]).await;
    let body = post_entity(&f.app, &liar, 400).await;
    assert_eq!(
        body["message"],
        "Deployment failed: metadata.scene.parcels must contain at least 1 valid item(s)"
    );
    assert!(deployed_entity_ids(&f.pool, &world).await.is_empty());

    let honest = deployment(&owner, &world, "Honest", "0,1", &["0,0", "0,1"]).await;
    post_entity(&f.app, &honest, 200).await;
}

#[tokio::test]
async fn unpublishing_requires_authority_over_the_whole_scene_footprint() {
    let Some(f) = fixture().await else { return };
    let (world, owner, grantee) = (world_name(), Wallet::random(), Wallet::random());
    f.names.set(&owner.address, world.label());

    let wide = deployment(&owner, &world, "Wide", "0,0", &["0,0", "0,1"]).await;
    post_entity(&f.app, &wide, 200).await;

    grant_parcels(&f.pool, &world, &grantee.address, &["0,0"]).await;

    let path = format!("/world/{}/scenes/0,0", world.display());
    expect(
        &f.app,
        Case::new("delete", &path).signed(&grantee).await,
        401,
    )
    .await;
    assert_eq!(
        deployed_entity_ids(&f.pool, &world).await,
        vec![wide.entity_id.clone()],
        "a grant on one parcel must not unpublish a scene that spans two"
    );

    grant_parcels(&f.pool, &world, &grantee.address, &["0,0", "0,1"]).await;
    expect(
        &f.app,
        Case::new("delete", &path).signed(&grantee).await,
        200,
    )
    .await;
    assert!(deployed_entity_ids(&f.pool, &world).await.is_empty());
}

fn bare_scene(base: &str, parcels: &[&str]) -> Value {
    json!({ "type": "scene", "timestamp": 1000, "pointers": parcels, "metadata": { "scene": { "base": base, "parcels": parcels } } })
}

#[tokio::test]
async fn a_base_outside_the_scene_footprint_never_becomes_the_scene_identity() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyliar",
        &bare_scene("100,100", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .expect("deploy liar");
    assert_eq!(
        store::scene_base_parcel(&f.pool, &world, "bafyliar")
            .await
            .unwrap(),
        Some("0,0".to_string()),
        "a base outside the footprint must fall back to the scene's own first parcel"
    );
    deploy(
        &f,
        &world,
        &owner,
        "bafyhonest",
        &bare_scene("1,1", &["1,0", "1,1"]),
        &["1,0", "1,1"],
        &Deploy::Owner,
    )
    .await
    .expect("deploy honest");
    assert_eq!(
        store::scene_base_parcel(&f.pool, &world, "bafyhonest")
            .await
            .unwrap(),
        Some("1,1".to_string())
    );
    let bases: Vec<(String, Option<String>)> = store::list_scenes(&f.pool, &world)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _, base)| (id, base))
        .collect();
    assert!(bases.contains(&("bafyliar".to_string(), Some("0,0".to_string()))));
    assert!(bases.contains(&("bafyhonest".to_string(), Some("1,1".to_string()))));
    // The resolver agrees: the liar's base is its first parcel; each scene
    // has its own storage scope whatever its base claims.
    let active = rooms::world_scene(&f.pool, &world, &SceneId::parse("bafyliar").unwrap())
        .await
        .unwrap()
        .unwrap();
    let honest = rooms::world_scene(&f.pool, &world, &SceneId::parse("bafyhonest").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.base.canonical(), "0,0");
    assert!(active.namespace.is_some() && honest.namespace.is_some());
    assert_ne!(active.namespace, honest.namespace);
    assert!(active.local);
}

#[tokio::test]
async fn a_scoped_deploy_replaces_only_the_authorized_identities() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyexisting",
        &bare_scene("0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    deploy(
        &f,
        &world,
        &owner,
        "bafyneighbour",
        &bare_scene("5,5", &["5,5"]),
        &["5,5"],
        &Deploy::Owner,
    )
    .await
    .unwrap();

    let overlapping = f
        .state
        .policy
        .overlapping_scenes(&world, &parcels(&["0,0"]))
        .await
        .unwrap();
    assert_eq!(
        overlapping,
        vec![(
            SceneId::parse("bafyexisting").unwrap(),
            parcels(&["0,0", "0,1"])
        )],
        "the overlap query must report the replaced scene's WHOLE footprint"
    );

    deploy(
        &f,
        &world,
        &owner,
        "bafynew",
        &bare_scene("0,0", &["0,0"]),
        &["0,0"],
        &Deploy::Scoped(vec![SceneId::parse("bafyexisting").unwrap()]),
    )
    .await
    .expect("scoped deploy");
    assert_eq!(
        entity_ids(&f, &world).await,
        vec!["bafyneighbour".to_string(), "bafynew".to_string()],
        "only the authorized identity may be replaced"
    );
}

#[tokio::test]
async fn a_scoped_deploy_rolls_back_when_an_unauthorized_scene_survives() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyexisting",
        &bare_scene("0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let err = deploy(
        &f,
        &world,
        &owner,
        "bafyracer",
        &bare_scene("0,0", &["0,0"]),
        &["0,0"],
        &Deploy::Scoped(Vec::new()),
    )
    .await
    .expect_err("stale authorization must not deploy");
    assert!(
        err.is_conflict() && err.to_string().contains(world.key()),
        "expected a replacement conflict naming {}, got {err:?}",
        world.key()
    );
    assert_eq!(
        entity_ids(&f, &world).await,
        vec!["bafyexisting".to_string()],
        "the whole transaction must roll back"
    );
}

fn scene_at(timestamp: i64, parcels: &[&str]) -> Value {
    json!({ "type": "scene", "timestamp": timestamp, "pointers": parcels, "metadata": { "scene": { "base": parcels[0], "parcels": parcels } } })
}

fn assert_newer_refusal(result: Result<(), super::http::ApiError>) {
    let err = result.expect_err("an older entity must not replace a newer scene");
    assert_eq!(err.status(), 400, "{err:?}");
    assert_eq!(
        err.public_message(),
        "Deployment failed: a newer scene is already deployed on one or more of these parcels."
    );
}

/// Upstream compares entity timestamps as JS numbers (doubles): an epoch-ms timestamp with three
/// decimals is newer than one a thousandth below it, and an equal one falls to the entity-id
/// tie-break.
#[tokio::test]
async fn fractional_timestamps_compare_as_numbers() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    let at = |timestamp: f64| {
        json!({ "type": "scene", "timestamp": timestamp, "pointers": ["0,0"],
                "metadata": { "scene": { "base": "0,0", "parcels": ["0,0"] } } })
    };
    let publish = |id: &'static str, timestamp: f64| {
        let (f, world, owner, entity) = (&f, &world, &owner, at(timestamp));
        async move { deploy(f, world, owner, id, &entity, &["0,0"], &Deploy::Owner).await }
    };
    const T: f64 = 1_731_100_000_000.123;
    publish("bafyb", T).await.unwrap();
    // Equal: the greater entity id wins.
    assert_newer_refusal(publish("bafya", T).await);
    publish("bafyc", T)
        .await
        .expect("an equal timestamp with a greater id");
    // A thousandth older loses whatever its id; a thousandth newer wins.
    assert_newer_refusal(publish("bafyz", 1_731_100_000_000.122).await);
    publish("bafyd", 1_731_100_000_000.124)
        .await
        .expect("a thousandth newer");
    assert_eq!(entity_ids(&f, &world).await, vec!["bafyd".to_string()]);
}

#[tokio::test]
async fn an_older_entity_never_replaces_a_newer_overlapping_scene() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafynewer",
        &scene_at(2000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();

    assert_newer_refusal(
        deploy(
            &f,
            &world,
            &owner,
            "bafyolder",
            &scene_at(1000, &["0,0", "0,1"]),
            &["0,0", "0,1"],
            &Deploy::Owner,
        )
        .await,
    );
    assert_newer_refusal(
        deploy(
            &f,
            &world,
            &owner,
            "bafyolder",
            &scene_at(1000, &["0,0"]),
            &["0,0"],
            &Deploy::Scoped(vec![SceneId::parse("bafynewer").unwrap()]),
        )
        .await,
    );
    assert_eq!(entity_ids(&f, &world).await, vec!["bafynewer".to_string()]);

    deploy(
        &f,
        &world,
        &owner,
        "bafyelsewhere",
        &scene_at(1000, &["5,5"]),
        &["5,5"],
        &Deploy::Owner,
    )
    .await
    .expect("a non-overlapping older entity deploys");
    deploy(
        &f,
        &world,
        &owner,
        "bafynewer",
        &scene_at(2000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .expect("re-publishing the deployed entity is not older than itself");
    deploy(
        &f,
        &world,
        &owner,
        "bafylatest",
        &scene_at(3000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .expect("a newer entity replaces the scene");
    assert_eq!(
        entity_ids(&f, &world).await,
        vec!["bafyelsewhere".to_string(), "bafylatest".to_string()]
    );
}

#[tokio::test]
async fn equal_timestamps_break_ties_by_entity_id() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafym",
        &scene_at(1000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();

    assert_newer_refusal(
        deploy(
            &f,
            &world,
            &owner,
            "bafya",
            &scene_at(1000, &["0,0"]),
            &["0,0"],
            &Deploy::Owner,
        )
        .await,
    );
    assert_eq!(entity_ids(&f, &world).await, vec!["bafym".to_string()]);

    deploy(
        &f,
        &world,
        &owner,
        "bafyz",
        &scene_at(1000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .expect("a higher entity id wins the tie");
    assert_eq!(entity_ids(&f, &world).await, vec!["bafyz".to_string()]);
}

#[tokio::test]
async fn equal_timestamps_compare_entity_ids_byte_wise() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "QmLegacyScene",
        &scene_at(1000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();

    deploy(
        &f,
        &world,
        &owner,
        "bafylocal",
        &scene_at(1000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .expect("'b' sorts after 'Q' byte-wise");
    assert_eq!(entity_ids(&f, &world).await, vec!["bafylocal".to_string()]);
}

#[tokio::test]
async fn a_future_dated_scene_counts_as_dated_at_its_deploy() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    let ahead = chrono::Utc::now().timestamp_millis() + 10 * 60 * 1000;
    deploy(
        &f,
        &world,
        &owner,
        "bafyfuture",
        &scene_at(ahead, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis();

    assert_newer_refusal(
        deploy(
            &f,
            &world,
            &owner,
            "bafyolder",
            &scene_at(now - 60_000, &["0,0"]),
            &["0,0"],
            &Deploy::Owner,
        )
        .await,
    );
    deploy(
        &f,
        &world,
        &owner,
        "bafyowner",
        &scene_at(now, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .expect("a publish signed after the future-dated deploy replaces it");
    assert_eq!(entity_ids(&f, &world).await, vec!["bafyowner".to_string()]);
}

#[tokio::test]
async fn a_stored_scene_without_a_numeric_timestamp_never_blocks() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    for (id, base) in [("bafytext", "0,0"), ("bafymissing", "1,0")] {
        deploy(
            &f,
            &world,
            &owner,
            id,
            &scene_at(5000, &[base]),
            &[base],
            &Deploy::Owner,
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE worlds.world_scenes SET entity = jsonb_set(entity, '{timestamp}', '\"5000\"') WHERE entity_id = 'bafytext'")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE worlds.world_scenes SET entity = entity - 'timestamp' WHERE entity_id = 'bafymissing'")
        .execute(&f.pool)
        .await
        .unwrap();

    deploy(
        &f,
        &world,
        &owner,
        "bafyolder",
        &scene_at(1000, &["0,0", "1,0"]),
        &["0,0", "1,0"],
        &Deploy::Owner,
    )
    .await
    .expect("malformed stored timestamps do not block");
    assert_eq!(entity_ids(&f, &world).await, vec!["bafyolder".to_string()]);
}

#[tokio::test]
async fn an_entity_without_an_integer_timestamp_is_refused() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    let mut undated = bare_scene("0,0", &["0,0"]);
    undated.as_object_mut().unwrap().remove("timestamp");
    let err = deploy(
        &f,
        &world,
        &owner,
        "bafyundated",
        &undated,
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap_err();
    assert!(err.is_bad_request(), "{err:?}");
    assert!(entity_ids(&f, &world).await.is_empty());
}

#[tokio::test]
async fn a_newer_mirrored_scene_keeps_the_world_mirrored() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    sqlx::query(
        "INSERT INTO worlds.worlds (name, mirror_source) VALUES ($1, 'https://upstream.example')",
    )
    .bind(world.key())
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size, mirror_source) \
         VALUES ($1, 'bafymirrored', '[]'::json, $2, $3, ARRAY['0,0'], 1, 'https://upstream.example')",
    )
    .bind(world.key())
    .bind(scene_at(2000, &["0,0"]))
    .bind(crate::ZERO_ADDRESS)
    .execute(&f.pool)
    .await
    .unwrap();

    assert_newer_refusal(
        deploy(
            &f,
            &world,
            &owner,
            "bafylocal",
            &scene_at(1000, &["0,0"]),
            &["0,0"],
            &Deploy::Owner,
        )
        .await,
    );
    let mirror: Option<String> =
        sqlx::query_scalar("SELECT mirror_source FROM worlds.worlds WHERE name = $1")
            .bind(world.key())
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(mirror.as_deref(), Some("https://upstream.example"));
    assert_eq!(
        deployed_entity_ids(&f.pool, &world).await,
        vec!["bafymirrored".to_string()]
    );
}

#[tokio::test]
async fn publishing_an_older_entity_answers_400_and_keeps_the_newer_scene() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random());
    f.names.set(&owner.address, world.label());
    let now = chrono::Utc::now().timestamp_millis();

    let newer = deployment_at(
        &owner,
        &world,
        "Newer",
        "0,0",
        &["0,0"],
        "thumb.png",
        b"n",
        now,
    )
    .await;
    post_entity(&f.app, &newer, 200).await;
    let older = deployment_at(
        &owner,
        &world,
        "Older",
        "0,0",
        &["0,0"],
        "thumb.png",
        b"o",
        now - 10_000,
    )
    .await;
    let body = post_entity(&f.app, &older, 400).await;
    assert_eq!(
        body,
        json!({ "error": "Bad request", "message": "Deployment failed: a newer scene is already deployed on one or more of these parcels." })
    );
    // Refused before any of its files is stored (upstream `entity-deployer.ts` fast-fail).
    assert!(!f.storage.exist(&older.thumb_hash).await.unwrap());
    assert_eq!(
        deployed_entity_ids(&f.pool, &world).await,
        vec![newer.entity_id]
    );
}

async fn blocked_since(pool: &PgPool, world: &WorldName) -> Option<chrono::NaiveDateTime> {
    sqlx::query_scalar("SELECT blocked_since FROM worlds.worlds WHERE name = $1")
        .bind(world.key())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn about_refuses(f: &Fixture, world: &WorldName) -> bool {
    let about = crate::queries::world_about(&f.pool, world.key())
        .await
        .unwrap();
    let cfg = crate::Settings::for_tests("https://worlds.example.org");
    matches!(
        crate::about::build_about(&cfg, world.key(), about),
        Err(e) if e.status() == 401
    )
}

#[tokio::test]
async fn a_disabled_world_refuses_publishes_until_enabled() {
    let Some(f) = fixture().await else { return };
    let (world, owner, grantee) = (world_name(), Wallet::random(), Wallet::random());
    f.names.set(&owner.address, world.label());
    post_entity(
        &f.app,
        &deployment(&owner, &world, "Seed", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    let seeded = deployed_entity_ids(&f.pool, &world).await;

    expect(
        &f.app,
        Case::new("post", format!("/admin/worlds/{}/disable", world.key())).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    let disabled_at = blocked_since(&f.pool, &world).await;
    assert!(disabled_at.is_some());

    let refused = post_entity(
        &f.app,
        &deployment(&owner, &world, "Owner", "0,0", &["0,0"]).await,
        403,
    )
    .await;
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("is disabled and does not accept deployments"),
        "{refused}"
    );
    grant_parcels(&f.pool, &world, &grantee.address, &["3,3"]).await;
    post_entity(
        &f.app,
        &deployment(&grantee, &world, "Scoped", "3,3", &["3,3"]).await,
        403,
    )
    .await;
    assert_eq!(
        deployed_entity_ids(&f.pool, &world).await,
        seeded,
        "nothing was published"
    );
    assert_eq!(blocked_since(&f.pool, &world).await, disabled_at);
    assert!(about_refuses(&f, &world).await);

    expect(
        &f.app,
        Case::new("post", format!("/admin/worlds/{}/enable", world.key())).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    post_entity(
        &f.app,
        &deployment(&owner, &world, "Owner", "0,0", &["0,0"]).await,
        200,
    )
    .await;
}

#[tokio::test]
async fn a_disable_racing_a_publish_is_refused_inside_the_transaction() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyseed",
        &bare_scene("0,0", &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    store::admin_set_world_blocked(&f.pool, &world, true)
        .await
        .unwrap();
    let err = deploy(
        &f,
        &world,
        &owner,
        "bafynext",
        &scene_at(2000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap_err();
    assert_eq!(err.status(), 403);
    assert_eq!(deployed_entity_ids(&f.pool, &world).await, ["bafyseed"]);
}

#[tokio::test]
async fn undeploy_scene_removes_only_the_authorized_identities() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafywide",
        &bare_scene("0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    deploy(
        &f,
        &world,
        &owner,
        "bafyother",
        &bare_scene("5,5", &["5,5"]),
        &["5,5"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let at = Parcel::parse("0,0").unwrap();
    assert_eq!(
        store::undeploy_scene(&f.pool, &world, at, Some(&[]))
            .await
            .unwrap(),
        0,
        "an empty authorization set deletes nothing"
    );
    assert_eq!(entity_ids(&f, &world).await.len(), 2);
    assert_eq!(
        store::undeploy_scene(
            &f.pool,
            &world,
            at,
            Some(&[SceneId::parse("bafywide").unwrap()])
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        entity_ids(&f, &world).await,
        vec!["bafyother".to_string()],
        "the unauthorized neighbour must survive"
    );
    assert_eq!(
        store::undeploy_scene(&f.pool, &world, Parcel::parse("5,5").unwrap(), None)
            .await
            .unwrap(),
        1
    );
    assert!(entity_ids(&f, &world).await.is_empty());
}

async fn stored_spawn(f: &Fixture, world: &WorldName) -> Option<String> {
    sqlx::query_scalar("SELECT spawn_coordinates FROM worlds.worlds WHERE name = $1")
        .bind(world.key())
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

async fn set_spawn(f: &Fixture, world: &WorldName, spawn: &str) {
    sqlx::query("UPDATE worlds.worlds SET spawn_coordinates = $2 WHERE name = $1")
        .bind(world.key())
        .bind(spawn)
        .execute(&f.pool)
        .await
        .unwrap();
}

/// WCS `undeployScene`/`undeployWorld`: a spawn left outside the remaining
/// scenes' rectangle moves to its floored centre; none left clears it.
#[tokio::test]
async fn undeploys_recentre_or_clear_the_spawn() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    for (id, parcels) in [
        ("bafyspawna", &["0,0", "1,0"][..]),
        ("bafyspawnb", &["-3,-1", "-2,-1"][..]),
        ("bafyspawnc", &["5,5"][..]),
    ] {
        deploy(
            &f,
            &world,
            &owner,
            id,
            &scene_at(1000, parcels),
            parcels,
            &Deploy::Owner,
        )
        .await
        .unwrap();
    }
    set_spawn(&f, &world, "-2,-1").await;
    let version = |f: &Fixture| {
        let pool = f.pool.clone();
        let key = world.key().to_string();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT settings_version FROM worlds.worlds WHERE name = $1",
            )
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let before = version(&f).await;
    store::undeploy_scene(&f.pool, &world, Parcel::parse("5,5").unwrap(), None)
        .await
        .unwrap();
    assert_eq!(stored_spawn(&f, &world).await.as_deref(), Some("-2,-1"));
    assert_eq!(
        version(&f).await,
        before,
        "an unchanged spawn bumps nothing"
    );

    store::undeploy_scene(&f.pool, &world, Parcel::parse("0,0").unwrap(), None)
        .await
        .unwrap();
    assert_eq!(stored_spawn(&f, &world).await.as_deref(), Some("-2,-1"));
    set_spawn(&f, &world, "0,0").await;
    deploy(
        &f,
        &world,
        &owner,
        "bafyspawnd",
        &scene_at(1000, &["9,9"]),
        &["9,9"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    store::undeploy_scene(&f.pool, &world, Parcel::parse("9,9").unwrap(), None)
        .await
        .unwrap();
    assert_eq!(
        stored_spawn(&f, &world).await.as_deref(),
        Some("-3,-1"),
        "outside -3..-2 x -1: the centre, floored"
    );
    assert_eq!(version(&f).await, before + 1);

    store::undeploy_world(&f.pool, &world).await.unwrap();
    assert_eq!(stored_spawn(&f, &world).await, None);
}

async fn retired_ids(f: &Fixture, world: &WorldName) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT entity_id FROM worlds.retired_world_scenes WHERE world_name = $1 ORDER BY entity_id",
    )
    .bind(world.key())
    .fetch_all(&f.pool)
    .await
    .unwrap()
}

/// Replaced and undeployed scenes are retired, not deleted: their content
/// stays referenced and their scene comms resolve (no storage scope) until
/// the eviction removes them; active readers never see them.
#[tokio::test]
async fn replaced_and_undeployed_scenes_are_retained_until_evicted() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyretold",
        &scene_at(1000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    deploy(
        &f,
        &world,
        &owner,
        "bafyretnew",
        &scene_at(2000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    assert_eq!(deployed_entity_ids(&f.pool, &world).await, ["bafyretnew"]);
    assert_eq!(retired_ids(&f, &world).await, ["bafyretold"]);
    let old = SceneId::parse("bafyretold").unwrap();
    let resolved = rooms::world_scene(&f.pool, &world, &old)
        .await
        .unwrap()
        .expect("a just-replaced scene still resolves");
    assert!(resolved.namespace.is_none(), "no storage scope");
    assert!(
        store::active_content_keys(&f.pool)
            .await
            .unwrap()
            .contains("bafyretold")
    );

    store::undeploy_world(&f.pool, &world).await.unwrap();
    assert_eq!(retired_ids(&f, &world).await, ["bafyretnew", "bafyretold"]);
    // A re-publish of a retired entity makes it active only.
    deploy(
        &f,
        &world,
        &owner,
        "bafyretnew",
        &scene_at(2000, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    assert_eq!(retired_ids(&f, &world).await, ["bafyretold"]);

    let week = std::time::Duration::from_secs(7 * 24 * 3600);
    store::evict_retired_scenes(&f.pool, week).await.unwrap();
    assert_eq!(retired_ids(&f, &world).await, ["bafyretold"]);
    sqlx::query(
        "UPDATE worlds.retired_world_scenes SET retired_at = retired_at - interval '8 days' WHERE world_name = $1",
    )
    .bind(world.key())
    .execute(&f.pool)
    .await
    .unwrap();
    assert!(store::evict_retired_scenes(&f.pool, week).await.unwrap() >= 1);
    assert!(retired_ids(&f, &world).await.is_empty());
    assert!(
        rooms::world_scene(&f.pool, &world, &old)
            .await
            .unwrap()
            .is_none()
    );
}

/// `GET /worlds` search: full-text (stemmed), escaped ILIKE wildcards.
#[tokio::test]
async fn the_worlds_search_matches_full_text_and_escapes_wildcards() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random().address;
    let (plain, percent) = (world_name(), world_name());
    for (world, id, title) in [
        (&plain, "bafysearcha", "Moonlight festival"),
        (&percent, "bafysearchb", "Sale 100% off"),
    ] {
        let mut scene = scene_at(1000, &["0,0"]);
        scene["metadata"]["display"] = json!({ "title": title });
        deploy(&f, world, &owner, id, &scene, &["0,0"], &Deploy::Owner)
            .await
            .unwrap();
    }
    let search = |term: &str| {
        let filters = WorldsListFilters {
            authorized_deployer: Some(owner.as_str().to_string()),
            search: Some(term.to_string()),
            has_deployed_scenes: None,
            banned_names: Vec::new(),
        };
        let pool = f.pool.clone();
        async move {
            let options = WorldsListOptions {
                limit: 50,
                offset: 0,
                order_by: WorldsOrderBy::Name,
                order_direction: OrderDirection::Asc,
            };
            store::list_worlds_public(&pool, &filters, &options)
                .await
                .unwrap()
                .0
                .into_iter()
                .map(|w| w.name)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        search("festivals").await,
        [plain.key()],
        "stemmed full-text"
    );
    assert_eq!(search("%").await, [percent.key()], "a literal %");
    assert_eq!(search("0_%").await, Vec::<String>::new(), "a literal _");
    assert_eq!(search("100%").await, [percent.key()]);
}

/// `(identity, parcels, active)` of a world's storage identities, oldest first.
async fn scene_identities(f: &Fixture, world: &WorldName) -> Vec<(Uuid, Vec<String>, bool)> {
    sqlx::query_as(
        "SELECT identity, parcels, active FROM explore.world_scene_place WHERE world_name = $1 ORDER BY created_at, identity",
    )
    .bind(world.key())
    .fetch_all(&f.pool)
    .await
    .unwrap()
}

async fn scene_identity(f: &Fixture, world: &WorldName, id: &str) -> Option<Uuid> {
    rooms::world_scene(&f.pool, world, &SceneId::parse(id).unwrap())
        .await
        .unwrap()
        .and_then(|d| d.namespace)
        .map(|ns| Uuid::parse_str(&ns.scene_namespace()).unwrap())
}

/// Two concurrent publishes splitting one footprint cannot share its identity.
#[tokio::test]
async fn concurrent_publishes_serialize_scene_identity_assignment() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafy-original",
        &bare_scene("0,0", &["0,0", "1,0"]),
        &["0,0", "1,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let original = scene_identity(&f, &world, "bafy-original").await.unwrap();
    let mut left = bare_scene("0,0", &["0,0"]);
    let mut right = bare_scene("1,0", &["1,0"]);
    left["timestamp"] = json!(2000);
    right["timestamp"] = json!(2000);
    let (a, b) = tokio::join!(
        deploy(
            &f,
            &world,
            &owner,
            "bafy-left",
            &left,
            &["0,0"],
            &Deploy::Owner
        ),
        deploy(
            &f,
            &world,
            &owner,
            "bafy-right",
            &right,
            &["1,0"],
            &Deploy::Owner
        ),
    );
    a.unwrap();
    b.unwrap();
    let a = scene_identity(&f, &world, "bafy-left").await.unwrap();
    let b = scene_identity(&f, &world, "bafy-right").await.unwrap();
    assert_ne!(a, b);
    assert!(a == original || b == original);
    let identities = scene_identities(&f, &world).await;
    assert_eq!(identities.len(), 2);
    assert!(identities.iter().all(|(_, _, active)| *active));
}

/// The publish and undeploy paths keep Places' overlap rule
/// (`scene_identity`): one overlapped identity is kept (a moved base too),
/// none or several mint a new one and retire the overlapped, an undeploy
/// retires its scene's; no storage row is ever deleted.
#[tokio::test]
async fn publishes_and_undeploys_keep_a_storage_identity_per_scene() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    let timed = |ts: i64, base: &str, parcels: &[&str]| {
        let mut scene = bare_scene(base, parcels);
        scene["timestamp"] = json!(ts);
        scene
    };
    let publish = |id: &'static str, scene: Value, footprint: &'static [&'static str]| {
        let (f, world, owner) = (&f, &world, &owner);
        async move {
            deploy(f, world, owner, id, &scene, footprint, &Deploy::Owner)
                .await
                .unwrap()
        }
    };
    publish("bafya", timed(1, "0,0", &["0,0", "0,1"]), &["0,0", "0,1"]).await;
    let a = scene_identity(&f, &world, "bafya").await.unwrap();
    sqlx::query(
        "INSERT INTO worlds.world_storage (realm, scene_namespace, key, value, value_size) VALUES ($1, $2, 'k', '1', 1)",
    )
    .bind(world.key())
    .bind(a.to_string())
    .execute(&f.pool)
    .await
    .unwrap();

    // A redeploy overlapping that scene alone keeps it, its base moved.
    publish("bafya2", timed(2, "0,2", &["0,1", "0,2"]), &["0,1", "0,2"]).await;
    assert_eq!(scene_identity(&f, &world, "bafya2").await, Some(a));
    assert_eq!(
        scene_identities(&f, &world).await,
        vec![(a, vec!["0,1".to_string(), "0,2".to_string()], true)]
    );

    // A scene overlapping none gets its own.
    publish("bafyb", timed(3, "5,5", &["5,5"]), &["5,5"]).await;
    let b = scene_identity(&f, &world, "bafyb").await.unwrap();
    assert_ne!(a, b);

    // One overlapping both gets a new one and retires both.
    publish("bafyc", timed(4, "5,5", &["0,2", "5,5"]), &["0,2", "5,5"]).await;
    let c = scene_identity(&f, &world, "bafyc").await.unwrap();
    assert!(c != a && c != b);
    let states: Vec<(Uuid, bool)> = scene_identities(&f, &world)
        .await
        .into_iter()
        .map(|(id, _, active)| (id, active))
        .collect();
    assert_eq!(states, vec![(a, false), (b, false), (c, true)]);

    // A publish beside it, then an undeploy of each: every identity retires.
    publish("bafyd", timed(5, "9,9", &["9,9"]), &["9,9"]).await;
    let d = scene_identity(&f, &world, "bafyd").await.unwrap();
    store::undeploy_scene(&f.pool, &world, Parcel::parse("0,2").unwrap(), None)
        .await
        .unwrap();
    let active: Vec<Uuid> = scene_identities(&f, &world)
        .await
        .into_iter()
        .filter_map(|(id, _, active)| active.then_some(id))
        .collect();
    assert_eq!(
        active,
        vec![d],
        "the undeploy retired its scene's identity only"
    );
    store::undeploy_world(&f.pool, &world).await.unwrap();
    assert!(
        scene_identities(&f, &world)
            .await
            .iter()
            .all(|(_, _, active)| !active)
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worlds.world_storage WHERE realm = $1 AND scene_namespace = $2",
    )
    .bind(world.key())
    .bind(a.to_string())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "storage rows outlive their identity");
}

async fn store_png(f: &Fixture) -> String {
    let hash = hash_bytes_v1(&PNG_MAGIC);
    f.storage
        .store(&hash, bytes::Bytes::from_static(&PNG_MAGIC))
        .await
        .unwrap();
    hash
}

#[tokio::test]
async fn deploy_populates_settings_and_read_surfaces() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    let thumb = store_png(&f).await;
    deploy(
        &f,
        &world,
        &owner,
        "bafyentity",
        &scene_entity(&world, "My World", &thumb, "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .expect("deploy_scene");

    let settings = store::get_world_settings(&f.pool, &world)
        .await
        .unwrap()
        .expect("settings");
    assert_eq!(settings.title.as_deref(), Some("My World"));
    assert_eq!(settings.content_rating.as_deref(), Some("E"));
    assert_eq!(settings.skybox_time, Some(36000));
    assert_eq!(
        settings.categories.as_deref(),
        Some(&["art".to_string(), "game".to_string()][..])
    );
    assert!(settings.single_player);
    assert!(
        settings.show_in_places,
        "no placesConfig.optOut declared: the column's default"
    );
    assert_eq!(settings.thumbnail_hash.as_deref(), Some(thumb.as_str()));
    assert_eq!(settings.spawn_coordinates.as_deref(), Some("0,0"));
    assert_eq!(settings.settings_version, 0);
    assert_eq!(settings.access_type.as_deref(), Some("unrestricted"));

    let opts = |authorized_deployer: Option<String>, has_deployed_scenes: Option<bool>| {
        (
            WorldsListFilters {
                authorized_deployer,
                search: Some(world.key().to_string()),
                has_deployed_scenes,
                banned_names: Vec::new(),
            },
            WorldsListOptions {
                limit: 50,
                offset: 0,
                order_by: WorldsOrderBy::Name,
                order_direction: OrderDirection::Asc,
            },
        )
    };
    let (filters, options) = opts(None, None);
    let (worlds, total) = store::list_worlds_public(&f.pool, &filters, &options)
        .await
        .unwrap();
    assert_eq!(total, 1);
    let w = &worlds[0];
    assert_eq!(w.name, world.key());
    assert_eq!(w.deployed_scenes, 1);
    assert!(w.last_deployed_at.is_some());
    assert!(w.single_player);
    assert!(w.show_in_places);
    assert_eq!(
        (w.min_x, w.max_x, w.min_y, w.max_y),
        (Some(0), Some(0), Some(0), Some(1))
    );

    let (filters, options) = opts(None, Some(true));
    assert_eq!(
        store::list_worlds_public(&f.pool, &filters, &options)
            .await
            .unwrap()
            .0
            .len(),
        1
    );
    let (filters, options) = opts(None, Some(false));
    assert!(
        store::list_worlds_public(&f.pool, &filters, &options)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    let (filters, options) = opts(Some(owner.as_str().to_string()), None);
    assert_eq!(
        store::list_worlds_public(&f.pool, &filters, &options)
            .await
            .unwrap()
            .0
            .len(),
        1
    );
    let (filters, options) = opts(
        Some("0x2222222222222222222222222222222222222222".into()),
        None,
    );
    assert!(
        store::list_worlds_public(&f.pool, &filters, &options)
            .await
            .unwrap()
            .0
            .is_empty()
    );

    let manifest = store::world_manifest(&f.pool, &world)
        .await
        .unwrap()
        .expect("manifest");
    assert_eq!(manifest.total, 2);
    assert_eq!(manifest.parcels, vec!["0,0".to_string(), "0,1".to_string()]);
    assert_eq!(manifest.spawn_coordinates.as_deref(), Some("0,0"));

    let body = expect(
        &f.app,
        Case::new("get", format!("/worlds?search={}", world.key())),
        200,
    )
    .await;
    assert_eq!(body["total"], 1);
    assert_eq!(
        body["worlds"][0]["shape"],
        json!({ "x1": 0, "x2": 0, "y1": 0, "y2": 1 })
    );
}

#[tokio::test]
async fn redeploy_refreshes_settings_only_when_scene_ends_up_sole_occupant() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyfirst",
        &scene_entity(&world, "First Title", "bafymissing", "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    deploy(
        &f,
        &world,
        &owner,
        "bafyaside",
        &scene_entity(&world, "Aside Title", "bafymissing", "5,5", &["5,5"]),
        &["5,5"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let settings = store::get_world_settings(&f.pool, &world)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        settings.title.as_deref(),
        Some("First Title"),
        "a deploy leaving another scene in place must not refresh settings"
    );
    assert_eq!(settings.settings_version, 0);

    deploy(
        &f,
        &world,
        &owner,
        "bafytakeover",
        &scene_entity(
            &world,
            "Takeover Title",
            "bafymissing",
            "0,0",
            &["0,0", "0,1", "5,5"],
        ),
        &["0,0", "0,1", "5,5"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let settings = store::get_world_settings(&f.pool, &world)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        settings.title.as_deref(),
        Some("Takeover Title"),
        "a deploy replacing every deployed scene must refresh settings"
    );
    assert_eq!(settings.settings_version, 1);
}

#[tokio::test]
async fn redeploy_preserves_owner_settings_the_scene_does_not_express() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyseed",
        &scene_entity(&world, "Seed Title", "bafymissing", "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let bare = json!({ "type": "scene", "timestamp": 2000, "pointers": ["0,0", "0,1"], "metadata": { "scene": { "base": "0,0", "parcels": ["0,0", "0,1"] } } });
    deploy(
        &f,
        &world,
        &owner,
        "bafybare",
        &bare,
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let settings = store::get_world_settings(&f.pool, &world)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        settings.title.as_deref(),
        Some("Seed Title"),
        "a field the redeploying scene does not declare must keep its stored value"
    );
    assert_eq!(settings.content_rating.as_deref(), Some("E"));
    assert!(settings.single_player);
    assert_eq!(
        settings.settings_version, 0,
        "a refresh that changes nothing must not bump the settings version"
    );
}

#[tokio::test]
async fn deploy_rejects_out_of_policy_scene_metadata() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    let svg = bytes::Bytes::from_static(b"<svg xmlns=\"a\"/>");
    let not_image = hash_bytes_v1(&svg);
    f.storage.store(&not_image, svg).await.unwrap();
    let entity = json!({
        "type": "scene",
        "timestamp": 1000,
        "pointers": ["0,0"],
        "content": [{ "file": "thumb.png", "hash": not_image }],
        "metadata": {
            "display": { "title": "ab", "description": "xy", "navmapThumbnail": "thumb.png" },
            "worldConfiguration": { "name": world.display(), "skyboxConfig": { "fixedTime": 99999999999i64 } },
            "scene": { "base": "0,0", "parcels": ["0,0"] },
            "tags": ["ok", 7],
            "rating": "XXX"
        }
    });
    deploy(
        &f,
        &world,
        &owner,
        "bafypolicy",
        &entity,
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let settings = store::get_world_settings(&f.pool, &world)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings.title, None, "a 2-char title is out of policy");
    assert_eq!(settings.description, None);
    assert_eq!(settings.content_rating, None);
    assert_eq!(
        settings.skybox_time, None,
        "an over-i32 fixedTime must not wrap"
    );
    assert_eq!(
        settings.categories, None,
        "a non-string tag rejects the whole array"
    );
    assert!(
        !settings.single_player,
        "no fixedAdapter declared: the column's default"
    );
    assert!(settings.show_in_places);
    assert_eq!(
        settings.thumbnail_hash, None,
        "a non-image navmapThumbnail must not be promoted into settings"
    );
}

#[tokio::test]
async fn settings_version_tracks_settings_writes() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    let version = || async {
        store::get_world_settings(&f.pool, &world)
            .await
            .unwrap()
            .unwrap()
            .settings_version
    };
    let fp = ["0,0", "0,1"];
    deploy(
        &f,
        &world,
        &owner,
        "bafyver1",
        &scene_entity(&world, "Version World", "bafymissing", "0,0", &fp),
        &fp,
        &Deploy::Owner,
    )
    .await
    .unwrap();
    assert_eq!(version().await, 0, "first deploy starts at the default");
    deploy(
        &f,
        &world,
        &owner,
        "bafyver2",
        &scene_entity(&world, "Version World", "bafymissing", "0,0", &fp),
        &fp,
        &Deploy::Owner,
    )
    .await
    .unwrap();
    assert_eq!(version().await, 0, "an unchanged republish must not bump");
    deploy(
        &f,
        &world,
        &owner,
        "bafyver3",
        &scene_entity(&world, "Version World Renamed", "bafymissing", "0,0", &fp),
        &fp,
        &Deploy::Owner,
    )
    .await
    .unwrap();
    assert_eq!(version().await, 1, "a settings-changing redeploy bumps");

    let (after_put, _) = store::update_world_settings(
        &f.pool,
        &world,
        &owner,
        Some(&owner),
        &WorldSettingsUpdate {
            title: Some("Owner Title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        after_put.settings_version, 2,
        "PUT with a settings field bumps"
    );
    let (after_spawn, _) = store::update_world_settings(
        &f.pool,
        &world,
        &owner,
        Some(&owner),
        &WorldSettingsUpdate {
            spawn_coordinates: Some("0,1".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        after_spawn.settings_version, 2,
        "spawn coordinates are not a versioned settings column"
    );
    store::store_access_for_owner(&f.pool, &world, &owner, &AccessSetting::Unrestricted)
        .await
        .unwrap();
    assert_eq!(version().await, 3, "an access change bumps");
}

#[tokio::test]
async fn settings_spawn_validation_runs_inside_the_update_transaction() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random().address;
    let spawn_update = |spawn: &str| WorldSettingsUpdate {
        spawn_coordinates: Some(spawn.into()),
        ..Default::default()
    };
    let bad_request = |err: super::http::ApiError| {
        assert_eq!(err.status(), 400, "{err:?}");
        err.public_message()
    };

    let spawnless = world_name();
    let err = store::update_world_settings(
        &f.pool,
        &spawnless,
        &owner,
        Some(&owner),
        &spawn_update("0,0"),
    )
    .await
    .expect_err("a world without scenes cannot take a spawn");
    assert!(bad_request(err).contains("has no deployed scenes"));
    assert!(
        store::get_world(&f.pool, &spawnless)
            .await
            .unwrap()
            .is_none(),
        "the row materialized for the lock rolls back with the failed validation"
    );

    let world = world_name();
    deploy(
        &f,
        &world,
        &owner,
        "bafyspawn",
        &scene_entity(&world, "Spawn World", "bafymissing", "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let err =
        store::update_world_settings(&f.pool, &world, &owner, Some(&owner), &spawn_update("5,5"))
            .await
            .expect_err("a spawn outside the world shape is refused");
    assert!(bad_request(err).contains("must be within the world shape rectangle"));
    assert_eq!(
        store::get_world_settings(&f.pool, &world)
            .await
            .unwrap()
            .unwrap()
            .spawn_coordinates
            .as_deref(),
        Some("0,0")
    );
    let (updated, old_spawn) =
        store::update_world_settings(&f.pool, &world, &owner, Some(&owner), &spawn_update("0,1"))
            .await
            .expect("in-shape spawn");
    assert_eq!(updated.spawn_coordinates.as_deref(), Some("0,1"));
    assert_eq!(old_spawn.as_deref(), Some("0,0"));
}

#[tokio::test]
async fn permission_parcels_lifecycle() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random().address;
    let deployer = Wallet::random().address;
    let streamer = Wallet::random().address;
    store::ensure_world(&f.pool, &world, &owner).await.unwrap();

    let added = store::grant_addresses_world_wide_permission(
        &f.pool,
        &world,
        Permission::Deployment,
        std::slice::from_ref(&deployer),
        None,
    )
    .await
    .unwrap();
    assert_eq!(added, vec![deployer.as_str().to_string()]);
    assert!(
        f.state
            .policy
            .has_world_wide_permission(&world, Permission::Deployment, &deployer)
            .await
            .unwrap()
    );

    let created = store::add_parcels_to_permission(
        &f.pool,
        &world,
        Permission::Streaming,
        &streamer,
        &parcels(&["00,00", "1,1"]),
    )
    .await
    .unwrap();
    assert!(created);

    let records = store::permission_records(&f.pool, &world).await.unwrap();
    let dep = records
        .iter()
        .find(|r| r.permission == "deployment")
        .unwrap();
    assert!(dep.is_world_wide);
    assert_eq!(dep.parcel_count, 0);
    let strm = records
        .iter()
        .find(|r| r.permission == "streaming")
        .unwrap();
    assert!(!strm.is_world_wide);
    assert_eq!(strm.parcel_count, 2);
    assert!(
        !f.state
            .policy
            .has_world_wide_permission(&world, Permission::Streaming, &streamer)
            .await
            .unwrap()
    );

    let (total, listed) = store::parcels_for_permission(
        &f.pool,
        &world,
        Permission::Streaming,
        &streamer,
        100,
        0,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(total, 2);
    assert_eq!(listed, vec!["0,0".to_string(), "1,1".to_string()]);
    let (_, boxed) = store::parcels_for_permission(
        &f.pool,
        &world,
        Permission::Streaming,
        &streamer,
        100,
        0,
        Some((0, 0, 0, 0)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(boxed, vec!["0,0".to_string()]);

    let (atot, addrs) = store::addresses_for_parcel_permission(
        &f.pool,
        &world,
        Permission::Streaming,
        &parcels(&["0,0"]),
        100,
        0,
    )
    .await
    .unwrap();
    assert_eq!(atot, 1);
    assert_eq!(addrs, vec![streamer.as_str().to_string()]);

    assert!(
        store::remove_parcels_from_permission(
            &f.pool,
            &world,
            Permission::Streaming,
            &streamer,
            &parcels(&["0,0"])
        )
        .await
        .unwrap()
        .is_some()
    );
    let records = store::permission_records(&f.pool, &world).await.unwrap();
    let strm = records
        .iter()
        .find(|r| r.permission == "streaming")
        .unwrap();
    assert!(!strm.is_world_wide);
    assert_eq!(strm.parcel_count, 1);

    // Removing the last parcel drops the grant instead of widening it.
    assert!(
        store::remove_parcels_from_permission(
            &f.pool,
            &world,
            Permission::Streaming,
            &streamer,
            &parcels(&["1,1"])
        )
        .await
        .unwrap()
        .is_some()
    );
    let records = store::permission_records(&f.pool, &world).await.unwrap();
    assert!(records.iter().all(|r| r.permission != "streaming"));
    assert!(
        !f.state
            .policy
            .has_world_wide_permission(&world, Permission::Streaming, &streamer)
            .await
            .unwrap()
    );
    assert!(
        !store::remove_parcels_from_permission(
            &f.pool,
            &world,
            Permission::Streaming,
            &streamer,
            &parcels(&["1,1"])
        )
        .await
        .unwrap()
        .is_some()
    );

    // A world-wide grant is untouched by removing parcels it never had.
    assert!(
        store::remove_parcels_from_permission(
            &f.pool,
            &world,
            Permission::Deployment,
            &deployer,
            &parcels(&["5,5"])
        )
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        f.state
            .policy
            .has_world_wide_permission(&world, Permission::Deployment, &deployer)
            .await
            .unwrap()
    );

    let removed = store::remove_addresses_permission(
        &f.pool,
        &world,
        Permission::Deployment,
        std::slice::from_ref(&deployer),
    )
    .await
    .unwrap();
    assert_eq!(removed, vec![deployer.as_str().to_string()]);
}

#[tokio::test]
async fn single_statement_permission_routes_keep_their_row_shapes() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random().address;
    let (a, b, c) = (
        Wallet::random().address,
        Wallet::random().address,
        Wallet::random().address,
    );

    assert!(store::get_world(&f.pool, &world).await.unwrap().is_none());
    assert!(
        store::permission_records(&f.pool, &world)
            .await
            .unwrap()
            .is_empty()
    );

    store::replace_world_wide_permission(
        &f.pool,
        &world,
        &owner,
        Permission::Deployment,
        &[a.clone(), b.clone()],
    )
    .await
    .unwrap();
    let added = store::grant_addresses_world_wide_permission(
        &f.pool,
        &world,
        Permission::Streaming,
        std::slice::from_ref(&c),
        None,
    )
    .await
    .unwrap();
    assert_eq!(added, vec![c.as_str().to_string()]);
    let added = store::grant_addresses_world_wide_permission(
        &f.pool,
        &world,
        Permission::Streaming,
        std::slice::from_ref(&c),
        None,
    )
    .await
    .unwrap();
    assert!(added.is_empty(), "a repeat grant adds nothing");

    assert_eq!(
        store::get_world(&f.pool, &world)
            .await
            .unwrap()
            .unwrap()
            .owner
            .as_ref(),
        Some(&owner)
    );
    assert_eq!(
        store::permission_records(&f.pool, &world)
            .await
            .unwrap()
            .len(),
        3
    );

    store::replace_world_wide_permission(
        &f.pool,
        &world,
        &owner,
        Permission::Deployment,
        &[a.clone(), c.clone()],
    )
    .await
    .unwrap();
    let records = store::permission_records(&f.pool, &world).await.unwrap();
    let deployment: Vec<&str> = records
        .iter()
        .filter(|r| r.permission == "deployment")
        .map(|r| r.address.as_str())
        .collect();
    let mut expected = vec![a.as_str(), c.as_str()];
    expected.sort();
    assert_eq!(deployment, expected);

    assert!(
        store::add_parcels_to_permission(
            &f.pool,
            &world,
            Permission::Deployment,
            &b,
            &parcels(&["00,00", "2,3"])
        )
        .await
        .unwrap()
    );
    assert!(
        !store::add_parcels_to_permission(
            &f.pool,
            &world,
            Permission::Deployment,
            &b,
            &parcels(&["2,3", "5,5"])
        )
        .await
        .unwrap()
    );
    let (total, page) =
        store::parcels_for_permission(&f.pool, &world, Permission::Deployment, &b, 2, 0, None)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        (total, page),
        (3, vec!["0,0".to_string(), "2,3".to_string()])
    );
    let (total, page) =
        store::parcels_for_permission(&f.pool, &world, Permission::Deployment, &b, 10, 10, None)
            .await
            .unwrap()
            .unwrap();
    assert_eq!((total, page), (3, Vec::new()));
    let (total, page) = store::parcels_for_permission(
        &f.pool,
        &world,
        Permission::Deployment,
        &b,
        10,
        0,
        Some((5, 5, 2, 3)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        (total, page),
        (2, vec!["2,3".to_string(), "5,5".to_string()])
    );
    assert!(
        store::parcels_for_permission(&f.pool, &world, Permission::Streaming, &b, 10, 0, None)
            .await
            .unwrap()
            .is_none()
    );

    assert!(
        f.state
            .policy
            .has_permission_covering(&world, Permission::Deployment, &a, &parcels(&["9,9"]))
            .await
            .unwrap()
    );
    assert!(
        f.state
            .policy
            .has_permission_covering(
                &world,
                Permission::Deployment,
                &b,
                &parcels(&["0,0", "5,5"])
            )
            .await
            .unwrap()
    );
    assert!(
        !f.state
            .policy
            .has_permission_covering(
                &world,
                Permission::Deployment,
                &b,
                &parcels(&["0,0", "9,9"])
            )
            .await
            .unwrap()
    );
    assert!(
        !f.state
            .policy
            .has_permission_covering(&world, Permission::Deployment, &owner, &parcels(&["0,0"]))
            .await
            .unwrap()
    );

    let (atot, addrs) = store::addresses_for_parcel_permission(
        &f.pool,
        &world,
        Permission::Deployment,
        &parcels(&["0,0"]),
        1,
        0,
    )
    .await
    .unwrap();
    assert_eq!(atot, 3);
    assert_eq!(addrs.len(), 1);
    let (atot, addrs) = store::addresses_for_parcel_permission(
        &f.pool,
        &world,
        Permission::Deployment,
        &parcels(&["0,0"]),
        5,
        9,
    )
    .await
    .unwrap();
    assert_eq!((atot, addrs), (3, Vec::new()));

    assert!(
        store::remove_parcels_from_permission(
            &f.pool,
            &world,
            Permission::Deployment,
            &b,
            &parcels(&["0,0"])
        )
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        !store::remove_parcels_from_permission(
            &f.pool,
            &world,
            Permission::Streaming,
            &b,
            &parcels(&["0,0"])
        )
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        !store::remove_parcels_from_permission(
            &f.pool,
            &world,
            Permission::Streaming,
            &b,
            &ParcelSet::new()
        )
        .await
        .unwrap()
        .is_some()
    );
    let (total, _) =
        store::parcels_for_permission(&f.pool, &world, Permission::Deployment, &b, 10, 0, None)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(total, 2);

    assert!(
        store::world_manifest(&f.pool, &world)
            .await
            .unwrap()
            .is_none(),
        "a world without a deployed scene has no manifest"
    );
}

/// Seeds the name deny list and empties it again on drop, so a failed
/// assertion leaves no entry behind.
struct DenyListed<'a>(&'a super::names::NameDenyList);

impl<'a> DenyListed<'a> {
    fn seed(names: &'a super::names::NameDenyList, entries: Vec<String>) -> Self {
        names.seed(entries);
        Self(names)
    }
}

impl Drop for DenyListed<'_> {
    fn drop(&mut self) {
        self.0.seed(Vec::new());
    }
}

#[derive(Default)]
struct RecordedRevocations(parking_lot::Mutex<Vec<super::Revocation>>);

#[async_trait::async_trait]
impl super::RevocationSink for RecordedRevocations {
    async fn revoke(&self, revocation: &super::Revocation) {
        self.0.lock().push(revocation.clone());
    }
}

impl RecordedRevocations {
    fn take(&self) -> Vec<super::Revocation> {
        std::mem::take(&mut *self.0.lock())
    }
}

async fn seed_community(pool: &PgPool, members: &[&Address]) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO social.communities (id, name, owner_address) VALUES ($1, 'c', $2)")
        .bind(id)
        .bind(Wallet::random().address)
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
    id
}

async fn leave_community(pool: &PgPool, community: Uuid, member: &Address) {
    sqlx::query(
        "DELETE FROM social.community_members WHERE community_id = $1 AND member_address = $2",
    )
    .bind(community)
    .bind(member)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn community_allow_lists_require_membership_and_revoke_on_exit() {
    use super::Revocation;
    use crate::policy::{Access, AccessGrant};
    let Some(f) = fixture().await else { return };
    let recorded = Arc::new(RecordedRevocations::default());
    f.state.add_revocation_sink(recorded.clone());
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let (member, listed, granted, stranger) = (
        Wallet::random().address,
        Wallet::random().address,
        Wallet::random().address,
        Wallet::random().address,
    );
    let joined = seed_community(&f.pool, &[&owner.address, &member, &listed, &granted]).await;
    let other = seed_community(&f.pool, &[&owner.address, &member]).await;
    let foreign = seed_community(&f.pool, &[&member]).await;
    let w = world.display();
    let access_path = format!("/world/{w}/permissions/access");
    let community_path = |c: Uuid| format!("/world/{w}/permissions/access/communities/{c}");
    let not_member = format!("You are not a member of the following communities: {foreign}.");

    // Adding a community the signer is not a member of is refused on both routes.
    let meta = |communities: Vec<String>| {
        json!({ "type": "allow-list", "wallets": [listed.as_str(), granted.as_str()], "communities": communities })
            .to_string()
    };
    let body = expect(
        &f.app,
        Case::new("post", &access_path)
            .signed_meta(&owner, &meta(vec![joined.to_string(), foreign.to_string()]))
            .await,
        400,
    )
    .await;
    assert_eq!(body["message"], not_member);
    expect(
        &f.app,
        Case::new("post", &access_path)
            .signed_meta(&owner, &meta(vec![joined.to_string()]))
            .await,
        204,
    )
    .await;
    let body = expect(
        &f.app,
        Case::new("put", community_path(foreign))
            .signed(&owner)
            .await,
        400,
    )
    .await;
    assert_eq!(body["message"], not_member);
    expect(
        &f.app,
        Case::new("put", community_path(other)).signed(&owner).await,
        204,
    )
    .await;
    recorded.take();

    // Admission: a member by community, the listed wallet once, by its grant.
    let access = async |who: &Address| {
        let now = chrono::Utc::now().naive_utc();
        f.state
            .policy
            .can_access(&world, who, None, None, None, now)
            .await
            .unwrap()
    };
    assert_eq!(
        access(&member).await,
        Access::Allowed(AccessGrant::Community)
    );
    assert_eq!(
        access(&listed).await,
        Access::Allowed(AccessGrant::AllowListed)
    );
    assert_eq!(access(&stranger).await, Access::NotAllowed);

    // Removing a wallet grant keeps a wallet a listed community still admits.
    sqlx::query("INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size) VALUES ($1, 'bafyc', '[]', '{}', 'owner', ARRAY['0,0'], 0)")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();
    expect(
        &f.app,
        Case::new("delete", format!("/world/{w}/permissions/access/{granted}"))
            .signed(&owner)
            .await,
        204,
    )
    .await;
    assert_eq!(recorded.take(), vec![]);
    assert_eq!(
        access(&granted).await,
        Access::Allowed(AccessGrant::Community)
    );

    // Leaving re-decides: the owner, the listed wallet and a member of
    // another listed community stay; a community no world lists kicks nobody.
    for who in [&owner.address, &listed, &member] {
        leave_community(&f.pool, joined, who).await;
        f.state.community_member_removed(joined, who).await;
    }
    f.state.community_member_removed(foreign, &member).await;
    assert_eq!(recorded.take(), vec![]);
    leave_community(&f.pool, other, &member).await;
    f.state.community_member_removed(other, &member).await;
    assert_eq!(
        recorded.take(),
        vec![Revocation::Address {
            world: world.clone(),
            address: member.clone()
        }]
    );

    // Deleting a community, or taking it off the list, re-decides everyone.
    let before: (uuid::Uuid, chrono::NaiveDateTime) = sqlx::query_as(
        "SELECT revision,changed_at FROM social.world_access_rechecks WHERE world_name=$1",
    )
    .bind(world.key())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE social.world_access_rechecks SET next_attempt_at=timezone('UTC',now())+interval '1 day' WHERE world_name=$1")
        .bind(world.key()).execute(&f.pool).await.unwrap();
    f.state.community_closed(joined).await;
    let after: (uuid::Uuid, chrono::NaiveDateTime, bool) = sqlx::query_as("SELECT revision,changed_at,next_attempt_at<=timezone('UTC',now()) FROM social.world_access_rechecks WHERE world_name=$1")
        .bind(world.key()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        (after.0, after.1),
        before,
        "community wake preserves edit ordering"
    );
    assert!(
        after.2,
        "community closure must persist a due retry before its wakeup"
    );
    assert_eq!(
        recorded.take(),
        vec![Revocation::AccessSetting {
            world: world.clone()
        }]
    );
    // A completed check parks the row at infinity (comms `complete_recheck`).
    // A closure makes it due again. The revision and the cutoff do not change.
    sqlx::query(
        "UPDATE social.world_access_rechecks SET next_attempt_at='infinity' WHERE world_name=$1",
    )
    .bind(world.key())
    .execute(&f.pool)
    .await
    .unwrap();
    f.state.community_closed(joined).await;
    let reopened: (uuid::Uuid, chrono::NaiveDateTime, bool) = sqlx::query_as("SELECT revision,changed_at,next_attempt_at<=timezone('UTC',now()) FROM social.world_access_rechecks WHERE world_name=$1")
        .bind(world.key()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        (reopened.0, reopened.1),
        before,
        "a closure is not a new edit"
    );
    assert!(reopened.2, "a closure reopens a completed check");
    assert_eq!(
        recorded.take(),
        vec![Revocation::AccessSetting {
            world: world.clone()
        }]
    );
    expect(
        &f.app,
        Case::new("delete", community_path(other))
            .signed(&owner)
            .await,
        204,
    )
    .await;
    assert_eq!(
        recorded.take(),
        vec![Revocation::AccessSetting {
            world: world.clone()
        }]
    );
}

/// A community exit reaches comms' LiveKit rooms: the wallet nothing else
/// admits is removed from the world's live room, a wallet grant keeps one.
#[tokio::test]
async fn a_community_exit_evicts_from_the_livekit_rooms() {
    use crate::comms::gatekeeper::{CommsState, Settings as CommsSettings};
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let room = crate::comms::rooms::worlds_service_scene_room(
        &world,
        &SceneId::parse("bafylivekit").unwrap(),
    );
    let requests = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
    let capture = requests.clone();
    let listed = room.clone();
    let fixture = Router::new().route(
        "/twirp/livekit.RoomService/{method}",
        axum::routing::post(
            move |axum::extract::Path(method): axum::extract::Path<String>,
                  axum::Json(body): axum::Json<Value>| {
                let (capture, listed) = (capture.clone(), listed.clone());
                async move {
                    capture.lock().unwrap().push((method.clone(), body));
                    axum::Json(match method.as_str() {
                        "ListRooms" => json!({ "rooms": [{ "name": listed, "sid": "RM_a", "metadata": "{}" }] }),
                        _ => json!({}),
                    })
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut settings = CommsSettings::for_tests();
    settings.livekit.api_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let comms = Arc::new(CommsState::new(
        f.pool.clone(),
        crate::http::Outbound::for_tests(),
        settings,
        None,
        None,
        f.state.policy.clone(),
        Arc::new(rooms::SceneResolver::new(f.pool.clone())),
        Arc::new(crate::comms::presence::Presence::new()),
        Arc::new(crate::comms::interfaces::test_support::StubDirectory::default()),
    ));
    f.state
        .add_revocation_sink(Arc::new(super::CommsRoomRevocations(comms)));
    sqlx::query("INSERT INTO worlds.worlds (name, access_type) VALUES ($1, 'allow-list')")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();
    let (member, granted) = (Wallet::random().address, Wallet::random().address);
    sqlx::query("INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'access', $2)")
        .bind(world.key())
        .bind(&granted)
        .execute(&f.pool)
        .await
        .unwrap();
    let community = seed_community(&f.pool, &[&member, &granted]).await;
    sqlx::query(
        "INSERT INTO worlds.world_access_communities (world_name, community_id) VALUES ($1, $2)",
    )
    .bind(world.key())
    .bind(community)
    .execute(&f.pool)
    .await
    .unwrap();

    for who in [&granted, &member] {
        leave_community(&f.pool, community, who).await;
        f.state.community_member_removed(community, who).await;
    }
    let removed = |who: &Address| {
        requests.lock().unwrap().iter().any(|(method, body)| {
            method == "RemoveParticipant"
                && body["room"] == room
                && body["identity"] == who.as_str()
        })
    };
    let evicted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !removed(&member) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(evicted.is_ok(), "{:?}", requests.lock().unwrap());
    assert!(!removed(&granted), "the wallet grant still admits it");
    server.abort();
}

#[tokio::test]
async fn a_community_closure_rechecks_livekit_when_its_queue_insert_fails() {
    use futures::FutureExt;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    let Ok(url) = std::env::var("CATALYST_TEST_DATABASE_URL") else {
        return;
    };
    let opts: PgConnectOptions = url.parse().unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(opts.clone().database("postgres"))
        .await
        .unwrap();
    let name = format!("world_closure_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&admin)
        .await
        .unwrap();
    // Fault-injection DDL must never lock or alter the shared scratch database.
    // Clean up the private database even if setup or an assertion panics.
    let outcome = std::panic::AssertUnwindSafe(async {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(opts.database(&name))
            .await
            .unwrap();
        crate::db::apply_schema(&pool).await.unwrap();
        let f = fixture_on_pool(
            pool,
            PersonalWorldsPolicy::disabled(),
            None,
            None,
            test_settings(),
            "",
        )
        .await;
        community_closure_with_failed_queue_insert(f).await;
    })
    .catch_unwind()
    .await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE \"{name}\" WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
    admin.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn community_closure_with_failed_queue_insert(f: Fixture) {
    use crate::comms::gatekeeper::{CommsState, Settings as CommsSettings};
    let world = world_name();
    let room = format!("world-{}", world.key());
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let (member, granted) = (Wallet::random().address, Wallet::random().address);
    let roster = json!({ "participants": [
        { "identity": granted.as_str() }, { "identity": member.as_str() }
    ] });
    let recorded = Arc::new(RecordedRevocations::default());
    f.state.add_revocation_sink(recorded.clone());
    let requests = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
    let capture = requests.clone();
    let listed = room.clone();
    let fixture = Router::new().route(
        "/twirp/livekit.RoomService/{method}",
        axum::routing::post(
            move |axum::extract::Path(method): axum::extract::Path<String>,
                  axum::Json(body): axum::Json<Value>| {
                let (capture, listed, roster) = (capture.clone(), listed.clone(), roster.clone());
                async move {
                    capture.lock().unwrap().push((method.clone(), body));
                    axum::Json(match method.as_str() {
                        "ListRooms" => json!({ "rooms": [{ "name": listed, "sid": "RM_a", "metadata": "{}" }] }),
                        "ListParticipants" => roster,
                        _ => json!({}),
                    })
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut settings = CommsSettings::for_tests();
    settings.livekit.api_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let comms = Arc::new(CommsState::new(
        f.pool.clone(),
        crate::http::Outbound::for_tests(),
        settings,
        None,
        None,
        f.state.policy.clone(),
        Arc::new(rooms::SceneResolver::new(f.pool.clone())),
        Arc::new(crate::comms::presence::Presence::new()),
        Arc::new(crate::comms::interfaces::test_support::StubDirectory::default()),
    ));
    f.state
        .add_revocation_sink(Arc::new(super::CommsRoomRevocations(comms)));
    sqlx::query("INSERT INTO worlds.worlds (name, access_type) VALUES ($1, 'allow-list')")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'access', $2)")
        .bind(world.key())
        .bind(&granted)
        .execute(&f.pool)
        .await
        .unwrap();
    let community = seed_community(&f.pool, &[&member, &granted]).await;
    sqlx::query(
        "INSERT INTO worlds.world_access_communities (world_name, community_id) VALUES ($1, $2)",
    )
    .bind(world.key())
    .bind(community)
    .execute(&f.pool)
    .await
    .unwrap();

    for who in [&granted, &member] {
        leave_community(&f.pool, community, who).await;
    }
    // Fail only this world's queue writes; policy and community rows remain readable.
    assert!(
        world
            .key()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    );
    let constraint = format!("closure_failure_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE social.world_access_rechecks ADD CONSTRAINT {constraint} CHECK (world_name <> '{}') NOT VALID",
        world.key()
    )))
    .execute(&f.pool)
    .await
    .unwrap();
    f.state.community_closed(community).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE social.world_access_rechecks DROP CONSTRAINT {constraint}"
    )))
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        recorded.take(),
        vec![super::Revocation::AccessRecheck {
            world: world.clone()
        }]
    );
    let queued: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM social.world_access_rechecks WHERE world_name=$1)",
    )
    .bind(world.key())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        !queued,
        "the advisory must not fabricate a durable intent or cutoff"
    );
    let removed = |who: &Address| {
        requests.lock().unwrap().iter().any(|(method, body)| {
            method == "RemoveParticipant"
                && body["room"] == room
                && body["identity"] == who.as_str()
        })
    };
    let evicted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !removed(&member) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(evicted.is_ok(), "{:?}", requests.lock().unwrap());
    assert!(!removed(&granted), "the wallet grant still admits it");
    server.abort();
}

#[tokio::test]
async fn allow_list_edits_are_guarded_updates() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random().address;
    let (abc, def) = (Wallet::random().address, Wallet::random().address);
    let (c1, c2) = (Uuid::new_v4(), Uuid::new_v4());
    let caps = store::AccessCaps::default();
    let version = || async {
        store::get_world_settings(&f.pool, &world)
            .await
            .unwrap()
            .unwrap()
            .settings_version
    };

    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddWallet(&abc)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::NotAllowList
    );
    store::store_access_for_owner(&f.pool, &world, &owner, &AccessSetting::Unrestricted)
        .await
        .unwrap();
    assert_eq!(version().await, 1);
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddWallet(&abc)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::NotAllowList
    );

    store::store_access_for_owner(
        &f.pool,
        &world,
        &owner,
        &AccessSetting::AllowList {
            wallets: vec![abc.as_str().to_uppercase()],
            communities: vec![],
        },
    )
    .await
    .unwrap();
    assert_eq!(version().await, 2);
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddWallet(&abc)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied,
        "presence still counts as a write"
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddWallet(&def)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddCommunity(c1)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddCommunity(c2)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::RemoveWallet(&abc)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::RemoveCommunity(c1)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    assert_eq!(version().await, 8);
    match store::get_world(&f.pool, &world)
        .await
        .unwrap()
        .unwrap()
        .access
    {
        AccessSetting::AllowList {
            wallets,
            communities,
        } => {
            assert_eq!(wallets, vec![def.as_str().to_string()]);
            assert_eq!(communities, vec![c2.to_string()]);
        }
        other => panic!("unexpected access {other:?}"),
    }

    // Caps: the wallet list refuses its 1001st entry, communities their 51st.
    let many: Vec<String> = (0..999).map(|i| format!("0x{i:040x}")).collect();
    let communities: Vec<String> = (0..50).map(|_| Uuid::new_v4().to_string()).collect();
    store::store_access_for_owner(
        &f.pool,
        &world,
        &owner,
        &AccessSetting::AllowList {
            wallets: many,
            communities: communities.clone(),
        },
    )
    .await
    .unwrap();
    let last = Wallet::random().address;
    let too_many = Wallet::random().address;
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddWallet(&last)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddWallet(&too_many)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::CapExceeded
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddWallet(&last)
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddCommunity(Uuid::new_v4())
        )
        .await
        .unwrap(),
        AllowListEditOutcome::CapExceeded
    );
    assert_eq!(
        store::modify_allow_list_access(
            &f.pool,
            &world,
            &owner,
            caps,
            AllowListEdit::AddCommunity(Uuid::parse_str(&communities[7]).unwrap())
        )
        .await
        .unwrap(),
        AllowListEditOutcome::Applied
    );
    // The policy reads the same rows: a listed wallet enters, a stranger does not.
    let now = chrono::Utc::now().naive_utc();
    assert_eq!(
        f.state
            .policy
            .can_access(&world, &last, None, None, None, now)
            .await
            .unwrap(),
        crate::policy::Access::Allowed(crate::policy::AccessGrant::AllowListed)
    );
    assert_eq!(
        f.state
            .policy
            .can_access(&world, &too_many, None, None, None, now)
            .await
            .unwrap(),
        crate::policy::Access::NotAllowed
    );
    // A type change drops the list: back to allow-list, nobody is listed.
    store::store_access_for_owner(&f.pool, &world, &owner, &AccessSetting::Unrestricted)
        .await
        .unwrap();
    store::store_access_for_owner(
        &f.pool,
        &world,
        &owner,
        &AccessSetting::AllowList {
            wallets: vec![],
            communities: vec![],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        f.state
            .policy
            .can_access(&world, &last, None, None, None, now)
            .await
            .unwrap(),
        crate::policy::Access::NotAllowed
    );
}

#[tokio::test]
async fn active_content_keys_names_every_referenced_blob_and_fails_closed_on_an_unreadable_entity()
{
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyoldscene",
        &scene_entity(&world, "Old", "bafyoldthumb", "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    // Same timestamp: the greater entity id wins the tie.
    deploy(
        &f,
        &world,
        &owner,
        "bafyupdatedscene",
        &scene_entity(&world, "New", "bafynewthumb", "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE worlds.worlds SET thumbnail_hash = 'aabbccdd' WHERE name = $1")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();

    let keys = store::active_content_keys(&f.pool)
        .await
        .expect("active keys");
    for expected in ["bafyupdatedscene", "bafynewthumb", "aabbccdd"] {
        assert!(keys.contains(expected), "missing active key {expected}");
    }
    for replaced in ["bafyoldscene", "bafyoldthumb"] {
        assert!(keys.contains(replaced), "retired scene dropped: {replaced}");
    }
    sqlx::query("DELETE FROM worlds.retired_world_scenes WHERE world_name = $1")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();
    let keys = store::active_content_keys(&f.pool)
        .await
        .expect("active keys");
    for replaced in ["bafyoldscene", "bafyoldthumb"] {
        assert!(
            !keys.contains(replaced),
            "evicted scene still referenced: {replaced}"
        );
    }

    // The set is a whole-table scan, so the fail-closed half runs in the same
    // test (a broken row must not be visible to the shared database's other
    // tests while they read the set).
    let (world, owner) = (world_name(), Wallet::random().address);
    let id = format!("bafybroken{}", Uuid::new_v4().simple());
    deploy(
        &f,
        &world,
        &owner,
        &id,
        &scene_entity(&world, "Broken", "bafythumb", "0,0", &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE worlds.world_scenes SET entity = $1::jsonb WHERE entity_id = $2")
        .bind(r#"{"content": [{"file": "a.png"}]}"#)
        .bind(&id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        store::active_content_keys(&f.pool).await.is_err(),
        "a content entry without a hash must abort the whole active set"
    );
    sqlx::query("UPDATE worlds.world_scenes SET entity = 'null'::jsonb WHERE entity_id = $1")
        .bind(&id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        store::active_content_keys(&f.pool).await.is_err(),
        "a non-object entity must abort the whole active set"
    );
    sqlx::query("DELETE FROM worlds.world_scenes WHERE entity_id = $1")
        .bind(&id)
        .execute(&f.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn scoped_replacement_conflicts_on_unauthorized_overlap() {
    let Some(f) = fixture().await else { return };
    // The 409 names the world as deployed (upstream `SceneReplacementConflictError`).
    let world = WorldName::parse(&format!("Mixed{}", world_name().display())).unwrap();
    let owner = Wallet::random().address;
    let thumb = store_png(&f).await;
    deploy(
        &f,
        &world,
        &owner,
        "bafyA",
        &scene_entity(&world, "A", &thumb, "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let conflict = deploy(
        &f,
        &world,
        &owner,
        "bafyB",
        &scene_entity(&world, "B", &thumb, "0,0", &["0,0"]),
        &["0,0"],
        &Deploy::Scoped(vec![]),
    )
    .await;
    assert!(
        matches!(&conflict, Err(e) if e.is_conflict()),
        "unauthorized overlap must 409, got {conflict:?}"
    );
    assert_eq!(
        conflict.unwrap_err().public_message(),
        format!(
            "Scene replacement authorization changed while deploying to world \"{}\". Please retry.",
            world.display()
        )
    );
    assert_eq!(entity_ids(&f, &world).await, vec!["bafyA".to_string()]);
    deploy(
        &f,
        &world,
        &owner,
        "bafyB",
        &scene_entity(&world, "B", &thumb, "0,0", &["0,0"]),
        &["0,0"],
        &Deploy::Scoped(vec![SceneId::parse("bafyA").unwrap()]),
    )
    .await
    .expect("deploy B replacing A");
    let scenes = store::list_scenes(&f.pool, &world).await.unwrap();
    assert_eq!(scenes.len(), 1);
    assert_eq!(scenes[0].0, "bafyB");
    assert_eq!(scenes[0].1, vec!["0,0".to_string()]);
}

#[tokio::test]
async fn admin_pages_and_the_index_agree_on_the_scene_count() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        "bafyidx",
        &scene_entity(&world, "Indexed", "bafythumb", "0,0", &["0,0", "0,1"]),
        &["0,0", "0,1"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let (page, total) = store::admin_list_worlds_page(&f.pool, 10_000, 0)
        .await
        .unwrap();
    let row = page.iter().find(|w| w.name == world.key()).unwrap();
    assert_eq!(row.scene_count, 1);
    assert_eq!(total, page.len() as i64);
    let (empty, total_past_end) = store::admin_list_worlds_page(&f.pool, 10, 1_000_000)
        .await
        .unwrap();
    assert!(empty.is_empty());
    // The past-the-end page counts separately, and tests running beside this
    // one on the shared scratch database only add worlds in between.
    assert!(total_past_end >= total, "{total_past_end} < {total}");
}

#[tokio::test]
async fn manifest_keeps_total_order_limit_and_empty_worlds_in_one_query() {
    let Some(f) = fixture().await else { return };
    let mixed = world_name();
    let empty = world_name();
    sqlx::query(
        "INSERT INTO worlds.worlds (name, spawn_coordinates) VALUES ($1, '-2,0'), ($2, NULL)",
    )
    .bind(mixed.key())
    .bind(empty.key())
    .execute(&f.pool)
    .await
    .unwrap();
    let parcels: Vec<String> = (-3..510).rev().map(|i| format!("{i},0")).collect();
    for id in ["a", "b"] {
        sqlx::query("INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size) VALUES ($1, $2, '[]', '{}', 'owner', $3, 0)")
            .bind(mixed.key())
            .bind(id)
            .bind(&parcels)
            .execute(&f.pool)
            .await
            .unwrap();
    }
    let upper = WorldName::parse(&mixed.display().to_uppercase()).unwrap();
    let manifest = store::world_manifest(&f.pool, &upper)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(manifest.total, 513);
    assert_eq!(
        manifest.parcels,
        (-3..497).map(|i| format!("{i},0")).collect::<Vec<_>>()
    );
    assert_eq!(manifest.spawn_coordinates.as_deref(), Some("-2,0"));
    for name in [empty.clone(), world_name()] {
        assert!(
            store::world_manifest(&f.pool, &name)
                .await
                .unwrap()
                .is_none()
        );
    }
    sqlx::query("UPDATE worlds.worlds SET spawn_coordinates = NULL WHERE name = $1")
        .bind(mixed.key())
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        store::world_manifest(&f.pool, &mixed)
            .await
            .unwrap()
            .unwrap()
            .spawn_coordinates
            .is_none()
    );
    let body = expect(
        &f.app,
        Case::new("get", format!("/world/{}/manifest", mixed.display())),
        200,
    )
    .await;
    assert_eq!(body["total"], 513);
    assert_eq!(body["spawn_coordinate"], json!({ "x": "0", "y": "0" }));
}

#[tokio::test]
async fn every_route_answers_its_contract() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    let stranger = Wallet::random();
    let outsider = Wallet::random();
    f.names.set(&owner.address, world.label());
    let app = &f.app;
    let w = world.display();

    let thumb = vec![0u8, 1, 2, 3, 4, 5, 6, 7];
    let good = deployment_with(
        &owner,
        &world,
        "Gate World",
        "0,0",
        &["0,0", "0,1"],
        "thumb.png",
        &thumb,
    )
    .await;
    let published = post_entity(app, &good, 200).await;
    assert_eq!(
        published["message"],
        format!(
            "Your scene was deployed to World \"{w}\" at parcels: 0,0, 0,1!\nAccess world: https://play.decentraland.org/?realm=https%3A%2F%2Fworlds.example%2Fworld%2F{w}&position=0%2C0"
        )
    );
    assert!(
        f.storage.exist(&good.thumb_hash).await.unwrap(),
        "the content file landed in the blob store"
    );
    assert!(
        f.storage.exist(&good.entity_id).await.unwrap(),
        "the entity file landed in the blob store"
    );
    let (junk, junk_type) = multipart_body(&[("something", None, b"else")]);
    expect(
        app,
        Case::new("post", "/entities").body(junk, &junk_type),
        400,
    )
    .await;
    let bad_thumb = deployment_with(
        &owner,
        &world,
        "Gate World",
        "0,0",
        &["0,0", "0,1"],
        "https://example.com/image.png",
        &thumb,
    )
    .await;
    post_entity(app, &bad_thumb, 400).await;
    let banned = world_name();
    {
        let _denied = DenyListed::seed(&f.state.names, vec![banned.label().to_string()]);
        f.names.set(&owner.address, banned.label());
        let denied = deployment(&owner, &banned, "Denied", "0,0", &["0,0"]).await;
        let body = post_entity(app, &denied, 400).await;
        assert!(body["message"].as_str().unwrap().contains("name deny list"));
    }

    expect(app, Case::new("get", "/worlds"), 200).await;
    expect(app, Case::new("get", "/worlds?sort=bogus"), 400).await;
    let active = expect(
        app,
        Case::new("post", "/entities/active").json(&json!({ "pointers": [w] })),
        200,
    )
    .await;
    assert_eq!(active[0]["id"], good.entity_id);
    assert_eq!(active[0]["metadata"]["owner"], owner.address.as_str());
    expect(
        app,
        Case::new("post", "/entities/active").json(&json!({})),
        400,
    )
    .await;
    expect(app, Case::new("get", format!("/world/{w}/manifest")), 200).await;
    expect(app, Case::new("get", "/world/nope.dcl.eth/manifest"), 404).await;

    let settings = expect(app, Case::new("get", format!("/world/{w}/settings")), 200).await;
    assert_eq!(settings["title"], "Gate World");
    assert_eq!(
        expect(app, Case::new("get", "/world/nope.dcl.eth/settings"), 404).await,
        json!({ "error": "World \"nope.dcl.eth\" not found or has no settings configured." })
    );
    let settings_path = format!("/world/{w}/settings");
    let (sbody, stype) = multipart_body(&[("title", None, b"Gate World Renamed")]);
    let updated = expect(
        app,
        Case::new("put", &settings_path)
            .signed(&owner)
            .await
            .body(sbody, &stype),
        200,
    )
    .await;
    assert_eq!(updated["settings"]["title"], "Gate World Renamed");
    assert_eq!(updated["settings"]["settings_version"], 1);
    let (sbody, stype) = multipart_body(&[("title", None, b"Gate World Renamed")]);
    expect(
        app,
        Case::new("put", &settings_path).body(sbody, &stype),
        400,
    )
    .await;
    let (sbody, stype) = multipart_body(&[("title", None, b"Stranger Title")]);
    assert_eq!(
        expect(
            app,
            Case::new("put", &settings_path)
                .signed(&stranger)
                .await
                .body(sbody, &stype),
            403,
        )
        .await,
        json!({ "error": "Unauthorized. You do not have permission to update settings for this world." })
    );
    let (big_body, big_type) = multipart_body(&[(
        "thumbnail",
        Some("thumbnail.png"),
        &vec![0u8; 2 * 1024 * 1024 + 1],
    )]);
    // `world-settings-handler.spec.ts:1045-1080`.
    assert_eq!(
        expect(
            app,
            Case::new("put", &settings_path)
                .signed(&owner)
                .await
                .body(big_body, &big_type),
            413,
        )
        .await,
        json!({ "error": "Payload Too Large", "message": "An uploaded file exceeds the maximum allowed size." })
    );

    let perms_path = format!("/world/{w}/permissions");
    let perms = expect(app, Case::new("get", &perms_path), 200).await;
    assert_eq!(perms["owner"], owner.address.as_str());
    assert_eq!(
        perms["permissions"]["access"],
        json!({ "type": "unrestricted" })
    );
    let deploy_perm_path = format!("/world/{w}/permissions/deployment");
    let meta = json!({ "type": "allow-list", "wallets": [stranger.display()] }).to_string();
    expect(
        app,
        Case::new("post", &deploy_perm_path)
            .signed_meta(&owner, &meta)
            .await,
        204,
    )
    .await;
    expect(
        app,
        Case::new("post", format!("/world/{w}/permissions/bogus"))
            .signed_meta(&owner, &json!({ "type": "allow-list" }).to_string())
            .await,
        400,
    )
    .await;
    expect(
        app,
        Case::new("post", &deploy_perm_path)
            .signed_meta(&stranger, &meta)
            .await,
        401,
    )
    .await;

    let addr_path = format!("/world/{w}/permissions/streaming/{}", stranger.display());
    expect(app, Case::new("put", &addr_path).signed(&owner).await, 204).await;
    expect(
        app,
        Case::new("put", format!("/world/{w}/permissions/streaming/zzz"))
            .signed(&owner)
            .await,
        400,
    )
    .await;
    let perms = expect(app, Case::new("get", &perms_path), 200).await;
    assert_eq!(
        perms["permissions"]["streaming"]["wallets"],
        json!([stranger.address.as_str()])
    );
    assert_eq!(
        perms["summary"][stranger.address.as_str()]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let parcels_path = format!(
        "/world/{w}/permissions/deployment/address/{}/parcels",
        stranger.display()
    );
    expect(
        app,
        Case::new("post", &parcels_path)
            .signed(&owner)
            .await
            .json(&json!({ "parcels": ["0,0"] })),
        204,
    )
    .await;
    let bogus = expect(
        app,
        Case::new(
            "post",
            format!(
                "/world/{w}/permissions/bogus/address/{}/parcels",
                stranger.display()
            ),
        )
        .signed(&owner)
        .await
        .json(&json!({ "parcels": ["0,0"] })),
        400,
    )
    .await;
    assert_eq!(
        bogus["message"],
        "Permission 'bogus' does not support allow-list. Only 'deployment' and 'streaming' do."
    );
    let listed = expect(app, Case::new("get", &parcels_path), 200).await;
    assert_eq!(listed, json!({ "total": 1, "parcels": ["0,0"] }));
    expect(
        app,
        Case::new(
            "get",
            format!(
                "/world/{w}/permissions/streaming/address/{}/parcels",
                owner.display()
            ),
        ),
        404,
    )
    .await;
    let holders = expect(
        app,
        Case::new("post", format!("/world/{w}/permissions/deployment/parcels"))
            .json(&json!({ "parcels": ["0,0"] })),
        200,
    )
    .await;
    assert_eq!(
        holders,
        json!({ "total": 1, "addresses": [stranger.address.as_str()] })
    );
    expect(
        app,
        Case::new("post", format!("/world/{w}/permissions/bogus/parcels"))
            .json(&json!({ "parcels": ["0,0"] })),
        400,
    )
    .await;
    expect(
        app,
        Case::new("delete", &parcels_path)
            .signed(&owner)
            .await
            .json(&json!({ "parcels": ["0,0"] })),
        204,
    )
    .await;
    expect(app, Case::new("get", &parcels_path), 404).await;
    assert!(
        !f.state
            .policy
            .has_world_wide_permission(&world, Permission::Deployment, &stranger.address)
            .await
            .unwrap()
    );
    let bogus = expect(
        app,
        Case::new(
            "delete",
            format!(
                "/world/{w}/permissions/bogus/address/{}/parcels",
                stranger.display()
            ),
        )
        .signed(&owner)
        .await
        .json(&json!({ "parcels": ["0,0"] })),
        400,
    )
    .await;
    assert_eq!(
        bogus["message"],
        "Permission 'bogus' does not support allow-list. Only 'deployment' and 'streaming' do."
    );
    expect(
        app,
        Case::new("delete", &addr_path).signed(&owner).await,
        204,
    )
    .await;
    expect(
        app,
        Case::new("delete", format!("/world/{w}/permissions/streaming/zzz"))
            .signed(&owner)
            .await,
        400,
    )
    .await;

    let access_path = format!("/world/{w}/permissions/access");
    expect(
        app,
        Case::new("post", &access_path)
            .signed_meta(
                &owner,
                &json!({ "type": "allow-list", "wallets": [], "communities": [] }).to_string(),
            )
            .await,
        204,
    )
    .await;
    let community = seed_community(&f.pool, &[&owner.address]).await;
    let community_path = format!("/world/{w}/permissions/access/communities/{community}");
    expect(
        app,
        Case::new("put", &community_path).signed(&owner).await,
        204,
    )
    .await;
    expect(
        app,
        Case::new(
            "put",
            format!("/world/{w}/permissions/access/communities/%20"),
        )
        .signed(&owner)
        .await,
        400,
    )
    .await;
    let perms = expect(app, Case::new("get", &perms_path), 200).await;
    assert_eq!(
        perms["permissions"]["access"],
        json!({ "type": "allow-list", "wallets": [], "communities": [community.to_string()] })
    );
    expect(
        app,
        Case::new("delete", &community_path).signed(&owner).await,
        204,
    )
    .await;
    expect(
        app,
        Case::new(
            "delete",
            format!("/world/{w}/permissions/access/communities/%20"),
        )
        .signed(&owner)
        .await,
        400,
    )
    .await;
    expect(
        app,
        Case::new("post", &access_path)
            .signed_meta(
                &owner,
                &json!({ "type": "shared-secret", "secret": "MySecret" }).to_string(),
            )
            .await,
        204,
    )
    .await;
    let now = chrono::Utc::now().naive_utc();
    assert_eq!(
        f.state
            .policy
            .can_access(&world, &stranger.address, Some("MySecret"), None, None, now)
            .await
            .unwrap(),
        crate::policy::Access::Allowed(crate::policy::AccessGrant::SharedSecret)
    );
    let perms = expect(app, Case::new("get", &perms_path), 200).await;
    assert_eq!(
        perms["permissions"]["access"],
        json!({ "type": "shared-secret" }),
        "the secret never leaves"
    );

    use crate::comms::gatekeeper::events::RoomEventSink;
    let room = crate::comms::rooms::worlds_service_world_room(&world);
    let join_body = serde_json::to_vec(&json!({ "event": "participant_joined", "room": { "name": room }, "participant": { "identity": owner.display() } })).unwrap();
    let _ = join_body;
    use crate::comms::protocol::bus::room_events::{RoomEvent, RoomEventKind, RoomType};
    let event = RoomEvent {
        kind: RoomEventKind::Joined as i32,
        room: room.clone(),
        room_type: RoomType::World as i32,
        realm_name: world.key().to_owned(),
        identity: owner.display(),
        ..Default::default()
    };
    // `serve` fans the webhook's events out to worlds beside the other consumers.
    struct Other(std::sync::Mutex<usize>);
    impl RoomEventSink for Other {
        fn observe_room_event(&self, _: &RoomEvent) {
            *self.0.lock().unwrap() += 1;
        }
    }
    let other = Arc::new(Other(std::sync::Mutex::new(0)));
    let fanout = crate::comms::gatekeeper::events::RoomEventFanout::new(vec![
        f.state.clone(),
        other.clone(),
    ]);
    fanout.observe_room_event(&event);
    fanout.observe_room_event(&RoomEvent {
        kind: RoomEventKind::Unspecified as i32,
        ..event
    });
    assert_eq!(*other.0.lock().unwrap(), 2);
    assert_eq!(
        f.state.presence.world_participant_count(world.key()),
        0,
        "the live presence is the gatekeeper's webhook to feed"
    );
    assert_eq!(f.state.flush_access_log().await.unwrap(), 1);

    expect(
        app,
        Case::new("get", "/admin/worlds").bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    expect(app, Case::new("get", "/admin/worlds"), 401).await;
    expect(app, Case::new("post", "/admin/gc?dry_run=true"), 401).await;
    let gc_runs = GC_RUNS.lock().await;
    let gc = expect(
        app,
        Case::new("post", "/gc?dry_run=true").bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(
        (gc["dryRun"].as_bool(), gc["removed"].as_u64()),
        (Some(true), Some(0)),
        "{gc}"
    );
    assert!(
        gc["minAgeSeconds"].as_u64().unwrap() >= 3600,
        "the floor holds"
    );
    assert!(
        gc["message"]
            .as_str()
            .unwrap()
            .starts_with("Garbage collection")
    );
    let unauthorized = expect(app, Case::new("post", "/gc").bearer("wrong"), 401).await;
    assert_eq!(unauthorized["message"], "Invalid authorization header");
    let running = catalyst_storage::gc::try_lock_run(&f.pool)
        .await
        .unwrap()
        .expect("no other run");
    let request = Request::builder()
        .method("POST")
        .uri("/gc?dry_run=true")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let busy = app.clone().oneshot(request).await.unwrap();
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(busy.headers()[header::RETRY_AFTER], "60");
    running.commit().await.unwrap();
    drop(gc_runs);
    let detail = expect(
        app,
        Case::new("get", format!("/admin/worlds/{w}")).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(detail["sceneCount"], 1);
    assert_eq!(detail["owner"], owner.address.as_str());
    expect(
        app,
        Case::new("get", "/admin/worlds/nope.dcl.eth").bearer(ADMIN_TOKEN),
        404,
    )
    .await;
    let ban = expect(
        app,
        Case::new(
            "get",
            format!(
                "/admin/worlds/{w}/ban-status?address={}&parcel=0,0",
                owner.display()
            ),
        )
        .bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(
        (
            ban["platformBanned"].as_bool(),
            ban["sceneBanned"].as_bool()
        ),
        (Some(false), Some(false))
    );
    expect(
        app,
        Case::new(
            "get",
            format!("/admin/worlds/{w}/ban-status?address={}", owner.display()),
        ),
        401,
    )
    .await;
    expect(
        app,
        Case::new("post", format!("/admin/worlds/{w}/disable")).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    expect(
        app,
        Case::new("post", "/admin/worlds/nope.dcl.eth/disable").bearer(ADMIN_TOKEN),
        404,
    )
    .await;
    expect(
        app,
        Case::new("post", format!("/admin/worlds/{w}/enable")).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    expect(
        app,
        Case::new("post", "/admin/worlds/nope.dcl.eth/enable").bearer(ADMIN_TOKEN),
        404,
    )
    .await;
    let blocked_path = format!("/admin/blocked/{}", stranger.display());
    expect(
        app,
        Case::new("post", &blocked_path).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    expect(app, Case::new("post", &blocked_path), 401).await;
    assert_eq!(
        f.state
            .policy
            .is_banned(&stranger.address, None, None, now)
            .await
            .unwrap(),
        Some(crate::policy::Ban::Blocked),
        "a blocked wallet is refused every world join"
    );
    let ban = expect(
        app,
        Case::new(
            "get",
            format!(
                "/admin/worlds/{w}/ban-status?address={}",
                stranger.display()
            ),
        )
        .bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(ban["platformBanned"], true);
    let listed = expect(
        app,
        Case::new("get", "/admin/blocked").bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert!(
        listed["blocked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["wallet"] == stranger.address.as_str())
    );
    expect(app, Case::new("get", "/admin/blocked"), 401).await;
    expect(
        app,
        Case::new("delete", &blocked_path).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    expect(
        app,
        Case::new("delete", &blocked_path).bearer(ADMIN_TOKEN),
        404,
    )
    .await;
    let log = expect(
        app,
        Case::new("get", format!("/admin/access-log?world={w}")).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(log["entries"][0]["action"], "join");
    assert_eq!(log["entries"][0]["address"], owner.address.as_str());
    let log = expect(
        app,
        Case::new(
            "get",
            format!("/admin/access-log?address={}", owner.display()),
        )
        .bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(log["entries"][0]["worldName"], world.key());
    expect(app, Case::new("get", "/admin/access-log"), 401).await;

    expect(
        app,
        Case::new("delete", format!("/world/{w}/scenes/0,0"))
            .signed(&outsider)
            .await,
        401,
    )
    .await;
    expect(
        app,
        Case::new("delete", format!("/world/{w}/scenes/0,0"))
            .signed(&owner)
            .await,
        200,
    )
    .await;
    // Nothing left standing there still answers 200 with the same message.
    let again = expect(
        app,
        Case::new("delete", format!("/world/{w}/scenes/0,0"))
            .signed(&owner)
            .await,
        200,
    )
    .await;
    assert_eq!(
        again,
        json!({ "message": "Scene at parcel 0,0 undeployed successfully" })
    );
    expect(
        app,
        Case::new("delete", format!("/world/{w}/scenes/1,x"))
            .signed(&owner)
            .await,
        400,
    )
    .await;
    post_entity(app, &good, 200).await;
    let refused = expect(
        app,
        Case::new("delete", format!("/entities/{w}"))
            .signed(&outsider)
            .await,
        400,
    )
    .await;
    assert_eq!(
        refused,
        json!({
            "error": "Bad request",
            "message": "Invalid request. You must have world-wide deployment permission to undeploy the entire world."
        })
    );
    let undeployed = expect(
        app,
        Case::new("delete", format!("/entities/{w}"))
            .signed(&owner)
            .await,
        200,
    )
    .await;
    assert_eq!(undeployed, json!({}));
    assert!(deployed_entity_ids(&f.pool, &world).await.is_empty());
    // Upstream answers the string `{}`, so `text/plain` (`undeploy-entity-handler.ts`).
    post_entity(app, &good, 200).await;
    let request = Case::new("delete", format!("/entities/{w}"))
        .signed(&owner)
        .await;
    let mut builder = Request::builder().method("DELETE").uri(&request.path);
    for (k, v) in &request.headers {
        builder = builder.header(k, v);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 64)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"{}");
}

const NEST: &str = "/worlds-content-server";

/// A request to `route` on the `worlds-content-server.<domain>` host: the
/// gateway nests it under [`NEST`] and forwards the client's path as
/// `x-original-path`; the signature is over `signed`.
async fn via_gateway(
    method: &'static str,
    route: &str,
    w: &Wallet,
    signed: &str,
    forwarded: Option<&str>,
    metadata: &str,
) -> Case {
    let case = Case::new(method, format!("{NEST}{route}"))
        .signed_over(w, signed, metadata)
        .await;
    match forwarded {
        Some(path) => case.header(ORIGINAL_PATH_HEADER, path),
        None => case,
    }
}

/// Every signed route family through the nest: a signature over the bare
/// path verifies with the gateway's `x-original-path`, one over the prefixed
/// path still verifies, and one for another route is refused.
#[tokio::test]
async fn signed_routes_verify_the_bare_path_through_the_gateway_nest() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let w = world.display();
    let app = &Router::new().nest(NEST, f.app.clone());
    let scene = deployment(&owner, &world, "Nest World", "0,0", &["0,0"]).await;
    post_entity(&f.app, &scene, 200).await;

    let settings = format!("/world/{w}/settings");
    let access = format!("/world/{w}/permissions/access");
    let grants = format!("/world/{w}/permissions/deployment");
    let scene_path = format!("/world/{w}/scenes/0,0");
    let world_path = format!("/entities/{w}");
    let prefixed = |route: &str| format!("{NEST}{route}");

    for (signed, forwarded, status) in [
        (settings.clone(), Some(settings.clone()), 200),
        (prefixed(&settings), None, 200),
        (access.clone(), Some(access.clone()), 401),
        (settings.clone(), None, 401),
    ] {
        let (body, ty) = multipart_body(&[("title", None, b"Nest World Renamed")]);
        let case = via_gateway(
            "put",
            &settings,
            &owner,
            &signed,
            forwarded.as_deref(),
            "{}",
        )
        .await;
        expect(app, case.body(body, &ty), status).await;
    }

    let grant = json!({ "type": "allow-list", "wallets": [] }).to_string();
    for (signed, forwarded, status) in [
        (grants.clone(), Some(grants.clone()), 204),
        (prefixed(&grants), None, 204),
        (access.clone(), Some(access.clone()), 401),
    ] {
        let case = via_gateway(
            "post",
            &grants,
            &owner,
            &signed,
            forwarded.as_deref(),
            &grant,
        )
        .await;
        expect(app, case, status).await;
    }

    let mut deployed = true;
    for (route, signed, forwarded, status) in [
        (
            &scene_path,
            world_path.clone(),
            Some(world_path.clone()),
            401,
        ),
        (
            &scene_path,
            scene_path.clone(),
            Some(scene_path.clone()),
            200,
        ),
        (&scene_path, prefixed(&scene_path), None, 200),
        (
            &world_path,
            scene_path.clone(),
            Some(scene_path.clone()),
            401,
        ),
        (
            &world_path,
            world_path.clone(),
            Some(world_path.clone()),
            200,
        ),
        (&world_path, prefixed(&world_path), None, 200),
    ] {
        if !deployed {
            post_entity(&f.app, &scene, 200).await;
        }
        let case = via_gateway("delete", route, &owner, &signed, forwarded.as_deref(), "{}").await;
        expect(app, case, status).await;
        deployed = status != 200;
    }
    assert!(deployed_entity_ids(&f.pool, &world).await.is_empty());
}

/// The owner-gated permission routes (`verify_owner`) through the nest: a
/// bare-path signature with `x-original-path` passes, another route's fails.
#[tokio::test]
async fn owner_gated_permission_routes_verify_the_bare_path_through_the_gateway_nest() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    let grantee = Wallet::random();
    f.names.set(&owner.address, world.label());
    let w = world.display();
    let app = &Router::new().nest(NEST, f.app.clone());
    let scene = deployment(&owner, &world, "Nest World", "0,0", &["0,0"]).await;
    post_entity(&f.app, &scene, 200).await;
    let access = format!("/world/{w}/permissions/access");
    expect(
        &f.app,
        Case::new("post", &access)
            .signed_meta(
                &owner,
                &json!({ "type": "allow-list", "wallets": [], "communities": [] }).to_string(),
            )
            .await,
        204,
    )
    .await;

    let address = format!("/world/{w}/permissions/streaming/{}", grantee.display());
    let parcels = format!(
        "/world/{w}/permissions/deployment/address/{}/parcels",
        grantee.display()
    );
    // Adding a community requires the signer to be one of its members.
    let community = format!(
        "/world/{w}/permissions/access/communities/{}",
        seed_community(&f.pool, &[&owner.address]).await
    );
    let body = json!({ "parcels": ["0,0"] });
    let steps: [(&'static str, &String, Option<&Value>); 6] = [
        ("put", &address, None),
        ("post", &parcels, Some(&body)),
        ("delete", &parcels, Some(&body)),
        ("put", &community, None),
        ("delete", &community, None),
        ("delete", &address, None),
    ];
    for (method, route, json) in steps {
        for (signed, status) in [(&access, 401), (route, 204)] {
            let case = via_gateway(method, route, &owner, signed, Some(signed), "{}").await;
            let case = match json {
                Some(v) => case.json(v),
                None => case,
            };
            expect(app, case, status).await;
        }
    }
    let perms = expect(
        &f.app,
        Case::new("get", format!("/world/{w}/permissions")),
        200,
    )
    .await;
    assert_eq!(
        perms["permissions"]["access"],
        json!({ "type": "allow-list", "wallets": [], "communities": [] })
    );
    assert_eq!(perms["permissions"]["streaming"]["wallets"], json!([]));
}

/// A local publish is queued in its transaction and sent, unchanged, to the
/// upstream's `POST /entities` with only the files it lacks; a scene
/// replaced before its send is superseded, never sent (the upstream applies
/// the last publish it receives); a refused publish is never queued.
#[tokio::test]
async fn local_publishes_are_forwarded_unchanged_and_replaced_ones_superseded() {
    use crate::auth::{AuthChain, verify::verify_auth_chain_async};
    use crate::content::create_entity::extract_auth_chain_from_fields;
    use tokio_util::sync::CancellationToken;

    let upstream = forward::test_support::FakeUpstream::start("").await;
    let Some(f) = fixture_with(PersonalWorldsPolicy::disabled(), Some(&upstream.base)).await else {
        return;
    };
    let forwarder = f.state.forwarder.clone().unwrap();
    let world = world_name();
    let owner = Wallet::random();
    let stranger = Wallet::random();
    f.names.set(&owner.address, world.label());

    let refused = deployment(&stranger, &world, "Stranger", "0,0", &["0,0"]).await;
    let (status, _) = hit(
        &f.app,
        Case::new("post", "/entities").body(refused.body.clone(), &refused.content_type),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    let first = deployment(&owner, &world, "First", "0,0", &["0,0"]).await;
    post_entity(&f.app, &first, 200).await;
    let second = deployment(&owner, &world, "Second", "0,0", &["0,0"]).await;
    post_entity(&f.app, &second, 200).await;
    let other = deployment(&owner, &world, "Other", "1,0", &["1,0"]).await;
    upstream.hold(&other.thumb_hash);
    post_entity(&f.app, &other, 200).await;

    let ids = [
        refused.entity_id.clone(),
        first.entity_id.clone(),
        second.entity_id.clone(),
        other.entity_id.clone(),
    ];
    let queued: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT entity_id, world_name, status FROM worlds.forward_outbox WHERE entity_id = ANY($1) ORDER BY id",
    )
    .bind(&ids[..])
    .fetch_all(&f.pool)
    .await
    .unwrap();
    let key = world.key().to_string();
    assert_eq!(
        queued,
        [&first, &second, &other]
            .iter()
            .map(|d| (d.entity_id.clone(), key.clone(), "queued".to_string()))
            .collect::<Vec<_>>()
    );

    let report = forwarder.run_pass(&CancellationToken::new()).await.unwrap();
    assert_eq!(
        (report.accepted, report.superseded, report.total()),
        (2, 1, 3)
    );

    let posts = upstream.posts();
    assert_eq!(posts.len(), 2);
    for (post, d, lacking) in [(&posts[0], &second, true), (&posts[1], &other, false)] {
        assert_eq!(post.content_length, Some(post.body.len() as u64));
        let form = forward::test_support::parse(post).await;
        assert_eq!(form.fields["entityId"], d.entity_id);
        let sent: AuthChain = serde_json::from_value(
            extract_auth_chain_from_fields(&form.fields)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        verify_auth_chain_async(&sent, &d.entity_id, None, None)
            .await
            .expect("the forwarded chain signs the forwarded entity");
        let stored: Value = sqlx::query_scalar(
            "SELECT deployment_auth_chain FROM worlds.world_scenes WHERE entity_id = $1",
        )
        .bind(&d.entity_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        let stored: AuthChain = serde_json::from_value(stored).unwrap();
        assert_eq!(sent.len(), stored.len());
        for (sent, stored) in sent.iter().zip(&stored) {
            assert_eq!(
                (&sent.link_type, &sent.payload),
                (&stored.link_type, &stored.payload)
            );
            assert_eq!(
                sent.signature.as_deref().unwrap_or_default(),
                stored.signature.as_deref().unwrap_or_default()
            );
        }
        let entity = f.storage.retrieve(&d.entity_id).await.unwrap().unwrap();
        assert_eq!(
            hash_bytes_v1(&entity),
            d.entity_id,
            "the client's entity bytes"
        );
        let mut expected = vec![(d.entity_id.clone(), entity)];
        if lacking {
            let thumb = f.storage.retrieve(&d.thumb_hash).await.unwrap().unwrap();
            expected.push((d.thumb_hash.clone(), thumb));
        }
        assert_eq!(form.files, expected);
    }

    let record = expect(
        &f.app,
        Case::new("get", format!("/admin/forwarding/{}", first.entity_id)).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(record["status"], json!("superseded"));
    assert_eq!(record["worldName"], json!(key));
    assert_eq!(record["target"], json!(upstream.base));
    expect(
        &f.app,
        Case::new("get", format!("/admin/forwarding/{}", first.entity_id)),
        401,
    )
    .await;
    expect(
        &f.app,
        Case::new("get", format!("/admin/forwarding/{}", refused.entity_id)).bearer(ADMIN_TOKEN),
        404,
    )
    .await;
    let page = expect(
        &f.app,
        Case::new("get", "/admin/forwarding?status=accepted&limit=1000").bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    let listed: Vec<&Value> = page["forwards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["entityId"])
        .collect();
    assert!(listed.contains(&&json!(second.entity_id)));
    assert!(listed.contains(&&json!(other.entity_id)));
    expect(
        &f.app,
        Case::new("get", "/admin/forwarding?status=bogus").bearer(ADMIN_TOKEN),
        400,
    )
    .await;

    // A re-publish of an accepted scene is a fresh record, queued again.
    sqlx::query("DELETE FROM worlds.world_scenes WHERE entity_id = $1")
        .bind(&other.entity_id)
        .execute(&f.pool)
        .await
        .unwrap();
    post_entity(&f.app, &other, 200).await;
    let again: (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM worlds.forward_outbox WHERE entity_id = $1")
            .bind(&other.entity_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(again, ("queued".to_string(), 0));

    // The upstream refuses it; the operator requeues it once the cause is
    // fixed. A superseded record is not requeued: the scene is gone here.
    upstream.answer(400, "refused upstream");
    let report = forwarder.run_pass(&CancellationToken::new()).await.unwrap();
    assert_eq!((report.rejected, report.total()), (1, 1));
    let retry = format!("/admin/forwarding/{}/retry", other.entity_id);
    expect(&f.app, Case::new("post", retry.clone()), 401).await;
    let record = expect(&f.app, Case::new("post", retry).bearer(ADMIN_TOKEN), 200).await;
    assert_eq!(
        (&record["status"], &record["attempts"], &record["worldName"]),
        (&json!("queued"), &json!(0), &json!(key))
    );
    let report = forwarder.run_pass(&CancellationToken::new()).await.unwrap();
    assert_eq!((report.accepted, report.total()), (1, 1));
    expect(
        &f.app,
        Case::new(
            "post",
            format!("/admin/forwarding/{}/retry", first.entity_id),
        )
        .bearer(ADMIN_TOKEN),
        409,
    )
    .await;
    expect(
        &f.app,
        Case::new(
            "post",
            format!("/admin/forwarding/{}/retry", refused.entity_id),
        )
        .bearer(ADMIN_TOKEN),
        404,
    )
    .await;

    sqlx::query("DELETE FROM worlds.forward_outbox WHERE entity_id = ANY($1)")
        .bind(&ids[..])
        .execute(&f.pool)
        .await
        .unwrap();
}

/// A publish the mirror took over before its send (the scene row now has a
/// `mirror_source`) is no longer this node's: superseded, never sent. The
/// requeue route answers 503 while no upstream is named, and the lane keeps
/// a worlds server's window (`DEPLOYMENT_TTL`, five minutes).
#[tokio::test]
async fn a_publish_mirrored_over_before_its_send_is_superseded() {
    use tokio_util::sync::CancellationToken;

    assert_eq!(
        (
            super::forwarding::LANE.max_age_ms,
            super::forwarding::LANE.max_ahead_ms
        ),
        (5 * 60_000, 15 * 60_000)
    );
    let upstream = forward::test_support::FakeUpstream::start("").await;
    let Some(f) = fixture_with(PersonalWorldsPolicy::disabled(), Some(&upstream.base)).await else {
        return;
    };
    let forwarder = f.state.forwarder.clone().unwrap();
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let d = deployment(&owner, &world, "Mirrored", "0,0", &["0,0"]).await;
    post_entity(&f.app, &d, 200).await;
    sqlx::query(
        "UPDATE worlds.world_scenes SET mirror_source = 'https://mirror.example' WHERE entity_id = $1",
    )
    .bind(&d.entity_id)
    .execute(&f.pool)
    .await
    .unwrap();

    let report = forwarder.run_pass(&CancellationToken::new()).await.unwrap();
    assert_eq!((report.superseded, report.total()), (1, 1));
    assert!(upstream.posts().is_empty() && upstream.probed().is_empty());
    let record = expect(
        &f.app,
        Case::new("get", format!("/admin/forwarding/{}", d.entity_id)).bearer(ADMIN_TOKEN),
        200,
    )
    .await;
    assert_eq!(record["status"], json!("superseded"));

    let off = fixture().await.unwrap();
    expect(
        &off.app,
        Case::new("post", format!("/admin/forwarding/{}/retry", d.entity_id)).bearer(ADMIN_TOKEN),
        503,
    )
    .await;

    sqlx::query("DELETE FROM worlds.forward_outbox WHERE entity_id = $1")
        .bind(&d.entity_id)
        .execute(&f.pool)
        .await
        .unwrap();
}

/// A world deployed by `owner`, who also holds its NAME.
async fn owned_world(f: &Fixture, owner: &Wallet) -> WorldName {
    let world = world_name();
    f.names.set(&owner.address, world.label());
    let fp = ["0,0", "0,1"];
    deploy(
        f,
        &world,
        &owner.address,
        &format!("bafy{}", Uuid::new_v4().simple()),
        &scene_entity(&world, "Owned World", "bafymissing", "0,0", &fp),
        &fp,
        &Deploy::Owner,
    )
    .await
    .unwrap();
    world
}

async fn put_settings(
    f: &Fixture,
    world: &WorldName,
    w: &Wallet,
    parts: &[(&str, Option<&str>, &[u8])],
    status: u16,
) -> Value {
    let (body, content_type) = multipart_body(parts);
    expect(
        &f.app,
        Case::new("put", format!("/world/{}/settings", world.display()))
            .signed(w)
            .await
            .body(body, &content_type),
        status,
    )
    .await
}

#[tokio::test]
async fn a_settings_thumbnail_is_stored_only_for_an_authorized_writer() {
    let Some(f) = fixture().await else { return };
    let (owner, stranger) = (Wallet::random(), Wallet::random());
    let world = owned_world(&f, &owner).await;
    let mut png = PNG_MAGIC.to_vec();
    png.extend_from_slice(Uuid::new_v4().as_bytes());
    let hash = hash_bytes_v1(&png);
    let parts: [(&str, Option<&str>, &[u8]); 1] = [("thumbnail", Some("t.png"), &png)];

    put_settings(&f, &world, &stranger, &parts, 403).await;
    assert!(
        !f.storage.exist(&hash).await.unwrap(),
        "an unauthorized PUT stores nothing"
    );

    let updated = put_settings(&f, &world, &owner, &parts, 200).await;
    assert_eq!(updated["settings"]["thumbnail_hash"], json!(hash));
    assert!(f.storage.exist(&hash).await.unwrap());
}

/// A settings edit is recorded for the name's holder, also when a collaborator with world-wide
/// deployment makes it, so the mirror keeps it while that holder holds the name.
#[tokio::test]
async fn a_collaborators_settings_edit_is_marked_for_the_holder() {
    let Some(f) = fixture().await else { return };
    let (owner, collaborator) = (Wallet::random(), Wallet::random());
    let world = owned_world(&f, &owner).await;
    store::grant_addresses_world_wide_permission(
        &f.pool,
        &world,
        Permission::Deployment,
        std::slice::from_ref(&collaborator.address),
        None,
    )
    .await
    .unwrap();
    put_settings(
        &f,
        &world,
        &collaborator,
        &[("title", None, b"Theirs")],
        200,
    )
    .await;
    let marked: (bool, Option<String>) = sqlx::query_as(
        "SELECT settings_set_here, settings_set_by FROM worlds.worlds WHERE name = $1",
    )
    .bind(world.key())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(marked, (true, Some(owner.address.as_str().to_string())));
}

/// The body is read after the signed fetch: an unsigned request of any
/// content type gets the signed-fetch refusal (`world-settings-handler.spec.ts:849-876`),
/// not the multipart parser's 400.
#[tokio::test]
async fn an_unsigned_settings_put_is_refused_by_the_signed_fetch_before_the_body() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let path = format!("/world/{}/settings", world.display());
    for content_type in ["application/json", "multipart/form-data", "text/plain"] {
        let body = expect(
            &f.app,
            Case::new("put", &path).body(b"{}".to_vec(), content_type),
            400,
        )
        .await;
        let text = body.to_string();
        assert!(
            text.contains("Invalid Auth Chain"),
            "{content_type}: {text}"
        );
        assert!(
            !text.contains("Content-Type") && !text.contains("Boundary"),
            "{text}"
        );
    }
    // Signed, the parser then refuses what is not a multipart form, in busboy's words.
    let body = expect(
        &f.app,
        Case::new("put", &path)
            .signed(&owner)
            .await
            .body(b"{}".to_vec(), "application/json"),
        400,
    )
    .await;
    assert_eq!(
        body,
        json!({ "error": "Bad request", "message": "Unsupported content type: application/json" })
    );
}

/// A declared `Content-Length` over the wire cap is refused before the body
/// is read (`multipart/middleware.ts:159-166`).
#[tokio::test]
async fn a_settings_put_declaring_too_many_bytes_answers_413() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let body = expect(
        &f.app,
        Case::new("put", format!("/world/{}/settings", world.display()))
            .signed(&owner)
            .await
            .header(
                "content-length",
                &(super::settings::MAX_SETTINGS_UPLOAD_WIRE_BYTES + 1).to_string(),
            )
            .body(Vec::new(), "multipart/form-data; boundary=x"),
        413,
    )
    .await;
    assert_eq!(
        body,
        json!({ "error": "Payload Too Large", "message": "The multipart request is too large." })
    );
}

/// A body that stops arriving answers 408 with `describeUploadTimeout`'s
/// text over the bytes received and the declared length.
#[tokio::test]
async fn a_stalled_settings_upload_answers_the_upstream_timeout_text() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let mut settings = Settings::for_tests(Some(ADMIN_TOKEN));
    settings.upload_limits.multipart_upload_timeout_ms = 200;
    let policy = Policy::new(
        f.pool.clone(),
        PersonalWorldsPolicy::disabled(),
        Some(f.market.clone()),
    );
    let app = router(Arc::new(CoreState::new(
        f.pool.clone(),
        f.storage.clone(),
        Arc::new(policy),
        settings,
        None,
    )));
    let path = format!("/world/{}/settings", world.display());
    let first: &[u8] = b"--x\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nSlow";
    use futures::StreamExt;
    let stalled =
        futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from_static(first))])
            .chain(futures::stream::pending());
    let mut builder = Request::builder().method("PUT").uri(&path);
    for (k, v) in signed_headers(&owner, "put", &path, "{}").await.iter() {
        builder = builder.header(k, v);
    }
    let request = builder
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
        .header(header::CONTENT_LENGTH, "4096")
        .body(Body::from_stream(stalled))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body,
        json!({
            "error": "Request Timeout",
            "message": super::settings::describe_upload_timeout(200, first.len() as u64, Some(4096)),
        })
    );
}

#[tokio::test]
async fn settings_empty_values_follow_the_upstream_handler() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;

    // The handler's own `{error: <message>}` (`world-settings-handler.ts:208-213`).
    let body = put_settings(&f, &world, &owner, &[("title", None, b"")], 400).await;
    assert_eq!(
        body,
        json!({ "error": "Invalid title: title cannot be empty. Expected between 3 and 100 characters." })
    );
    let body = put_settings(&f, &world, &owner, &[("description", None, b"")], 400).await;
    assert_eq!(
        body,
        json!({ "error": "Invalid description: description cannot be empty. Expected between 3 and 1000 characters." })
    );
    let body = put_settings(
        &f,
        &world,
        &owner,
        &[("spawn_coordinates", None, b"9,9")],
        400,
    )
    .await;
    assert_eq!(
        body,
        json!({ "error": "Invalid spawnCoordinates \"9,9\". It must be within the world shape rectangle: (0,0) to (0,1)." }),
        "the store's spawn refusal is a validation error too"
    );

    put_settings(
        &f,
        &world,
        &owner,
        &[
            ("categories", None, b"art"),
            ("single_player", None, b"true"),
            ("show_in_places", None, b"true"),
            ("content_rating", None, b"T"),
        ],
        200,
    )
    .await;
    let body = put_settings(
        &f,
        &world,
        &owner,
        &[
            ("categories", None, b"game"),
            ("categories", None, b""),
            ("single_player", None, b""),
            ("show_in_places", None, b""),
            ("spawn_coordinates", None, b""),
            ("content_rating", None, b""),
        ],
        200,
    )
    .await;
    let s = &body["settings"];
    assert_eq!(
        s["categories"],
        json!(["art"]),
        "an empty category drops the field"
    );
    assert_eq!(s["single_player"], json!(true));
    assert_eq!(s["show_in_places"], json!(true));
    assert_eq!(s["spawn_coordinates"], json!("0,0"));
    assert_eq!(s["content_rating"], json!("T"));
}

#[tokio::test]
async fn parcel_bodies_are_schema_validated_on_every_parcel_route() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let grantee = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let w = world.display();
    let address_path = format!(
        "/world/{w}/permissions/deployment/address/{}/parcels",
        grantee.display()
    );
    let holders_path = format!("/world/{w}/permissions/deployment/parcels");
    let too_many: Vec<String> = (0..501).map(|i| format!("{i},0")).collect();
    let bad_bodies = [
        json!({ "parcels": [] }),
        json!({ "parcels": too_many }),
        json!({ "parcels": ["a,b"] }),
        json!({ "parcels": ["0,0"], "extra": 1 }),
        json!({}),
    ];

    for body in &bad_bodies {
        for (method, path, signed) in [
            ("post", &address_path, true),
            ("delete", &address_path, true),
            ("post", &holders_path, false),
        ] {
            let mut case = Case::new(method, path.as_str());
            if signed {
                case = case.signed(&owner).await;
            }
            let answer = expect(&f.app, case.json(body), 400).await;
            assert_eq!(
                answer["message"], "Invalid JSON body",
                "{method} {path} {body}"
            );
            // The schema validator's own shape, not the worlds envelope.
            assert_eq!(answer["ok"], false, "{answer}");
            assert!(answer["data"][0]["keyword"].is_string(), "{answer}");
        }
    }
    expect(
        &f.app,
        Case::new("post", holders_path.as_str())
            .body(b"{\"parcels\":[\"0,0\"]}".to_vec(), "text/plain"),
        415,
    )
    .await;
    assert!(
        store::permission_records(&f.pool, &world)
            .await
            .unwrap()
            .is_empty(),
        "no rejected body granted anything"
    );

    let max: Vec<String> = (0..500)
        .map(|i| format!("{},{}", i % 100, i / 100))
        .collect();
    expect(
        &f.app,
        Case::new("post", address_path.as_str())
            .signed(&owner)
            .await
            .json(&json!({ "parcels": max })),
        204,
    )
    .await;
}

#[tokio::test]
async fn parcel_listings_paginate_like_http_commons() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let grantee = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let w = world.display();
    let path = format!(
        "/world/{w}/permissions/streaming/address/{}/parcels",
        grantee.display()
    );
    let granted: Vec<String> = (0..150).map(|i| format!("{i},0")).collect();
    expect(
        &f.app,
        Case::new("post", path.as_str())
            .signed(&owner)
            .await
            .json(&json!({ "parcels": granted })),
        204,
    )
    .await;

    for (query, expected) in [
        ("", 100),
        ("?limit=1000", 100),
        ("?limit=abc", 100),
        ("?limit=0", 100),
        ("?limit=10&offset=-5", 10),
        ("?limit=10&offset=145", 5),
    ] {
        let page = expect(&f.app, Case::new("get", format!("{path}{query}")), 200).await;
        assert_eq!(page["total"], 150, "{query}");
        assert_eq!(
            page["parcels"].as_array().unwrap().len(),
            expected,
            "{query}"
        );
    }
    let boxed = expect(
        &f.app,
        Case::new("get", format!("{path}?x1=0&y1=0&x2=1.5&y2=0")),
        200,
    )
    .await;
    assert_eq!(boxed["parcels"], json!(["0,0", "1,0"]));
    let bad = expect(
        &f.app,
        Case::new("get", format!("{path}?x1=a&y1=0&x2=1&y2=0")),
        400,
    )
    .await;
    assert_eq!(
        bad["message"],
        "Bounding box parameters must be valid integers."
    );

    let holders = expect(
        &f.app,
        Case::new(
            "post",
            format!("/world/{w}/permissions/streaming/parcels?limit=1000&offset=x"),
        )
        .json(&json!({ "parcels": ["0,0"] })),
        200,
    )
    .await;
    assert_eq!(holders["addresses"], json!([grantee.address.as_str()]));
}

#[tokio::test]
async fn deleting_an_address_grant_requires_a_valid_world() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let grantee = Wallet::random();
    let delete = |world: &WorldName| {
        Case::new(
            "delete",
            format!(
                "/world/{}/permissions/deployment/{}",
                world.display(),
                grantee.display()
            ),
        )
    };

    // Upstream's `isWorldValid` needs the row, not a scene: an owner who
    // added a collaborator before the first deploy can remove it.
    let (sceneless, sceneless_owner) = (world_name(), Wallet::random());
    f.names.set(&sceneless_owner.address, sceneless.label());
    let grant = Case::new(
        "post",
        format!("/world/{}/permissions/deployment", sceneless.display()),
    )
    .signed_meta(
        &sceneless_owner,
        &json!({ "type": "allow-list", "wallets": [grantee.address.as_str()] }).to_string(),
    )
    .await;
    expect(&f.app, grant, 204).await;
    expect(
        &f.app,
        delete(&sceneless).signed(&sceneless_owner).await,
        204,
    )
    .await;
    assert!(
        store::permission_records(&f.pool, &sceneless)
            .await
            .unwrap()
            .is_empty()
    );
    // The `access` branch on the same world: the wallet leaves the
    // allow-list and is re-decided (`RemoveWallet` + `revoke_unless_admitted`).
    let recorded = Arc::new(RecordedRevocations::default());
    f.state.add_revocation_sink(recorded.clone());
    let allow = Case::new(
        "post",
        format!("/world/{}/permissions/access", sceneless.display()),
    )
    .signed_meta(
        &sceneless_owner,
        &json!({ "type": "allow-list", "wallets": [grantee.address.as_str()] }).to_string(),
    )
    .await;
    expect(&f.app, allow, 204).await;
    recorded.take();
    let remove = Case::new(
        "delete",
        format!(
            "/world/{}/permissions/access/{}",
            sceneless.display(),
            grantee.display()
        ),
    )
    .signed(&sceneless_owner)
    .await;
    expect(&f.app, remove, 204).await;
    let perms = expect(
        &f.app,
        Case::new("get", format!("/world/{}/permissions", sceneless.display())),
        200,
    )
    .await;
    assert_eq!(perms["permissions"]["access"]["wallets"], json!([]));
    assert_eq!(
        recorded.take(),
        vec![super::Revocation::Address {
            world: sceneless.clone(),
            address: grantee.address.clone(),
        }]
    );
    let (unknown, unknown_owner) = (world_name(), Wallet::random());
    f.names.set(&unknown_owner.address, unknown.label());
    expect(&f.app, delete(&unknown).signed(&unknown_owner).await, 404).await;

    // The deny list as `getRawWorldRecords` filters it: the entry without
    // `.dcl`/`.eth` against the stored name, so a bare world name matches.
    let (bare, bare_owner) = (
        WorldName::parse(&format!(
            "bare{}",
            &Uuid::new_v4().simple().to_string()[..8]
        ))
        .unwrap(),
        Wallet::random(),
    );
    store::store_access_for_owner(
        &f.pool,
        &bare,
        &bare_owner.address,
        &AccessSetting::NftOwnership {
            nft: "urn:x".into(),
        },
    )
    .await
    .unwrap();
    let access = || async {
        let path = format!("/world/{}/permissions", bare.display());
        expect(&f.app, Case::new("get", path), 200).await["permissions"]["access"]["type"].clone()
    };
    assert!(
        store::world_is_valid(&f.pool, &f.state.policy, &f.state.names, &bare)
            .await
            .unwrap()
    );
    assert_eq!(access().await, "nft-ownership");
    {
        let _denied = DenyListed::seed(&f.state.names, vec![format!("{}.dcl.eth", bare.key())]);
        assert!(
            !store::world_is_valid(&f.pool, &f.state.policy, &f.state.names, &bare)
                .await
                .unwrap()
        );
        // `getAccessForWorld` reads the filtered-out world as the default.
        assert_eq!(access().await, "unrestricted");
        // A bare name has no owner, so the route answers 401 before this check.
        expect(&f.app, delete(&bare).signed(&bare_owner).await, 401).await;
    }

    let world = owned_world(&f, &owner).await;
    {
        // The one entry an owned world's stored name equals once `.eth` and
        // `.dcl` are stripped once each (`String.replace`): the owner's
        // DELETE answers 404 through the route.
        let entry = format!("{}.dcl.eth", world.key());
        let _denied = DenyListed::seed(&f.state.names, vec![entry]);
        let body = expect(&f.app, delete(&world).signed(&owner).await, 404).await;
        assert_eq!(
            body["message"],
            json!(format!("World \"{}\" not found.", world.display()))
        );
    }
    {
        // `isWorldValid` first asks `checkNameDenyList` (`component.ts:71-74`):
        // the name lowercased without its first `.eth` and `.dcl` against each
        // entry lowercased, so a plain label entry in any case answers 404,
        // while `getAccessForWorld` (route 1) still reads the stored access.
        store::store_access_for_owner(
            &f.pool,
            &world,
            &owner.address,
            &AccessSetting::NftOwnership {
                nft: "urn:x".into(),
            },
        )
        .await
        .unwrap();
        let _denied = DenyListed::seed(&f.state.names, vec![world.label().to_uppercase()]);
        let body = expect(&f.app, delete(&world).signed(&owner).await, 404).await;
        assert_eq!(
            body["message"],
            json!(format!("World \"{}\" not found.", world.display()))
        );
        let path = format!("/world/{}/permissions", world.display());
        let body = expect(&f.app, Case::new("get", path), 200).await;
        assert_eq!(body["permissions"]["access"]["type"], "nft-ownership");
    }
    expect(&f.app, delete(&world).signed(&owner).await, 204).await;
    assert!(
        store::admin_set_world_blocked(&f.pool, &world, true)
            .await
            .unwrap()
    );
    let body = expect(&f.app, delete(&world).signed(&owner).await, 404).await;
    assert_eq!(
        body["message"],
        json!(format!("World \"{}\" not found.", world.display()))
    );
    assert!(
        store::admin_set_world_blocked(&f.pool, &world, false)
            .await
            .unwrap()
    );
    expect(&f.app, delete(&world).signed(&owner).await, 204).await;
}

/// `ACCESS_MAX_WALLETS` and `ACCESS_MAX_COMMUNITIES` reach the routes: the
/// full-document POST (communities first) and both single-item PUTs.
#[tokio::test]
async fn access_allow_list_caps_from_settings_bound_every_route() {
    let Some(f) = fixture_with_access_caps(1, 1).await else {
        return;
    };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let w = world.display();
    let access_path = format!("/world/{w}/permissions/access");
    let (a, b) = (Wallet::random().address, Wallet::random().address);
    let c1 = seed_community(&f.pool, &[&owner.address]).await;
    let c2 = seed_community(&f.pool, &[&owner.address]).await;
    for (meta, message) in [
        (
            json!({ "type": "allow-list", "wallets": [a.as_str(), b.as_str()], "communities": [c1, c2] }),
            "Too many communities. Maximum allowed is 1, but 2 were provided.",
        ),
        (
            json!({ "type": "allow-list", "wallets": [a.as_str(), b.as_str()] }),
            "Too many wallets in allow-list. Maximum allowed is 1, but 2 were provided.",
        ),
    ] {
        let case = Case::new("post", access_path.as_str())
            .signed_meta(&owner, &meta.to_string())
            .await;
        let body = expect(&f.app, case, 400).await;
        assert_eq!(body["message"], message, "{meta}");
    }
    let full = json!({ "type": "allow-list", "wallets": [a.as_str()], "communities": [c1] });
    let case = Case::new("post", access_path.as_str())
        .signed_meta(&owner, &full.to_string())
        .await;
    expect(&f.app, case, 204).await;
    for (path, message) in [
        (
            format!("{access_path}/{b}"),
            "Cannot add wallet: allow-list would exceed the maximum of 1 wallets.",
        ),
        (
            format!("{access_path}/communities/{c2}"),
            "Too many communities. Maximum allowed is 1, cannot add more.",
        ),
    ] {
        let case = Case::new("put", path.as_str()).signed(&owner).await;
        let body = expect(&f.app, case, 400).await;
        assert_eq!(body["message"], message, "{path}");
    }
}

/// `POST /world/:n/permissions/:p` checks the signature before the world
/// name, as the other owner-gated routes do.
#[tokio::test]
async fn posting_a_permission_checks_the_signature_before_the_world_name() {
    let Some(f) = fixture().await else { return };
    let path = "/world/not%20a%20world/permissions/deployment";
    let body = expect(&f.app, Case::new("post", path), 400).await;
    assert!(body.to_string().contains("Invalid Auth Chain"), "{body}");
    let signed = Case::new("post", path).signed(&Wallet::random()).await;
    let body = expect(&f.app, signed, 400).await;
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Invalid world name")),
        "{body}"
    );
}

/// Upstream runs the signed fetch as middleware, ahead of the schema
/// validator and the handler's path checks (`routes.ts:269-314`).
#[tokio::test]
async fn owner_gated_permission_routes_check_the_signature_first() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let w = world.display();
    let parcels = format!("/world/{w}/permissions/bogus/address/zzz/parcels");
    let cases = [
        Case::new("post", parcels.as_str()).json(&json!({})),
        Case::new("delete", parcels.as_str()).body(b"x".to_vec(), "text/plain"),
        Case::new("put", format!("/world/{w}/permissions/bogus/zzz")),
        Case::new("delete", format!("/world/{w}/permissions/bogus/zzz")),
        Case::new(
            "put",
            format!("/world/{w}/permissions/access/communities/not-a-uuid"),
        ),
        Case::new(
            "delete",
            format!("/world/{w}/permissions/access/communities/%20"),
        ),
    ];
    for case in cases {
        let body = expect(&f.app, case, 400).await;
        assert!(body.to_string().contains("Invalid Auth Chain"), "{body}");
    }
}

/// Upstream's router tries each route matching the path in turn; paths
/// that fall between `access/communities/:id`, `:permission_name/parcels`
/// and `:permission_name/:address` land where upstream's do
/// (`get-addresses-for-parcel-permission.spec.ts:51-62`).
#[tokio::test]
async fn permission_paths_between_the_static_and_named_routes_route_like_upstream() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let w = world.display();
    let body = expect(
        &f.app,
        Case::new("post", format!("/world/{w}/permissions/access/parcels"))
            .json(&json!({ "parcels": ["0,0"] })),
        400,
    )
    .await;
    assert_eq!(
        body["message"],
        "Permission 'access' does not support allow-list. Only 'deployment' and 'streaming' do."
    );
    for (method, path, address) in [
        ("put", "deployment/parcels", "parcels"),
        ("delete", "streaming/parcels", "parcels"),
        ("put", "access/communities", "communities"),
        ("delete", "access/communities", "communities"),
    ] {
        let case = Case::new(method, format!("/world/{w}/permissions/{path}"))
            .signed(&owner)
            .await;
        let body = expect(&f.app, case, 400).await;
        assert_eq!(
            body["message"],
            json!(format!("Invalid address: {address}.")),
            "{method} {path}"
        );
    }
}

/// `get-parcels.spec.ts:150-158`, `delete-parcels.spec.ts:137-151`: the
/// address as sent.
#[tokio::test]
async fn a_missing_parcel_grant_names_the_address_as_sent() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let grantee = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let w = world.display();
    let a = grantee.display();
    let path = format!("/world/{w}/permissions/deployment/address/{a}/parcels");
    let body = expect(&f.app, Case::new("get", path.as_str()), 404).await;
    assert_eq!(
        body["message"],
        json!(format!(
            "Address {a} does not have deployment permission for world {w}."
        ))
    );
    let case = Case::new("delete", path.as_str())
        .signed(&owner)
        .await
        .json(&json!({ "parcels": ["0,0"] }));
    let body = expect(&f.app, case, 400).await;
    assert_eq!(
        body["message"],
        json!(format!(
            "Permission not found. Address {a} does not have deployment permission for world {w}."
        ))
    );
}

/// The whole worlds router as `serve` mounts it, over the fixture's pool and
/// blob store.
async fn served_app(f: &Fixture) -> Router {
    let (worlds, blobs) = served_states(f);
    served_app_over(f, worlds, blobs).await
}

/// The read states `served_app` mounts, for a test that seeds them first.
fn served_states(f: &Fixture) -> (Arc<crate::WorldsState>, Arc<crate::content::ContentState>) {
    served_states_with(f, crate::Settings::for_tests("https://worlds.example.org"))
}

fn served_states_with(
    f: &Fixture,
    settings: crate::Settings,
) -> (Arc<crate::WorldsState>, Arc<crate::content::ContentState>) {
    let worlds = Arc::new(crate::WorldsState::new(
        f.pool.clone(),
        f.storage.clone(),
        settings,
        None,
        crate::http::Outbound::for_tests(),
    ));
    let blobs = Arc::new(crate::content::ContentState::new(
        f.pool.clone(),
        crate::content::Settings::for_tests(),
        f.storage.clone(),
    ));
    (worlds, blobs)
}

async fn served_app_over(
    f: &Fixture,
    worlds: Arc<crate::WorldsState>,
    blobs: Arc<crate::content::ContentState>,
) -> Router {
    let stage5 = crate::Stage5 {
        core: f.state.clone(),
        storage: Arc::new(crate::storage::StorageState::new(
            f.pool.clone(),
            f.state.policy.clone(),
            crate::storage::Settings::for_tests(None),
            None,
        )),
        scene_host: None,
        fed: crate::fed::FedState::boot(f.pool.clone(), Default::default(), None)
            .await
            .unwrap(),
        writes_enabled: true,
    };
    crate::app_router(worlds, blobs, &stage5)
}

/// The deploy clients ask the worlds host which files it already holds
/// before `POST /entities`; a file a world deploy stored is reported held.
#[tokio::test]
async fn available_content_under_the_worlds_prefix_reports_deployed_files() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let d = deployment(&owner, &world, "Held World", "0,0", &["0,0"]).await;
    post_entity(&f.app, &d, 200).await;
    let missing = hash_bytes_v1(Uuid::new_v4().as_bytes());
    let app = served_app(&f).await;

    let body = expect(
        &app,
        Case::new(
            "get",
            format!(
                "/worlds-content-server/available-content?cid={}&cid={missing}",
                d.thumb_hash
            ),
        ),
        200,
    )
    .await;
    assert_eq!(
        body,
        json!([
            { "cid": d.thumb_hash, "available": true },
            { "cid": missing, "available": false },
        ])
    );

    let body = expect(
        &app,
        Case::new("get", "/worlds-content-server/available-content"),
        400,
    )
    .await;
    assert_eq!(
        body,
        json!({ "error": "Bad request", "message": "At least one cid query parameter is required." })
    );

    let body = expect(
        &app,
        Case::new(
            "get",
            format!(
                "/worlds-content-server/available-content?cid={}&cid=bafkreinotacid",
                d.thumb_hash
            ),
        ),
        400,
    )
    .await;
    assert_eq!(
        body,
        json!({ "error": "Bad request", "message": "Invalid cid format." })
    );

    let too_many = vec![format!("cid={missing}"); 501].join("&");
    let body = expect(
        &app,
        Case::new(
            "get",
            format!("/worlds-content-server/available-content?{too_many}"),
        ),
        400,
    )
    .await;
    assert_eq!(
        body,
        json!({ "error": "Bad request", "message": "Too many cid query parameters. Maximum allowed is 500." })
    );

    let preflight = app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/worlds-content-server/available-content")
                .header(header::ORIGIN, "https://creator.example.org")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://creator.example.org"
    );
}

async fn served(app: &Router, method: &str, path: &str) -> (StatusCode, Option<String>, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let ty = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ty, body.to_vec())
}

/// WCS answers `/contents` (and `/ipfs`) with a sniffed Content-Type and
/// also under a thumbnail's legacy SHA-256 hex key.
#[tokio::test]
async fn worlds_contents_sniff_and_answer_legacy_sha256_keys() {
    let Some(f) = fixture().await else { return };
    let app = served_app(&f).await;
    // `file-type` reads a PNG's chunks up to its IDAT: an IHDR, then an IDAT of 16 unique bytes.
    let png = bytes::Bytes::from(
        [
            &PNG_MAGIC[..],
            b"\0\0\0\x0dIHDR",
            &[0; 17],
            b"\0\0\0\x10IDAT",
            Uuid::new_v4().as_bytes(),
            &[0; 4],
        ]
        .concat(),
    );
    let png_cid = hash_bytes_v1(&png);
    f.storage.store(&png_cid, png.clone()).await.unwrap();
    // sha256("abc"), stored under the raw-leaf CIDv1 a deploy or import gives it.
    let abc_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    f.storage
        .store(&hash_bytes_v1(b"abc"), bytes::Bytes::from_static(b"abc"))
        .await
        .unwrap();

    for prefix in ["contents", "ipfs"] {
        for method in ["GET", "HEAD"] {
            let path = format!("{NEST}/{prefix}/{png_cid}");
            let (status, ty, _) = served(&app, method, &path).await;
            assert_eq!(status, StatusCode::OK, "{method} {path}");
            assert_eq!(ty.as_deref(), Some("image/png"), "{method} {path}");
            let path = format!("{NEST}/{prefix}/{abc_hex}");
            let (status, _, _) = served(&app, method, &path).await;
            assert_eq!(status, StatusCode::OK, "{method} {path}");
        }
    }
    let (_, _, body) = served(&app, "GET", &format!("{NEST}/contents/{abc_hex}")).await;
    assert_eq!(body, b"abc");
    let (status, _, _) = served(
        &app,
        "GET",
        &format!("{NEST}/contents/{}", abc_hex.to_uppercase()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// WR-06: the prefix-only reads carry the CORS layer the worlds routes do (a
/// cross-origin GET is answered, a preflight is a 204), as upstream's
/// server-wide CORS answers every path.
#[tokio::test]
async fn worlds_prefix_reads_answer_cors_and_preflights() {
    let Some(f) = fixture().await else { return };
    let app = served_app(&f).await;
    let png = bytes::Bytes::from([&PNG_MAGIC[..], Uuid::new_v4().as_bytes()].concat());
    let png_cid = hash_bytes_v1(&png);
    f.storage.store(&png_cid, png).await.unwrap();
    let origin = "https://explorer.example.org";
    for path in [
        format!("{NEST}/status"),
        format!("{NEST}/contents/{png_cid}"),
        format!("{NEST}/ipfs/{png_cid}"),
        format!("{NEST}/openapi.json"),
        format!("{NEST}/available-content?cid={png_cid}"),
        format!("{NEST}/health/live"),
    ] {
        for method in ["GET", "OPTIONS"] {
            let (status, headers, _) =
                served_with(&app, method, &path, &[("origin", origin)]).await;
            let want = if method == "GET" {
                StatusCode::OK
            } else {
                StatusCode::NO_CONTENT
            };
            assert_eq!(status, want, "{method} {path}");
            assert_eq!(
                headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                origin,
                "{method} {path}"
            );
        }
    }
}

async fn served_with(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, body.to_vec())
}

/// `content-file-handler.ts`: a miss is a bare 404 (WR-03), HEAD ignores
/// `Range` (WR-05), and `file-type`'s answers for fonts and ISO-BMFF brands
/// (WR-04).
#[tokio::test]
async fn worlds_contents_answer_wcs_misses_heads_and_types() {
    let Some(f) = fixture().await else { return };
    let app = served_app(&f).await;
    let missing = hash_bytes_v1(Uuid::new_v4().as_bytes());
    for path in [
        format!("{NEST}/contents/{missing}"),
        format!("{NEST}/ipfs/{missing}"),
        format!("{NEST}/contents/not-a-key"),
    ] {
        let (status, _, body) = served_with(&app, "GET", &path, &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            body.is_empty(),
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let woff2 = [&b"wOF2\x00\x01\x00\x00"[..], Uuid::new_v4().as_bytes()].concat();
    let woff2_cid = hash_bytes_v1(&woff2);
    f.storage
        .store(&woff2_cid, bytes::Bytes::from(woff2.clone()))
        .await
        .unwrap();
    let path = format!("{NEST}/contents/{woff2_cid}");
    let range = [("range", "bytes=0-3")];
    let (status, headers, _) = served_with(&app, "HEAD", &path, &range).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], woff2.len().to_string());
    assert!(headers.get(header::CONTENT_RANGE).is_none());
    assert_eq!(headers[header::CONTENT_TYPE], "font/woff2");
    let (status, headers, body) = served_with(&app, "GET", &path, &range).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"wOF2");
    assert_eq!(headers[header::CONTENT_TYPE], "font/woff2");

    let m4a = [
        &b"\x00\x00\x00\x20ftypM4A \x00\x00\x00\x00"[..],
        Uuid::new_v4().as_bytes(),
    ]
    .concat();
    let m4a_cid = hash_bytes_v1(&m4a);
    f.storage
        .store(&m4a_cid, bytes::Bytes::from(m4a))
        .await
        .unwrap();
    let (_, ty, _) = served(&app, "GET", &format!("{NEST}/contents/{m4a_cid}")).await;
    assert_eq!(ty.as_deref(), Some("audio/x-m4a"));
}

/// `URLSearchParams.getAll('cid')` reads only `cid` (WR-24), and every
/// requested cid is listed, a denylisted one as not held (WR-11).
#[tokio::test]
async fn available_content_reads_literal_cids_and_lists_denylisted_ones() {
    let Some(f) = fixture().await else { return };
    let held = bytes::Bytes::from(Uuid::new_v4().as_bytes().to_vec());
    let held_cid = hash_bytes_v1(&held);
    f.storage.store(&held_cid, held).await.unwrap();
    let denied = bytes::Bytes::from(Uuid::new_v4().as_bytes().to_vec());
    let denied_cid = hash_bytes_v1(&denied);
    f.storage.store(&denied_cid, denied).await.unwrap();
    let (worlds, blobs) = served_states(&f);
    blobs.denylist.add(&denied_cid);
    let app = served_app_over(&f, worlds, blobs).await;

    let body = expect(
        &app,
        Case::new(
            "get",
            format!("{NEST}/available-content?cid%5B%5D={held_cid}&cid%5B0%5D={held_cid}"),
        ),
        400,
    )
    .await;
    assert_eq!(
        body,
        json!({ "error": "Bad request", "message": "At least one cid query parameter is required." })
    );
    let body = expect(
        &app,
        Case::new(
            "get",
            format!(
                "{NEST}/available-content?cid={denied_cid}&cid={held_cid}&cid%5B%5D=x&cid={denied_cid}"
            ),
        ),
        200,
    )
    .await;
    assert_eq!(
        body,
        json!([
            { "cid": denied_cid, "available": false },
            { "cid": held_cid, "available": true },
        ])
    );
}

/// `/index` (`worlds-indexer.ts`): a deny-listed world is listed, as upstream's
/// filter (full names against suffix-stripped entries) lists it (WR-17),
/// missing fields are `''`, left out or `Date.now()` (WR-18), and the
/// thumbnail links the worlds `/contents` (WR-12).
#[tokio::test]
async fn index_lists_deny_listed_worlds_and_answers_upstream_fields() {
    let Some(f) = fixture().await else { return };
    let (listed, banned, blocked) = (world_name(), world_name(), world_name());
    for world in [&listed, &banned, &blocked] {
        let owner = Wallet::random();
        f.names.set(&owner.address, world.label());
        let d = deployment(&owner, world, "Indexed", "0,0", &["0,0"]).await;
        post_entity(&f.app, &d, 200).await;
    }
    // `getBannedNames` holds `BLOCKED_WORLD_NAMES` as configured (lowercased)
    // and the list entries suffix-stripped; both compare with the full name.
    let (worlds, blobs) = served_states_with(
        &f,
        crate::Settings {
            blocked_world_names: format!(" {} ,", blocked.display().to_uppercase()),
            ..crate::Settings::for_tests("https://worlds.example.org")
        },
    );
    worlds.names.seed(vec![banned.label().to_uppercase()]);
    let app = served_app_over(&f, worlds, blobs).await;

    // A page size of its own: the index memo is process-wide (other tests'
    // worlds may share the page).
    let body = expect(&app, Case::new("get", "/index?limit=9871"), 200).await;
    let entry = |world: &WorldName| {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == world.key())
            .cloned()
    };
    assert!(entry(&banned).is_some(), "{body}");
    assert!(entry(&blocked).is_none(), "{body}");
    let listed = entry(&listed).expect("the allowed world is listed");
    let scene = &listed["scenes"][0];
    assert_eq!(scene["title"], "Indexed");
    assert_eq!(scene["description"], "");
    assert!(scene.get("runtimeVersion").is_none(), "{scene}");
    assert!(scene["timestamp"].as_i64().unwrap() > 0);
    let thumb = scene["thumbnail"].as_str().unwrap();
    assert!(
        thumb.starts_with("https://worlds.example.org/worlds-content-server/contents/"),
        "{thumb}"
    );
}

/// `getEntityForWorlds`: a pointer is looked up as sent, so surrounding
/// whitespace finds nothing (WR-14), and `metadata.owner` is the row's owner,
/// `null` when it has none (WR-15).
#[tokio::test]
async fn active_entities_look_up_pointers_as_sent_and_fold_a_null_owner() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let d = deployment(&owner, &world, "Active", "0,0", &["0,0"]).await;
    post_entity(&f.app, &d, 200).await;
    let active = |pointer: String| {
        Case::new("post", "/entities/active").json(&json!({ "pointers": [pointer] }))
    };

    let body = expect(&f.app, active(format!(" {}", world.key())), 200).await;
    assert_eq!(body, json!([]));
    sqlx::query("UPDATE worlds.worlds SET owner = NULL WHERE name = $1")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();
    let body = expect(&f.app, active(world.display().to_uppercase()), 200).await;
    assert_eq!(body[0]["id"], json!(d.entity_id));
    assert_eq!(body[0]["metadata"]["owner"], Value::Null);
}

/// `index-handler.ts` reads `searchParams.get('limit')` and
/// `searchParams.get('offset')`, the first of a repeated key, so a repeated
/// key is not a 400 and pages as its first values alone. With at least three
/// indexed worlds, the last values (2, 0) or a last limit (2, 1) would list
/// two worlds, and a last offset (1, 0) another page than (1, 1).
#[tokio::test]
async fn index_reads_the_first_of_a_repeated_query_key() {
    let Some(f) = fixture().await else { return };
    for _ in 0..3 {
        let (owner, world) = (Wallet::random(), world_name());
        f.names.set(&owner.address, world.label());
        let d = deployment(&owner, &world, "Indexed", "0,0", &["0,0"]).await;
        post_entity(&f.app, &d, 200).await;
    }
    let app = served_app(&f).await;
    let page = |query: &str| Case::new("get", format!("{NEST}/index?{query}"));
    let repeated = expect(&app, page("limit=1&limit=2&offset=1&offset=0"), 200).await;
    let first = expect(&app, page("limit=1&offset=1"), 200).await;
    assert_eq!(repeated["data"].as_array().unwrap().len(), 1, "{repeated}");
    assert_eq!(repeated["data"], first["data"]);
}

/// `checkNameDenyList` refuses a `BLOCKED_WORLD_NAMES` entry (trimmed,
/// lowercased, the full name), so `POST /entities/active` leaves it out.
#[tokio::test]
async fn active_entities_skip_a_blocked_world_name() {
    let Some(f) = fixture().await else { return };
    let (allowed, blocked) = (world_name(), world_name());
    for world in [&allowed, &blocked] {
        let owner = Wallet::random();
        f.names.set(&owner.address, world.label());
        let d = deployment(&owner, world, "Active", "0,0", &["0,0"]).await;
        post_entity(&f.app, &d, 200).await;
    }
    let blocking = fixture_on_pool(
        f.pool.clone(),
        PersonalWorldsPolicy::disabled(),
        None,
        None,
        test_settings(),
        &format!(" , {} ", blocked.display().to_uppercase()),
    )
    .await;
    let pointers = json!({ "pointers": [allowed.display(), blocked.display()] });
    let body = expect(
        &blocking.app,
        Case::new("post", "/entities/active").json(&pointers),
        200,
    )
    .await;
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            e["metadata"]["worldConfiguration"]["name"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(names, vec![allowed.display()]);
}

/// `BLOCKED_WORLD_NAMES` (`name-deny-list-checker.ts:8-13,45-49,61-65`): a
/// whole configured name is refused by `/about` (`world-about-handler.ts:22-24`),
/// `/manifest` (`world-manifest-handler.ts:33-35`) and `/limits`
/// (`world-limits-handler.ts:54-56`), and `GET /worlds` leaves it out of the
/// page and the total (`worlds-manager.ts:1371,1409-1413`); the same label
/// under another suffix is not blocked.
#[tokio::test]
async fn configured_world_names_are_hidden_from_the_world_reads() {
    let Some(f) = fixture().await else { return };
    let blocked = world_name();
    let sibling = format!("{}.eth", blocked.label());
    let owner = Wallet::random();
    f.names.set(&owner.address, blocked.label());
    let d = deployment(&owner, &blocked, "Blocked", "0,0", &["0,0"]).await;
    post_entity(&f.app, &d, 200).await;
    sqlx::query("INSERT INTO worlds.worlds (name) VALUES ($1)")
        .bind(&sibling)
        .execute(&f.pool)
        .await
        .unwrap();

    let configured = format!(" {} , ", blocked.display().to_uppercase());
    let blocking = fixture_on_pool(
        f.pool.clone(),
        PersonalWorldsPolicy::disabled(),
        None,
        None,
        test_settings(),
        &configured,
    )
    .await;
    let (worlds, blobs) = served_states_with(
        &blocking,
        crate::Settings {
            blocked_world_names: configured.clone(),
            ..crate::Settings::for_tests("https://worlds.example.org")
        },
    );
    let app = served_app_over(&blocking, worlds, blobs).await;
    let name = blocked.display();

    let body = expect(&app, Case::new("get", format!("/world/{name}/about")), 404).await;
    assert_eq!(
        body,
        json!({"error": "Not Found", "message": format!("World \"{name}\" has no scene deployed.")})
    );
    let body = expect(
        &app,
        Case::new("get", format!("/world/{name}/manifest")),
        404,
    )
    .await;
    assert_eq!(body["message"], format!("World \"{name}\" not found."));
    let body = expect(&app, Case::new("get", format!("/world/{name}/limits")), 404).await;
    assert_eq!(
        body["message"],
        format!("World \"{}\" not found.", blocked.key())
    );

    let body = expect(
        &app,
        Case::new("get", format!("/worlds?search={}", blocked.label())),
        200,
    )
    .await;
    let names: Vec<&str> = body["worlds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec![sibling.as_str()], "{body}");
    assert_eq!(body["total"], 1);

    // The unblocked node serves the same world.
    expect(
        &f.app,
        Case::new("get", format!("/world/{name}/manifest")),
        200,
    )
    .await;
}

/// Upstream builds one `nameDenyListChecker` (`src/components.ts:166`) and hands it to
/// `isWorldValid`, the world reads and deploy validation, so they answer from one memo. The core
/// reads the policy's list, so an entry the core's memo holds refuses the comms admission at once
/// (a second list would read the DAO table, which holds no such entry, and admit the world).
#[tokio::test]
async fn the_core_and_the_comms_admission_share_one_name_deny_list() {
    use crate::comms::interfaces::AccessPolicy;
    use crate::policy::{AccessType, WorldAdmission};
    let Some(f) = fixture().await else { return };
    assert!(Arc::ptr_eq(&f.state.names, f.state.policy.names()));
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    let manifest = || Case::new("get", format!("/world/{}/manifest", world.display()));
    let now = chrono::Utc::now().naive_utc();
    assert_eq!(
        f.state.policy.world_admission(&world, now).await.unwrap(),
        WorldAdmission::Open(AccessType::Unrestricted)
    );
    expect(&f.app, manifest(), 200).await;

    let _denied = DenyListed::seed(&f.state.names, vec![world.label().to_string()]);
    assert_eq!(
        f.state.policy.world_admission(&world, now).await.unwrap(),
        WorldAdmission::Unknown
    );
    expect(&f.app, manifest(), 404).await;
}

/// `BLOCKED_WORLD_NAMES` on the permission routes: `getAccessForWorld` reads
/// a blocked world as the default `unrestricted` (`getBannedNames` keeps the
/// whole configured name, `worlds-manager.ts:327,345-349`), and the owner's
/// DELETE answers 404 through `isWorldValid` (`component.ts:71-74`).
#[tokio::test]
async fn configured_world_names_are_invalid_on_the_permission_routes() {
    let Some(f) = fixture().await else { return };
    let owner = Wallet::random();
    let world = owned_world(&f, &owner).await;
    store::store_access_for_owner(
        &f.pool,
        &world,
        &owner.address,
        &AccessSetting::NftOwnership {
            nft: "urn:x".into(),
        },
    )
    .await
    .unwrap();
    let blocking = fixture_on_pool(
        f.pool.clone(),
        PersonalWorldsPolicy::disabled(),
        None,
        None,
        test_settings(),
        &format!(" , {} ", world.display().to_uppercase()),
    )
    .await;
    blocking.names.set(&owner.address, world.label());
    let w = world.display();
    let delete = || {
        Case::new(
            "delete",
            format!(
                "/world/{w}/permissions/deployment/{}",
                Wallet::random().address.as_str()
            ),
        )
    };
    let read = || Case::new("get", format!("/world/{w}/permissions"));

    let body = expect(&blocking.app, read(), 200).await;
    assert_eq!(body["permissions"]["access"]["type"], "unrestricted");
    let body = expect(&blocking.app, delete().signed(&owner).await, 404).await;
    assert_eq!(body["message"], json!(format!("World \"{w}\" not found.")));

    // The unblocked node reads the stored access and accepts the DELETE.
    let body = expect(&f.app, read(), 200).await;
    assert_eq!(body["permissions"]["access"]["type"], "nft-ownership");
    expect(&f.app, delete().signed(&owner).await, 204).await;
}

/// `JSON.stringify` of `JSON.parse(<jsonb text>)`: an object's members in
/// `jsonb`'s order (key length, then bytes).
fn jsonb_js_members(object: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort_by(|a, b| (a.len(), a.as_bytes()).cmp(&(b.len(), b.as_bytes())));
    keys.into_iter()
        .map(|k| (k.clone(), jsonb_js_text(&object[k])))
        .collect()
}

fn jsonb_js_text(value: &Value) -> String {
    match value {
        Value::Object(object) => js_object(jsonb_js_members(object)),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(jsonb_js_text)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => other.to_string(),
    }
}

fn js_object(members: Vec<(String, String)>) -> String {
    let members: Vec<String> = members
        .into_iter()
        .map(|(k, v)| format!("{}:{v}", Value::String(k)))
        .collect();
    format!("{{{}}}", members.join(","))
}

/// `{...members, [key]: value}`.
fn js_spread(
    mut members: Vec<(String, String)>,
    key: &str,
    value: String,
) -> Vec<(String, String)> {
    match members.iter_mut().find(|(k, _)| k == key) {
        Some(member) => member.1 = value,
        None => members.push((key.to_string(), value)),
    }
    members
}

/// `getEntityForWorlds` answers `{...row.entity, id, metadata: {...metadata,
/// owner}}` over node-postgres' parse of the `jsonb` column: the body bytes
/// keep the stored member order with `owner` last in the metadata (WR-15).
#[tokio::test]
async fn active_entities_answer_upstream_member_order() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let d = deployment(&owner, &world, "Ordered", "0,0", &["0,0"]).await;
    post_entity(&f.app, &d, 200).await;

    let (stored, stored_owner): (Value, String) = sqlx::query_as(
        "SELECT ws.entity, w.owner FROM worlds.world_scenes ws JOIN worlds.worlds w ON w.name = ws.world_name WHERE ws.world_name = $1",
    )
    .bind(world.key())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let stored = stored.as_object().unwrap();
    let metadata = js_spread(
        jsonb_js_members(stored["metadata"].as_object().unwrap()),
        "owner",
        Value::String(stored_owner).to_string(),
    );
    let entity = js_spread(
        js_spread(
            jsonb_js_members(stored),
            "id",
            Value::String(d.entity_id.clone()).to_string(),
        ),
        "metadata",
        js_object(metadata),
    );
    let expected = format!("[{}]", js_object(entity));

    let request = Request::builder()
        .method("POST")
        .uri("/entities/active")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "pointers": [world.display()] }).to_string(),
        ))
        .unwrap();
    let response = f.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), expected);
    assert!(expected.contains(r#","owner":"0x"#), "{expected}");
}

/// WCS's status checks answer under the worlds prefix.
#[tokio::test]
async fn worlds_health_probes_answer_under_the_prefix() {
    let Some(f) = fixture().await else { return };
    let app = served_app(&f).await;
    let (status, _, body) = served(&app, "GET", &format!("{NEST}/health/live")).await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"alive"[..]));
    for probe in ["ready", "startup"] {
        let (status, ty, body) = served(&app, "GET", &format!("{NEST}/health/{probe}")).await;
        assert_eq!(status, StatusCode::OK, "{probe}");
        assert_eq!(
            ty.as_deref(),
            Some("application/health+json; charset=utf-8")
        );
        assert_eq!(body, crate::shared::health::PASS.as_bytes());
    }
    let (status, _, _) = served(&app, "GET", "/health/live").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn cid(tag: char) -> String {
    format!("ba{}", tag.to_string().repeat(57))
}

#[tokio::test]
async fn world_scenes_page_filter_and_count_like_the_old_listing() {
    let Some(f) = fixture().await else { return };
    let app = served_app(&f).await;
    let (world, owner, stranger) = (world_name(), Wallet::random(), Wallet::random());
    let w = world.display().to_string();
    let (a, b, c) = (cid('a'), cid('b'), cid('c'));
    for (i, (id, footprint)) in [
        (&a, &["0,0"][..]),
        (&b, &["5,5", "5,6"][..]),
        (&c, &["-3,2"][..]),
    ]
    .into_iter()
    .enumerate()
    {
        deploy(
            &f,
            &world,
            &owner.address,
            id,
            &scene_at(1000 + i as i64, footprint),
            footprint,
            &Deploy::Owner,
        )
        .await
        .unwrap();
    }
    let ids = |body: &Value| -> Vec<String> {
        body["scenes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["entityId"].as_str().unwrap().to_string())
            .collect()
    };
    let list = |query: &str| Case::new("get", format!("/world/{w}/scenes{query}"));

    let page = expect(&app, list("?limit=2"), 200).await;
    assert_eq!(
        (ids(&page), page["total"].clone()),
        (vec![a.clone(), b.clone()], json!(3))
    );
    let page = expect(&app, list("?limit=2&offset=2"), 200).await;
    assert_eq!(
        (ids(&page), page["total"].clone()),
        (vec![c.clone()], json!(3))
    );
    let page = expect(&app, list("?limit=abc&offset=-1"), 200).await;
    assert_eq!(ids(&page).len(), 3);
    assert!(
        page["scenes"][0]["createdAt"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );

    let page = expect(&app, list("?x1=6&x2=4&y1=4&y2=6"), 200).await;
    assert_eq!(
        (ids(&page), page["total"].clone()),
        (vec![b.clone()], json!(1))
    );
    assert_eq!(
        expect(&app, list("?x1=1"), 400).await,
        json!({ "error": "Bad request", "message": "Bounding box requires all of x1, x2, y1, y2 to be provided." })
    );
    let page = expect(&app, list(&format!("?entity_id={c}")), 200).await;
    assert_eq!(ids(&page), vec![c.clone()]);
    expect(&app, list("?entity_id=Qm1"), 400).await;
    expect(&app, list("?authorized_deployer=0x12"), 400).await;

    let by = |who: &Wallet| list(&format!("?authorized_deployer={}", who.display()));
    assert_eq!(expect(&app, by(&owner), 200).await["total"], json!(3));
    assert_eq!(expect(&app, by(&stranger), 200).await["total"], json!(0));
    grant_parcels(&f.pool, &world, &stranger.address, &["9,9"]).await;
    assert_eq!(expect(&app, by(&stranger), 200).await["total"], json!(3));

    let post = |body: Value| Case::new("post", format!("/world/{w}/scenes")).json(&body);
    let page = expect(&app, post(json!({ "coordinates": ["5,06", "-3,2"] })), 200).await;
    assert_eq!(
        (ids(&page), page["total"].clone()),
        (vec![b.clone(), c.clone()], json!(2))
    );
    let page = expect(
        &app,
        Case::new("post", format!("/world/{w}/scenes?limit=1&offset=1"))
            .json(&json!({ "coordinates": ["0,0", "5,5"] })),
        200,
    )
    .await;
    assert_eq!((ids(&page), page["total"].clone()), (vec![b], json!(2)));
    // `@dcl/schema-validator-component` 1.0.1's bodies (`scenes-handler.spec.ts:153-156` asserts `message`).
    assert_eq!(
        expect(&app, post(json!({ "coordinates": [] })), 400).await,
        json!({ "ok": false, "message": "Invalid JSON body", "data": [{
            "instancePath": "/coordinates", "schemaPath": "#/properties/coordinates/minItems",
            "keyword": "minItems", "params": { "limit": 1 }, "message": "must NOT have fewer than 1 items" }] })
    );
    assert_eq!(
        expect(
            &app,
            Case::new("post", format!("/world/{w}/scenes")).body(b"{}".to_vec(), "text/plain"),
            415,
        )
        .await,
        json!({ "ok": false, "message": "Content-Type must be application/json" })
    );
    assert_eq!(
        expect(
            &app,
            Case::new("post", format!("/world/{w}/scenes")).body(b"".to_vec(), "application/json"),
            400,
        )
        .await,
        json!({ "ok": false, "message": "Unexpected end of JSON input" })
    );
    let page = expect(&app, post(json!({ "coordinates": ["99999999999,0"] })), 200).await;
    assert_eq!(
        page["total"],
        json!(0),
        "a coordinate past i32 matches no scene"
    );
    // The Content-Type is checked before the body is read; a body over the
    // cap answers `@dcl/http-server`'s own 413.
    let over = vec![b' '; crate::scenes::MAX_SCENES_QUERY_BODY_BYTES + 1];
    assert_eq!(
        expect(
            &app,
            Case::new("post", format!("/world/{w}/scenes")).body(over.clone(), "text/plain"),
            415,
        )
        .await,
        json!({ "ok": false, "message": "Content-Type must be application/json" })
    );
    let (status, body) = hit(
        &app,
        Case::new("post", format!("/world/{w}/scenes")).body(over, "application/json"),
    )
    .await;
    assert_eq!((status.as_u16(), body), (413, json!("Payload Too Large")));
}

#[tokio::test]
async fn wallet_contribute_lists_the_signer_grants() {
    let Some(f) = fixture().await else { return };
    let (scoped, wide, contributor) = (world_name(), world_name(), Wallet::random());
    grant_parcels(&f.pool, &scoped, &contributor.address, &["0,0", "0,1"]).await;
    grant_parcels(&f.pool, &wide, &contributor.address, &[]).await;
    sqlx::query(
        "INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'streaming', $2)",
    )
    .bind(wide.key())
    .bind(&contributor.address)
    .execute(&f.pool)
    .await
    .unwrap();

    expect(&f.app, Case::new("get", "/wallet/contribute"), 400).await;
    let body = expect(
        &f.app,
        Case::new("get", "/wallet/contribute")
            .signed(&contributor)
            .await,
        200,
    )
    .await;
    assert_eq!(body["count"], json!(2));
    let mut domains = body["domains"].as_array().unwrap().clone();
    domains.sort_by_key(|d| d["name"].as_str().unwrap().to_string());
    let expected = |w: &WorldName, perms: Value, parcels: i64| json!({ "name": w.key(), "user_permissions": perms, "size": "0", "owner": null, "parcelCount": parcels });
    let mut want = vec![
        expected(&scoped, json!(["deployment"]), 2),
        expected(&wide, json!(["deployment", "streaming"]), 0),
    ];
    want.sort_by_key(|d| d["name"].as_str().unwrap().to_string());
    assert_eq!(domains, want);
}

#[tokio::test]
async fn wallet_stats_report_names_space_and_block() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (held, other, owner) = (world_name(), world_name(), Wallet::random());
    f.names.set(&owner.address, held.label());
    f.names.set(&owner.address, other.label());
    deploy(
        &f,
        &held,
        &owner.address,
        &cid('d'),
        &scene_at(1, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let path = format!("/wallet/{}/stats", owner.address.as_str());
    let body = expect(&f.app, Case::new("get", &path), 200).await;
    assert_eq!(
        body,
        json!({
            "wallet": owner.address.as_str(),
            "dclNames": [{ "name": held.key(), "size": "1" }],
            "ensNames": [],
            "usedSpace": "1",
            "maxAllowedSpace": crate::policy::MAX_SCENE_SIZE_BYTES.to_string(),
        }),
        "an unlimited allowance reports the per-scene caps of its DCL worlds"
    );
    f.allowance.set(&owner.address, Some(5 * 1024 * 1024));
    let body = expect(&f.app, Case::new("get", &path), 200).await;
    assert_eq!(body["maxAllowedSpace"], json!("5242880"));
    // A platform ban is not a quota block.
    sqlx::query("INSERT INTO worlds.blocked_wallets (wallet) VALUES ($1)")
        .bind(&owner.address)
        .execute(&f.pool)
        .await
        .unwrap();
    let body = expect(&f.app, Case::new("get", &path), 200).await;
    assert_eq!(body.get("blockedSince"), None);
    sqlx::query(
        "INSERT INTO worlds.quota_blocks (wallet, created_at, updated_at) VALUES ($1, $2, $2)",
    )
    .bind(&owner.address)
    .bind(chrono::Utc::now().naive_utc())
    .execute(&f.pool)
    .await
    .unwrap();
    let body = expect(&f.app, Case::new("get", &path), 200).await;
    assert!(body["blockedSince"].as_str().unwrap().ends_with('Z'));
    // An allowance outage degrades to the same caps.
    f.allowance.set(&owner.address, None);
    let body = expect(&f.app, Case::new("get", &path), 200).await;
    assert_eq!(
        body["maxAllowedSpace"],
        json!(crate::policy::MAX_SCENE_SIZE_BYTES.to_string())
    );
    assert_eq!(
        expect(&f.app, Case::new("get", "/wallet/0x12/stats"), 400).await,
        json!({ "error": "Bad request", "message": "Invalid request. Missing or invalid wallet in request url param." })
    );
}

#[tokio::test]
async fn permissions_owner_falls_back_to_the_name_holder() {
    let Some(f) = fixture().await else { return };
    let (world, unheld, holder) = (world_name(), world_name(), Wallet::random());
    f.names.set(&holder.address, world.label());
    let body = expect(
        &f.app,
        Case::new("get", format!("/world/{}/permissions", world.display())),
        200,
    )
    .await;
    assert_eq!(body["owner"], json!(holder.address.as_str()));
    let body = expect(
        &f.app,
        Case::new("get", format!("/world/{}/permissions", unheld.display())),
        200,
    )
    .await;
    // `get-permissions.spec.ts`: `body.owner` is undefined.
    assert!(body.get("owner").is_none(), "{body}");
}

#[tokio::test]
async fn permissions_answer_503_while_the_owner_is_undetermined() {
    let Some((f, ens)) = fixture_with_ens(1 << 20).await else {
        return;
    };
    ens.set_down(true);
    let world = ens_world_name();
    expect(
        &f.app,
        Case::new("get", format!("/world/{}/permissions", world.display())),
        503,
    )
    .await;
}

#[tokio::test]
async fn worlds_listing_pages_leniently_with_millisecond_timestamps() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    deploy(
        &f,
        &world,
        &owner,
        &cid('e'),
        &scene_at(1, &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let path = format!("/worlds?limit=abc&offset=-4&search={}", world.label());
    let body = expect(&f.app, Case::new("get", path), 200).await;
    assert_eq!(body["total"], json!(1));
    let at = body["worlds"][0]["last_deployed_at"].as_str().unwrap();
    assert!(
        at.ends_with('Z') && at.len() == "2026-01-01T00:00:00.000Z".len(),
        "{at}"
    );
    expect(&f.app, Case::new("get", "/worlds?limit=5000"), 200).await;
}

/// `GET /worlds` as `worlds-handler.ts` reads its query, and the deny list as
/// `getWorlds` applies it: `name != ALL(<entries without .eth/.dcl>)`, which
/// hides a bare-name world and keeps `<banned>.dcl.eth`.
#[tokio::test]
async fn worlds_listing_reads_the_query_like_url_search_params_and_hides_banned_bare_names() {
    let Some(f) = fixture().await else { return };
    let label = format!("wl{}", &Uuid::new_v4().simple().to_string()[..10]);
    let ens = format!("{label}.dcl.eth");
    for name in [&label, &ens] {
        sqlx::query("INSERT INTO worlds.worlds (name) VALUES ($1)")
            .bind(name)
            .execute(&f.pool)
            .await
            .unwrap();
    }
    let names = |body: &Value| -> Vec<String> {
        let mut n: Vec<String> = body["worlds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["name"].as_str().unwrap().to_string())
            .collect();
        n.sort();
        n
    };
    let list = format!("/worlds?search={label}");
    let body = expect(&f.app, Case::new("get", &list), 200).await;
    assert_eq!(names(&body), vec![label.clone(), ens.clone()]);
    f.state.names.seed(vec![ens.clone()]);
    let body = expect(&f.app, Case::new("get", &list), 200).await;
    assert_eq!(
        names(&body),
        vec![ens.clone()],
        "the stripped entry hides the bare name only"
    );
    assert_eq!(body["total"], json!(1));
    f.state.names.seed(Vec::new());

    // The first of a repeated key, as `searchParams.get` reads it.
    let body = expect(
        &f.app,
        Case::new("get", format!("{list}&sort=name&sort=bogus&search=other")),
        200,
    )
    .await;
    assert_eq!(body["total"], json!(2));
    // `EthAddress.validate`: no `0X`, no surrounding whitespace.
    let address = Uuid::new_v4().simple().to_string() + "00000000";
    for bad in [format!("0X{address}"), format!("%200x{address}")] {
        expect(
            &f.app,
            Case::new("get", format!("/worlds?authorized_deployer={bad}")),
            400,
        )
        .await;
    }
    expect(
        &f.app,
        Case::new("get", format!("/worlds?authorized_deployer=0x{address}")),
        200,
    )
    .await;
    // The 64 limit counts UTF-16 units: 30 CJK characters are 90 bytes.
    let cjk = "%E6%97%A5".repeat(30);
    expect(
        &f.app,
        Case::new("get", format!("/worlds?search={cjk}")),
        200,
    )
    .await;
    let long = "a".repeat(65);
    assert_eq!(
        expect(
            &f.app,
            Case::new("get", format!("/worlds?search={long}")),
            400
        )
        .await["message"],
        "Invalid search parameter: must be at most 64 characters."
    );
}

/// A signed publish of `entity` uploading the entity file and `files`.
async fn signed_deployment(wallet: &Wallet, entity: &Value, files: &[&[u8]]) -> Deployment {
    let entity_bytes = serde_json::to_vec(entity).unwrap();
    let entity_id = hash_bytes_v1(&entity_bytes);
    let chain = chain_fields(&wallet.chain(&entity_id).await);
    let names: Vec<String> = files.iter().map(|f| hash_bytes_v1(f)).collect();
    let mut parts: Vec<(&str, Option<&str>, &[u8])> =
        vec![("entityId", None, entity_id.as_bytes())];
    parts.extend(chain.iter().map(|(k, v)| (k.as_str(), None, v.as_bytes())));
    parts.push((&entity_id, Some(&entity_id), &entity_bytes));
    for (name, file) in names.iter().zip(files) {
        parts.push((name, Some(name), file));
    }
    let (body, content_type) = multipart_body(&parts);
    Deployment {
        body,
        content_type,
        entity_id,
        thumb_hash: String::new(),
    }
}

/// A valid scene entity of `world` on `parcels`, listing `content`.
fn publish_entity(world: &WorldName, parcels: &[&str], content: Value) -> Value {
    json!({
        "version": "v3",
        "type": "scene",
        "timestamp": chrono::Utc::now().timestamp_millis(),
        "pointers": parcels,
        "content": content,
        "metadata": {
            "main": "bin/index.js",
            "worldConfiguration": { "name": world.display() },
            "scene": { "base": parcels[0], "parcels": parcels }
        }
    })
}

async fn refused(app: &Router, owner: &Wallet, entity: Value, files: &[&[u8]], message: &str) {
    let d = signed_deployment(owner, &entity, files).await;
    let body = post_entity(app, &d, 400).await;
    assert_eq!(body["error"], "Bad request");
    assert_eq!(body["message"], message, "{entity}");
}

#[tokio::test]
async fn a_publish_answers_the_first_failed_check_in_the_old_words() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random());
    f.names.set(&owner.address, world.label());
    let file: &[u8] = b"a content file";
    let hash = hash_bytes_v1(file);
    let listed = json!([{ "file": "a.bin", "hash": hash }]);
    let app = &f.app;

    let mut no_version = publish_entity(&world, &["0,0"], listed.clone());
    no_version.as_object_mut().unwrap().remove("version");
    let message = "Deployment failed: must have required property 'version'";
    refused(app, &owner, no_version, &[file], message).await;

    let mut no_main = publish_entity(&world, &["0,0"], listed.clone());
    no_main["metadata"].as_object_mut().unwrap().remove("main");
    let message = "Deployment failed: metadata must have required property 'main'";
    refused(app, &owner, no_main, &[file], message).await;

    let mut profile = publish_entity(&world, &["0,0"], listed.clone());
    profile["type"] = json!("profile");
    let message = "Deployment failed: Entity type profile is not supported.";
    refused(app, &owner, profile, &[file], message).await;

    let five = ["0,0", "0,1", "0,2", "0,3", "0,4"];
    let message = "Deployment failed: Max allowed scene dimensions is 4 parcels.";
    refused(
        app,
        &owner,
        publish_entity(&world, &five, listed.clone()),
        &[file],
        message,
    )
    .await;

    let extra: &[u8] = b"not listed";
    let message = format!(
        "Deployment failed: Extra file detected {}",
        hash_bytes_v1(extra)
    );
    let entity = publish_entity(&world, &["0,0"], listed.clone());
    refused(app, &owner, entity, &[file, extra], &message).await;

    let message = format!(
        "Deployment failed: The file {hash} (a.bin) is neither present in the storage or in the provided entity"
    );
    let entity = publish_entity(&world, &["0,0"], listed.clone());
    refused(app, &owner, entity, &[], &message).await;

    let mut minimap = publish_entity(&world, &["0,0"], listed.clone());
    minimap["metadata"]["worldConfiguration"]["miniMapConfig"] = json!({ "dataImage": "map.png" });
    let message = "Deployment failed: The file map.png is not present in the entity.";
    refused(app, &owner, minimap, &[file], message).await;

    let mut skybox = publish_entity(&world, &["0,0"], listed.clone());
    skybox["metadata"]["worldConfiguration"]["skyboxConfig"] = json!({ "textures": ["sky.png"] });
    let message = "Deployment failed: The texture file sky.png is not present in the entity.";
    refused(app, &owner, skybox, &[file], message).await;

    let max = super::deploy::DEFAULT_MAX_FILE_COUNT;
    let many = Value::Array(vec![listed[0].clone(); max + 1]);
    let message = format!(
        "Deployment failed: The deployment has too many files. The maximum allowed is {max} but the deployment has {}.",
        max + 1
    );
    refused(
        app,
        &owner,
        publish_entity(&world, &["0,0"], many),
        &[file],
        &message,
    )
    .await;
    assert!(deployed_entity_ids(&f.pool, &world).await.is_empty());

    let mut ok = publish_entity(&world, &["0,0"], listed.clone());
    ok["metadata"]["worldConfiguration"]["miniMapConfig"] = json!({ "dataImage": "a.bin" });
    ok["metadata"]["worldConfiguration"]["skyboxConfig"] = json!({ "textures": ["a.bin"] });
    let d = signed_deployment(&owner, &ok, &[file]).await;
    post_entity(app, &d, 200).await;
    assert_eq!(
        deployed_entity_ids(&f.pool, &world).await,
        vec![d.entity_id]
    );
}

async fn set_limit_override(
    pool: &PgPool,
    world: &WorldName,
    parcels: Option<i32>,
    size: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO worlds.world_limit_overrides (world_name, max_parcels, max_size_bytes) VALUES ($1, $2, $3) \
         ON CONFLICT (world_name) DO UPDATE SET max_parcels = $2, max_size_bytes = $3",
    )
    .bind(world.key())
    .bind(parcels)
    .bind(size)
    .execute(pool)
    .await
    .unwrap();
}

fn limits_case(world: &WorldName, query: &str) -> Case {
    Case::new("get", format!("/world/{}/limits{query}", world.display()))
}

#[tokio::test]
async fn world_limits_report_what_a_publish_is_checked_against() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random());
    f.names.set(&owner.address, world.label());
    let app = &f.app;

    assert_eq!(
        expect(app, limits_case(&world, ""), 200).await,
        json!({ "maxSceneSizeInBytes": crate::policy::MAX_SCENE_SIZE_BYTES.to_string(), "maxParcels": 4 })
    );
    expect(app, limits_case(&world, "?parcels=0,0&parcels=1,0"), 200).await;
    for bad in [
        "?parcels=0,0&parcels=0,0",
        "?parcels=00,1",
        "?parcels=151,0",
        "?parcels=",
    ] {
        expect(app, limits_case(&world, bad), 400).await;
    }

    let bare = WorldName::parse(world.label()).unwrap();
    expect(app, limits_case(&bare, ""), 404).await;

    let unowned = world_name();
    expect(app, limits_case(&unowned, ""), 404).await;
    // A bare `foo` is not owned by the holder of `foo.dcl.eth`.
    let bare = WorldName::parse(world.owner_label().unwrap()).unwrap();
    expect(app, limits_case(&bare, ""), 404).await;
    set_limit_override(&f.pool, &unowned, None, Some(1234)).await;
    assert_eq!(
        expect(app, limits_case(&unowned, ""), 200).await,
        json!({ "maxSceneSizeInBytes": "1234", "maxParcels": 4 }),
        "an override's size needs no owner"
    );

    // A publish reads the same limits.
    set_limit_override(&f.pool, &world, Some(5), Some(10)).await;
    assert_eq!(
        expect(app, limits_case(&world, ""), 200).await,
        json!({ "maxSceneSizeInBytes": "10", "maxParcels": 5 })
    );
    let file: &[u8] = b"twelve bytes";
    let five = ["0,0", "0,1", "0,2", "0,3", "0,4"];
    let content = json!([{ "file": "a.bin", "hash": hash_bytes_v1(file) }]);
    let entity = publish_entity(&world, &five, content);
    let message = "Deployment failed: The deployment is too big. The maximum total size allowed is 10 bytes for scenes. You can upload up to 10 bytes but you tried to upload 12.";
    refused(app, &owner, entity.clone(), &[file], message).await;
    set_limit_override(&f.pool, &world, Some(5), None).await;
    post_entity(app, &signed_deployment(&owner, &entity, &[file]).await, 200).await;

    let _denied = DenyListed::seed(&f.state.names, vec![world.label().to_string()]);
    let denied = expect(app, limits_case(&world, ""), 404).await;
    assert_eq!(
        denied["message"],
        format!("World \"{}\" not found.", world.key())
    );
}

#[tokio::test]
async fn an_oversized_deploy_body_answers_413() {
    let Some(f) = fixture().await else { return };
    let mut case =
        Case::new("post", "/entities").body(Vec::new(), "multipart/form-data; boundary=x");
    case = case.header(
        "content-length",
        &(super::deploy::MAX_UPLOAD_WIRE_SIZE_BYTES + 1).to_string(),
    );
    let body = expect(&f.app, case, 413).await;
    assert_eq!(
        body,
        json!({ "error": "Payload Too Large", "message": "The multipart request is too large." })
    );
}

async fn wait_spool_empty(f: &Fixture) {
    let root = super::upload_spool::spool_root(&f.storage);
    for _ in 0..200 {
        let empty = match std::fs::read_dir(&root) {
            Ok(mut entries) => entries.next().is_none(),
            Err(_) => true,
        };
        if empty {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "the upload spool under {} was not cleaned up",
        root.display()
    );
}

#[tokio::test]
async fn deploy_parts_are_spooled_to_disk_and_removed_after_success_and_failure() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());

    let d = deployment(&owner, &world, "Spooled", "0,0", &["0,0"]).await;
    post_entity(&f.app, &d, 200).await;
    assert!(f.storage.exist(&d.entity_id).await.unwrap());
    assert!(f.storage.exist(&d.thumb_hash).await.unwrap());
    wait_spool_empty(&f).await;

    let stranger = Wallet::random();
    let refused = deployment(&stranger, &world, "Refused", "0,0", &["0,0"]).await;
    let (status, _) = hit(
        &f.app,
        Case::new("post", "/entities").body(refused.body.clone(), &refused.content_type),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    assert!(!f.storage.exist(&refused.entity_id).await.unwrap());
    wait_spool_empty(&f).await;
}

/// The upload-slot cap (`MAX_CONCURRENT_UPLOADS`, this tree's) refuses `POST /entities` with
/// upstream's byte-budget 503 (`multipart/errors.ts`), as the in-flight bytes do (WD-11): with a
/// cap of one and a slot held, the publish is shed.
#[tokio::test]
async fn a_publish_past_the_upload_slots_gets_the_byte_budget_503() {
    let mut settings = test_settings();
    settings.upload_limits.max_concurrent_uploads = 1;
    // The slot counter is process-wide: other tests' uploads only make the shed more certain.
    let _held = super::upload_limits::try_acquire_upload_slot(u64::MAX).expect("a slot");
    let Some(f) = fixture_full(PersonalWorldsPolicy::disabled(), None, None, settings).await else {
        return;
    };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let d = deployment(&owner, &world, "Shed", "0,0", &["0,0"]).await;
    let body = expect(
        &f.app,
        Case::new("post", "/entities").body(d.body.clone(), &d.content_type),
        503,
    )
    .await;
    assert_eq!(
        body,
        json!({
            "error": "Service Unavailable",
            "message": super::upload_limits::BYTES_SHED_MESSAGE,
        })
    );
    assert!(!f.storage.exist(&d.entity_id).await.unwrap());
}

#[tokio::test]
async fn a_source_past_its_concurrent_uploads_gets_429() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let ip = format!("198.51.100.{}", rand::random::<u8>());
    let limits = &f.state.settings.upload_limits;
    let held: Vec<_> = (0..limits.max_concurrent_uploads_per_source)
        .map(|_| {
            super::upload_limits::try_acquire_source(
                &ip,
                1,
                limits.max_concurrent_uploads_per_source,
                limits.max_in_flight_upload_bytes_per_source,
            )
            .expect("under the source cap")
        })
        .collect();

    let d = deployment(&owner, &world, "Throttled", "0,0", &["0,0"]).await;
    let case = || {
        Case::new("post", "/entities")
            .body(d.body.clone(), &d.content_type)
            .header("x-real-ip", &ip)
    };
    let body = expect(&f.app, case(), 429).await;
    assert_eq!(
        body,
        json!({ "error": "Too Many Requests", "message": "Too many uploads in progress from this client, please retry shortly." })
    );
    drop(held);
    expect(&f.app, case(), 200).await;
}

#[tokio::test]
async fn a_deploy_reusing_a_blob_the_collector_removed_is_deferred_under_the_store_lock() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let owner = Wallet::random();
    f.names.set(&owner.address, world.label());
    let reused = hash_bytes_v1(b"an aged, unreferenced blob");
    f.storage
        .store(
            &reused,
            bytes::Bytes::from_static(b"an aged, unreferenced blob"),
        )
        .await
        .unwrap();
    let entity = scene_entity(&world, "Reuse", &reused, "0,0", &["0,0"]);
    let entity_bytes = serde_json::to_vec(&entity).unwrap();
    let entity_id = hash_bytes_v1(&entity_bytes);
    f.storage
        .store(&entity_id, bytes::Bytes::from(entity_bytes))
        .await
        .unwrap();

    // The collector's batch holds the exclusive side; the publish waits on the shared side.
    let mut batch = catalyst_storage::gc::lock_store_exclusive(&f.pool)
        .await
        .unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *batch)
        .await
        .unwrap();
    let publish = tokio::spawn({
        let (pool, storage, world, owner, entity, entity_id) = (
            f.pool.clone(),
            f.storage.clone(),
            world.clone(),
            owner.address.clone(),
            entity.clone(),
            entity_id.clone(),
        );
        async move {
            store::deploy_scene(
                &pool,
                &storage,
                SceneDeployment {
                    world: &world,
                    name_owner: Some(&owner),
                    entity_id: &entity_id,
                    deployer: &owner,
                    deployment_auth_chain: &json!([]),
                    entity: &entity,
                    parcels: &parcels(&["0,0"]),
                    size: 1,
                    replacement: &Deploy::Owner,
                    forward_to: None,
                    quota: None,
                },
            )
            .await
        }
    });
    wait_for_lock(&f.pool, blocker, 1).await;
    assert!(!publish.is_finished(), "the publish waits for the batch");
    f.storage.delete_strict(&reused).await.unwrap();
    batch.commit().await.unwrap();

    let err = publish.await.unwrap().expect_err("the blob is gone");
    assert_eq!(err.status(), 503, "retryable, not a client error: {err:?}");
    assert!(
        err.public_message().contains(&reused) && err.public_message().contains("retry"),
        "{}",
        err.public_message()
    );
    assert!(entity_ids(&f, &world).await.is_empty());
}

// ── storage allowance, quota blocks and user notices ─────────────────────

async fn set_scene_size(pool: &PgPool, world: &WorldName, entity_id: &str, size: i64) {
    sqlx::query(
        "UPDATE worlds.world_scenes SET size = $3 WHERE world_name = $1 AND entity_id = $2",
    )
    .bind(world.key())
    .bind(entity_id)
    .bind(size)
    .execute(pool)
    .await
    .unwrap();
}

/// A scene of `size` bytes on `footprint`, published by `owner`.
async fn sized_scene(
    f: &Fixture,
    world: &WorldName,
    owner: &Address,
    tag: char,
    footprint: &[&str],
    size: i64,
) {
    let id = cid(tag);
    deploy(
        f,
        world,
        owner,
        &id,
        &scene_at(1, footprint),
        footprint,
        &Deploy::Owner,
    )
    .await
    .unwrap();
    set_scene_size(&f.pool, world, &id, size).await;
}

fn notices_of(
    f: &Fixture,
    kind: &str,
    to: &Address,
) -> Vec<crate::comms::interfaces::UserNotification> {
    f.social
        .user_notifications
        .lock()
        .unwrap()
        .iter()
        .filter(|n| n.kind == kind && &n.recipient == to)
        .cloned()
        .collect()
}

#[tokio::test]
async fn the_publish_cap_is_what_the_owners_allowance_leaves() {
    let Some(f) = fixture().await else { return };
    let (a, b, owner) = (world_name(), world_name(), Wallet::random());
    f.names.set(&owner.address, a.label());
    f.names.set(&owner.address, b.label());
    sized_scene(&f, &a, &owner.address, 'a', &["0,0"], 100).await;
    sized_scene(&f, &a, &owner.address, 'b', &["1,0"], 50).await;
    sized_scene(&f, &b, &owner.address, 'c', &["0,0"], 100).await;
    f.allowance.set(&owner.address, Some(300));
    let app = &f.app;
    let size = |body: Value| body["maxSceneSizeInBytes"].as_str().unwrap().to_string();

    // Only the scenes the publish replaces are credited back; no parcels credits the world.
    assert_eq!(
        size(expect(app, limits_case(&a, "?parcels=0,0"), 200).await),
        "150"
    );
    assert_eq!(size(expect(app, limits_case(&a, ""), 200).await), "200");
    assert_eq!(
        size(expect(app, limits_case(&b, "?parcels=5,5"), 200).await),
        "50"
    );
    f.allowance.set(&owner.address, Some(100));
    assert_eq!(
        size(expect(app, limits_case(&b, "?parcels=5,5"), 200).await),
        "0"
    );
    // Capped per scene.
    f.allowance.set(&owner.address, Some(i64::MAX));
    assert_eq!(
        size(expect(app, limits_case(&b, ""), 200).await),
        crate::policy::MAX_SCENE_SIZE_BYTES.to_string()
    );
    // A size override is outside the allowance.
    set_limit_override(&f.pool, &b, None, Some(7)).await;
    f.allowance.set(&owner.address, None);
    assert_eq!(size(expect(app, limits_case(&b, ""), 200).await), "7");
    expect(app, limits_case(&a, ""), 503).await;

    // A publish is held to the same cap: 12 bytes over the 0,0 scene.
    let file: &[u8] = b"twelve bytes";
    let content = json!([{ "file": "a.bin", "hash": hash_bytes_v1(file) }]);
    let entity = publish_entity(&a, &["0,0"], content);
    let d = signed_deployment(&owner, &entity, &[file]).await;
    let body = post_entity(app, &d, 503).await;
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("storage allowance"),
        "{body}"
    );
    // `b`'s override is outside the allowance: only the 1,0 scene stays charged.
    f.allowance.set(&owner.address, Some(50 + 10));
    let message = "Deployment failed: The deployment is too big. The maximum total size allowed is 10 bytes for scenes. You can upload up to 10 bytes but you tried to upload 12.";
    refused(app, &owner, entity.clone(), &[file], message).await;
    f.allowance.set(&owner.address, Some(50 + 12));
    post_entity(app, &signed_deployment(&owner, &entity, &[file]).await, 200).await;
}

#[tokio::test]
async fn a_quota_block_warns_restricts_after_two_days_and_restores() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (world, owner) = (world_name(), Wallet::random().address);
    f.names.set(&owner, world.label());
    sized_scene(&f, &world, &owner, 'q', &["0,0"], 250).await;
    f.allowance.set(&owner, Some(100));
    // Postgres keeps microseconds.
    let now = chrono::SubsecRound::trunc_subsecs(chrono::Utc::now().naive_utc(), 6);
    let t0 = now - chrono::TimeDelta::days(3);
    let hours = |h: i64| t0 + chrono::TimeDelta::hours(h);
    let block_row = || async {
        sqlx::query_as::<_, (chrono::NaiveDateTime, chrono::NaiveDateTime)>(
            "SELECT created_at, updated_at FROM worlds.quota_blocks WHERE wallet = $1",
        )
        .bind(&owner)
        .fetch_optional(&f.pool)
        .await
        .unwrap()
    };
    use super::notices::{ACCESS_RESTORED, ACCESS_RESTRICTED, MISSING_RESOURCES};
    use crate::comms::interfaces::AccessPolicy;

    assert!(f.state.block_if_over_quota(&owner, t0).await.unwrap());
    let warned = notices_of(&f, MISSING_RESOURCES, &owner);
    assert_eq!(warned.len(), 1);
    assert_eq!(warned[0].occurred_at, t0);
    assert!(!f.state.unblock_if_under_quota(&owner, hours(1)).await);

    // Re-detected inside the grace: refreshed, no second warning.
    assert!(
        f.state
            .block_if_over_quota(&owner, hours(12))
            .await
            .unwrap()
    );
    assert_eq!(block_row().await, Some((t0, hours(12))));
    assert_eq!(notices_of(&f, MISSING_RESOURCES, &owner).len(), 1);
    assert!(notices_of(&f, ACCESS_RESTRICTED, &owner).is_empty());

    // The world stays open for the grace, then /about and comms refuse it.
    let open = crate::policy::WorldAdmission::Open(crate::policy::AccessType::Unrestricted);
    assert_eq!(
        f.state
            .policy
            .world_admission(&world, hours(47))
            .await
            .unwrap(),
        open
    );
    assert_eq!(
        f.state
            .policy
            .world_admission(&world, hours(49))
            .await
            .unwrap(),
        crate::policy::WorldAdmission::Blocked
    );
    assert!(about_refuses(&f, &world).await);
    assert!(
        !store::world_is_valid(&f.pool, &f.state.policy, &f.state.names, &world)
            .await
            .unwrap()
    );

    assert!(
        f.state
            .block_if_over_quota(&owner, hours(49))
            .await
            .unwrap()
    );
    let restricted = notices_of(&f, ACCESS_RESTRICTED, &owner);
    assert_eq!(restricted.len(), 1);
    assert_eq!(restricted[0].occurred_at, hours(48));

    // An allowance outage keeps the block.
    f.allowance.set(&owner, None);
    assert!(
        f.state
            .block_if_over_quota(&owner, hours(60))
            .await
            .is_err()
    );
    assert!(!f.state.unblock_if_under_quota(&owner, hours(60)).await);
    assert!(block_row().await.is_some());

    f.allowance.set(&owner, Some(1000));
    assert!(f.state.unblock_if_under_quota(&owner, now).await);
    assert_eq!(block_row().await, None);
    let restored = notices_of(&f, ACCESS_RESTORED, &owner);
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored[0].metadata["url"],
        "https://builder.example/worlds?tab=dcl"
    );
    assert!(!about_refuses(&f, &world).await);
    assert!(!f.state.unblock_if_under_quota(&owner, now).await);
    assert!(!f.state.block_if_over_quota(&owner, now).await.unwrap());

    // A size-overridden world does not count against the allowance.
    f.allowance.set(&owner, Some(100));
    set_limit_override(&f.pool, &world, None, Some(1024)).await;
    assert!(!f.state.block_if_over_quota(&owner, now).await.unwrap());
}

#[tokio::test]
async fn a_publish_or_undeploy_that_frees_space_lifts_the_block() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (world, owner) = (world_name(), Wallet::random());
    f.names.set(&owner.address, world.label());
    sized_scene(&f, &world, &owner.address, 'u', &["0,0"], 250).await;
    f.allowance.set(&owner.address, Some(100));
    let now = chrono::Utc::now().naive_utc();
    assert!(
        f.state
            .block_if_over_quota(&owner.address, now)
            .await
            .unwrap()
    );

    expect(
        &f.app,
        Case::new("delete", format!("/world/{}/scenes/0,0", world.display()))
            .signed(&owner)
            .await,
        200,
    )
    .await;
    // The recheck runs off the request.
    let mut left: Option<String> = Some(String::new());
    for _ in 0..100 {
        left = sqlx::query_scalar("SELECT wallet FROM worlds.quota_blocks WHERE wallet = $1")
            .bind(&owner.address)
            .fetch_optional(&f.pool)
            .await
            .unwrap();
        if left.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(left, None);
    assert_eq!(
        notices_of(&f, super::notices::ACCESS_RESTORED, &owner.address).len(),
        1
    );
}

#[tokio::test]
async fn the_allowance_follows_the_live_name_holder() {
    let Some(f) = fixture().await else { return };
    let (theirs, mine, owner, previous) = (
        world_name(),
        world_name(),
        Wallet::random().address,
        Wallet::random().address,
    );
    // `theirs` was sold to `owner` but is still stored under `previous`.
    sized_scene(&f, &theirs, &previous, 't', &["0,0"], 200).await;
    sized_scene(&f, &mine, &owner, 'm', &["0,0"], 100).await;
    f.names.set(&owner, theirs.label());
    f.names.set(&owner, mine.label());
    f.allowance.set(&owner, Some(300));
    let limits = f.state.policy.world_limits(&theirs).await.unwrap();
    let parcels = ["0,0".to_string()];
    let cap = f
        .state
        .max_scene_size(&theirs, &limits, Some(&owner), Some(&parcels))
        .await
        .unwrap();
    assert_eq!(cap, 200);
    let cap = f
        .state
        .max_scene_size(&mine, &limits, Some(&owner), Some(&parcels))
        .await
        .unwrap();
    assert_eq!(cap, 100);
    let held = f.state.held_worlds(&previous).await.unwrap();
    let usage = super::quota::wallet_usage(&f.pool, &previous, held.as_deref())
        .await
        .unwrap();
    assert!(usage.dcl_names.is_empty(), "{usage:?}");
    let path = format!("/wallet/{}/stats", owner.as_str());
    let body = expect(&f.app, Case::new("get", &path), 200).await;
    assert_eq!(body["usedSpace"], json!("300"));
}

#[tokio::test]
async fn a_size_override_world_leaves_the_allowance_to_the_other_names() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (big, other, owner) = (world_name(), world_name(), Wallet::random());
    f.names.set(&owner.address, big.label());
    f.names.set(&owner.address, other.label());
    sized_scene(&f, &big, &owner.address, 'o', &["0,0"], 1000).await;
    set_limit_override(&f.pool, &big, None, Some(10_000)).await;
    f.allowance.set(&owner.address, Some(500));
    let size = |body: Value| body["maxSceneSizeInBytes"].as_str().unwrap().to_string();
    assert_eq!(
        size(expect(&f.app, limits_case(&other, ""), 200).await),
        "500"
    );
    assert_eq!(
        size(expect(&f.app, limits_case(&big, ""), 200).await),
        "10000"
    );
    let now = chrono::Utc::now().naive_utc();
    assert!(
        !f.state
            .block_if_over_quota(&owner.address, now)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_quota_block_waits_for_a_concurrent_unblock_and_reevaluates() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (world, owner) = (world_name(), Wallet::random().address);
    f.names.set(&owner, world.label());
    sized_scene(&f, &world, &owner, 'r', &["0,0"], 250).await;
    f.allowance.set(&owner, Some(100));

    // An undeploy holds the wallet's quota lock while it frees the space.
    let mut held = f.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("worlds.quota:{}", owner.as_str()))
        .execute(&mut *held)
        .await
        .unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let (state, wallet) = (f.state.clone(), owner.clone());
    let pass = tokio::spawn(async move {
        state
            .block_if_over_quota(&wallet, chrono::Utc::now().naive_utc())
            .await
    });
    wait_for_lock(&f.pool, blocker, 1).await;
    assert!(!pass.is_finished(), "the block waits for the lock");
    sqlx::query("DELETE FROM worlds.world_scenes WHERE world_name = $1")
        .bind(world.key())
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();

    assert!(!pass.await.unwrap().unwrap());
    let block: Option<String> =
        sqlx::query_scalar("SELECT wallet FROM worlds.quota_blocks WHERE wallet = $1")
            .bind(&owner)
            .fetch_optional(&f.pool)
            .await
            .unwrap();
    assert_eq!(block, None);
    assert!(notices_of(&f, super::notices::MISSING_RESOURCES, &owner).is_empty());
}

#[tokio::test]
async fn a_quota_block_spares_the_personal_and_mirrored_worlds() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (named, mirrored, owner) = (world_name(), world_name(), Wallet::random().address);
    let personal = WorldName::parse(&format!("{}.dcl.eth", owner.as_str())).unwrap();
    let long_ago = chrono::SubsecRound::trunc_subsecs(chrono::Utc::now().naive_utc(), 6)
        - chrono::TimeDelta::days(3);
    sqlx::query(
        "INSERT INTO worlds.quota_blocks (wallet, created_at, updated_at) VALUES ($1, $2, $2)",
    )
    .bind(&owner)
    .bind(long_ago)
    .execute(&f.pool)
    .await
    .unwrap();
    for (world, mirror) in [
        (&named, None),
        (&personal, None),
        (&mirrored, Some("https://upstream.example")),
    ] {
        sqlx::query("INSERT INTO worlds.worlds (name, owner, mirror_source) VALUES ($1, $2, $3)")
            .bind(world.key())
            .bind(&owner)
            .bind(mirror)
            .execute(&f.pool)
            .await
            .unwrap();
    }
    let since = |world: &WorldName| {
        let (pool, state, world) = (f.pool.clone(), f.state.clone(), world.clone());
        async move {
            let row = state.policy.world(&world).await.unwrap().unwrap();
            let about = crate::queries::world_about(&pool, world.key())
                .await
                .unwrap();
            assert_eq!(
                about.world.unwrap().quota_blocked_since,
                row.quota_blocked_since
            );
            row.quota_blocked_since
        }
    };
    assert_eq!(since(&named).await, Some(long_ago));
    assert_eq!(since(&personal).await, None);
    assert_eq!(since(&mirrored).await, None);

    sqlx::query("DELETE FROM worlds.worlds WHERE name = ANY($1::text[])")
        .bind([named.key(), personal.key(), mirrored.key()])
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM worlds.quota_blocks WHERE wallet = $1")
        .bind(&owner)
        .execute(&f.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_quota_pass_blocks_owners_over_their_allowance_and_lifts_stale_blocks() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (over, under, unknown) = (
        Wallet::random().address,
        Wallet::random().address,
        Wallet::random().address,
    );
    for (owner, tag) in [(&over, 'x'), (&under, 'y'), (&unknown, 'z')] {
        let world = world_name();
        f.names.set(owner, world.label());
        sized_scene(&f, &world, owner, tag, &["0,0"], 250).await;
    }
    f.allowance.set(&over, Some(100));
    f.allowance.set(&under, Some(1000));
    f.allowance.set(&unknown, None);
    let long_ago = chrono::Utc::now().naive_utc() - chrono::TimeDelta::days(10);
    for wallet in [&under, &unknown] {
        sqlx::query(
            "INSERT INTO worlds.quota_blocks (wallet, created_at, updated_at) VALUES ($1, $2, $2)",
        )
        .bind(wallet)
        .bind(long_ago)
        .execute(&f.pool)
        .await
        .unwrap();
    }
    f.state.quota_pass().await.unwrap();
    let blocked: Vec<String> = sqlx::query_scalar(
        "SELECT wallet FROM worlds.quota_blocks WHERE wallet = ANY($1::text[]) ORDER BY wallet",
    )
    .bind([&over, &under, &unknown])
    .fetch_all(&f.pool)
    .await
    .unwrap();
    let mut want = vec![over.as_str().to_string(), unknown.as_str().to_string()];
    want.sort();
    assert_eq!(
        blocked, want,
        "the over-quota owner is blocked, the unevaluated one kept"
    );
    assert_eq!(
        notices_of(&f, super::notices::MISSING_RESOURCES, &over).len(),
        1
    );
    assert_eq!(
        notices_of(&f, super::notices::ACCESS_RESTORED, &under).len(),
        1
    );
    assert!(notices_of(&f, super::notices::ACCESS_RESTORED, &unknown).is_empty());
}

#[tokio::test]
async fn permission_edits_notify_each_address_actually_changed() {
    use super::notices::{PERMISSION_GRANTED, PERMISSION_REVOKED};
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random());
    let [a, b, c, d, e] = [(); 5].map(|_| Wallet::random());
    f.names.set(&owner.address, world.label());
    post_entity(
        &f.app,
        &deployment(&owner, &world, "Seed", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    let app = &f.app;
    let w = world.display().to_string();
    let count = |kind: &str, to: &Wallet| notices_of(&f, kind, &to.address).len();
    let set_deployers = |wallets: Vec<String>| {
        let meta = json!({ "type": "allow-list", "wallets": wallets }).to_string();
        let owner = &owner;
        let path = format!("/world/{w}/permissions/deployment");
        async move {
            expect(
                app,
                Case::new("post", &path).signed_meta(owner, &meta).await,
                204,
            )
            .await
        }
    };

    set_deployers(vec![a.display(), b.display()]).await;
    assert_eq!(
        (count(PERMISSION_GRANTED, &a), count(PERMISSION_GRANTED, &b)),
        (1, 1)
    );
    let granted = &notices_of(&f, PERMISSION_GRANTED, &a.address)[0];
    assert_eq!(
        granted.metadata,
        json!({
            "title": "World Permission Granted",
            "description": format!("You have been granted deployment permission for world {}.", world.key()),
            "world": world.key(),
            "permissions": ["deployment"],
            "url": "https://builder.example/worlds?tab=dcl",
            "address": a.address.as_str(),
        })
    );
    set_deployers(vec![b.display(), c.display()]).await;
    assert_eq!(count(PERMISSION_REVOKED, &a), 1);
    assert_eq!(
        (count(PERMISSION_GRANTED, &b), count(PERMISSION_GRANTED, &c)),
        (1, 1)
    );

    let streamer = format!("/world/{w}/permissions/streaming/{}", d.display());
    expect(app, Case::new("put", &streamer).signed(&owner).await, 204).await;
    expect(app, Case::new("put", &streamer).signed(&owner).await, 204).await;
    assert_eq!(
        count(PERMISSION_GRANTED, &d),
        1,
        "only an added grant notifies"
    );
    expect(
        app,
        Case::new("delete", &streamer).signed(&owner).await,
        204,
    )
    .await;
    expect(
        app,
        Case::new("delete", &streamer).signed(&owner).await,
        204,
    )
    .await;
    assert_eq!(
        count(PERMISSION_REVOKED, &d),
        1,
        "only a removed grant notifies"
    );
    let revoked = &notices_of(&f, PERMISSION_REVOKED, &d.address)[0];
    assert_eq!(
        revoked.metadata["description"],
        format!(
            "Your streaming permission for world {} has been revoked.",
            world.key()
        )
    );
    // Granted again within the hour: the latest notice is the grant.
    expect(app, Case::new("put", &streamer).signed(&owner).await, 204).await;
    assert_eq!(count(PERMISSION_GRANTED, &d), 2);
    let latest = f
        .social
        .user_notifications
        .lock()
        .unwrap()
        .iter()
        .rfind(|n| n.recipient == d.address)
        .map(|n| n.kind)
        .unwrap();
    assert_eq!(latest, PERMISSION_GRANTED);

    let parcels_path = format!(
        "/world/{w}/permissions/deployment/address/{}/parcels",
        e.display()
    );
    for _ in 0..2 {
        expect(
            app,
            Case::new("post", &parcels_path)
                .signed(&owner)
                .await
                .json(&json!({ "parcels": ["0,0", "1,0"] })),
            204,
        )
        .await;
    }
    assert_eq!(count(PERMISSION_GRANTED, &e), 1);
    let remove = |parcels: Value| {
        let (owner, path) = (&owner, &parcels_path);
        async move {
            expect(
                app,
                Case::new("delete", path)
                    .signed(owner)
                    .await
                    .json(&json!({ "parcels": parcels })),
                204,
            )
            .await
        }
    };
    remove(json!(["0,0"])).await;
    assert_eq!(
        count(PERMISSION_REVOKED, &e),
        0,
        "a grant keeping parcels is not revoked"
    );
    remove(json!(["1,0"])).await;
    assert_eq!(
        count(PERMISSION_REVOKED, &e),
        1,
        "dropping the last parcel revokes the grant"
    );
}

/// Two first publishes of one world by different holders serialize: the
/// second sees the first's owner and reports it replaced.
#[tokio::test]
async fn concurrent_first_publishes_by_different_holders_report_the_replaced_owner() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    let mut held = f.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(store::publish_lock_key(&world))
        .execute(&mut *held)
        .await
        .unwrap();
    let mut publishes = Vec::new();
    for (i, base) in ["0,0", "5,5"].into_iter().enumerate() {
        let owner = Wallet::random().address;
        let thumb = hash_bytes_v1(b"race thumb");
        f.storage
            .store(&thumb, bytes::Bytes::from_static(b"race thumb"))
            .await
            .unwrap();
        let entity = scene_entity(&world, &format!("Race {i}"), &thumb, base, &[base]);
        let entity_bytes = serde_json::to_vec(&entity).unwrap();
        let entity_id = hash_bytes_v1(&entity_bytes);
        f.storage
            .store(&entity_id, bytes::Bytes::from(entity_bytes))
            .await
            .unwrap();
        let (pool, storage, world) = (f.pool.clone(), f.storage.clone(), world.clone());
        publishes.push(tokio::spawn(async move {
            store::deploy_scene(
                &pool,
                &storage,
                SceneDeployment {
                    world: &world,
                    name_owner: Some(&owner),
                    entity_id: &entity_id,
                    deployer: &owner,
                    deployment_auth_chain: &json!([]),
                    entity: &entity,
                    parcels: &parcels(&[base]),
                    size: 1,
                    replacement: &Deploy::Owner,
                    forward_to: None,
                    quota: None,
                },
            )
            .await
            .map(|previous| (owner, previous))
        }));
    }
    // Both publishes queue on the world's lock before either reads the owner.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
               AND ((classid::bigint << 32) | objid::bigint) = hashtextextended($1, 0)",
        )
        .bind(store::publish_lock_key(&world))
        .fetch_one(&f.pool)
        .await
        .unwrap();
        if waiting == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "both publishes wait on the world's publish lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    held.commit().await.unwrap();
    let mut outcomes = Vec::new();
    for publish in publishes {
        outcomes.push(publish.await.unwrap().unwrap());
    }
    let replaced: Vec<&Address> = outcomes.iter().filter_map(|(_, p)| p.as_ref()).collect();
    assert_eq!(replaced.len(), 1, "{outcomes:?}");
    let (first, _) = outcomes.iter().find(|(_, p)| p.is_none()).unwrap();
    assert_eq!(replaced[0], first);
}

fn ens_world_name() -> WorldName {
    WorldName::parse(&format!(
        "e{}.eth",
        &Uuid::new_v4().simple().to_string()[..12]
    ))
    .unwrap()
}

/// The reconcile job rewrites a stale `worlds.owner` from the authority and
/// re-decides the previous holder in the live rooms; a name the authority
/// cannot answer for keeps its stored owner.
#[tokio::test]
async fn the_owner_reconcile_follows_a_name_transfer() {
    let Some((f, ens)) = fixture_with_ens(1 << 20).await else {
        return;
    };
    let recorded = Arc::new(RecordedRevocations::default());
    f.state.add_revocation_sink(recorded.clone());
    let world = world_name();
    let (old, new) = (Wallet::random(), Wallet::random());
    f.names.set(&old.address, world.label());
    post_entity(
        &f.app,
        &deployment(&old, &world, "Before", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    sqlx::query("UPDATE worlds.worlds SET access_type = 'allow-list' WHERE name = $1")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();
    f.names.forget(&old.address);
    f.names.set(&new.address, world.label());
    f.market.graph.names.clear();
    f.market.graph.name_owner.clear();

    let unresolved = ens_world_name();
    let ens_owner = Wallet::random();
    deploy(
        &f,
        &unresolved,
        &ens_owner.address,
        "bafyensreconcile",
        &bare_scene("0,0", &["0,0"]),
        &["0,0"],
        &Deploy::Owner,
    )
    .await
    .unwrap();
    let sceneless = world_name();
    sqlx::query("INSERT INTO worlds.worlds (name, owner) VALUES ($1, $2)")
        .bind(sceneless.key())
        .bind(old.address.as_str())
        .execute(&f.pool)
        .await
        .unwrap();
    f.names.set(&new.address, sceneless.label());
    ens.set_down(true);

    let report = super::owners::reconcile_owners(&f.state).await.unwrap();
    assert!(report.updated >= 1 && report.unresolved >= 1, "{report:?}");
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(new.address.as_str())
    );
    assert_eq!(
        stored_owner(&f.pool, &unresolved).await.as_deref(),
        Some(ens_owner.address.as_str()),
        "an unresolved name keeps its stored owner"
    );
    assert_eq!(
        stored_owner(&f.pool, &sceneless).await.as_deref(),
        Some(old.address.as_str()),
        "a world without a local scene is not asked about"
    );
    assert!(
        f.market
            .graph
            .name_owner
            .get_fresh(&sceneless.owner_label().unwrap().to_string())
            .is_none()
    );
    assert!(recorded.take().contains(&super::Revocation::Address {
        world: world.clone(),
        address: old.address.clone(),
    }));

    super::owners::reconcile_owners(&f.state).await.unwrap();
    assert!(
        !recorded
            .take()
            .iter()
            .any(|r| matches!(r, super::Revocation::Address { world: w, .. } if *w == world)),
        "a current row is left alone"
    );
}

/// `ALLOW_ENS_DOMAINS`: an ENS world publishes for its registry holder under
/// `ENS_MAX_SIZE`, which an override row replaces; off, `.eth` is refused.
#[tokio::test]
async fn an_ens_world_publishes_for_its_holder_under_the_ens_cap() {
    let Some((f, ens)) = fixture_with_ens(10).await else {
        return;
    };
    let (world, owner, stranger) = (ens_world_name(), Wallet::random(), Wallet::random());
    ens.register(world.key(), &owner.address);
    let app = &f.app;
    assert_eq!(
        expect(app, limits_case(&world, ""), 200).await,
        json!({ "maxSceneSizeInBytes": "10", "maxParcels": 4 })
    );
    let file: &[u8] = b"twelve bytes";
    let content = json!([{ "file": "a.bin", "hash": hash_bytes_v1(file) }]);
    let entity = publish_entity(&world, &["0,0"], content);
    refused(
        app,
        &stranger,
        entity.clone(),
        &[file],
        &no_permission(world.display()),
    )
    .await;
    let message = "Deployment failed: The deployment is too big. The maximum total size allowed is 10 bytes for scenes. You can upload up to 10 bytes but you tried to upload 12.";
    refused(app, &owner, entity.clone(), &[file], message).await;
    set_limit_override(&f.pool, &world, None, Some(1000)).await;
    refused(app, &owner, entity.clone(), &[file], message).await;
    assert_eq!(
        expect(app, limits_case(&world, ""), 200).await,
        json!({ "maxSceneSizeInBytes": "10", "maxParcels": 4 }),
        "ENS_MAX_SIZE wins over an override, as upstream"
    );
    let file: &[u8] = b"tiny";
    let content = json!([{ "file": "a.bin", "hash": hash_bytes_v1(file) }]);
    let entity = publish_entity(&world, &["0,0"], content);
    post_entity(app, &signed_deployment(&owner, &entity, &[file]).await, 200).await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(owner.address.as_str())
    );
    let meta = json!({ "type": "allow-list", "wallets": [] }).to_string();
    let path = format!("/world/{}/permissions/deployment", world.display());
    expect(
        app,
        Case::new("post", &path).signed_meta(&owner, &meta).await,
        204,
    )
    .await;

    let Some(off) = fixture().await else { return };
    let other = ens_world_name();
    let entity = publish_entity(&other, &["0,0"], json!([]));
    let message = format!(
        "Deployment failed: Only .dcl.eth world names are supported for publishing (got \"{}\")",
        other.display()
    );
    refused(&off.app, &owner, entity, &[], &message).await;
}

/// A new holder's publish rewrites `worlds.owner` and re-decides the previous
/// holder at once; the reconcile that follows finds nothing left to change.
#[tokio::test]
async fn a_new_holder_publishes_then_the_reconcile_finds_nothing_to_change() {
    let Some(f) = fixture().await else { return };
    let recorded = Arc::new(RecordedRevocations::default());
    f.state.add_revocation_sink(recorded.clone());
    let world = world_name();
    let (old, new) = (Wallet::random(), Wallet::random());
    f.names.set(&old.address, world.label());
    post_entity(
        &f.app,
        &deployment(&old, &world, "Before", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    sqlx::query("UPDATE worlds.worlds SET access_type = 'allow-list' WHERE name = $1")
        .bind(world.key())
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(recorded.take().is_empty());

    f.names.forget(&old.address);
    f.names.set(&new.address, world.label());
    f.market.graph.names.clear();
    post_entity(
        &f.app,
        &deployment(&new, &world, "After", "0,0", &["0,0"]).await,
        200,
    )
    .await;
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(new.address.as_str())
    );
    assert!(
        recorded.take().contains(&super::Revocation::Address {
            world: world.clone(),
            address: old.address.clone(),
        }),
        "the previous holder is re-decided on the publish"
    );

    let report = super::owners::reconcile_owners(&f.state).await.unwrap();
    assert_eq!(report.updated, 0, "{report:?}");
    assert!(
        !recorded
            .take()
            .iter()
            .any(|r| matches!(r, super::Revocation::Address { world: w, .. } if *w == world))
    );
}

#[tokio::test]
async fn one_edit_notifies_at_most_the_capped_number_of_addresses() {
    use super::notices::PERMISSION_GRANTED;
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random());
    f.names.set(&owner.address, world.label());
    let many: Vec<Address> = (0..super::permissions::MAX_NOTICES_PER_EDIT + 5)
        .map(|_| Wallet::random().address)
        .collect();
    let wallets: Vec<&str> = many.iter().map(Address::as_str).collect();
    let meta = json!({ "type": "allow-list", "wallets": wallets }).to_string();
    let path = format!("/world/{}/permissions/deployment", world.display());
    expect(
        &f.app,
        Case::new("post", &path).signed_meta(&owner, &meta).await,
        204,
    )
    .await;
    let notified = many
        .iter()
        .filter(|a| !notices_of(&f, PERMISSION_GRANTED, a).is_empty())
        .count();
    assert_eq!(notified, super::permissions::MAX_NOTICES_PER_EDIT);
}

#[tokio::test]
async fn without_an_allowance_source_a_publish_is_charged_to_nobody() {
    let Some(f) = fixture().await else { return };
    let (world, owner) = (world_name(), Wallet::random().address);
    f.names.set(&owner, world.label());
    f.allowance.set(&owner, Some(1));
    f.allowance
        .unconfigured
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let limits = f.state.policy.world_limits(&world).await.unwrap();
    let cap = f
        .state
        .scene_cap(&world, &limits, Some(&owner), Some(&["0,0".to_string()]))
        .await
        .unwrap();
    assert_eq!(cap.max, limits.max_size_bytes);
    assert!(cap.charge.is_none());
}

/// A NAME's new holder is evaluated by the next pass, before the owner reconcile.
#[tokio::test]
async fn the_quota_pass_evaluates_the_live_name_holder() {
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (world, previous, holder) = (
        world_name(),
        Wallet::random().address,
        Wallet::random().address,
    );
    f.names.set(&previous, world.label());
    sized_scene(&f, &world, &previous, 'p', &["0,0"], 250).await;
    f.names.forget(&previous);
    f.names.set(&holder, world.label());
    f.market.graph.names.clear();
    f.market.graph.name_owner.clear();
    f.allowance.set(&holder, Some(100));
    f.state.quota_pass().await.unwrap();
    let blocked: Option<String> =
        sqlx::query_scalar("SELECT wallet FROM worlds.quota_blocks WHERE wallet = $1")
            .bind(&holder)
            .fetch_optional(&f.pool)
            .await
            .unwrap();
    assert_eq!(blocked.as_deref(), Some(holder.as_str()));
    assert_eq!(
        stored_owner(&f.pool, &world).await.as_deref(),
        Some(previous.as_str())
    );
    sqlx::query("DELETE FROM worlds.quota_blocks WHERE wallet = $1")
        .bind(&holder)
        .execute(&f.pool)
        .await
        .unwrap();
}

/// Two publishes validated against the same remaining allowance: the
/// transaction re-check under the wallet lock admits only the first.
#[tokio::test]
async fn concurrent_publishes_cannot_overrun_the_allowance_together() {
    let Some(f) = fixture().await else { return };
    let (a, b, owner) = (world_name(), world_name(), Wallet::random().address);
    f.names.set(&owner, a.label());
    f.names.set(&owner, b.label());
    f.allowance.set(&owner, Some(150));
    let limits = f.state.policy.world_limits(&a).await.unwrap();
    let footprint = ["0,0".to_string()];
    let mut charges = Vec::new();
    for world in [&a, &b] {
        let cap = f
            .state
            .scene_cap(world, &limits, Some(&owner), Some(&footprint))
            .await
            .unwrap();
        assert_eq!(cap.max, 150);
        charges.push(cap.charge.expect("charged to the allowance"));
    }

    let mut held = f.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("worlds.quota:{}", owner.as_str()))
        .execute(&mut *held)
        .await
        .unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let mut publishes = Vec::new();
    for ((world, charge), tag) in [&a, &b].into_iter().zip(charges).zip(['c', 'e']) {
        let (id, entity) = (cid(tag), scene_at(1, &["0,0"]));
        f.storage
            .store(&id, bytes::Bytes::from(entity.to_string()))
            .await
            .unwrap();
        let (pool, storage, world, owner) = (
            f.pool.clone(),
            f.storage.clone(),
            world.clone(),
            owner.clone(),
        );
        publishes.push(tokio::spawn(async move {
            store::deploy_scene(
                &pool,
                &storage,
                SceneDeployment {
                    world: &world,
                    name_owner: Some(&owner),
                    entity_id: &id,
                    deployer: &owner,
                    deployment_auth_chain: &json!([]),
                    entity: &entity,
                    parcels: &parcels(&["0,0"]),
                    size: 100,
                    replacement: &Deploy::Owner,
                    forward_to: None,
                    quota: Some(&charge),
                },
            )
            .await
        }));
    }
    wait_for_lock(&f.pool, blocker, 2).await;
    assert!(
        publishes.iter().all(|p| !p.is_finished()),
        "both wait for the lock"
    );
    held.commit().await.unwrap();

    let mut refused = Vec::new();
    for publish in publishes {
        if let Err(e) = publish.await.unwrap() {
            refused.push(e);
        }
    }
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].status(), 400);
    assert!(
        refused[0]
            .public_message()
            .contains("You can upload up to 50 bytes but you tried to upload 100"),
        "{}",
        refused[0].public_message()
    );
    let held = f.state.held_worlds(&owner).await.unwrap();
    let usage = super::quota::wallet_usage(&f.pool, &owner, held.as_deref())
        .await
        .unwrap();
    assert_eq!(usage.used_space, 100);
}

/// A quota block follows the NAME's live holder, not a stale stored owner.
#[tokio::test]
async fn a_quota_block_follows_the_live_name_holder() {
    use crate::comms::interfaces::AccessPolicy;
    let Some(f) = fixture().await else { return };
    let _quota = super::quota::test_support::QUOTA_DB.lock().await;
    let (world, seller, buyer) = (
        world_name(),
        Wallet::random().address,
        Wallet::random().address,
    );
    f.names.set(&seller, world.label());
    sized_scene(&f, &world, &seller, 's', &["0,0"], 1).await;
    f.names.forget(&seller);
    f.names.set(&buyer, world.label());
    f.market.graph.names.clear();
    f.market.graph.name_owner.clear();
    let long_ago = chrono::Utc::now().naive_utc() - chrono::TimeDelta::days(3);
    let block = |wallet: &Address| {
        sqlx::query(
            "INSERT INTO worlds.quota_blocks (wallet, created_at, updated_at) VALUES ($1, $2, $2)",
        )
        .bind(wallet.as_str().to_string())
        .bind(long_ago)
        .execute(&f.pool)
    };
    let now = chrono::Utc::now().naive_utc();
    block(&seller).await.unwrap();
    assert_eq!(
        f.state.policy.world_admission(&world, now).await.unwrap(),
        crate::policy::WorldAdmission::Open(crate::policy::AccessType::Unrestricted),
        "the seller's block does not follow the sold name"
    );
    assert_eq!(
        f.state.policy.quota_blocked_since(&world).await.unwrap(),
        None
    );
    assert!(
        store::world_is_valid(&f.pool, &f.state.policy, &f.state.names, &world)
            .await
            .unwrap()
    );
    block(&buyer).await.unwrap();
    assert_eq!(
        f.state.policy.world_admission(&world, now).await.unwrap(),
        crate::policy::WorldAdmission::Blocked
    );
    assert!(
        !store::world_is_valid(&f.pool, &f.state.policy, &f.state.names, &world)
            .await
            .unwrap()
    );
    sqlx::query("DELETE FROM worlds.quota_blocks WHERE wallet = ANY($1::text[])")
        .bind([seller.as_str(), buyer.as_str()])
        .execute(&f.pool)
        .await
        .unwrap();
}

mod partial_uploads;
mod reprocess_ab;

/// Every signed worlds route answers a refused signed fetch as upstream's
/// `onError` does (`undeploy-entity-handler.spec.ts:39-55`,
/// `scenes-handler.spec.ts:833-856`, `world-settings-handler.spec.ts:849-876`,
/// `post-permissions.spec.ts:355-365`).
#[tokio::test]
async fn signed_routes_refuse_with_the_crypto_middleware_body() {
    let Some(f) = fixture().await else { return };
    let w = world_name().display().to_string();
    let address = Wallet::random().address.to_string();
    let community = Uuid::new_v4();
    let routes = [
        ("put", format!("/world/{w}/settings")),
        ("get", "/wallet/contribute".to_string()),
        ("delete", format!("/world/{w}/scenes/0,0")),
        ("delete", format!("/entities/{w}")),
        ("post", format!("/world/{w}/permissions/access")),
        (
            "put",
            format!("/world/{w}/permissions/deployment/{address}"),
        ),
        (
            "delete",
            format!("/world/{w}/permissions/deployment/{address}"),
        ),
        (
            "put",
            format!("/world/{w}/permissions/access/communities/{community}"),
        ),
        (
            "delete",
            format!("/world/{w}/permissions/access/communities/{community}"),
        ),
    ];
    let wallet = Wallet::random();
    // The settings route reads a multipart body; an empty one carries the signed fetch.
    let case = |method: &'static str, path: &str| {
        let case = Case::new(method, path.to_string());
        if path.ends_with("/settings") {
            case.body(b"--x--\r\n".to_vec(), "multipart/form-data; boundary=x")
        } else {
            case
        }
    };
    for (method, path) in routes {
        let body = expect(&f.app, case(method, &path), 400).await;
        assert_eq!(
            body,
            json!({
                "error": "Invalid Auth Chain",
                "message": "This endpoint requires a signed fetch request. See ADR-44."
            }),
            "{method} {path}"
        );
        let body = expect(
            &f.app,
            case(method, &path)
                .signed_over(&wallet, "/some/other/path", "{}")
                .await,
            401,
        )
        .await;
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("Invalid signature: ")),
            "{method} {path}: {body}"
        );
        assert_eq!(
            body["message"],
            "This endpoint requires a signed fetch request. See ADR-44."
        );
    }
}

/// Upstream `test/integration/multipart-error-handling.spec.ts`, `extract-auth-chain.ts` and
/// `deploy-entity-handler.ts` `authenticateRequest`: what `POST /entities` answers before it
/// reads the entity.
#[tokio::test]
async fn deploy_form_and_auth_chain_refusals_answer_upstreams_texts() {
    let Some(f) = fixture().await else { return };
    let app = &f.app;
    let refused = |message: String| json!({ "error": "Bad request", "message": message });
    let post = |bytes: Vec<u8>, content_type: String| async move {
        expect(
            app,
            Case::new("post", "/entities").body(bytes, &content_type),
            400,
        )
        .await
    };

    let body = post(
        br#"{"entityId":"test"}"#.to_vec(),
        "application/json".into(),
    )
    .await;
    assert_eq!(
        body,
        refused("Unsupported content type: application/json".into())
    );

    let boundary = "----TestBoundary";
    let multipart_type = format!("multipart/form-data; boundary={boundary}");
    let truncated = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"entityId\"\r\n\r\nsome-id\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\nContent-Type: application/octet-stream\r\n\r\nincomplete file da"
    );
    let body = post(truncated.into_bytes(), multipart_type.clone()).await;
    assert_eq!(body, refused("Unexpected end of form".into()));

    let (bytes, content_type) = multipart_body(&[("someOtherField", None, b"value")]);
    assert_eq!(
        post(bytes, content_type).await,
        refused("A string was expected".into())
    );

    let (bytes, content_type) = multipart_body(&[("entityId", None, b"bafyx")]);
    assert_eq!(
        post(bytes, content_type).await,
        refused("Missing auth chain".into())
    );

    let wallet = Wallet::random();
    let entity_id = hash_bytes_v1(b"an entity");
    let chain_over = |payload: String| {
        let wallet = &wallet;
        async move { chain_fields(&wallet.chain(&payload).await) }
    };
    let form = |chain: Vec<(String, String)>| {
        let mut parts: Vec<(&str, Option<&str>, &[u8])> =
            vec![("entityId", None, entity_id.as_bytes())];
        parts.extend(chain.iter().map(|(k, v)| (k.as_str(), None, v.as_bytes())));
        multipart_body(&parts)
    };

    let mut bad_signer = chain_over(entity_id.clone()).await;
    bad_signer[1].1 = "nope".to_string();
    let (bytes, content_type) = form(bad_signer);
    assert_eq!(
        post(bytes, content_type).await,
        refused("Deployment failed: Invalid signer: nope".into())
    );

    let other = hash_bytes_v1(b"another entity");
    let (bytes, content_type) = form(chain_over(other.clone()).await);
    assert_eq!(
        post(bytes, content_type).await,
        refused(format!(
            "Deployment failed: ERROR: Invalid final authority. Expected: {entity_id}. Current {other}."
        ))
    );

    let mut unknown_type = chain_over(entity_id.clone()).await;
    unknown_type[3].1 = "NOT_A_LINK".to_string();
    let (bytes, content_type) = form(unknown_type);
    assert_eq!(
        post(bytes, content_type).await,
        refused("Deployment failed: must be equal to one of the allowed values".into())
    );
}
