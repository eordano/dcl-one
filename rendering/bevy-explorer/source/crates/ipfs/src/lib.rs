mod batch_sizes;
pub mod bevy_variants;
pub mod content_hash;
mod debug_panel;
mod download;
use download::{InFlightDownloads, RequestMetrics};
mod priority_gate;
pub use priority_gate::{PriorityGate, Ticket};
#[cfg(feature = "ipfs_debug")]
mod ipfs_debug;
pub mod ipfs_path;
mod request_metrics;
#[cfg(any(target_arch = "wasm32", test))]
pub mod web_cache;

use std::{
    borrow::Cow,
    io::ErrorKind,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering},
        Arc, Weak,
    },
};

#[cfg(feature = "ipfs_debug")]
use common::structs::SceneJsFile;
use common::util::ReportErr;
use common::util::UrlLoopbackExt;
use futures_io::{AsyncRead, AsyncSeek};
use web_time::{Duration, Instant};

use anyhow::anyhow;
use async_std::io::WriteExt;
use bevy::{
    asset::{
        io::{
            AssetReader, AssetReaderError, AssetSource, AssetSourceId, AsyncSeekForward,
            ErasedAssetReader, Reader, VecReader,
        },
        meta::Settings,
        Asset, AssetLoader, AssetPath, LoadState, UntypedAssetId,
    },
    diagnostic::{
        Diagnostic, DiagnosticMeasurement, DiagnosticPath, DiagnosticsStore, RegisterDiagnostic,
    },
    ecs::system::SystemParam,
    platform::collections::HashMap,
    prelude::*,
    reflect::TypePath,
    tasks::{ConditionalSendFuture, IoTaskPool, Task},
};
use tokio::sync::watch;

#[cfg(not(target_arch = "wasm32"))]
use bevy::asset::io::file::FileAssetReader;
#[cfg(not(target_arch = "wasm32"))]
use platform::project_directories;

#[cfg(target_arch = "wasm32")]
use bevy::asset::io::wasm::HttpWasmAssetReader;

use bevy_console::{ConsoleCommand, PrintConsoleLine};
use common::{
    rpc::{RpcResultReceiver, RpcResultSender},
    sets::RealmLifecycle,
    structs::{
        server_mode, AppConfig, CommsConfig, CurrentRealm, PreviewMode, ServerConfiguration,
        WorldHold,
    },
    util::TaskCompat,
};
use ipfs_path::IpfsAsset;
use platform::AsyncRwLock;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use wallet::Wallet;

use console::{DoAddConsoleCommand, PendingConsoleResponses};

#[allow(unused_imports)]
use platform::ReqwestBuilderExt;

#[cfg(feature = "ipfs_debug")]
use crate::ipfs_debug::{IpfsDebug, IpfsDebugReceiver, IpfsDebugStatus};
pub mod world_access;
use common::structs::WorldPasswordPrompter;
use world_access::WorldCommsSecret;

use common::util::JoinRelativeExt;

use self::ipfs_path::{content_file_path, CacheKey, IpfsKey, IpfsPath, IpfsType};
use common::partition::LoadPriority;

const IPFS_IN_FLIGHT_DIAGNOSTIC_PATH: DiagnosticPath = DiagnosticPath::const_new("IPFS_IN_FLIGHT");
static IPFS_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);
const IPFS_SUCCESS_DIAGNOSTIC_PATH: DiagnosticPath = DiagnosticPath::const_new("IPFS_SUCCESS");
static IPFS_SUCCESS: AtomicU32 = AtomicU32::new(0);
const IPFS_FAILED_DIAGNOSTIC_PATH: DiagnosticPath = DiagnosticPath::const_new("IPFS_FAILED");
static IPFS_FAILED: AtomicU32 = AtomicU32::new(0);
const IPFS_CACHED_DIAGNOSTIC_PATH: DiagnosticPath = DiagnosticPath::const_new("IPFS_CACHED");
static IPFS_CACHED: AtomicU32 = AtomicU32::new(0);
const IPFS_NON_IPFS_DIAGNOSTIC_PATH: DiagnosticPath = DiagnosticPath::const_new("IPFS_NON_IPFS");
static IPFS_NON_IPFS: AtomicU32 = AtomicU32::new(0);

#[derive(Serialize, Deserialize, Debug)]
pub struct TypedIpfsRef {
    pub file: String,
    pub hash: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EntityDefinitionJson {
    pub id: Option<String>,
    pub pointers: Vec<String>,
    pub content: Vec<TypedIpfsRef>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Asset, Debug, Default, TypePath)]
pub struct EntityDefinition {
    pub id: String,
    pub pointers: Vec<String>,
    pub content: ContentMap,
    pub metadata: Option<serde_json::Value>,
}

impl IpfsAsset for EntityDefinition {
    fn ext() -> &'static str {
        "entity_definition"
    }
}

impl IpfsAsset for SceneJsFile {
    fn ext() -> &'static str {
        "js"
    }
}

#[derive(Default)]
pub struct EntityDefinitionLoader;

impl EntityDefinitionLoader {
    pub async fn load_internal(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        id_fn: impl FnOnce() -> String + Send + Sync,
    ) -> Result<EntityDefinition, anyhow::Error> {
        let mut bytes = Vec::default();
        reader.read_to_end(&mut bytes).await?;

        let maybe_definition_json = {
            // try to parse as a vec
            let definition_json_vec: Result<Vec<EntityDefinitionJson>, _> =
                serde_json::from_reader(bytes.as_slice());
            match definition_json_vec {
                Ok(mut vec) => vec.pop(),
                Err(_) => {
                    // else try to parse as a single item
                    Some(serde_json::from_reader(bytes.as_slice())?)
                }
            }
        };
        let Some(definition_json) = maybe_definition_json else {
            // if the source was an empty vec, we have loaded a pointer with no content, just set default
            return Ok(EntityDefinition::default());
        };
        let content = ContentMap::from_content(
            definition_json
                .content
                .into_iter()
                .map(|ipfs| (ipfs.file, ipfs.hash)),
        );
        let id = definition_json.id.unwrap_or_else(id_fn);

        let definition = EntityDefinition {
            id,
            pointers: definition_json.pointers,
            content,
            metadata: definition_json.metadata,
        };
        Ok(definition)
    }
}

impl AssetLoader for EntityDefinitionLoader {
    type Asset = EntityDefinition;
    type Settings = ();
    type Error = anyhow::Error;

    fn load(
        &self,
        reader: &mut dyn Reader,
        settings: &Self::Settings,
        load_context: &mut bevy::asset::LoadContext,
    ) -> impl ConditionalSendFuture<Output = Result<Self::Asset, Self::Error>> {
        let id_fn = || {
            // we must have been loaded as an entity with the format "$ipfs/$entity/{hash}.entity_type" - use the ipfs path to resolve the id
            load_context
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .split_once('.')
                .unwrap()
                .0
                .to_owned()
        };

        self.load_internal(reader, settings, id_fn)
    }

    fn extensions(&self) -> &[&str] {
        &["entity_definition"]
    }
}

#[derive(Default)]
pub struct SceneJsLoader;

impl AssetLoader for SceneJsLoader {
    type Asset = SceneJsFile;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut bevy::asset::LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::default();
        reader.read_to_end(&mut bytes).await?;
        Ok(SceneJsFile(Arc::new(String::from_utf8(bytes).map_err(
            |e| std::io::Error::new(ErrorKind::InvalidData, e),
        )?)))
    }

    fn extensions(&self) -> &[&str] {
        &["js"]
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContentMap(pub HashMap<String, String>);

impl ContentMap {
    /// Keys are stored lowercase, and put through the same [`content_file_path`] the access side
    /// applies to a requested file, so the two agree. Anything the access side makes relative
    /// (`c:/x.png`, `/x.png`) would otherwise be deployable but unreferenceable.
    fn key(file: &str) -> String {
        content_file_path(file).to_lowercase()
    }

    pub fn from_content(content: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut map = HashMap::<String, String>::default();
        for (file, hash) in content {
            let key = Self::key(&file);
            if let Some(prev) = map.get(&key) {
                if *prev != hash {
                    warn!("content map key `{key}` collision: `{prev}` shadowed by `{hash}`");
                }
            }
            map.insert(key, hash);
        }
        Self(map)
    }

    pub fn hash<'a>(&'a self, file: &str) -> Option<Cow<'a, str>> {
        self.0.get(Self::key(file).as_str()).map(Into::into)
    }

    pub fn files(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    pub fn values(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    pub fn new_single(file: String, hash: String) -> Self {
        Self::from_content([(file, hash)])
    }

    pub fn with(mut self, file: String, hash: String) -> Self {
        self.0.insert(Self::key(&file), hash);
        self
    }

    /// The map as a scene packed under `prefix` sees it: every `{prefix}{file}` entry becomes
    /// `{file}` and wins over an unprefixed entry of the same name; unprefixed entries are kept;
    /// entries under another prefix (the other body shape of a smart wearable) are dropped.
    pub fn strip_prefix(&self, prefix: &str, other_prefixes: &[&str]) -> Self {
        let prefix = prefix.to_lowercase();
        let mut map = HashMap::<String, String>::default();
        for (file, hash) in self.0.iter() {
            if file.starts_with(&prefix) {
                continue;
            }
            if other_prefixes
                .iter()
                .any(|other| file.starts_with(&other.to_lowercase()))
            {
                continue;
            }
            map.insert(file.clone(), hash.clone());
        }
        for (file, hash) in self.0.iter() {
            if let Some(stripped) = file.strip_prefix(&prefix) {
                map.insert(stripped.to_owned(), hash.clone());
            }
        }
        Self(map)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SceneIpfsLocation {
    Hash(String),
    Urn(String),
    /// A content file of a registered collection whose loader produces the definition (a smart
    /// wearable's scene, `{shape}.smart_wearable` in the wearable's collection).
    ContentFile {
        hash: String,
        file: String,
    },
}

#[derive(Resource, Clone)]
pub struct IpfsResource {
    pub inner: Arc<IpfsIo>,
}

impl std::ops::Deref for IpfsResource {
    type Target = IpfsIo;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(SystemParam)]
pub struct IpfsAssetServer<'w, 's> {
    server: Res<'w, AssetServer>,
    ipfs: Res<'w, IpfsResource>,
    _p: PhantomData<&'s ()>,
}

impl IpfsAssetServer<'_, '_> {
    pub fn load_content_file<T: Asset>(
        &self,
        file_path: &str,
        content_hash: &str,
    ) -> Result<Handle<T>, anyhow::Error> {
        // note - we can't resolve paths to hashes here because some loaders use the path to locate dependent assets (e.g. gltf embedded textures)
        // TODO we could use this immediate resolution for file types that don't rely on context
        // TODO or we could add a `canonicalize` method to bevy's AssetIo trait
        // let ipfs_io = self.asset_io().downcast_ref::<IpfsIo>().unwrap();
        // let context = ipfs_io.context.blocking_read();
        // let collection = context
        //     .collections
        //     .get(content_hash)
        //     .ok_or(anyhow::anyhow!("collection not found: {content_hash}"))?;
        // let hash = collection
        //     .hash(&normalize_path(file_path))
        //     .ok_or(anyhow::anyhow!(
        //         "file_path not found in collection: {file_path}"
        //     ))?;
        // // TODO use registered loaders to extract extension
        // let file_path = Path::new(file_path);
        // let file_name = file_path.file_name().unwrap().to_str().unwrap();
        // let path = format!("$ipfs/$entity//{hash}.{file_name}");
        // Ok(self.load(path))
        let ipfs_path = IpfsPath::new(IpfsType::new_content_file(
            content_hash.to_owned(),
            file_path.to_owned(),
        ));
        Ok(self.server.load(PathBuf::from(&ipfs_path)))
    }

    pub fn load_content_file_with_settings<T: Asset, S: Settings>(
        &self,
        file_path: &str,
        content_hash: &str,
        settings: impl Fn(&mut S) + Send + Sync + 'static,
    ) -> Result<Handle<T>, anyhow::Error> {
        let ipfs_path = IpfsPath::new(IpfsType::new_content_file(
            content_hash.to_owned(),
            file_path.to_owned(),
        ));
        Ok(self
            .server
            .load_with_settings(PathBuf::from(&ipfs_path), settings))
    }

    pub fn load_urn<T: IpfsAsset>(&self, urn: &str) -> Result<Handle<T>, anyhow::Error> {
        let ext = T::ext();
        let ipfs_path = IpfsPath::new_from_urn::<T>(urn)?;
        let hash = ipfs_path
            .context_free_hash()?
            .ok_or(anyhow::anyhow!("urn did not resolve to a hash"))?;
        let path = format!("$ipfs/$entity/{hash}.{ext}");

        if let Some(base_url) = ipfs_path.base_url() {
            // update the context
            let ipfs_io = self.ipfs();
            let mut context = ipfs_io.context.blocking_write();
            context.modifiers.insert(
                hash,
                IpfsModifier {
                    base_url: Some(base_url.to_owned()),
                },
            );
        }
        Ok(self.server.load(path))
    }

    pub fn load_url_uncached<T: IpfsAsset>(&self, url: &str) -> Handle<T> {
        let ext = T::ext();
        let ipfs_path = IpfsPath::new_from_url_uncached(url, ext);
        self.server.load(PathBuf::from(&ipfs_path))
    }

    pub fn load_hash<T: IpfsAsset>(&self, hash: &str) -> Handle<T> {
        let ext = T::ext();
        let path = format!("$ipfs/$entity/{hash}.{ext}");
        self.server.load(path)
    }

    // Load a raw content hash that lives inside a scene's collection. Routes
    // the request through the scene's registered modifier (e.g. a portable's
    // local content server) so b64-prefixed content hashes resolve to the
    // scene's origin rather than the realm content URL.
    pub fn load_scene_content_hash<T: IpfsAsset>(
        &self,
        scene_hash: &str,
        content_hash: &str,
    ) -> Handle<T> {
        self.server
            .load(Self::scene_content_hash_path::<T>(scene_hash, content_hash))
    }

    /// The asset path [`Self::load_scene_content_hash`] loads.
    pub fn scene_content_hash_path<T: IpfsAsset>(
        scene_hash: &str,
        content_hash: &str,
    ) -> AssetPath<'static> {
        let ext = T::ext();
        AssetPath::from(format!(
            "$ipfs/$scene_content/{scene_hash}/{content_hash}.{ext}"
        ))
    }

    pub fn active_endpoint(&self) -> Option<String> {
        self.ipfs()
            .realm_config_receiver
            .borrow()
            .as_ref()
            .and_then(|c| c.about.content.as_ref())
            .map(|content| format!("{}/entities/active", content.public_url))
    }

    pub fn ipfs(&self) -> &Arc<IpfsIo> {
        &self.ipfs.inner
    }

    pub fn asset_server(&self) -> &AssetServer {
        &self.server
    }

    pub fn ipfs_cache_path(&self) -> Option<&Path> {
        self.ipfs().cache_path()
    }

    pub fn load_state(&self, id: impl Into<UntypedAssetId>) -> LoadState {
        self.server.load_state(id)
    }

    pub fn is_connected(&self) -> bool {
        self.ipfs.realm_config_receiver.borrow().is_some()
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EndpointConfig {
    pub healthy: bool,
    pub public_url: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ServerAbout {
    pub content: Option<EndpointConfig>,
    pub comms: Option<CommsConfig>,
    pub configurations: Option<ServerConfiguration>,
    pub lambdas: Option<EndpointConfig>,
}

impl ServerAbout {
    pub fn content_url(&self) -> Option<&str> {
        self.content.as_ref().map(|c| c.public_url.as_str())
    }

    /// Catalysts serve `content` and `lambdas` `publicUrl`s with a trailing slash; every
    /// consumer joins with `format!("{url}/…")`, and the rate limiter in front of the
    /// catalysts treats the resulting `/content//contents/` as its own path.
    pub fn trim_public_urls(&mut self) {
        for endpoint in [&mut self.content, &mut self.lambdas].into_iter().flatten() {
            let trimmed = endpoint.public_url.trim_end_matches('/').len();
            endpoint.public_url.truncate(trimmed);
        }
    }
}

impl Default for ServerAbout {
    fn default() -> Self {
        Self {
            content: None,
            comms: Some(CommsConfig {
                healthy: true,
                protocol: "v3".to_owned(),
                fixed_adapter: Some("offline:offline".to_owned()),
                adapter: None,
            }),
            configurations: Default::default(),
            lambdas: Default::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ServerScenes {
    scenes: Vec<ServerScene>,
    total: usize,
}

// This replicates the structure of a `scene.json`, maybe extend if needed, but
// for multi-scene worlds, only `entity_id` is needed
#[expect(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ServerScene {
    #[serde(rename = "worldName")]
    world_name: String,
    deployer: String,
    #[serde(rename = "entityId")]
    entity_id: String,
}

/// `GET {world}/manifest`: the world's occupied parcels and the parcel it spawns players on, the
/// document unity-explorer lands on (the asset-bundle-registry serves the same one). Only the
/// spawn is read; the worlds content server sends its coordinates as strings, the registry as
/// numbers.
#[derive(Debug, Deserialize)]
struct WorldManifest {
    spawn_coordinate: Option<ManifestCoordinate>,
}

#[derive(Debug, Deserialize)]
struct ManifestCoordinate {
    x: ManifestNumber,
    y: ManifestNumber,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestNumber {
    Number(i32),
    Text(String),
}

impl ManifestNumber {
    fn value(&self) -> Option<i32> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(text) => text.trim().parse().ok(),
        }
    }
}

pub struct IpfsIoPlugin {
    pub preview: bool,
    pub assets_root: Option<String>,
    pub starting_realm: Option<String>,
    pub content_server_override: Option<String>,
    pub num_slots: usize,
}

impl Plugin for IpfsIoPlugin {
    fn build(&self, app: &mut App) {
        info!("remote server: {:?}", self.starting_realm);

        let file_path = self.assets_root.clone().unwrap_or("assets".to_owned());
        #[cfg(not(target_arch = "wasm32"))]
        let default_reader = FileAssetReader::new(file_path.clone());
        #[cfg(target_arch = "wasm32")]
        let default_reader = HttpWasmAssetReader::new(file_path.clone());

        #[cfg(not(target_arch = "wasm32"))]
        let cache_root = if self.assets_root.is_some() {
            // use app folder for unit tests
            Some(default_reader.root_path().join("cache"))
        } else {
            Some(
                project_directories()
                    .unwrap()
                    .data_local_dir()
                    .join("cache"),
            )
        };
        #[cfg(target_arch = "wasm32")]
        let cache_root = None;
        info!("cache folder {cache_root:?}");
        if let Some(root) = &cache_root {
            std::fs::create_dir_all(root)
                .unwrap_or_else(|_| panic!("failed to write to assets folder {root:?}"));
        }

        #[cfg(feature = "ipfs_debug")]
        let (debug_overlay_sender, debug_overlay_receiver) = tokio::sync::mpsc::unbounded_channel();
        let ipfs_io = IpfsIo::new(
            self.preview,
            Box::new(default_reader),
            cache_root,
            HashMap::default(),
            self.num_slots,
            #[cfg(feature = "ipfs_debug")]
            debug_overlay_sender,
        );
        let ipfs_io = Arc::new(ipfs_io);
        let passthrough = PassThroughReader {
            inner: ipfs_io.clone(),
        };

        for (path, _) in ipfs_io.request_metrics.counters() {
            app.register_diagnostic(Diagnostic::new(path));
        }
        app.insert_resource(IpfsResource { inner: ipfs_io });
        app.insert_resource(RealmInitialLocation::Base);

        #[cfg(feature = "hot_reload")]
        app.register_asset_source(
            AssetSourceId::Default,
            AssetSource::build()
                .with_reader(move || Box::new(passthrough.clone()))
                .with_watcher(AssetSource::get_default_watcher(
                    file_path,
                    Duration::from_millis(300),
                )),
        );
        #[cfg(not(feature = "hot_reload"))]
        app.register_asset_source(
            AssetSourceId::Default,
            AssetSource::build().with_reader(move || Box::new(passthrough.clone())),
        );

        app.add_event::<ChangeRealmEvent>();
        app.init_resource::<WorldCommsSecret>();
        app.init_resource::<CurrentRealm>();
        app.add_systems(
            PostUpdate,
            (change_realm, clean_cache).before(RealmLifecycle),
        );

        app.add_console_command::<ChangeRealmCommand, _>(change_realm_command);
        app.add_plugins(debug_panel::WebRequestsDebugPanelPlugin);
        app.add_plugins(bevy_variants::BevyVariantsPlugin);

        app.register_diagnostic(Diagnostic::new(IPFS_IN_FLIGHT_DIAGNOSTIC_PATH));
        app.register_diagnostic(Diagnostic::new(IPFS_SUCCESS_DIAGNOSTIC_PATH));
        app.register_diagnostic(Diagnostic::new(IPFS_FAILED_DIAGNOSTIC_PATH));
        app.register_diagnostic(Diagnostic::new(IPFS_CACHED_DIAGNOSTIC_PATH));
        app.register_diagnostic(Diagnostic::new(IPFS_NON_IPFS_DIAGNOSTIC_PATH));
        app.add_systems(PostUpdate, ipfs_diagnostics);
        app.add_systems(Update, apply_max_downloads);

        #[cfg(feature = "ipfs_debug")]
        app.add_plugins(ipfs_debug::IpfsDebugPlugin)
            .insert_resource(IpfsDebugReceiver(debug_overlay_receiver));
    }

    fn finish(&self, app: &mut App) {
        app.init_asset::<EntityDefinition>()
            .init_asset::<SceneJsFile>()
            .init_asset_loader::<EntityDefinitionLoader>()
            .init_asset_loader::<SceneJsLoader>();

        if let Some(realm) = &self.starting_realm {
            let ipfs = app.world().resource::<IpfsResource>().clone();
            let realm = realm.clone();
            let content_server_override = self.content_server_override.clone();
            IoTaskPool::get()
                .spawn_compat(async move {
                    // a failure is published by set_realm itself (no realm to stay in at boot)
                    let _ = ipfs.set_realm(realm, content_server_override, None).await;
                })
                .detach();
        }
    }
}

#[derive(Resource, PartialEq, Debug)]
pub enum RealmInitialLocation {
    None,
    Base,
    /// Land on this parcel of the new realm (a teleport that named its realm).
    Parcel(IVec2),
}

/// Switch to a new realm
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/changerealm")]
struct ChangeRealmCommand {
    new_realm: String,
    content_server_override: Option<String>,
}

fn change_realm_command(
    mut input: ConsoleCommand<ChangeRealmCommand>,
    mut writer: EventWriter<ChangeRealmEvent>,
    mut target: ResMut<RealmInitialLocation>,
    mut pending: ResMut<PendingConsoleResponses>,
) {
    if let Some(Ok(command)) = input.take() {
        *target = RealmInitialLocation::Base;
        debug!("change realm command -> base");
        let (response, rx) = RpcResultSender::channel();
        let realm = command.new_realm.clone();
        writer.write(ChangeRealmEvent {
            new_realm: command.new_realm,
            content_server_override: command.content_server_override,
            response,
            report: false,
        });
        pending.push_receiver(
            rx,
            move |result| result.map(|()| format!("Realm set to `{realm}`")),
            input.take_responder(),
        );
    }
}

#[derive(Event, Clone)]
pub struct ChangeRealmEvent {
    pub new_realm: String,
    pub content_server_override: Option<String>,
    /// Answered once the new realm is set, or with the reason it could not be (the current realm
    /// is kept in that case).
    pub response: RpcResultSender<Result<(), String>>,
    /// Print the outcome to the console (and so the system chat): set when nothing else will tell
    /// the player, e.g. a scene that is torn down by the change it asked for.
    pub report: bool,
}

/// A realm change being attempted, see [`change_realm`].
pub struct InFlightRealmChange {
    id: u64,
    realm: String,
    report: bool,
    result: RpcResultReceiver<Result<(), String>>,
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub fn change_realm(
    mut change_realm_requests: EventReader<ChangeRealmEvent>,
    ipfs: Res<IpfsResource>,
    mut realm_change: Local<Option<tokio::sync::watch::Receiver<Option<RealmConfig>>>>,
    mut current_realm: ResMut<CurrentRealm>,
    mut print: EventWriter<PrintConsoleLine>,
    preview_mode: Res<PreviewMode>,
    mut target: ResMut<RealmInitialLocation>,
    mut in_flight: Local<Vec<InFlightRealmChange>>,
    mut issued: Local<u64>,
    // the latest issued change's id, watched by the earlier ones so they stand down
    mut latest_issued: Local<Option<tokio::sync::watch::Sender<u64>>>,
    world_hold: Option<Res<WorldHold>>,
    wallet: Option<Res<Wallet>>,
    prompter: Option<Res<WorldPasswordPrompter>>,
    mut world_secret: ResMut<WorldCommsSecret>,
    mut commands: Commands,
) {
    // Polled before the realm watch: a change's result is sent after its realm config, so a
    // success seen here is applied below in this same run.
    let mut released = false;
    let latest = *issued;
    in_flight.retain_mut(|change| {
        let result = match change.result.poll_once() {
            Ok(None) => return true,
            Ok(Some(result)) => result,
            Err(()) => return false,
        };
        // A failed change leaves the player in the current realm, so the landing target it set up
        // for the new one must not carry over to a later change. Only the latest request owns it.
        if result.is_err() && change.id == latest {
            *target = RealmInitialLocation::None;
        }
        if result.is_ok() {
            released = true;
        }
        if change.report {
            print.write(PrintConsoleLine::new(match result {
                Ok(_) => format!("Realm set to `{}`", change.realm),
                Err(e) => format!("Failed to set realm `{}`: {e}", change.realm),
            }));
        }
        false
    });

    match *realm_change {
        None => *realm_change = Some(ipfs.realm_config_receiver.clone()),
        Some(ref mut realm_change) => {
            if realm_change.has_changed().unwrap_or_default() {
                if let Some(RealmConfig {
                    about_url,
                    address: realm,
                    about,
                    connected,
                    secret,
                    spawn_parcel,
                }) = &*realm_change.borrow_and_update()
                {
                    let mut config = about.configurations.clone().unwrap_or_default();
                    // An orchestrated engine is on no realm of its own: it hosts scenes from
                    // several at once, and its `--realm` is only where content is served from.
                    // Leaving the name set would hand every one of those scenes a realm none of
                    // them is in — the realm a scene belongs to arrives with it, on `add-scene`.
                    if common::structs::multi_tenant() {
                        config.realm_name = None;
                    }

                    *current_realm = CurrentRealm {
                        about_url: about_url.clone(),
                        address: realm.clone(),
                        connected: *connected,
                        config,
                        comms: about.comms.clone(),
                        public_url: about
                            .content
                            .as_ref()
                            .map(|c| c.public_url.clone())
                            .unwrap_or_default(),
                        spawn_parcel: *spawn_parcel,
                    };
                    // with the realm, before the comms login it starts
                    world_secret.0 = secret.clone();
                }
            }
        }
    }

    // the boot realm never lands in `in_flight`, so a lobby holds until it asks to enter a realm
    if released && world_hold.is_some() {
        commands.remove_resource::<WorldHold>();
    }

    if !change_realm_requests.is_empty() {
        if preview_mode.is_preview {
            const DISABLED: &str = "Changing realm is disabled in preview mode.";
            print.write(PrintConsoleLine {
                line: DISABLED.to_owned(),
            });
            for request in change_realm_requests.read() {
                request.response.send(Err(DISABLED.to_owned()));
            }
        } else {
            let mut requests = change_realm_requests.read().collect::<Vec<_>>();
            let request = requests.pop().unwrap();
            for superseded in requests {
                superseded
                    .response
                    .send(Err(world_access::SUPERSEDED.to_owned()));
            }

            let ipfs = ipfs.clone();
            let new_realm = map_realm_name(&request.new_realm);
            let content_server_override = request.content_server_override.to_owned();
            let response = request.response.clone();
            let (sx, rx) = RpcResultSender::channel();
            *issued += 1;
            let latest_issued =
                latest_issued.get_or_insert_with(|| tokio::sync::watch::channel(0).0);
            latest_issued.send_replace(*issued);
            let superseded = world_access::Superseded {
                latest: latest_issued.subscribe(),
                id: *issued,
            };
            in_flight.push(InFlightRealmChange {
                id: *issued,
                realm: request.new_realm.clone(),
                report: request.report,
                result: rx,
            });
            // A world is entered only once it says the player may (see world_access). Not on a
            // server (it hosts, it never enters) and not without an identity (nothing to check
            // against, and no comms to be kept out of).
            let world = world_access::world_name(&new_realm)
                .filter(|_| !server_mode())
                .zip(wallet.as_deref().filter(|w| w.address().is_some()).cloned());
            // re-entering the current world: its password is known
            let known_secret = world
                .as_ref()
                .filter(|(world, _)| {
                    world_access::world_name(&current_realm.about_url).as_ref() == Some(world)
                })
                .and_then(|_| world_secret.0.clone());
            let prompter = prompter.as_deref().cloned();
            let client = ipfs.client();
            IoTaskPool::get()
                .spawn_compat(async move {
                    let mut superseded = superseded;
                    let result = async {
                        let secret = match &world {
                            Some((world, wallet)) => {
                                world_access::check_world_access(
                                    &new_realm,
                                    world,
                                    wallet,
                                    known_secret,
                                    &client,
                                    prompter.as_ref(),
                                    &mut superseded,
                                )
                                .await?
                            }
                            None => None,
                        };
                        if superseded.is_superseded() {
                            return Err(world_access::SUPERSEDED.to_owned());
                        }
                        ipfs.set_realm(new_realm, content_server_override, secret)
                            .await
                            .map_err(|e| e.to_string())
                    }
                    .await;
                    response.send(result.clone());
                    sx.send(result);
                })
                .detach();
        }
    }
}

pub fn map_realm_name(request: &str) -> String {
    if request.ends_with(".dcl.eth") && !request.starts_with("https://") {
        common::base_domain::url(
            common::base_domain::Service::WorldsServer,
            &format!("/world/{request}"),
        )
    } else {
        request.to_owned()
    }
}

pub struct IpfsModifier {
    pub base_url: Option<String>,
}

#[derive(Clone)]
pub struct IpfsEntity {
    pub collection: ContentMap,
    pub metadata: Option<String>,
}

#[derive(Default)]
pub struct IpfsContext {
    about_url: String,
    base_url: String,
    entities: HashMap<String, IpfsEntity>,
    about: Option<ServerAbout>,
    modifiers: HashMap<String, IpfsModifier>,
    failed_remotes: HashMap<String, Instant>,
    num_slots: usize,
}

impl IpfsContext {
    /// The prefix an entity's content hashes are fetched under (`{prefix}{hash}`): the
    /// `baseUrl` its urn carried, else the realm's contents endpoint.
    fn contents_base_url(&self, hash: &str) -> Option<String> {
        self.modifiers
            .get(hash)
            .and_then(|m| m.base_url.clone())
            .or_else(|| {
                self.about
                    .as_ref()
                    .and_then(ServerAbout::content_url)
                    .map(|url| format!("{url}/contents/"))
            })
    }
}

fn clean_cache(mut exit: EventReader<AppExit>, config: Res<AppConfig>, ipfas: IpfsAssetServer) {
    if exit.read().last().is_some() {
        ipfas.ipfs().trim_cache(config.cache_bytes);
    }
}

/// Realm state broadcast over the realm-config watch channel.
pub struct RealmConfig {
    pub about_url: String,
    pub address: String,
    pub about: ServerAbout,
    /// whether we actually connected; false when realm resolution failed
    pub connected: bool,
    /// The password the realm (a world) was entered with, for its signed comms login
    pub secret: Option<String>,
    /// The parcel the realm (a world) spawns players on, from its manifest
    pub spawn_parcel: Option<IVec2>,
}

fn server_retry_delay(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Option<Duration> {
    if !matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        return None;
    }
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let now = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .ok()?;
    platform::http::parse_retry_after(value, std::time::UNIX_EPOCH + now)
}

fn readiness_retry_delay(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    remaining: Duration,
    retries: &mut u8,
) -> Option<Duration> {
    let delay = if let Some(delay) = server_retry_delay(status, headers) {
        if *retries >= 3 || delay > platform::http::MAX_RETRY_AFTER || delay >= remaining {
            return None;
        }
        *retries += 1;
        delay
    } else {
        Duration::from_millis(150)
    };
    (delay < remaining).then_some(delay)
}

pub struct IpfsIo {
    /// Effective preview-only delay before a real outbound request, without holding a slot.
    preview_delay_ms: AtomicU32,
    is_preview: bool, // determines whether we always retry failed assets immediately
    default_io: Box<dyn ErasedAssetReader>,
    default_fs_path: Option<PathBuf>,
    next_cache_part: AtomicU64,
    realm_config_receiver: tokio::sync::watch::Receiver<Option<RealmConfig>>,
    realm_config_sender: tokio::sync::watch::Sender<Option<RealmConfig>>,
    pub context: AsyncRwLock<IpfsContext>,
    request_slots: Arc<PriorityGate>,
    request_metrics: RequestMetrics,
    reqno: AtomicU16,
    static_files: HashMap<&'static str, &'static str>,
    // Directories `file://` urls may read from. file realms are only minted by
    // lookup_local_realm from a locally-supplied path (which registers its root here) — but a
    // `file://` baseUrl can also be smuggled in a urn from a remote realm/portable, and those
    // must not read local disk.
    #[cfg(not(target_arch = "wasm32"))]
    allowed_file_roots: std::sync::RwLock<Vec<PathBuf>>,
    client: reqwest::Client,
    // downloads under way, by url (`share_download`)
    in_flight: InFlightDownloads,
    pub(crate) bevy_variants: bevy_variants::BevyVariants,
    #[cfg(feature = "ipfs_debug")]
    debug_overlay_sender: tokio::sync::mpsc::UnboundedSender<IpfsDebug>,
}

impl IpfsIo {
    pub fn new(
        is_preview: bool,
        default_io: Box<dyn ErasedAssetReader>,
        default_fs_path: Option<PathBuf>,
        static_paths: HashMap<&'static str, &'static str>,
        num_slots: usize,
        #[cfg(feature = "ipfs_debug")] debug_overlay_sender: tokio::sync::mpsc::UnboundedSender<
            IpfsDebug,
        >,
    ) -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(None);

        Self {
            is_preview,
            default_io,
            default_fs_path,
            next_cache_part: AtomicU64::new(0),
            realm_config_receiver: receiver,
            realm_config_sender: sender,
            context: AsyncRwLock::new(IpfsContext {
                num_slots,
                ..Default::default()
            }),
            request_slots: PriorityGate::new(num_slots),
            request_metrics: RequestMetrics::default(),
            preview_delay_ms: AtomicU32::new(0),
            reqno: default(),
            static_files: static_paths,
            #[cfg(not(target_arch = "wasm32"))]
            allowed_file_roots: Default::default(),
            client: platform::http_client_builder()
                .connect_timeout(Duration::from_secs(5))
                .use_native_tls()
                .user_agent("DCLExplorer/0.1")
                .build()
                .unwrap(),
            in_flight: Default::default(),
            bevy_variants: Default::default(),
            #[cfg(feature = "ipfs_debug")]
            debug_overlay_sender,
        }
    }

    fn preview_request_delay(&self) -> Option<Duration> {
        if !self.is_preview {
            return None;
        }
        let ms = self.preview_delay_ms.load(Ordering::Relaxed);
        (ms > 0).then(|| Duration::from_millis(ms.into()))
    }

    /// Allow `file://` reads under `root` (canonical). See `allowed_file_roots`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn add_allowed_file_root(&self, root: PathBuf) {
        let mut roots = self.allowed_file_roots.write().unwrap();
        if !roots.contains(&root) {
            roots.push(root);
        }
    }

    pub fn trim_cache(&self, max_size: u64) {
        let Some(cache_path) = self.cache_path() else {
            return;
        };

        let Ok(folder) = std::fs::read_dir(cache_path) else {
            return;
        };

        let mut files = folder
            .filter_map(|f| {
                let Ok(f) = f else { return None };

                let Ok(metadata) = f.metadata() else {
                    return None;
                };

                if metadata.is_file() {
                    let accessed = metadata
                        .accessed()
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    Some((accessed, (metadata.len(), f.path())))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        files.sort_by_key(|(time, _)| *time);

        let mut total_size = 0;
        for (_, (_, to_delete)) in files.iter().rev().skip_while(|(_, (size, path))| {
            total_size += *size;
            debug!("keeping {path:?}, total now {total_size}/{max_size}");
            total_size < max_size
        }) {
            if let Err(e) = std::fs::remove_file(to_delete) {
                warn!("failed to remove cache file {to_delete:?}: {e}");
            }
        }
    }

    /// An entity that owns downloads under `hash` (a scene root, an avatar wearing a wearable)
    /// has appeared: its requests rank by `key`, the best of its holders' for a shared hash,
    /// until `release_download_priority`. A request whose hash nobody registered ranks with the
    /// requests that have no hash, after every registered one; a hash released while requests
    /// of its wait ranks last.
    pub fn register_download_priority(&self, hash: &str, key: LoadPriority) {
        self.request_slots.register(hash, key);
    }

    /// The owner of downloads under `hash` has gone.
    pub fn release_download_priority(&self, hash: &str) {
        self.request_slots.unregister(hash);
    }

    /// This frame's keys for the registered hashes, written in place: the first write of a hash
    /// replaces its key, later writes in the same call take the best.
    pub fn update_download_priorities<'a>(
        &self,
        keys: impl IntoIterator<Item = (&'a str, LoadPriority)>,
    ) {
        self.request_slots.publish(keys);
    }

    pub fn scene_priority(&self, scene: &str) -> Option<LoadPriority> {
        self.request_slots.scene_priority(scene)
    }

    /// Whether a grant before the next frame may read the download priorities: a download
    /// waits for a request slot, or holds one whose release can go to a newcomer.
    pub fn reads_download_priorities(&self) -> bool {
        self.request_slots.reads_keys()
    }

    /// The request slots themselves, for callers that queue their own tickets (tests).
    pub fn request_slots(&self) -> &Arc<PriorityGate> {
        &self.request_slots
    }

    pub fn set_concurrent_remote_count(&self, count: usize) {
        let mut context = self.context.blocking_write();
        if count == context.num_slots {
            return;
        }

        if count > context.num_slots {
            self.request_slots.add_permits(count - context.num_slots);
            context.num_slots = count;
        } else {
            let freed = self.request_slots.forget_permits(context.num_slots - count);
            if freed != context.num_slots - count {
                // we couldn't free enough permits, never mind
                warn!(
                    "could only free {freed} permits, will catch up on settings apply or restart"
                );
            }
            context.num_slots -= freed;
        }
    }

    /// Switch to `new_realm`. The current realm is only left once the new one has answered, so a
    /// failure keeps the player where they are; with no current realm (boot) a failure is published
    /// as a disconnected realm, as before.
    pub async fn set_realm(
        &self,
        new_realm: String,
        content_server_override: Option<String>,
        secret: Option<String>,
    ) -> Result<(), anyhow::Error> {
        let res = self
            .set_realm_inner(new_realm.clone(), content_server_override, secret)
            .await;
        if let Err(e) = &res {
            error!("failed to set realm: {e}");
            if self.context.read().await.about.is_none() {
                self.realm_config_sender
                    .send(Some(RealmConfig {
                        about_url: new_realm.clone(),
                        address: new_realm,
                        about: Default::default(),
                        connected: false,
                        secret: None,
                        spawn_parcel: None,
                    }))
                    .expect("channel closed");
            }
        }
        res
    }

    pub fn set_realm_about(&self, about: ServerAbout) {
        let mut write = self.context.blocking_write();
        write.base_url = String::default();
        write.about = Some(about.clone());
        self.realm_config_sender
            .send(Some(RealmConfig {
                about_url: "manual value".to_owned(),
                address: "manual value".to_owned(),
                about,
                connected: true,
                secret: None,
                spawn_parcel: None,
            }))
            .expect("channel closed");
    }

    pub async fn get_realm_info(&self) -> (String, Option<ServerAbout>) {
        let context = self.context.read().await;
        (
            context
                .about_url
                .strip_suffix("/about")
                .unwrap_or(&context.about_url)
                .to_owned(),
            context.about.clone(),
        )
    }

    async fn set_realm_inner(
        &self,
        new_realm: String,
        content_server_override: Option<String>,
        secret: Option<String>,
    ) -> Result<(), anyhow::Error> {
        let mut retries = 0;
        let mut about;
        let mut final_url;
        loop {
            let about_raw =
                platform::TracedJsonResponse::send(self.client.get(format!("{new_realm}/about")))
                    .await
                    .map_err(|e| anyhow!(e))?;
            if about_raw.status() != StatusCode::OK {
                return Err(anyhow!("status: {}", about_raw.discard()));
            }
            final_url = about_raw.url().to_string();

            about = about_raw.json::<ServerAbout>().await?;
            if about.configurations.as_ref().is_some_and(|config| {
                config
                    .scenes_urn
                    .as_ref()
                    .is_some_and(|scenes| !scenes.is_empty())
                    || config.map.is_some()
            }) {
                break;
            }
            // with no scenes and no map data, we will not have much of value
            // sometimes this occurs for misdeployed load balancers, so let's retry a couple of times
            retries += 1;
            if retries == 3 {
                break;
            }
        }

        // The destination answered: only now leave the current realm.
        self.realm_config_sender.send(None).expect("channel closed");
        let mut write = self.context.write().await;
        if write.about.is_some() {
            info!("disconnecting");
        }

        write.about = None;
        drop(write);

        if let Err(e) = self.update_scene_urns(&mut about, &new_realm).await {
            error!("failed to update scene urns: {e}");
        }
        let spawn_parcel = match self.world_spawn_parcel(&about, &new_realm).await {
            Ok(spawn_parcel) => spawn_parcel,
            Err(e) => {
                warn!("failed to fetch the world manifest: {e}");
                None
            }
        };

        let mut write = self.context.write().await;
        if let (Some(cs), Some(content)) = (&content_server_override, about.content.as_mut()) {
            content.public_url = format!("{cs}/content/");
        }
        about.trim_public_urls();
        let base_url = about
            .content_url()
            .map(|c| c.strip_suffix("/content").unwrap_or(c));
        write.base_url = base_url.unwrap_or(&final_url).to_owned();
        write.about_url = final_url.clone();
        write.about = Some(about.clone());
        self.realm_config_sender
            .send(Some(RealmConfig {
                about_url: final_url,
                address: write.base_url.clone(),
                about,
                connected: true,
                secret,
                spawn_parcel,
            }))
            .expect("channel closed");
        Ok(())
    }

    async fn connected(&self) -> Result<(), anyhow::Error> {
        if self.realm_config_receiver.borrow().is_some() {
            return Ok(());
        }

        let mut watcher = self.realm_config_receiver.clone();

        loop {
            if self.realm_config_receiver.borrow().is_some() {
                return Ok(());
            }

            watcher.changed().await?;
        }
    }

    pub fn add_collection(
        &self,
        hash: String,
        collection: ContentMap,
        modifier: Option<IpfsModifier>,
        metadata: Option<String>,
    ) {
        Self::insert_collection(
            &mut self.context.blocking_write(),
            hash,
            collection,
            modifier,
            metadata,
        );
    }

    /// [`Self::add_collection`] for async code (an asset loader), which must not block.
    pub async fn add_collection_async(
        &self,
        hash: String,
        collection: ContentMap,
        modifier: Option<IpfsModifier>,
        metadata: Option<String>,
    ) {
        Self::insert_collection(
            &mut *self.context.write().await,
            hash,
            collection,
            modifier,
            metadata,
        );
    }

    fn insert_collection(
        context: &mut IpfsContext,
        hash: String,
        collection: ContentMap,
        modifier: Option<IpfsModifier>,
        metadata: Option<String>,
    ) {
        let entity = IpfsEntity {
            collection,
            metadata,
        };

        if let Some(modifier) = modifier {
            context.modifiers.insert(hash.clone(), modifier);
        }

        context.entities.insert(hash, entity);
    }

    /// Merge additional path→hash entries into an existing collection (creating it if absent),
    /// WITHOUT clobbering the collection's other entries — unlike `add_collection`, which replaces.
    /// Injects imported-asset files into the *current scene's* content map at runtime.
    pub async fn merge_collection(&self, hash: &str, extra: ContentMap) {
        let mut write = self.context.write().await;
        match write.entities.get_mut(hash) {
            Some(entity) => entity.collection.0.extend(extra.0),
            None => {
                write.entities.insert(
                    hash.to_owned(),
                    IpfsEntity {
                        collection: extra,
                        metadata: None,
                    },
                );
            }
        }
    }

    /// True if bytes for `hash` are already in the on-disk content cache.
    pub fn is_cached(&self, hash: &str) -> bool {
        self.cache_path()
            .and_then(|p| p.join_relative(hash))
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    pub fn cache_path(&self) -> Option<&Path> {
        self.default_fs_path.as_deref()
    }

    /// The cached bytes under `key` (native only: on web the worker's cache is read through
    /// `web_cache`).
    async fn read_disk_cache(&self, key: &CacheKey) -> Result<Option<Vec<u8>>, std::io::Error> {
        let Some(cached) = self
            .cache_path()
            .and_then(|root| root.join_relative(key.name()))
        else {
            return Ok(None);
        };
        let Ok(mut reader) = self.default_io.read(&cached).await else {
            return Ok(None);
        };
        let mut buffer = Vec::default();
        reader.read_to_end(&mut buffer).await?;
        Ok(Some(buffer))
    }

    /// Drops the cached bytes under `key`, if there are any (native only: on web the worker's
    /// cache entry goes through `web_cache`).
    #[cfg(not(target_arch = "wasm32"))]
    async fn forget_disk_cache(&self, key: &CacheKey) -> Result<(), std::io::Error> {
        let Some(cached) = self
            .cache_path()
            .and_then(|root| root.join_relative(key.name()))
        else {
            return Ok(());
        };
        match async_fs::remove_file(&cached).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    // load entities from pointers and cache urls
    pub fn active_entities(
        self: &Arc<Self>,
        request: ActiveEntitiesRequest,
        endpoint: Option<&str>,
    ) -> ActiveEntityTask {
        let active_url = match endpoint {
            Some(url) => Some(url.to_owned()),
            None => self
                .realm_config_receiver
                .borrow()
                .as_ref()
                .and_then(|c| c.about.content.as_ref())
                .map(|content| content.public_url.to_owned()),
        }
        .map(|url| format!("{url}/entities/active"));

        match request {
            ActiveEntitiesRequest::Pointers(pointers) => {
                let client = self.client.clone();
                let delay = self.preview_request_delay();
                IoTaskPool::get().spawn_compat(async move {
                    let active_url = active_url.ok_or(anyhow!("not connected"))?;
                    if let Some(delay) = delay {
                        async_std::task::sleep(delay).await;
                    }
                    let body = serde_json::to_string(&ActiveEntitiesPointersRequest { pointers })?;
                    // Headers-phase timeout + body inactivity timeout (the total is
                    // left unbounded, so a large-but-progressing response — e.g. bulk
                    // scene resolution — isn't killed mid-transfer).
                    let fetched = platform::fetch(
                        client
                            .post(active_url)
                            .header("content-type", "application/json")
                            .body(body),
                        Duration::from_secs(30),
                        Duration::from_secs(10),
                    )
                    .await
                    .map_err(|e| match e {
                        platform::FetchError::Headers => {
                            anyhow!("timed out awaiting active-entities headers")
                        }
                        platform::FetchError::Send(e) => anyhow!(e),
                        platform::FetchError::Status(s, _) => anyhow!("status: {s}"),
                        platform::FetchError::Stalled => {
                            anyhow!("active-entities response stalled")
                        }
                        platform::FetchError::Body(e) => anyhow!(e),
                    })?;
                    let active_entities: ActiveEntitiesResponse =
                        serde_json::from_slice(&fetched.body).map_err(|e| anyhow!(e))?;
                    let mut res = Vec::default();
                    for entity in active_entities.0 {
                        // return active entity struct
                        res.push(EntityDefinition {
                            id: entity.id.unwrap(),
                            pointers: entity.pointers,
                            metadata: entity.metadata,
                            content: ContentMap::from_content(
                                entity
                                    .content
                                    .into_iter()
                                    .map(|ipfs| (ipfs.file, ipfs.hash)),
                            ),
                        });
                    }

                    Ok(res)
                })
            }
            ActiveEntitiesRequest::Urns(paths) => {
                // we simulate the active entities functionality we want here (load all scenes from a set of urns)
                let ipfs = self.clone();
                IoTaskPool::get().spawn_compat(async move {
                    let mut results = Vec::default();
                    let loader = EntityDefinitionLoader;

                    for path in paths.iter() {
                        let hash = path.context_free_hash().unwrap().unwrap();
                        let pathbuf = std::path::PathBuf::from(path);
                        let Ok(mut reader) = ipfs.read(&pathbuf).await else {
                            continue;
                        };

                        if let Ok(ent) = loader.load_internal(&mut reader, &(), || hash).await {
                            results.push(ent)
                        }
                    }

                    Ok(results)
                })
            }
        }
    }

    pub fn client(&self) -> reqwest::Client {
        self.client.clone()
    }

    pub async fn ipfs_hash(&self, ipfs_path: &IpfsPath) -> Option<String> {
        ipfs_path.hash(&*self.context.read().await)
    }

    pub async fn entity_definition(&self, hash: &str) -> Option<(IpfsEntity, String)> {
        let context = self.context.read().await;
        Some((
            context.entities.get(hash)?.clone(),
            context.contents_base_url(hash).unwrap_or_default(),
        ))
    }

    pub fn base_urls(&self) -> HashMap<String, String> {
        self.context
            .blocking_read()
            .modifiers
            .iter()
            .filter_map(|(k, v)| {
                v.base_url
                    .as_ref()
                    .map(|burl| (k.to_owned(), burl.to_owned()))
            })
            .collect()
    }

    // note - blocking. use from a blockable thread
    pub fn content_url(&self, file_path: &str, content_hash: &str) -> Option<String> {
        let ipfs_path = IpfsPath::new(IpfsType::new_content_file(
            content_hash.to_owned(),
            file_path.to_owned(),
        ));
        let res = ipfs_path.to_url(&self.context.blocking_read()).ok();
        res
    }

    /// Poll the scene's content server until each of `file_paths` (resolved via the scene
    /// collection) is fetchable, sharing one `timeout` budget; returns whether all became available.
    /// Used after writing imported files to wait for the dev server to index them before the
    /// renderer first tries to load them — otherwise that first load 404s and is never retried. On
    /// web the successful GETs also warm the service-worker cache for the exact URLs the loader uses.
    pub async fn await_contents_available(
        &self,
        file_paths: &[String],
        content_hash: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let mut all = true;
        for file_path in file_paths {
            let Some(url) = self.content_url(file_path, content_hash) else {
                all = false;
                continue;
            };
            let mut retries = 0;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    all = false;
                    break;
                }
                let Ok(response) = async_std::future::timeout(
                    remaining,
                    self.client
                        .get(&url)
                        .timeout(Duration::from_secs(10))
                        .send(),
                )
                .await
                else {
                    all = false;
                    break;
                };
                let delay = match response {
                    Ok(resp) if resp.status().is_success() => break,
                    Ok(resp) => readiness_retry_delay(
                        resp.status(),
                        resp.headers(),
                        deadline.saturating_duration_since(Instant::now()),
                        &mut retries,
                    ),
                    Err(_) => Some(Duration::from_millis(150)),
                };
                let Some(delay) =
                    delay.filter(|d| *d < deadline.saturating_duration_since(Instant::now()))
                else {
                    all = false;
                    break;
                };
                async_std::task::sleep(delay).await;
            }
        }
        all
    }

    /// For a `dcl start` scene — whose files the dev server addresses by
    /// `b64-<base64(`{absolutePath}-{machineId}`)>` (see @dcl/sdk-commands `b64HashingFunction`) and
    /// serves by decoding that back to a path — compute the hash a newly-written file at
    /// project-relative `rel` (original case) will be served under, so the live content-map merge
    /// uses the dev server's hash rather than the source CID (which it doesn't know, hence the 404s
    /// that only a reload fixed). Recovers the project root + machine id from any existing `b64-`
    /// entry in the scene's collection. Returns None when the scene isn't b64-addressed (native /
    /// deployed scenes — there the source hash + local cache is used instead).
    pub async fn local_b64_hash_for(&self, scene_hash: &str, rel: &str) -> Option<String> {
        use base64::{prelude::BASE64_STANDARD, Engine};
        let read = self.context.read().await;
        let collection = &read.entities.get(scene_hash)?.collection;
        for (key, h) in collection.0.iter() {
            let Some((project_root, machine_id)) = b64_parts(key, h) else {
                continue;
            };
            let abs = format!("{project_root}/{rel}-{machine_id}");
            return Some(format!("b64-{}", BASE64_STANDARD.encode(abs.as_bytes())));
        }
        None
    }

    /// The clean absolute project root of a `dcl start` scene, recovered from any existing `b64-`
    /// collection entry — a file's encoded path has clean directory segments (only the filename
    /// carries the `-machineId` suffix), so slicing before `/{key}-` yields the real root. None for
    /// native/deployed scenes (entries aren't in the `b64-<path>-machineId` form). Lets the web save
    /// locate the project folder under a granted directory handle.
    pub async fn local_project_root(&self, scene_hash: &str) -> Option<String> {
        let read = self.context.read().await;
        let collection = &read.entities.get(scene_hash)?.collection;
        for (key, h) in collection.0.iter() {
            if let Some((project_root, _)) = b64_parts(key, h) {
                return Some(project_root);
            }
        }
        None
    }

    /// The stored entity metadata (the scene's scene.json) for a scene, if any.
    pub async fn scene_metadata(&self, scene_hash: &str) -> Option<String> {
        self.context
            .read()
            .await
            .entities
            .get(scene_hash)?
            .metadata
            .clone()
    }

    /// The sorted file paths in a scene's content map (the collection keys), or empty if the scene
    /// isn't loaded. For the editor's content-file pickers; includes imported assets merged into the
    /// collection. Paths are lowercased (as stored).
    pub async fn scene_content_files(&self, scene_hash: &str) -> Vec<String> {
        let read = self.context.read().await;
        let mut files: Vec<String> = read
            .entities
            .get(scene_hash)
            .map(|e| e.collection.0.keys().cloned().collect())
            .unwrap_or_default();
        files.sort();
        files
    }

    /// Re-fetch the current scene's entity from its content server and replace the collection. A
    /// `dcl start` dev server's content map is the *entire* project glob (minus .dclignore), not
    /// just the referenced files, so this picks up files added to the project *outside* the editor
    /// without a scene reload. Imported assets (written to disk by `/init_asset`) are naturally
    /// included. Acts only on local (`dcl start`, b64-addressed) scenes — deployed scenes have an
    /// immutable content map. Returns true if the collection was refreshed.
    pub async fn refresh_scene_collection(self: &Arc<Self>, scene_hash: &str) -> bool {
        // local scenes only; deployed content maps don't change
        if self.local_project_root(scene_hash).await.is_none() {
            return false;
        }
        // the scene's base parcel pointer, from its stored scene.json
        let pointer = {
            let read = self.context.read().await;
            read.entities
                .get(scene_hash)
                .and_then(|e| e.metadata.as_deref())
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|j| {
                    j.get("scene")
                        .and_then(|s| s.get("base"))
                        .and_then(|b| b.as_str())
                        .map(str::to_owned)
                })
        };
        let Some(pointer) = pointer else {
            return false;
        };
        let Ok(defs) = self
            .active_entities(ActiveEntitiesRequest::Pointers(vec![pointer]), None)
            .await
        else {
            return false;
        };
        // exactly one scene at the base parcel; match by id when present, else take the first
        let Some(def) = defs
            .iter()
            .find(|d| d.id == scene_hash)
            .or_else(|| defs.first())
        else {
            return false;
        };
        let collection = def.content.clone();
        let mut write = self.context.write().await;
        match write.entities.get_mut(scene_hash) {
            Some(entity) => {
                entity.collection = collection;
                true
            }
            None => false,
        }
    }

    pub fn about_url(&self) -> Option<String> {
        self.realm_config_receiver
            .borrow()
            .as_ref()
            .map(|c| c.about_url.clone())
    }

    pub fn lambda_endpoint(&self) -> Option<String> {
        self.realm_config_receiver
            .borrow()
            .as_ref()
            .and_then(|c| c.about.lambdas.as_ref())
            .map(|l| l.public_url.clone())
    }

    pub fn contents_endpoint(&self) -> Option<String> {
        self.realm_config_receiver
            .borrow()
            .as_ref()
            .and_then(|c| c.about.content.as_ref())
            .map(|content| format!("{}/contents/", content.public_url))
    }

    pub fn entities_endpoint(&self) -> Option<String> {
        self.realm_config_receiver
            .borrow()
            .as_ref()
            .and_then(|c| c.about.content.as_ref())
            .map(|content| format!("{}/entities/", content.public_url))
    }

    async fn update_scene_urns(
        &self,
        about: &mut ServerAbout,
        new_realm: &str,
    ) -> Result<(), anyhow::Error> {
        let scenes_raw =
            platform::TracedJsonResponse::send(self.client.get(format!("{new_realm}/scenes")))
                .await
                .map_err(|e| anyhow!(e))?;
        if scenes_raw.status() == StatusCode::NOT_FOUND {
            scenes_raw.discard();
            return Ok(());
        } else if scenes_raw.status() != StatusCode::OK {
            return Err(anyhow!("status: {}", scenes_raw.discard()));
        }
        let scenes = scenes_raw.json::<ServerScenes>().await?;

        if scenes.total > 0 {
            if let Some(scenes_urn) = about.configurations.as_mut().and_then(|config| {
                config
                    .scenes_urn
                    .as_mut()
                    .filter(|scenes| !scenes.is_empty())
            }) {
                if scenes_urn.len() != scenes.total {
                    let ipfs_path = IpfsPath::new_from_urn::<EntityDefinition>(&scenes_urn[0])?;

                    *scenes_urn = scenes
                        .scenes
                        .into_iter()
                        .map(|scene| {
                            format!(
                                "urn:decentraland:entity:{}{}",
                                scene.entity_id,
                                if let Some(base_url) = ipfs_path.get(&IpfsKey::BaseUrl) {
                                    format!("?=&baseUrl={}", base_url)
                                } else {
                                    "".to_owned()
                                }
                            )
                        })
                        .collect();
                }
            }
        }
        Ok(())
    }

    /// The parcel a world spawns players on, from `GET {realm}/manifest`. A world's `about` lists
    /// its scenes but not where to land; the manifest's `spawn_coordinate` is the world setting
    /// unity-explorer lands on (the base of the first scene unless the owner set one). A realm
    /// without scene urns (a city) has no manifest to ask for.
    async fn world_spawn_parcel(
        &self,
        about: &ServerAbout,
        new_realm: &str,
    ) -> Result<Option<IVec2>, anyhow::Error> {
        let has_scene_urns = about.configurations.as_ref().is_some_and(|config| {
            config
                .scenes_urn
                .as_ref()
                .is_some_and(|urns| !urns.is_empty())
        });
        if !has_scene_urns {
            return Ok(None);
        }
        let manifest_raw = self
            .client
            .get(format!("{new_realm}/manifest"))
            .send()
            .await
            .map_err(|e| anyhow!(e))?;
        if manifest_raw.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        } else if manifest_raw.status() != StatusCode::OK {
            return Err(anyhow!("status: {}", manifest_raw.status()));
        }
        let manifest = manifest_raw
            .json::<WorldManifest>()
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(manifest
            .spawn_coordinate
            .and_then(|spawn| Some(IVec2::new(spawn.x.value()?, spawn.y.value()?))))
    }
}

pub type ActiveEntityTask = Task<Result<Vec<EntityDefinition>, anyhow::Error>>;

#[derive(Debug)]
pub struct ActiveEntity {
    pub id: String,
    pub pointers: Vec<String>,
    pub content: ContentMap,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ActiveEntitiesPointersRequest {
    pointers: Vec<String>,
}

pub enum ActiveEntitiesRequest {
    Pointers(Vec<String>),
    Urns(Vec<IpfsPath>),
}

#[derive(Deserialize, Debug)]
pub struct ActiveEntitiesResponse(Vec<EntityDefinitionJson>);

impl IpfsIo {
    /// Read an asset: a plain path from the default reader, an ipfs path from the cache or the
    /// remote (`download`).
    pub async fn read<'a>(
        self: &'a Arc<Self>,
        path: &'a Path,
    ) -> Result<Box<dyn Reader + 'a>, AssetReaderError> {
        platform::compat(async move {
            let wrap_err = |e: anyhow::Error| {
                if e.to_string().contains("file not found") {
                    bevy::asset::io::AssetReaderError::NotFound(path.to_owned())
                } else {
                    bevy::asset::io::AssetReaderError::Io(Arc::new(std::io::Error::other(format!(
                        "w: {e}"
                    ))))
                }
            };

            let start = web_time::Instant::now();
            let ipfs_io_read_state = IpfsIoReadState {
                #[cfg(feature = "ipfs_debug")]
                sender: &self.debug_overlay_sender,
                path: Cow::Borrowed(path),
                #[cfg(feature = "ipfs_debug")]
                start,
            };

            debug!("request: {:?}", path);

            let maybe_ipfs_path =
                ipfs_io_read_state.send_failure(IpfsPath::new_from_path(path).map_err(wrap_err))?;
            debug!("ipfs: {maybe_ipfs_path:?}");
            let ipfs_path = match maybe_ipfs_path {
                Some(ipfs_path) => ipfs_path,
                // non-ipfs files are loaded as normal
                None => {
                    let data = ipfs_io_read_state.send_failure(self.default_io.read(path).await)?;
                    ipfs_io_read_state.send_non_ipfs(0);
                    return Ok(data);
                }
            };

            #[cfg(target_arch = "wasm32")]
            if let Some(indexdb_path) = ipfs_path.to_indexdb() {
                use futures_lite::io::AsyncReadExt;
                let mut file =
                    ipfs_io_read_state.send_failure(web_fs::File::open(indexdb_path).await)?;
                let mut daft_buffer = Vec::default();
                ipfs_io_read_state.send_failure(file.read_to_end(&mut daft_buffer).await)?;
                ipfs_io_read_state.send_cached(daft_buffer.len());
                return Ok(Box::new(VecReader::new(daft_buffer)));
            }

            // a content file (scene, wearable or emote) abgen has a verified variant of reads as
            // what that variant decodes to (a texture's DDS)
            if let Some(variant) = self.read_bevy_variant(&ipfs_path, path).await {
                if variant.cached {
                    ipfs_io_read_state.send_cached(variant.bytes.len());
                }
                return Ok(Box::new(VecReader::new(variant.bytes)));
            }

            let cache_key = ipfs_path.cache_key(&*self.context.read().await);

            if let Some(key) = &cache_key {
                debug!("hash: {}", key.name());
                if let Some(cached) =
                    ipfs_io_read_state.send_failure(self.read_disk_cache(key).await)?
                {
                    ipfs_io_read_state.send_cached(cached.len());
                    return Ok(Box::new(VecReader::new(cached)));
                }
            }

            debug!(
                "remote ({})",
                cache_key
                    .as_ref()
                    .map(|key| format!("hash {} not found", key.name()))
                    .unwrap_or_else(|| "uncached".to_owned())
            );

            let token = self.reqno.fetch_add(1, Ordering::SeqCst);

            // wait till connected
            ipfs_io_read_state.send_failure(self.connected().await.map_err(wrap_err))?;

            let context = self.context.read().await;
            let remote = ipfs_path.to_url(&context).map_err(wrap_err);

            if remote.is_err() {
                // check for default file
                if let Some(static_path) = ipfs_path
                    .filename()
                    .and_then(|file_path| self.static_files.get(file_path.as_ref()))
                {
                    let data = ipfs_io_read_state
                        .send_failure(self.default_io.read(Path::new(static_path)).await)?;
                    ipfs_io_read_state.send_cached(0);
                    return Ok(data);
                }
            }
            let remote = ipfs_io_read_state.send_failure(remote)?;

            // file realm: a `file://` baseUrl (local static scene export, e.g. `--system-scene <dir>`)
            // reads straight from disk — no cache write, no request slot, no retries.
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(local) = remote.strip_prefix("file://") {
                // url normalization writes windows paths as `file:///C:/...`; the path
                // component's leading slash isn't part of the disk path, so strip it
                // when a drive letter follows (unix absolute paths keep theirs)
                let local = match local.as_bytes() {
                    [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => &local[1..],
                    _ => local,
                };
                // canonicalize (resolving `..` and symlinks) and require a registered root, so
                // urn-supplied baseUrls / content hashes can't address arbitrary disk paths
                let local = std::fs::canonicalize(local)
                    .ok()
                    .filter(|canonical| {
                        self.allowed_file_roots
                            .read()
                            .unwrap()
                            .iter()
                            .any(|root| canonical.starts_with(root))
                    })
                    .ok_or_else(|| {
                        warn!("refusing file read outside registered file realms: `{remote}`");
                        AssetReaderError::Io(Arc::new(std::io::Error::other(format!(
                            "file realm read `{remote}`: not found or not permitted"
                        ))))
                    });
                let data = ipfs_io_read_state.send_failure(local.and_then(|local| {
                    std::fs::read(local).map_err(|e| {
                        AssetReaderError::Io(Arc::new(std::io::Error::other(format!(
                            "file realm read `{remote}`: {e}"
                        ))))
                    })
                }))?;
                ipfs_io_read_state.send_cached(data.len());
                return Ok(Box::new(VecReader::new(data)));
            }

            let fail_time = context.failed_remotes.get(&remote).cloned();
            drop(context);

            // loopback content is local already, and a preview server's is mutable: never cached
            let cache_key =
                cache_key.filter(|_| !url::Url::parse(&remote).is_ok_and(|url| url.is_loopback()));

            // in wasm we add a custom header to allow the service worker to cache ipfs requests
            // across content servers: anything with a cache key (content by hash, and urls by
            // their digest), whatever path it was asked for under
            #[cfg(target_arch = "wasm32")]
            let sw_caches = cache_key.is_some();

            // the worker's cache is the disk cache here: read it directly, so a cached file
            // doesn't wait on a request slot behind every download in the queue
            #[cfg(target_arch = "wasm32")]
            if sw_caches {
                if let Some(cached) = web_cache::read(&remote).await {
                    ipfs_io_read_state.send_cached(cached.len());
                    return Ok(Box::new(VecReader::new(cached)));
                }
            }

            if let Some(fail_time) = fail_time {
                // wait 10 secs before retrying failed assets
                if self.is_preview
                    || Instant::now()
                        .checked_duration_since(fail_time)
                        .unwrap_or_default()
                        > Duration::from_secs(10)
                {
                    self.context.write().await.failed_remotes.remove(&remote);
                } else {
                    return ipfs_io_read_state.send_failure(Err(AssetReaderError::Io(Arc::new(
                        std::io::Error::other(format!("(repeat request for failed `{remote}`)")),
                    ))));
                }
            }

            // the scene the file belongs to decides its place in the request slot queue
            let scene = ipfs_path.scene_hash().map(str::to_owned);

            // nothing to share an uncacheable download under
            let Some(key) = cache_key else {
                let data = self
                    .clone()
                    .download(token, path.to_owned(), remote, None, scene)
                    .await?;
                return Ok(Box::new(AsyncCursor::new(data)));
            };
            // one download per url: a second read of a url already in flight (the same file
            // under another asset path, say) takes the first's bytes instead of queueing a
            // download of its own behind everything requested since. By url, not by hash: the
            // bytes are shared before the hash check, and a scene names its own server
            let (download, task) = self.share_download(key, token, path.to_owned(), remote, scene);
            let joined = task.is_none();
            if let Some(task) = task {
                IoTaskPool::get().spawn_compat(task).detach();
            }
            let mut result = download.result();
            let data = result
                .wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|result| result.clone())
                // only a panic ends the task without a result: our own arc keeps it alive
                .unwrap_or_else(|| {
                    Err(AssetReaderError::Io(Arc::new(std::io::Error::other(
                        "download ended without a result",
                    ))))
                });
            if joined {
                let data = ipfs_io_read_state.send_failure(data)?;
                ipfs_io_read_state.send_cached(data.len());
                return Ok(Box::new(AsyncCursor::new(data)));
            }
            // the download reported for the reader that started it
            Ok(Box::new(AsyncCursor::new(data?)))
        })
        .await
    }
}

/// A download's bytes, shared with the readers that asked for the same key while it was in flight.
#[derive(Clone, Default)]
pub struct SharedBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::Deref for SharedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

pub struct AsyncCursor<T>(async_std::io::Cursor<T>);

impl<T> AsyncSeekForward for AsyncCursor<T>
where
    T: Unpin + AsRef<[u8]>,
{
    fn poll_seek_forward(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        offset: u64,
    ) -> std::task::Poll<std::io::Result<u64>> {
        let mut pinned = unsafe { self.map_unchecked_mut(|slf| &mut slf.0) };
        pinned
            .as_mut()
            .poll_seek(cx, std::io::SeekFrom::Current(offset as i64))
    }
}

impl<T> AsyncRead for AsyncCursor<T>
where
    T: AsRef<[u8]> + Unpin,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut pinned = unsafe { self.map_unchecked_mut(|slf| &mut slf.0) };
        pinned.as_mut().poll_read(cx, buf)
    }
}

impl<T> Reader for AsyncCursor<T> where T: AsRef<[u8]> + Unpin + Send + Sync {}

impl<T> AsyncCursor<T> {
    pub fn new(data: T) -> Self {
        Self(async_std::io::Cursor::new(data))
    }
}

#[derive(Clone)]
pub struct PassThroughReader {
    inner: Arc<IpfsIo>,
}

impl AssetReader for PassThroughReader {
    async fn read<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        self.inner.read(path).await
    }

    async fn read_meta<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        if IpfsPath::new_from_path(path).is_ok() {
            Err(AssetReaderError::NotFound(path.to_owned()))
        } else {
            self.inner.default_io.read_meta(path).await
        }
    }

    async fn read_directory<'a>(
        &'a self,
        path: &'a Path,
    ) -> Result<Box<bevy::asset::io::PathStream>, AssetReaderError> {
        self.inner.default_io.read_directory(path).await
    }

    async fn is_directory<'a>(&'a self, path: &'a Path) -> Result<bool, AssetReaderError> {
        self.inner.default_io.is_directory(path).await
    }
}

fn ipfs_diagnostics(mut diagnostics: ResMut<DiagnosticsStore>, ipfs: Res<IpfsResource>) {
    let time = web_time::Instant::now();

    let diagnostics_insert =
        |diagnostics: &mut DiagnosticsStore, path: &DiagnosticPath, atomic: &AtomicU32| {
            if let Some(diagnostic) = diagnostics.get_mut(path) {
                diagnostic.add_measurement(DiagnosticMeasurement {
                    time,
                    value: atomic.load(Ordering::Relaxed) as f64,
                });
            };
        };

    diagnostics_insert(
        &mut diagnostics,
        &IPFS_IN_FLIGHT_DIAGNOSTIC_PATH,
        &IPFS_IN_FLIGHT,
    );
    diagnostics_insert(
        &mut diagnostics,
        &IPFS_SUCCESS_DIAGNOSTIC_PATH,
        &IPFS_SUCCESS,
    );
    diagnostics_insert(&mut diagnostics, &IPFS_FAILED_DIAGNOSTIC_PATH, &IPFS_FAILED);
    diagnostics_insert(&mut diagnostics, &IPFS_CACHED_DIAGNOSTIC_PATH, &IPFS_CACHED);
    diagnostics_insert(
        &mut diagnostics,
        &IPFS_NON_IPFS_DIAGNOSTIC_PATH,
        &IPFS_NON_IPFS,
    );
    for (path, counter) in ipfs.inner.request_metrics.counters() {
        if let Some(diagnostic) = diagnostics.get_mut(&path) {
            diagnostic.add_measurement(DiagnosticMeasurement {
                time,
                value: counter.load(Ordering::Relaxed) as f64,
            });
        }
    }
}

struct IpfsIoReadState<'a> {
    #[cfg(feature = "ipfs_debug")]
    sender: &'a tokio::sync::mpsc::UnboundedSender<IpfsDebug>,
    path: Cow<'a, Path>,
    #[cfg(feature = "ipfs_debug")]
    start: web_time::Instant,
}

impl<'a> IpfsIoReadState<'a> {
    #[cfg(feature = "ipfs_debug")]
    fn send(&self, length: usize, status: IpfsDebugStatus) {
        let duration = web_time::Instant::now() - self.start;
        self.sender
            .send(IpfsDebug {
                path: self.path.to_path_buf(),
                status,
                duration,
                length,
            })
            .report();
    }

    fn send_success(&self, length: usize) {
        IPFS_SUCCESS.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "ipfs_debug")]
        self.send(length, IpfsDebugStatus::Success);
    }

    fn send_cached(&self, length: usize) {
        IPFS_CACHED.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "ipfs_debug")]
        self.send(length, IpfsDebugStatus::Cached);
    }

    fn send_non_ipfs(&self, length: usize) {
        IPFS_NON_IPFS.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "ipfs_debug")]
        self.send(length, IpfsDebugStatus::NonIpfs);
    }

    fn send_failure<T, E>(&self, error: Result<T, E>) -> Result<T, E> {
        if error.is_err() {
            IPFS_FAILED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "ipfs_debug")]
            self.send(0, IpfsDebugStatus::Failure);
        }
        error
    }
}

struct DeferredDropper(&'static AtomicU32);

impl DeferredDropper {
    fn new(atomic: &'static AtomicU32) -> Self {
        atomic.fetch_add(1, Ordering::SeqCst);
        Self(atomic)
    }
}

impl Drop for DeferredDropper {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Decodes a dev server's `b64-<base64(`{projectRoot}{sep}{key}-{machineId}`)>` content hash for
/// collection key `key` into its `(projectRoot, machineId)` parts. None for any hash not in that
/// form — a native or deployed scene's entries are plain CIDs.
fn b64_parts(key: &str, hash: &str) -> Option<(String, String)> {
    use base64::{prelude::BASE64_STANDARD, Engine};
    let b64 = hash.strip_prefix("b64-")?;
    let decoded = String::from_utf8(BASE64_STANDARD.decode(b64).ok()?).ok()?;
    let (project_root, machine_id) = b64_split_at_key(&decoded, key)?;
    Some((project_root.to_owned(), machine_id.to_owned()))
}

/// Splits a dev server's b64 hash payload — plain `"{projectRoot}{sep}{key}-{machineId}"` or
/// content-versioned `"{projectRoot}{sep}{key}\0{mtime}-{machineId}"` (sdk-commands embeds the
/// file's mtime after a NUL byte so the id changes when the file does; paths can't contain NUL,
/// so the forms are unambiguous) — at its `/{key}` marker into (projectRoot, machineId), both in
/// their original case and separators. The marker is matched case-insensitively (collection keys
/// are lowercased while the encoded path keeps its casing) and separator-insensitively (a Windows
/// dev server encodes backslash paths); both normalizations are byte-for-byte over the ASCII they
/// rewrite, so an index found in the normalized copy still slices `decoded` itself.
fn b64_split_at_key<'a>(decoded: &'a str, key: &str) -> Option<(&'a str, &'a str)> {
    let normalized = decoded.replace('\\', "/").to_lowercase();
    if let Some(idx) = normalized.rfind(&format!("/{key}\u{0}")) {
        // mtime is digits only, so the first '-' after the NUL starts the machineId
        let (_mtime, machine_id) = decoded[idx + key.len() + 2..].split_once('-')?;
        return Some((&decoded[..idx], machine_id));
    }
    let marker = format!("/{key}-");
    let idx = normalized.rfind(&marker)?;
    Some((&decoded[..idx], &decoded[idx + marker.len()..]))
}

#[cfg(test)]
mod tests {
    use super::{
        b64_split_at_key, ContentMap, EndpointConfig, IpfsContext, IpfsModifier, ServerAbout,
    };

    #[cfg(not(target_arch = "wasm32"))]
    fn read_request_headers(stream: &std::net::TcpStream) {
        use std::io::{BufRead, BufReader};
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        assert!(BufReader::new(stream)
            .lines()
            .any(|line| line.unwrap().is_empty()));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn content_retry_stops_after_four_attempts() {
        use std::io::Write;
        use std::sync::Arc;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(pair) => break pair,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(5))
                        }
                        Err(e) => panic!("server accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                read_request_headers(&stream);
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => panic!("unexpected fifth attempt"),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(e) => panic!("server accept: {e}"),
                }
            }
        });
        let ipfs = Arc::new(super::IpfsIo::new(
            false,
            Box::new(super::FileAssetReader::new("assets")),
            None,
            Default::default(),
            1,
            #[cfg(feature = "ipfs_debug")]
            tokio::sync::mpsc::unbounded_channel().0,
        ));
        assert!(ipfs
            .download(
                1,
                "first".into(),
                format!("http://{address}/first"),
                None,
                None
            )
            .await
            .is_err());
        server.join().unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn engine_request_keeps_its_slot_until_the_body_finishes() {
        use std::io::Write;
        use std::sync::{mpsc, Arc};
        use std::time::Duration;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (started, ready) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_headers(&stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n")
                .unwrap();
            started.send(()).unwrap();
            resumed.recv_timeout(Duration::from_secs(5)).unwrap();
            stream.write_all(b"x").unwrap();
        });
        let ipfs = Arc::new(super::IpfsIo::new(
            false,
            Box::new(super::FileAssetReader::new("assets")),
            None,
            Default::default(),
            1,
            #[cfg(feature = "ipfs_debug")]
            tokio::sync::mpsc::unbounded_channel().0,
        ));
        let client = ipfs.client();
        let request = client.get(format!("http://{address}/")).build().unwrap();
        let io = ipfs.clone();
        let task = tokio::spawn(async move {
            io.fetch_request(
                request,
                client,
                Duration::from_secs(5),
                Duration::from_secs(5),
                None,
            )
            .await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match ready.try_recv() {
                Ok(()) => break,
                Err(mpsc::TryRecvError::Disconnected) => panic!("server exited before headers"),
                Err(mpsc::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    async_std::task::sleep(Duration::from_millis(5)).await
                }
                Err(mpsc::TryRecvError::Empty) => panic!("server did not send headers"),
            }
        }
        assert!(ipfs.request_slots.try_acquire().is_none());
        resume.send(()).unwrap();
        let result = task.await.unwrap();
        assert!(matches!(result, Ok(response) if response.body == b"x"));
        assert!(ipfs.request_slots.try_acquire().is_some());
        server.join().unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn unsigned_content_fetch_retries_503_and_reports_terminal_404() {
        use std::io::Write;
        use std::sync::Arc;
        use std::time::Duration;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for response in [
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ] {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(pair) => break pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                        Err(e) => panic!("server accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                read_request_headers(&stream);
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let ipfs = Arc::new(super::IpfsIo::new(
            false,
            Box::new(super::FileAssetReader::new("assets")),
            None,
            Default::default(),
            1,
            #[cfg(feature = "ipfs_debug")]
            tokio::sync::mpsc::unbounded_channel().0,
        ));
        let client = ipfs.client();
        let make_request = || client.get(format!("http://{address}/")).build().unwrap();
        let first = ipfs
            .fetch_request(
                make_request(),
                client.clone(),
                Duration::from_secs(5),
                Duration::from_secs(5),
                None,
            )
            .await;
        assert!(matches!(first, Ok(response) if response.body == b"x"));
        let missing = ipfs
            .fetch_request(
                make_request(),
                client,
                Duration::from_secs(5),
                Duration::from_secs(5),
                None,
            )
            .await;
        assert!(
            matches!(missing, Err(platform::FetchError::Status(status, _)) if status.as_u16() == 404)
        );
        server.join().unwrap();
    }

    #[test]
    fn readiness_poll_respects_retry_after_and_remaining_deadline() {
        use reqwest::{
            header::{HeaderMap, RETRY_AFTER},
            StatusCode,
        };
        use std::time::Duration;

        let mut headers = HeaderMap::new();
        let mut retries = 0;
        let mut poll = |status, headers: &HeaderMap, seconds| {
            super::readiness_retry_delay(
                status,
                headers,
                Duration::from_secs(seconds),
                &mut retries,
            )
        };
        let cadence = Some(Duration::from_millis(150));
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            headers.clear();
            assert_eq!(poll(status, &headers, 3), cadence);
            headers.insert(RETRY_AFTER, "invalid".parse().unwrap());
            assert_eq!(poll(status, &headers, 3), cadence);
            headers.insert(RETRY_AFTER, "61".parse().unwrap());
            assert_eq!(poll(status, &headers, 90), None);
            headers.insert(RETRY_AFTER, "18446744073709551616".parse().unwrap());
            assert_eq!(poll(status, &headers, 90), None);
        }

        let status = StatusCode::SERVICE_UNAVAILABLE;
        headers.insert(RETRY_AFTER, "2".parse().unwrap());
        assert_eq!(poll(status, &headers, 2), None);
        for _ in 0..3 {
            assert_eq!(poll(status, &headers, 3), Some(Duration::from_secs(2)));
        }
        assert_eq!(poll(status, &headers, 3), None);
        headers.clear();
        assert_eq!(poll(status, &headers, 3), cadence);
        assert_eq!(poll(StatusCode::NOT_FOUND, &headers, 3), cadence);
        assert_eq!(poll(status, &headers, 0), None);
    }

    #[test]
    fn contents_base_url_is_the_urn_prefix_else_the_realms_contents_endpoint() {
        let mut context = IpfsContext {
            about: Some(ServerAbout {
                content: Some(EndpointConfig {
                    healthy: true,
                    public_url: "https://peer-ap1.decentraland.org/content".to_owned(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // a pointer-loaded scene has no modifier
        assert_eq!(
            context.contents_base_url("bafypointer").as_deref(),
            Some("https://peer-ap1.decentraland.org/content/contents/")
        );
        // a urn-loaded scene's modifier is already the full contents prefix
        context.modifiers.insert(
            "bafyworld".to_owned(),
            IpfsModifier {
                base_url: Some(
                    "https://worlds-content-server.decentraland.org/contents/".to_owned(),
                ),
            },
        );
        assert_eq!(
            context.contents_base_url("bafyworld").as_deref(),
            Some("https://worlds-content-server.decentraland.org/contents/")
        );
    }

    #[test]
    fn trim_public_urls_drops_the_abouts_trailing_slash() {
        let mut about = ServerAbout {
            content: Some(EndpointConfig {
                healthy: true,
                public_url: "https://peer-ap1.decentraland.org/content/".to_owned(),
            }),
            lambdas: Some(EndpointConfig {
                healthy: true,
                public_url: "https://peer-ap1.decentraland.org/lambdas/".to_owned(),
            }),
            ..Default::default()
        };
        about.trim_public_urls();
        assert_eq!(
            about.content_url(),
            Some("https://peer-ap1.decentraland.org/content")
        );
        assert_eq!(
            about.lambdas.unwrap().public_url,
            "https://peer-ap1.decentraland.org/lambdas"
        );
    }

    #[test]
    fn splits_unix_path() {
        let decoded = "/Users/bob/scene/models/Tree.glb-my-mac";
        let split = b64_split_at_key(decoded, "models/tree.glb");
        assert_eq!(split, Some(("/Users/bob/scene", "my-mac")));
    }

    #[test]
    fn splits_windows_path() {
        let decoded = r"C:\Users\bob\scene\models\Tree.glb-my-pc";
        let split = b64_split_at_key(decoded, "models/tree.glb");
        assert_eq!(split, Some((r"C:\Users\bob\scene", "my-pc")));
    }

    #[test]
    fn no_split_for_other_key() {
        assert_eq!(
            b64_split_at_key(r"C:\Users\bob\scene\models\Tree.glb-pc", "scene.json"),
            None
        );
    }

    #[test]
    fn splits_content_versioned_path() {
        let decoded = "/Users/bob/scene/models/Tree.glb\u{0}1786032377138-my-mac";
        let split = b64_split_at_key(decoded, "models/tree.glb");
        assert_eq!(split, Some(("/Users/bob/scene", "my-mac")));
    }

    #[test]
    fn splits_content_versioned_windows_path() {
        let decoded = "C:\\Users\\bob\\scene\\models\\Tree.glb\u{0}1786032377138-my-pc";
        let split = b64_split_at_key(decoded, "models/tree.glb");
        assert_eq!(split, Some((r"C:\Users\bob\scene", "my-pc")));
    }

    #[test]
    fn content_map_keys_match_the_access_side() {
        let map = ContentMap::from_content([
            ("c:/Textures/X.png".to_owned(), "hash_drive".to_owned()),
            ("/a/Y.png".to_owned(), "hash_root".to_owned()),
            (r"b\Z.png".to_owned(), "hash_bs".to_owned()),
        ]);

        // the drive/root forms resolved before the access side started forcing srcs relative;
        // normalizing the keys the same way keeps them referenceable, under either spelling
        for (src, expected) in [
            ("c:/Textures/X.png", "hash_drive"),
            ("Textures/X.png", "hash_drive"),
            ("/a/Y.png", "hash_root"),
            ("a/y.png", "hash_root"),
            (r"b\Z.png", "hash_bs"),
            ("b/z.png", "hash_bs"),
        ] {
            assert_eq!(map.hash(src).as_deref(), Some(expected), "{src}");
        }
    }
}

/// Follows the max downloads setting
fn apply_max_downloads(config: Option<Res<common::structs::AppConfig>>, ipfs: Res<IpfsResource>) {
    if let Some(config) = config.filter(|config| config.is_changed()) {
        ipfs.set_concurrent_remote_count(config.max_concurrent_remotes);
    }
}
