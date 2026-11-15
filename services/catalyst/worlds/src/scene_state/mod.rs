//! The authoritative scene host (the old `catalyrst-scene-state`, upstream
//! decentraland/scene-state-server): the server-side half of an SDK7
//! multiplayer scene. Each hosted scene runs its `game.js` in its own V8
//! isolate on its own thread ([`jsruntime`]) or, with the JS runtime off or a
//! scene without code, as a plain CRDT relay ([`runtime::RelayRuntime`]);
//! clients join over `GET /ws/{scene}` ([`ws`]), authenticate with a
//! signed-fetch frame ([`auth`]), are admitted by the worlds permission core
//! ([`crate::policy`]) and exchange CRDT batches the engine
//! ([`crdt`]) merges and the host relays.
//!
//! What changed from the old service, and why:
//!
//! - **Scene source**: the old host fetched `/world/{name}/about` of a
//!   `WORLD_SERVER_URL` and the files behind `scenesUrn[0]` over HTTP. Here
//!   the active deployment is the ONE room resolver's reading of the shared
//!   rows (`rooms::world_active_scene`, `genesis_scene`, `genesis_scene_at`)
//!   and the files come from the ONE blob store ([`loader`]). A scene whose
//!   deployment this node only mirrors is refused: mirroring grants no host
//!   authority (DATABASE.md). Genesis scenes host under the same model: the
//!   key is a scene id or a parcel, the storage scope is the resolver's
//!   namespace (`main`, base parcel; a world scene's is its identity).
//! - **Admission**: the old host authenticated the frame and admitted anyone;
//!   the LiveKit token the worlds service minted was the only gate. Here a
//!   world scene admits through `Policy::can_access` (blocked/platform/scene
//!   bans, owner, the access setting; a shared secret travels in the signed
//!   metadata), a Genesis scene through `Policy::is_banned`. A revocation
//!   made through the worlds routes (an allow-list removal, an access
//!   setting or secret change, an operator block) reaches the running
//!   sockets at once: the host is a [`RevocationSink`] of the worlds core
//!   ([`SceneHostState::revoke`]); the `scene-host-sweep` job re-runs the
//!   decision for every connected client as the safety net for changes made
//!   elsewhere ([`SceneHostState::spawn_jobs`]). Presence is not proof of
//!   permission.
//! - **Redeploy**: the sweep also re-resolves each hosted scene and reloads
//!   it when the active deployment moved (the old host needed the debugging
//!   reload call); `/ws/{scene}` loads an unhosted scene on first contact
//!   (`SCENE_HOST_AUTOLOAD`) under `SCENE_HOST_MAX_SCENES`.
//! - **Storage**: guest code reaches world/player storage only through
//!   `~system/SignedFetch`, confined to the configured storage origin
//!   ([`jsruntime`]'s `validate_scene_url`) and signed by a scoped storage
//!   delegation ([`delegation`]): an ephemeral key the authoritative signer
//!   scoped to exactly `(world, scene id, parcel)` for a bounded time. The
//!   delegation is minted in-process from `SCENE_HOST_AUTHORITATIVE_KEY`
//!   (the old `catalyrst-deploy-signer --serve-delegations` minter folded
//!   in), or by that external minter, or pre-minted for development; the
//!   storage routes verify the claim. The scene never sees the key, SQL or
//!   another scene's scope.
//! - **Persistence**: none. The restart contract is: re-resolve the active
//!   deployment, reload the files, mint a fresh delegation; CRDT/tick state
//!   and client entity ranges live in the runtime, player state in
//!   `worlds.player_storage` behind the storage routes.
//!
//! The JS engine is `v8` (rusty_v8 152.2.0, the old crate's pin): the scene
//! bundle is the same code the explorer runs, and per-scene isolates give
//! the isolation, heap limit, wall-clock watchdog and cancellation the plan
//! requires. It is the one native dependency of this module (a prebuilt
//! `librusty_v8.a`; `RUSTY_V8_ARCHIVE` points the build at it, as the old
//! flake did).

pub mod admin;
pub mod auth;
pub mod crdt;
pub mod delegation;
pub mod handlers;
pub mod jsruntime;
pub mod loader;
pub mod protocol;
pub mod runtime;
pub mod scene;
pub mod ws;

#[cfg(test)]
mod tests {
    mod auth_frame_payloads;
    mod formal_properties;
    mod formal_refutations;
    mod host;
    mod signed_fetch;
}

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::Router;
use axum::routing::{get, post};
use sqlx::PgPool;

use crate::auth::eip1654::Eip1654Validator;
use crate::comms::world_identity::{Parcel, SceneId, WorldName, is_world_realm_name};
use crate::core::{Revocation, RevocationSink};
use crate::http::{HttpClientCfg, Outbound};
use crate::jobs::{Jobs, PeriodicCfg};
use crate::policy::{AccessGrant, Policy};
use crate::shared::cors;
use crate::shared::nul_guard;
use catalyst_storage::Storage;

use delegation::{DelegationMinter, StorageDelegation, Wallet};
use runtime::RuntimeLimits;
use scene::SceneManager;

pub use loader::{LOCAL_SCENE_NAME, load_or_reload};

/// The old gateway's prefix for this service (`catalyrst/nixos/web.nix`,
/// `/scene-state` → :5209 with the prefix stripped): mount [`router`] here.
pub const ROUTE_PREFIX: &str = "/scene-state";

pub const DEFAULT_REALM_NAME: &str = "dcl-one";

/// Which scene a `/ws/{scene}` (or admin) path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneKey {
    /// `localScene`: the file at `SCENE_HOST_LOCAL_SCENE_PATH` (development).
    Local,
    /// A world (`*.eth`): its active scene.
    World(WorldName),
    /// A Genesis City scene by entity id, in the shared realm.
    Genesis(SceneId),
    /// The Genesis City scene standing on a parcel (`x,y`).
    GenesisParcel(Parcel),
}

impl SceneKey {
    /// `localScene`, a parcel, a world name, else a scene id.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw == LOCAL_SCENE_NAME {
            return Ok(Self::Local);
        }
        if let Ok(parcel) = Parcel::parse(raw) {
            return Ok(Self::GenesisParcel(parcel));
        }
        if is_world_realm_name(raw) {
            return WorldName::parse(raw)
                .map(Self::World)
                .map_err(|e| e.to_string());
        }
        SceneId::parse(raw)
            .map(Self::Genesis)
            .map_err(|e| e.to_string())
    }

    /// The name a hosted scene is registered under (one per world, one per
    /// Genesis scene id; a parcel names its own key until
    /// [`SceneHostState::canonical_key`] resolves it to the scene id).
    pub fn room_key(&self) -> String {
        match self {
            Self::Local => LOCAL_SCENE_NAME.to_string(),
            Self::World(w) => w.key().to_string(),
            Self::Genesis(id) => id.as_str().to_string(),
            Self::GenesisParcel(p) => p.canonical(),
        }
    }

    pub fn world(&self) -> Option<&WorldName> {
        match self {
            Self::World(w) => Some(w),
            _ => None,
        }
    }
}

impl std::fmt::Display for SceneKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.room_key())
    }
}

/// The settings as the process configuration carries them (the old
/// `catalyrst-scene-state` environment, prefixed `SCENE_HOST_` where a name
/// would collide with another module's); [`Settings::resolve`] checks them.
#[derive(Clone)]
pub struct RawSettings {
    /// `SCENE_HOST_LOCAL_SCENE_PATH`: a local `game.js` served as `localScene`.
    pub local_scene_path: Option<String>,
    /// `SCENE_HOST_DEBUGGING_SECRET`: the `/debugging/reload` secret.
    pub debugging_secret: Option<String>,
    /// `SCENE_HOST_ADMIN_TOKEN`: the bearer of `/admin/scene/*`; falls back
    /// to the debugging secret, as the old binary did.
    pub admin_token: Option<String>,
    /// `SCENE_HOST_AUTH_TIMEOUT_SECS` (5): the auth frame deadline.
    pub auth_timeout_secs: u64,
    /// `SCENE_HOST_DISABLE_JS_RUNTIME` (false): every scene is a plain relay.
    pub disable_js_runtime: bool,
    /// `REALM_NAME`: what `Runtime.getRealm` tells the scene; the storage
    /// scope of a Genesis scene is `main` whatever this says.
    pub realm_name: Option<String>,
    /// `COMMIT_HASH` for `/status`.
    pub commit_hash: Option<String>,
    /// `JS_HEAP_LIMIT_MB` (384).
    pub js_heap_limit_mb: usize,
    /// `JS_TICK_BUDGET_MS` (250): the wall-clock watchdog per eval/tick.
    pub js_tick_budget_ms: u64,
    /// `JS_SHUTDOWN_JOIN_MS` (2000).
    pub js_shutdown_join_ms: u64,
    /// `JS_UPDATE_FAILURE_CAP` (30): consecutive `onUpdate` throws before teardown.
    pub js_update_failure_cap: usize,
    /// `CLIENT_OUTBOUND_MAX` (1024) / `CLIENT_INBOUND_MAX` (1024).
    pub client_outbound_max: usize,
    pub client_inbound_max: usize,
    /// `CRDT_MAX_COMPONENTS` (100000).
    pub crdt_max_components: usize,
    /// `WS_MAX_FRAME_BYTES` (2 MiB).
    pub ws_max_frame_bytes: usize,
    /// `CRDT_MAX_VALUE_BYTES` (256 KiB): largest single component payload a scene's state keeps.
    pub crdt_max_value_bytes: usize,
    /// `CRDT_MAX_BYTES` (32 MiB): payload bytes one scene's state keeps.
    pub crdt_max_bytes: usize,
    /// `CLIENT_INBOUND_MAX_BYTES` (8 MiB): unread client bytes queued toward the scene, per client.
    pub client_inbound_max_bytes: usize,
    /// `SCENE_INBOUND_MAX_BYTES` (32 MiB): unread client bytes queued toward
    /// one scene, a quarter of it for frames held before the observer registers.
    pub scene_inbound_max_bytes: usize,
    /// `CLIENT_OUTBOUND_MAX_BYTES` (8 MiB): scene bytes queued to one client.
    pub client_outbound_max_bytes: usize,
    /// `JS_ARRAY_BUFFER_LIMIT_MB` (128): ArrayBuffer memory one isolate holds outside its JS heap.
    pub js_array_buffer_limit_mb: usize,
    /// `SCENE_HOST_STORAGE_URL`: the world-storage origin, the ONLY origin
    /// `~system/SignedFetch` may reach; unset disables scene storage.
    pub storage_url: Option<String>,
    /// `SCENE_HOST_STORAGE_ALLOW_HTTP` (false): admit an `http://` loopback
    /// origin (a node serving its own storage routes in development).
    pub storage_allow_http: bool,
    /// `SCENE_HOST_AUTHORITATIVE_KEY` (or `_FILE`): the authoritative
    /// server's private key; delegations are minted in-process.
    pub authoritative_key: Option<String>,
    /// `SCENE_HOST_DELEGATION_TTL_SECS` (3600) for in-process minting.
    pub delegation_ttl_secs: u64,
    /// `SCENE_HOST_DELEGATION_MINTER_URL` / `_TOKEN`: the external minter
    /// (`catalyrst-deploy-signer --serve-delegations`) when the key stays
    /// outside this process.
    pub delegation_minter_url: Option<String>,
    pub delegation_minter_token: Option<String>,
    /// `SCENE_HOST_STORAGE_DELEGATION`: a pre-minted base64 envelope
    /// (development; disables renewal).
    pub storage_delegation: Option<String>,
    /// `SIGNED_FETCH_MAX_RESPONSE_BYTES` (2 MiB), `SIGNED_FETCH_MAX_BODY_BYTES`
    /// (1 MiB), `SIGNED_FETCH_MAX_IN_FLIGHT` (8), `SIGNED_FETCH_TIMEOUT_MS` (10000).
    pub signed_fetch_max_response_bytes: usize,
    pub signed_fetch_max_body_bytes: usize,
    pub signed_fetch_max_in_flight: usize,
    pub signed_fetch_timeout_ms: u64,
    /// `SCENE_HOST_MAX_SCENES` (64): hosted scenes at once (one isolate,
    /// two threads and up to `JS_HEAP_LIMIT_MB` each).
    pub max_scenes: usize,
    /// `SCENE_HOST_AUTOLOAD` (true): `/ws/{scene}` loads, for an admitted
    /// client, an unhosted scene that has a local active deployment with
    /// `authoritativeMultiplayer` set.
    pub autoload: bool,
    /// `SCENE_HOST_SWEEP_INTERVAL_SECS` (30; 0 disables): the re-admission and
    /// redeploy sweep.
    pub sweep_interval_secs: u64,
    /// `SCENE_HOST_IDLE_UNLOAD_SECS` (300; 0 disables): the sweep unloads a
    /// scene that has had no client for this long, including one loaded by
    /// the admin or reload routes.
    pub idle_unload_secs: u64,
}

/// Masks the key, the tokens and the pre-minted delegation.
impl std::fmt::Debug for RawSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |v: &Option<String>| v.as_ref().map(|_| "<set>");
        f.debug_struct("RawSettings")
            .field("local_scene_path", &self.local_scene_path)
            .field("debugging_secret", &set(&self.debugging_secret))
            .field("admin_token", &set(&self.admin_token))
            .field("auth_timeout_secs", &self.auth_timeout_secs)
            .field("disable_js_runtime", &self.disable_js_runtime)
            .field("realm_name", &self.realm_name)
            .field("commit_hash", &self.commit_hash)
            .field("js_heap_limit_mb", &self.js_heap_limit_mb)
            .field("js_tick_budget_ms", &self.js_tick_budget_ms)
            .field("js_shutdown_join_ms", &self.js_shutdown_join_ms)
            .field("js_update_failure_cap", &self.js_update_failure_cap)
            .field("client_outbound_max", &self.client_outbound_max)
            .field("client_inbound_max", &self.client_inbound_max)
            .field("crdt_max_components", &self.crdt_max_components)
            .field("ws_max_frame_bytes", &self.ws_max_frame_bytes)
            .field("crdt_max_value_bytes", &self.crdt_max_value_bytes)
            .field("crdt_max_bytes", &self.crdt_max_bytes)
            .field("client_inbound_max_bytes", &self.client_inbound_max_bytes)
            .field("scene_inbound_max_bytes", &self.scene_inbound_max_bytes)
            .field("client_outbound_max_bytes", &self.client_outbound_max_bytes)
            .field("js_array_buffer_limit_mb", &self.js_array_buffer_limit_mb)
            .field("storage_url", &self.storage_url)
            .field("storage_allow_http", &self.storage_allow_http)
            .field("authoritative_key", &set(&self.authoritative_key))
            .field("delegation_ttl_secs", &self.delegation_ttl_secs)
            .field("delegation_minter_url", &self.delegation_minter_url)
            .field(
                "delegation_minter_token",
                &set(&self.delegation_minter_token),
            )
            .field("storage_delegation", &set(&self.storage_delegation))
            .field(
                "signed_fetch_max_response_bytes",
                &self.signed_fetch_max_response_bytes,
            )
            .field(
                "signed_fetch_max_body_bytes",
                &self.signed_fetch_max_body_bytes,
            )
            .field(
                "signed_fetch_max_in_flight",
                &self.signed_fetch_max_in_flight,
            )
            .field("signed_fetch_timeout_ms", &self.signed_fetch_timeout_ms)
            .field("max_scenes", &self.max_scenes)
            .field("autoload", &self.autoload)
            .field("sweep_interval_secs", &self.sweep_interval_secs)
            .field("idle_unload_secs", &self.idle_unload_secs)
            .finish()
    }
}

impl Default for RawSettings {
    fn default() -> Self {
        Self {
            local_scene_path: None,
            debugging_secret: None,
            admin_token: None,
            auth_timeout_secs: 5,
            disable_js_runtime: false,
            realm_name: None,
            commit_hash: None,
            js_heap_limit_mb: 384,
            js_tick_budget_ms: 250,
            js_shutdown_join_ms: 2000,
            js_update_failure_cap: 30,
            client_outbound_max: 1024,
            client_inbound_max: 1024,
            crdt_max_components: 100_000,
            ws_max_frame_bytes: 2 * 1024 * 1024,
            crdt_max_value_bytes: 256 * 1024,
            crdt_max_bytes: 32 * 1024 * 1024,
            client_inbound_max_bytes: 8 * 1024 * 1024,
            scene_inbound_max_bytes: 32 * 1024 * 1024,
            client_outbound_max_bytes: 8 * 1024 * 1024,
            js_array_buffer_limit_mb: 128,
            storage_url: None,
            storage_allow_http: false,
            authoritative_key: None,
            delegation_ttl_secs: 3600,
            delegation_minter_url: None,
            delegation_minter_token: None,
            storage_delegation: None,
            signed_fetch_max_response_bytes: 2 * 1024 * 1024,
            signed_fetch_max_body_bytes: 1024 * 1024,
            signed_fetch_max_in_flight: 8,
            signed_fetch_timeout_ms: 10_000,
            max_scenes: 64,
            autoload: true,
            sweep_interval_secs: 30,
            idle_unload_secs: 300,
        }
    }
}

/// The storage origin scenes may reach.
#[derive(Debug, Clone)]
pub struct StorageOrigin {
    pub origin: url::Url,
    pub allow_http_loopback: bool,
}

/// Where a hosted scene's storage delegation comes from.
#[derive(Clone)]
pub enum DelegationSource {
    /// No minter and no envelope: storage requests fail closed.
    None,
    /// A pre-minted envelope (development): never renewed.
    Static(StorageDelegation),
    /// Minted per scene and renewed before expiry.
    Minter(Arc<DelegationMinter>),
}

#[derive(Clone)]
pub struct Settings {
    pub local_scene_path: Option<String>,
    pub debugging_secret: Option<String>,
    pub admin_token: Option<String>,
    pub auth_timeout: Duration,
    pub disable_js_runtime: bool,
    pub realm_name: String,
    pub commit_hash: Option<String>,
    pub limits: RuntimeLimits,
    pub ws_max_frame_bytes: usize,
    pub storage: Option<StorageOrigin>,
    pub delegations: DelegationSource,
    pub max_scenes: usize,
    pub autoload: bool,
    pub sweep_interval: Option<Duration>,
    pub idle_unload: Option<Duration>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Settings {
    /// Checks the storage origin and the delegation source at startup; a
    /// storage origin without any delegation source is a warning (requests
    /// fail closed), a malformed one an error.
    pub fn resolve(raw: RawSettings, http: &Outbound) -> anyhow::Result<Self> {
        let debugging_secret = non_empty(raw.debugging_secret);
        let admin_token = non_empty(raw.admin_token).or_else(|| debugging_secret.clone());
        let storage = match non_empty(raw.storage_url) {
            Some(url) => Some(StorageOrigin {
                origin: jsruntime::parse_origin(&url, raw.storage_allow_http)
                    .map_err(|reason| anyhow!("SCENE_HOST_STORAGE_URL rejected: {reason}"))?,
                allow_http_loopback: raw.storage_allow_http,
            }),
            None => None,
        };
        let ttl = Duration::from_secs(raw.delegation_ttl_secs.max(1));
        let delegations = if let Some(encoded) = non_empty(raw.storage_delegation) {
            let d = delegation::parse_storage_delegation(&encoded).ok_or_else(|| {
                anyhow!("SCENE_HOST_STORAGE_DELEGATION did not parse as a v1 delegation envelope")
            })?;
            DelegationSource::Static(d)
        } else if let Some(key) = non_empty(raw.authoritative_key) {
            let wallet =
                Wallet::from_hex(&key).map_err(|e| anyhow!("SCENE_HOST_AUTHORITATIVE_KEY: {e}"))?;
            tracing::info!(authoritative = %wallet.address(), "scene host mints storage delegations in-process");
            DelegationSource::Minter(Arc::new(DelegationMinter::Local {
                authoritative: Arc::new(wallet),
                ttl,
            }))
        } else if let Some(url) = non_empty(raw.delegation_minter_url) {
            let parsed =
                url::Url::parse(&url).context("SCENE_HOST_DELEGATION_MINTER_URL is not a URL")?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(anyhow!(
                    "SCENE_HOST_DELEGATION_MINTER_URL must be an http(s) URL"
                ));
            }
            DelegationSource::Minter(Arc::new(DelegationMinter::Remote {
                http: http.raw().clone(),
                url: url.trim_end_matches('/').to_string(),
                token: non_empty(raw.delegation_minter_token),
            }))
        } else {
            DelegationSource::None
        };
        if storage.is_some() && matches!(delegations, DelegationSource::None) {
            tracing::warn!(
                "SCENE_HOST_STORAGE_URL set without SCENE_HOST_AUTHORITATIVE_KEY, SCENE_HOST_DELEGATION_MINTER_URL or \
                 SCENE_HOST_STORAGE_DELEGATION; scene storage requests fail closed"
            );
        }
        for (name, value) in [
            ("CRDT_MAX_VALUE_BYTES", raw.crdt_max_value_bytes),
            ("CRDT_MAX_BYTES", raw.crdt_max_bytes),
            ("CLIENT_INBOUND_MAX_BYTES", raw.client_inbound_max_bytes),
            ("SCENE_INBOUND_MAX_BYTES", raw.scene_inbound_max_bytes),
            ("CLIENT_OUTBOUND_MAX_BYTES", raw.client_outbound_max_bytes),
            ("JS_ARRAY_BUFFER_LIMIT_MB", raw.js_array_buffer_limit_mb),
        ] {
            if value == 0 {
                return Err(anyhow!("{name} must be at least 1"));
            }
        }
        // getMessages copies a client's whole queue, crdtGetState the whole snapshot.
        if raw.js_array_buffer_limit_mb.saturating_mul(1024 * 1024)
            < raw.client_inbound_max_bytes.max(raw.crdt_max_bytes)
        {
            return Err(anyhow!(
                "JS_ARRAY_BUFFER_LIMIT_MB is below CLIENT_INBOUND_MAX_BYTES or CRDT_MAX_BYTES"
            ));
        }
        if raw.crdt_max_value_bytes > raw.crdt_max_bytes {
            return Err(anyhow!("CRDT_MAX_VALUE_BYTES exceeds CRDT_MAX_BYTES"));
        }
        // A client must be able to queue at least one frame of the largest size.
        if raw.client_inbound_max_bytes < raw.ws_max_frame_bytes {
            return Err(anyhow!(
                "CLIENT_INBOUND_MAX_BYTES is below WS_MAX_FRAME_BYTES"
            ));
        }
        // A quarter of the scene bound is for frames held before registration.
        let readable = raw.scene_inbound_max_bytes
            - runtime::InboundBudget::held_share(raw.scene_inbound_max_bytes);
        if readable < raw.client_inbound_max_bytes {
            return Err(anyhow!(
                "three quarters of SCENE_INBOUND_MAX_BYTES are below CLIENT_INBOUND_MAX_BYTES"
            ));
        }
        if admin_token.is_none() {
            tracing::warn!(
                "SCENE_HOST_ADMIN_TOKEN / SCENE_HOST_DEBUGGING_SECRET unset; the scene admin and reload routes refuse every caller"
            );
        }
        Ok(Self {
            local_scene_path: non_empty(raw.local_scene_path),
            debugging_secret,
            admin_token,
            auth_timeout: Duration::from_secs(raw.auth_timeout_secs.max(1)),
            disable_js_runtime: raw.disable_js_runtime,
            realm_name: non_empty(raw.realm_name).unwrap_or_else(|| DEFAULT_REALM_NAME.to_string()),
            commit_hash: non_empty(raw.commit_hash),
            limits: RuntimeLimits {
                js_heap_limit_mb: raw.js_heap_limit_mb,
                js_tick_budget_ms: raw.js_tick_budget_ms,
                js_shutdown_join_ms: raw.js_shutdown_join_ms,
                js_update_failure_cap: raw.js_update_failure_cap,
                client_inbound_max: raw.client_inbound_max,
                client_outbound_max: raw.client_outbound_max,
                crdt_max_components: raw.crdt_max_components,
                fetch_max_response_bytes: raw.signed_fetch_max_response_bytes,
                fetch_max_body_bytes: raw.signed_fetch_max_body_bytes,
                fetch_max_in_flight: raw.signed_fetch_max_in_flight,
                fetch_timeout_ms: raw.signed_fetch_timeout_ms,
                crdt_max_value_bytes: raw.crdt_max_value_bytes,
                crdt_max_bytes: raw.crdt_max_bytes,
                client_inbound_max_bytes: raw.client_inbound_max_bytes,
                scene_inbound_max_bytes: raw.scene_inbound_max_bytes,
                client_outbound_max_bytes: raw.client_outbound_max_bytes,
                js_array_buffer_limit_mb: raw.js_array_buffer_limit_mb,
            },
            ws_max_frame_bytes: raw.ws_max_frame_bytes,
            storage,
            delegations,
            max_scenes: raw.max_scenes.max(1),
            autoload: raw.autoload,
            sweep_interval: (raw.sweep_interval_secs > 0)
                .then(|| Duration::from_secs(raw.sweep_interval_secs)),
            idle_unload: (raw.idle_unload_secs > 0)
                .then(|| Duration::from_secs(raw.idle_unload_secs)),
        })
    }

    /// The defaults with the JS runtime on and no storage: for tests.
    pub fn for_tests() -> Self {
        Self::resolve(RawSettings::default(), &Outbound::for_tests()).expect("the defaults resolve")
    }
}

pub struct SceneHostState {
    pub(crate) secret_max_attempts: u32,
    pub pool: PgPool,
    pub storage: Arc<Storage>,
    /// The permission core: admission and re-admission of every socket.
    pub policy: Arc<Policy>,
    pub settings: Settings,
    pub scenes: SceneManager,
    /// The client `~system/SignedFetch` sends through: the guarded client with
    /// no redirects (a storage answer's redirect is handed to the scene, never
    /// followed) and the SignedFetch timeouts; the loopback is admitted only
    /// for an operator-allowed `http://` storage origin.
    pub(crate) fetch_http: Outbound,
    pub eip1654: Option<Arc<dyn Eip1654Validator>>,
    /// Joins and unloads exclude each other briefly; loads of one key run
    /// one at a time without holding up other keys.
    loads: SceneLocks,
    /// The jobs' root token once `spawn_jobs` ran: every per-scene delegation
    /// renewal loop stops with it, not only when its scene is dropped.
    shutdown: std::sync::OnceLock<tokio_util::sync::CancellationToken>,
    /// What a V8 OOM parked since start: refused until restart.
    parked: parking_lot::Mutex<ParkedScenes>,
}

/// Scenes this host retired after a V8 OOM parked their isolate.
#[derive(Default)]
struct ParkedScenes {
    /// Room keys refused from then on.
    keys: std::collections::HashSet<String>,
    /// Genesis parcels refused from then on, so a redeploy under a new id
    /// cannot take the slot again.
    parcels: std::collections::HashSet<String>,
    /// Isolates still held; each keeps a slot.
    isolates: usize,
}

pub type SharedState = Arc<SceneHostState>;

/// How loads and joins exclude each other.
#[derive(Default)]
struct SceneLocks {
    /// Held, never across an await, while a scene is looked up and a client
    /// added, or a scene removed.
    membership: parking_lot::Mutex<()>,
    /// One lock per key: loads of the same key run one at a time.
    per_key: parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Loads in flight, counted against the scene cap.
    loading: std::sync::atomic::AtomicUsize,
}

struct SlotReservation<'a>(Option<&'a std::sync::atomic::AtomicUsize>);

impl Drop for SlotReservation<'_> {
    fn drop(&mut self) {
        if let Some(loading) = self.0 {
            loading.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl SceneHostState {
    pub fn with_secret_max_attempts(mut self, max: u32) -> Self {
        self.secret_max_attempts = max.max(1);
        self
    }

    pub fn new(
        pool: PgPool,
        storage: Arc<Storage>,
        policy: Arc<Policy>,
        settings: Settings,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> anyhow::Result<Self> {
        let timeout = Duration::from_millis(settings.limits.fetch_timeout_ms.max(1));
        let cfg = HttpClientCfg::default()
            .with_total_timeout(timeout)
            .with_connect_timeout(timeout.min(Duration::from_secs(5)))
            .with_user_agent("catalyst-scene-host");
        let mut fetch_http = Outbound::new(cfg).context("building the SignedFetch client")?;
        if settings
            .storage
            .as_ref()
            .is_some_and(|s| s.allow_http_loopback)
        {
            fetch_http = fetch_http.allowing_private_hosts();
        }
        Ok(Self {
            secret_max_attempts:
                crate::comms::gatekeeper::admission::world_mint::DEFAULT_SHARED_SECRET_MAX_ATTEMPTS,
            pool,
            storage,
            policy,
            settings,
            scenes: SceneManager::new(),
            fetch_http,
            eip1654,
            loads: SceneLocks::default(),
            shutdown: std::sync::OnceLock::new(),
            parked: parking_lot::Mutex::new(ParkedScenes::default()),
        })
    }

    /// A child of the jobs' root token (a fresh, never-cancelled token before
    /// `spawn_jobs`, as in tests): what a scene's background work stops on.
    pub fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown
            .get()
            .map(|t| t.child_token())
            .unwrap_or_default()
    }

    pub fn eip1654(&self) -> Option<&dyn Eip1654Validator> {
        self.eip1654.as_deref()
    }

    /// The key a scene is hosted under: a Genesis parcel names the scene
    /// standing on it, so every parcel of a scene shares one host.
    pub async fn canonical_key(&self, key: &SceneKey) -> SceneKey {
        if let SceneKey::GenesisParcel(_) = key
            && let Ok(Some(id)) = loader::current_scene_id(self, key).await
        {
            return SceneKey::Genesis(id);
        }
        key.clone()
    }

    /// The scene hosted for `key`, parcel aliases included.
    pub async fn hosted(&self, key: &SceneKey) -> Option<Arc<scene::Scene>> {
        self.scenes.get(&self.canonical_key(key).await.room_key())
    }

    /// Loads (or reloads) `key` under its key's load lock and the scene cap.
    /// A reload reserves a slot too, for the time its old scene is down.
    pub async fn load(&self, key: &SceneKey) -> anyhow::Result<()> {
        let key = self.canonical_key(key).await;
        let lock = self.load_lock(&key);
        let _guard = lock.lock().await;
        self.retire_parked();
        let _slot = self.reserve_slot(&key)?;
        load_or_reload(self, &key).await
    }

    /// The lock loads of `key` take; loads of other keys never wait on it.
    pub(crate) fn load_lock(&self, key: &SceneKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.loads.per_key.lock();
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(locks.entry(key.room_key()).or_default())
    }

    /// Holds a place under the scene cap for a load of `key` until the
    /// returned guard drops; a reload of a hosted `key` is never refused.
    fn reserve_slot(&self, key: &SceneKey) -> anyhow::Result<SlotReservation<'_>> {
        let _membership = self.loads.membership.lock();
        let loading = &self.loads.loading;
        // A reload keeps its slot while the old scene is down.
        if self.scenes.get(&key.room_key()).is_some() {
            loading.fetch_add(1, Ordering::SeqCst);
            return Ok(SlotReservation(Some(loading)));
        }
        // Isolates parked by an OOM keep their memory, so they hold a slot.
        let held = self.scenes.len() + loading.load(Ordering::SeqCst) + self.parked_count();
        if held >= self.settings.max_scenes {
            anyhow::bail!(
                "scene host is full ({} scenes hosted, SCENE_HOST_MAX_SCENES)",
                self.settings.max_scenes
            );
        }
        loading.fetch_add(1, Ordering::SeqCst);
        Ok(SlotReservation(Some(loading)))
    }

    /// Removes and kicks the scene hosted under `name`, atomically with joins.
    /// Every removal goes through here or [`Self::unload_idle`], which both
    /// record an OOM-parked scene ([`Self::note_removed`]).
    pub(crate) fn remove_hosted(&self, name: &str) -> Option<(Arc<scene::Scene>, usize)> {
        let _membership = self.loads.membership.lock();
        let scene = self.scenes.remove(name)?;
        self.note_removed(&scene);
        let kicked = scene.kick_all();
        Some((scene, kicked))
    }

    /// Adds an admitted client to the scene hosting `identity`'s deployment.
    /// The lookup and the add happen under the membership lock unloads take,
    /// so a client never lands in an unloaded scene; only a missing scene
    /// waits, on its own key's load lock. A scene that is no longer hosted is
    /// loaded again only if it sets `authoritativeMultiplayer`; a different
    /// deployment now hosted under the key refuses the join (the client was
    /// admitted for the old one).
    pub async fn join<R>(
        &self,
        identity: &scene::SceneIdentity,
        add: impl FnOnce(&Arc<scene::Scene>) -> R,
    ) -> anyhow::Result<(Arc<scene::Scene>, R)> {
        let key = &identity.key;
        let mut add = Some(add);
        for _ in 0..2 {
            if let Some(joined) = self.try_join(identity, &mut add)? {
                return Ok(joined);
            }
            let lock = self.load_lock(key);
            let _guard = lock.lock().await;
            if let Some(joined) = self.try_join(identity, &mut add)? {
                return Ok(joined);
            }
            let _slot = self.reserve_slot(key)?;
            loader::load_scene(self, key, true).await?;
        }
        Err(anyhow!("{key} was not hosted after loading"))
    }

    fn try_join<R, F: FnOnce(&Arc<scene::Scene>) -> R>(
        &self,
        identity: &scene::SceneIdentity,
        add: &mut Option<F>,
    ) -> anyhow::Result<Option<(Arc<scene::Scene>, R)>> {
        let _membership = self.loads.membership.lock();
        match self.scenes.get(&identity.key.room_key()) {
            Some(scene) if scene.identity.scene_id() == identity.scene_id() => {
                let add = add.take().ok_or_else(|| anyhow!("client already added"))?;
                let added = add(&scene);
                Ok(Some((scene, added)))
            }
            Some(_) => Err(anyhow!(
                "{} was redeployed while the client was admitted",
                identity.key
            )),
            None => Ok(None),
        }
    }

    /// Unloads every scene other than the local one that has had no client
    /// for the idle timeout; each check and removal is atomic with joins.
    pub async fn unload_idle(&self) -> usize {
        let Some(timeout) = self.settings.idle_unload else {
            return 0;
        };
        let mut unloaded = 0;
        for scene in self.scenes.all() {
            let key = scene.identity.key.clone();
            let _membership = self.loads.membership.lock();
            let still_hosted = self
                .scenes
                .get(&key.room_key())
                .is_some_and(|hosted| Arc::ptr_eq(&hosted, &scene));
            if still_hosted
                && key != SceneKey::Local
                && scene.idle_for().is_some_and(|idle| idle >= timeout)
            {
                tracing::info!(scene = %key, "no clients for the idle timeout; unloading");
                self.scenes.remove(&key.room_key());
                self.note_removed(&scene);
                scene.kick_all();
                unloaded += 1;
            }
        }
        unloaded
    }

    /// Unloads every scene an OOM parked; [`Self::note_removed`] records it.
    pub fn retire_parked(&self) -> usize {
        if jsruntime::parked_isolates() == 0 {
            return 0;
        }
        let mut retired = 0;
        for scene in self.scenes.all() {
            if scene.runtime.parked()
                && self.remove_hosted(&scene.identity.key.room_key()).is_some()
            {
                retired += 1;
            }
        }
        retired
    }

    /// For a removed scene an OOM parked: its key (and a Genesis scene's
    /// parcels) are refused until restart and its isolate keeps a slot.
    /// Per runtime: a Genesis scene hosted under two keys is two runtimes.
    fn note_removed(&self, scene: &scene::Scene) {
        if !scene.runtime.parked() {
            return;
        }
        let key = &scene.identity.key;
        tracing::error!(scene = %key, "scene ran out of memory; unloaded and not loaded again until restart");
        let mut parked = self.parked.lock();
        parked.keys.insert(key.room_key());
        if matches!(key, SceneKey::Genesis(_) | SceneKey::GenesisParcel(_))
            && let Some(deployment) = &scene.identity.deployment
        {
            parked.parcels.extend(deployment.parcels.canonical());
        }
        parked.isolates += 1;
    }

    /// Whether an OOM parked the scene under `room_key` since start.
    pub fn is_key_parked(&self, room_key: &str) -> bool {
        self.parked.lock().keys.contains(room_key)
    }

    /// Whether a scene on any of `parcels` was parked by an OOM since start.
    pub fn covers_parked_parcel(&self, parcels: &[String]) -> bool {
        let parked = self.parked.lock();
        parcels.iter().any(|p| parked.parcels.contains(p))
    }

    /// Isolates this host retired after an OOM; each keeps a slot.
    pub fn parked_count(&self) -> usize {
        self.parked.lock().isolates
    }

    /// Stops hosting `key`: every client is kicked and the runtime dropped.
    pub fn unload(&self, key: &SceneKey) -> usize {
        match self.remove_hosted(&key.room_key()) {
            Some((_, kicked)) => {
                tracing::info!(scene = %key, kicked, "scene unloaded");
                kicked
            }
            None => 0,
        }
    }

    /// Closes every socket `address` holds on any hosted scene: the local
    /// call a platform block makes so revocation reaches the running room
    /// without waiting for the sweep.
    pub fn kick_address(&self, address: &crate::shared::identity::Address) -> usize {
        self.scenes
            .all()
            .iter()
            .map(|scene| scene.kick_address(address.as_str()))
            .sum()
    }

    /// Closes the sockets `address` holds on `world`'s scene.
    pub fn kick_from_world(
        &self,
        world: &WorldName,
        address: &crate::shared::identity::Address,
    ) -> usize {
        self.scenes
            .all()
            .iter()
            .filter(|s| s.identity.key.world() == Some(world))
            .map(|s| s.kick_address(address.as_str()))
            .sum()
    }

    /// Decides every connected client of `world`'s scene again, now, and
    /// kicks the refused. With `void_secrets` a client admitted by the
    /// shared secret is not carried by that prior grant: the secret changed,
    /// it must present the new one.
    pub async fn readmit_world(&self, world: &WorldName, void_secrets: bool) -> usize {
        let now = chrono::Utc::now().naive_utc();
        let mut kicked = 0;
        for scene in self
            .scenes
            .all()
            .into_iter()
            .filter(|s| s.identity.key.world() == Some(world))
        {
            for client in scene.clients() {
                let Ok(address) = crate::shared::identity::Address::parse(&client.address) else {
                    continue;
                };
                let prior = client
                    .grant
                    .filter(|g| !void_secrets || *g != AccessGrant::SharedSecret);
                if let Err(reason) =
                    ws::admit(self, &scene.identity, &address, None, prior, now).await
                {
                    if let ws::AdmissionRefused::Undetermined(why) = &reason {
                        tracing::warn!(scene = %scene.identity.key, %address, %why, "admission undetermined; client kept");
                        continue;
                    }
                    tracing::info!(scene = %scene.identity.key, %address, %reason, "admission revoked; closing the socket");
                    if scene.remove_connection(client.index, client.id) {
                        kicked += 1;
                    }
                }
            }
        }
        kicked
    }

    /// One sweep: reload every hosted scene whose active deployment moved
    /// (or unload one that lost it), then re-run admission for every
    /// connected client and kick the refused.
    pub async fn sweep(&self) -> SweepSummary {
        let mut summary = SweepSummary {
            unloaded: self.retire_parked() + self.unload_idle().await,
            ..SweepSummary::default()
        };
        for scene in self.scenes.all() {
            let key = scene.identity.key.clone();
            match loader::current_scene_id(self, &key).await {
                Ok(Some(current)) if Some(&current) != scene.identity.scene_id() => {
                    tracing::info!(scene = %key, from = ?scene.identity.scene_id(), to = %current, "active deployment moved; reloading");
                    match self.load(&key).await {
                        Ok(()) => summary.reloaded += 1,
                        Err(e) => {
                            tracing::warn!(scene = %key, error = %e, "reload after redeploy failed; unloading");
                            self.unload(&key);
                            summary.unloaded += 1;
                        }
                    }
                    continue;
                }
                Ok(Some(_)) => {}
                Ok(None) if key != SceneKey::Local => {
                    tracing::info!(scene = %key, "no local active deployment any more; unloading");
                    self.unload(&key);
                    summary.unloaded += 1;
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(scene = %key, error = %e, "sweep could not resolve the active deployment; keeping the scene");
                    summary.errors += 1;
                    continue;
                }
            }
            let now = chrono::Utc::now().naive_utc();
            for client in scene.clients() {
                let Ok(address) = crate::shared::identity::Address::parse(&client.address) else {
                    continue;
                };
                match ws::admit(self, &scene.identity, &address, None, client.grant, now).await {
                    Ok(_) => {}
                    // An authority or store fault keeps the client, as the comms re-admit does.
                    Err(ws::AdmissionRefused::Undetermined(why)) => {
                        tracing::warn!(scene = %key, %address, %why, "admission undetermined; client kept");
                        summary.errors += 1;
                    }
                    Err(reason) => {
                        tracing::info!(scene = %key, %address, %reason, "admission revoked; closing the socket");
                        if scene.remove_connection(client.index, client.id) {
                            summary.kicked += 1;
                        }
                    }
                }
            }
        }
        summary
    }

    /// The `scene-host-sweep` periodic job and, when configured, the one-shot
    /// load of the local development scene.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs) {
        let _ = self.shutdown.set(jobs.root_token());
        if self.settings.local_scene_path.is_some() {
            let state = self.clone();
            jobs.spawn("scene-host-local-scene", move |_ctx| async move {
                if let Err(e) = state.load(&SceneKey::Local).await {
                    tracing::warn!(error = %e, "failed to load SCENE_HOST_LOCAL_SCENE_PATH");
                }
                Ok(())
            });
        }
        let Some(interval) = self.settings.sweep_interval else {
            tracing::info!(
                "SCENE_HOST_SWEEP_INTERVAL_SECS=0: no re-admission sweep; revocation reaches running rooms only through kick_address"
            );
            return;
        };
        let state = self.clone();
        let cfg = PeriodicCfg::every(interval)
            .after_first_interval()
            .with_error_backoff(Duration::from_secs(300));
        jobs.spawn_periodic("scene-host-sweep", cfg, move |ctx| {
            let state = state.clone();
            async move {
                let summary = state.sweep().await;
                ctx.progress(format!("{} scenes hosted; {summary:?}", state.scenes.len()));
                Ok(())
            }
        });
    }
}

#[async_trait::async_trait]
impl RevocationSink for SceneHostState {
    async fn revoke(&self, revocation: &Revocation) {
        let kicked = match revocation {
            Revocation::Address { world, address } => self.kick_from_world(world, address),
            Revocation::AccessSetting { world } => self.readmit_world(world, true).await,
            Revocation::AccessRecheck { world } => self.readmit_world(world, false).await,
            Revocation::Platform { address } => self.kick_address(address),
        };
        tracing::info!(?revocation, kicked, "scene host applied the revocation");
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepSummary {
    pub reloaded: usize,
    pub unloaded: usize,
    pub kicked: usize,
    pub errors: usize,
}

/// The old routes, prefix-free: nest under [`ROUTE_PREFIX`]. `/status` and
/// `/ping` collide with the content server's at the root, so this router is
/// never merged there.
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/ping", get(handlers::ping))
        .route("/status", get(handlers::status))
        .route("/debugging/reload", post(handlers::reload))
        .route("/admin/scene/{scene}/crdt", get(admin::inspect_crdt))
        .route("/admin/scene/{scene}/kick-all", post(admin::kick_all))
        .route("/admin/scene/{scene}/reset", post(admin::reset_state))
        .route("/ws/{scene}", get(ws::ws_upgrade))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(axum::middleware::from_fn(nul_guard::nul_guard_middleware))
        .layer(axum::middleware::from_fn(cors::cors_middleware))
        .with_state(state)
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn scene_keys_classify_local_parcel_world_and_scene_id() {
        assert_eq!(SceneKey::parse("localScene").unwrap(), SceneKey::Local);
        assert_eq!(SceneKey::parse(" 01,-2 ").unwrap().room_key(), "1,-2");
        let w = SceneKey::parse("Foo.dcl.eth").unwrap();
        assert_eq!(w.room_key(), "foo.dcl.eth");
        assert_eq!(w.world().unwrap().display(), "Foo.dcl.eth");
        assert_eq!(
            SceneKey::parse("bafyScene").unwrap().room_key(),
            "bafyScene"
        );
        assert!(SceneKey::parse("").is_err());
        assert!(SceneKey::parse("a b").is_err());
    }

    #[test]
    fn settings_resolve_the_delegation_source_in_the_old_precedence() {
        let http = Outbound::for_tests();
        let s = Settings::resolve(RawSettings::default(), &http).unwrap();
        assert!(s.storage.is_none() && matches!(s.delegations, DelegationSource::None));
        assert_eq!(s.realm_name, DEFAULT_REALM_NAME);
        assert_eq!(s.auth_timeout, Duration::from_secs(5));
        assert_eq!(s.sweep_interval, Some(Duration::from_secs(30)));
        assert!(s.admin_token.is_none());

        let mut raw = RawSettings {
            debugging_secret: Some("dbg".into()),
            ..RawSettings::default()
        };
        raw.storage_url = Some("http://127.0.0.1:5210".into());
        assert!(
            Settings::resolve(raw.clone(), &http).is_err(),
            "http origin needs the loopback opt-in"
        );
        raw.storage_allow_http = true;
        raw.authoritative_key =
            Some("0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318".into());
        raw.delegation_minter_url = Some("http://127.0.0.1:1".into());
        raw.sweep_interval_secs = 0;
        let s = Settings::resolve(raw.clone(), &http).unwrap();
        assert_eq!(
            s.admin_token.as_deref(),
            Some("dbg"),
            "the admin token falls back to the debugging secret"
        );
        assert!(s.storage.as_ref().unwrap().allow_http_loopback);
        assert!(
            matches!(&s.delegations, DelegationSource::Minter(m) if matches!(**m, DelegationMinter::Local { .. }))
        );
        assert!(s.sweep_interval.is_none());
        raw.authoritative_key = None;
        let s = Settings::resolve(raw.clone(), &http).unwrap();
        assert!(
            matches!(&s.delegations, DelegationSource::Minter(m) if matches!(**m, DelegationMinter::Remote { .. }))
        );
        raw.authoritative_key = Some("nope".into());
        assert!(Settings::resolve(raw, &http).is_err());
    }

    #[test]
    fn byte_limits_are_validated() {
        let http = Outbound::for_tests();
        let ok = |tune: fn(&mut RawSettings)| {
            let mut raw = RawSettings::default();
            tune(&mut raw);
            Settings::resolve(raw, &http).is_ok()
        };
        assert!(ok(|_| {}));
        assert!(!ok(|r| r.client_outbound_max_bytes = 0));
        assert!(!ok(|r| r.js_array_buffer_limit_mb = 0));
        assert!(!ok(
            |r| r.js_array_buffer_limit_mb = r.crdt_max_bytes / (1024 * 1024) - 1
        ));
        assert!(!ok(|r| r.crdt_max_value_bytes = r.crdt_max_bytes + 1));
        assert!(!ok(
            |r| r.client_inbound_max_bytes = r.ws_max_frame_bytes - 1
        ));
        assert!(!ok(
            |r| r.scene_inbound_max_bytes = r.client_inbound_max_bytes
        ));
        // The boundary: three quarters of the scene bound hold exactly one client.
        assert!(ok(|r| {
            r.client_inbound_max_bytes = 3 << 20;
            r.scene_inbound_max_bytes = 4 << 20;
        }));
        assert!(!ok(|r| {
            r.client_inbound_max_bytes = 3 << 20;
            r.scene_inbound_max_bytes = (4 << 20) - 4;
        }));
    }

    #[test]
    fn debug_output_never_carries_the_secrets() {
        let raw = RawSettings {
            debugging_secret: Some("dbg-secret-0xabab".into()),
            admin_token: Some("admin-0xabab".into()),
            authoritative_key: Some(
                "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318".into(),
            ),
            delegation_minter_token: Some("minter-0xabab".into()),
            storage_delegation: Some("envelope-0xabab".into()),
            ..RawSettings::default()
        };
        let shown = format!("{raw:?}");
        assert!(
            !shown.contains("abab") && !shown.contains("4c0883"),
            "{shown}"
        );
        assert!(
            shown.contains("authoritative_key: Some(\"<set>\")"),
            "{shown}"
        );
    }
}
