//! Loading a scene into the host: the active deployment through the ONE
//! room resolver, the scene entity from the shared rows, `game.js` and
//! `main.crdt` from the ONE blob store, a storage delegation scoped to the
//! resolver's namespace, then the runtime. Nothing is fetched over the
//! network: a scene this node does not hold locally is not hosted here.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sqlx::Row;

use crate::comms::world_identity::{RealmId, SceneId};
use crate::rooms::{self, ActiveDeployment};

use super::delegation::{self, DelegationScope, DelegationSlot};
use super::jsruntime::StorageCtx;
use super::runtime::{JsRuntime, RelayRuntime, SceneRuntime, ServerTransportConfig};
use super::scene::{Scene, SceneIdentity};
use super::{DelegationSource, SceneHostState, SceneKey};

pub const LOCAL_SCENE_NAME: &str = "localScene";

/// The old `FETCH_MAX_BODY_BYTES` default: the largest scene code the host
/// hands an isolate.
pub const MAX_SCENE_CODE_BYTES: usize = 50 * 1024 * 1024;

/// The active deployment of a key and the two files the host runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScene {
    pub deployment: ActiveDeployment,
    pub code_hash: String,
    pub crdt_hash: Option<String>,
    /// scene.json `authoritativeMultiplayer: true`: the scene asks for a server.
    pub authoritative: bool,
}

#[derive(Deserialize)]
struct SceneEntity {
    metadata: SceneMetadata,
    #[serde(default)]
    content: Vec<ContentEntry>,
}

#[derive(Deserialize)]
struct SceneMetadata {
    main: String,
    #[serde(default, rename = "authoritativeMultiplayer")]
    authoritative_multiplayer: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ContentEntry {
    file: String,
    hash: String,
}

/// `(game.js hash, main.crdt hash)` from a scene's `main` and file list.
fn scene_files(main: &str, files: &[(String, String)]) -> Result<(String, Option<String>)> {
    let code = files
        .iter()
        .find(|(file, _)| file == main)
        .map(|(_, hash)| hash.clone())
        .ok_or_else(|| anyhow!("cannot find entry point for scene"))?;
    let crdt = files
        .iter()
        .find(|(file, _)| file == "main.crdt")
        .map(|(_, hash)| hash.clone());
    Ok((code, crdt))
}

fn genesis_realm(state: &SceneHostState) -> RealmId {
    RealmId::Shared(state.settings.realm_name.clone())
}

/// The deployment `key` names right now, without its files: what the sweep
/// compares against the hosted scene. `Ok(None)` when nothing local is
/// active (or the key is the local development scene).
pub async fn current_scene_id(state: &SceneHostState, key: &SceneKey) -> Result<Option<SceneId>> {
    Ok(active_deployment(state, key).await?.map(|d| d.scene_id))
}

async fn active_deployment(
    state: &SceneHostState,
    key: &SceneKey,
) -> Result<Option<ActiveDeployment>> {
    let dep = match key {
        SceneKey::Local => return Ok(None),
        SceneKey::World(world) => rooms::world_active_scene(&state.pool, world).await?,
        SceneKey::Genesis(id) => {
            rooms::genesis_scene(&state.pool, &genesis_realm(state), id).await?
        }
        SceneKey::GenesisParcel(parcel) => {
            rooms::genesis_scene_at(&state.pool, &genesis_realm(state), *parcel).await?
        }
    };
    Ok(dep.filter(|d| d.local))
}

/// The active deployment and its files, or `None` when this node holds no
/// local active deployment for `key`. A mirrored world scene is `None` too:
/// it exists here only as a copy, and a copy grants no host authority.
pub async fn resolve(state: &SceneHostState, key: &SceneKey) -> Result<Option<ResolvedScene>> {
    let Some(deployment) = active_deployment(state, key).await? else {
        return Ok(None);
    };
    let (main, files, authoritative): (String, Vec<(String, String)>, bool) = match key {
        SceneKey::Local => unreachable!("the local scene has no deployment"),
        SceneKey::World(world) => {
            let entity: serde_json::Value = sqlx::query_scalar(
                "SELECT entity FROM worlds.world_scenes WHERE world_name = $1 AND entity_id = $2",
            )
            .bind(world.key())
            .bind(&deployment.scene_id)
            .fetch_one(&state.pool)
            .await
            .context("reading the scene entity")?;
            let entity: SceneEntity =
                serde_json::from_value(entity).context("parse scene entity")?;
            (
                entity.metadata.main,
                entity
                    .content
                    .into_iter()
                    .map(|c| (c.file, c.hash))
                    .collect(),
                entity.metadata.authoritative_multiplayer == Some(serde_json::Value::Bool(true)),
            )
        }
        SceneKey::Genesis(_) | SceneKey::GenesisParcel(_) => {
            let rows = sqlx::query(
                "SELECT d.entity_metadata ->> 'main' AS main, \
                 (d.entity_metadata -> 'authoritativeMultiplayer')::jsonb = 'true'::jsonb AS authoritative, \
                 cf.key, cf.content_hash \
                 FROM content.deployments d \
                 LEFT JOIN content.content_files cf ON cf.deployment = d.id \
                 WHERE d.entity_id = $1",
            )
            .bind(&deployment.scene_id)
            .fetch_all(&state.pool)
            .await
            .context("reading the scene deployment")?;
            let main: Option<String> = rows.first().and_then(|r| r.get("main"));
            let authoritative = rows
                .first()
                .and_then(|r| r.get::<Option<bool>, _>("authoritative"))
                .unwrap_or(false);
            let files = rows
                .iter()
                .filter_map(|r| {
                    Some((
                        r.get::<Option<String>, _>("key")?,
                        r.get::<Option<String>, _>("content_hash")?,
                    ))
                })
                .collect();
            (
                main.ok_or_else(|| anyhow!("scene {} declares no main file", deployment.scene_id))?,
                files,
                authoritative,
            )
        }
    };
    let (code_hash, crdt_hash) = scene_files(&main, &files)?;
    Ok(Some(ResolvedScene {
        deployment,
        code_hash,
        crdt_hash,
        authoritative,
    }))
}

async fn read_blob(state: &SceneHostState, hash: &str, what: &str) -> Result<Vec<u8>> {
    let bytes = state
        .storage
        .retrieve(hash)
        .await
        .with_context(|| format!("reading {what} {hash} from the blob store"))?;
    let bytes = bytes.ok_or_else(|| anyhow!("{what} {hash} is not in the blob store"))?;
    if bytes.len() > MAX_SCENE_CODE_BYTES {
        bail!(
            "{what} {hash} is {} bytes, over the {MAX_SCENE_CODE_BYTES} byte cap",
            bytes.len()
        );
    }
    Ok(bytes.to_vec())
}

fn code_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(
        bytes
            .strip_prefix(b"\xef\xbb\xbf".as_slice())
            .unwrap_or(bytes),
    )
    .into_owned()
}

pub async fn from_local(path: &str) -> Result<String> {
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read local scene {path}"))
}

/// Replaces whatever runs under `key` with the current deployment. A failure
/// leaves nothing hosted under the key (the old host dropped the previous
/// scene first too).
pub async fn load_or_reload(state: &SceneHostState, key: &SceneKey) -> Result<()> {
    load_scene(state, key, false).await
}

/// [`load_or_reload`]; with `require_opt_in` a scene that does not set
/// `authoritativeMultiplayer` is refused. Whatever was hosted under the key
/// is stopped first either way, so callers that require opt-in only load
/// keys that are not hosted.
pub(super) async fn load_scene(
    state: &SceneHostState,
    key: &SceneKey,
    require_opt_in: bool,
) -> Result<()> {
    let name = key.room_key();
    if state.is_key_parked(&name) {
        bail!("{name} ran out of memory earlier; it is not loaded again until restart");
    }
    if let Some((prev, _)) = state.remove_hosted(&name) {
        tracing::info!(scene = %name, "stopping existing scene");
        drop(prev);
    }
    let resolved = match key {
        SceneKey::Local => None,
        _ => {
            let resolved = resolve(state, key).await?.ok_or_else(|| match key {
                SceneKey::World(w) => anyhow!(
                    "world {} has no scene published on this node (mirrored scenes are not hosted)",
                    w.display()
                ),
                other => anyhow!("no local active Genesis scene for {other}"),
            })?;
            if require_opt_in && !resolved.authoritative {
                bail!("{key} does not enable authoritativeMultiplayer");
            }
            Some(resolved)
        }
    };

    let (hash, source, static_crdt, identity, scope) = match key {
        SceneKey::Local => {
            let path = state
                .settings
                .local_scene_path
                .clone()
                .ok_or_else(|| anyhow!("SCENE_HOST_LOCAL_SCENE_PATH not set"))?;
            let src = from_local(&path).await?;
            (
                LOCAL_SCENE_NAME.to_string(),
                src,
                Vec::new(),
                SceneIdentity::local(),
                None,
            )
        }
        _ => {
            let resolved = resolved.expect("resolved above for every non-local key");
            if !matches!(key, SceneKey::World(_))
                && state.covers_parked_parcel(&resolved.deployment.parcels.canonical())
            {
                bail!(
                    "{name} stands on parcels of a scene that ran out of memory; not loaded until restart"
                );
            }
            let code = read_blob(state, &resolved.code_hash, "scene code").await?;
            let static_crdt = match &resolved.crdt_hash {
                Some(h) => read_blob(state, h, "main.crdt").await?,
                None => Vec::new(),
            };
            let realm = match key {
                SceneKey::World(w) => RealmId::World(w.clone()),
                _ => genesis_realm(state),
            };
            // The parcel the storage routes resolve back to this scene: its base.
            let scope = DelegationScope {
                world: realm.storage_realm().to_string(),
                scene_id: resolved.deployment.scene_id.as_str().to_string(),
                parcel: resolved.deployment.base.canonical(),
            };
            let hash = resolved.deployment.scene_id.as_str().to_string();
            (
                hash,
                code_text(&code),
                static_crdt,
                SceneIdentity {
                    key: key.clone(),
                    deployment: Some(resolved.deployment),
                },
                Some(scope),
            )
        }
    };

    let mut renewal: Option<tokio::task::JoinHandle<()>> = None;
    let runtime: Arc<dyn SceneRuntime> = if state.settings.disable_js_runtime
        || source.trim().is_empty()
    {
        tracing::info!(scene = %name, %hash, "loading with RelayRuntime (JS disabled or empty source)");
        Arc::new(RelayRuntime::with_limits(
            hash.clone(),
            ServerTransportConfig::default(),
            &state.settings.limits,
        ))
    } else {
        let realm = match key {
            SceneKey::World(w) => w.display().to_string(),
            _ => state.settings.realm_name.clone(),
        };
        if super::jsruntime::is_parked(&hash) {
            bail!("scene {hash} ran out of memory earlier; it is not loaded again until restart");
        }
        tracing::info!(scene = %name, %hash, "loading with JsRuntime (server-side game.js)");
        let storage = match build_storage_ctx(state, scope).await {
            Some((ctx, task)) => {
                renewal = task;
                Some(ctx)
            }
            None => None,
        };
        Arc::new(JsRuntime::new(
            hash.clone(),
            source,
            realm,
            state.settings.limits,
            static_crdt,
            storage,
        ))
    };
    let scene = Arc::new(Scene::new_with_renewal(
        name.clone(),
        runtime,
        renewal,
        identity,
    ));
    state.scenes.insert(name.clone(), scene);
    tracing::info!(scene = %name, %hash, "scene loaded");
    Ok(())
}

/// The scene's storage capability: the configured origin, and a delegation
/// slot filled from the configured source (a pre-minted envelope, or a first
/// mint for `scope` plus the renewal loop). Without an origin the scene has
/// no storage; without a usable source its requests fail closed.
async fn build_storage_ctx(
    state: &SceneHostState,
    scope: Option<DelegationScope>,
) -> Option<(StorageCtx, Option<tokio::task::JoinHandle<()>>)> {
    let origin = state.settings.storage.as_ref()?;
    let slot: DelegationSlot = Arc::new(parking_lot::Mutex::new(None));
    let mut renew_tx = None;
    let mut task = None;

    match (&state.settings.delegations, scope) {
        (DelegationSource::Static(d), _) => *slot.lock() = Some(d.clone()),
        (DelegationSource::Minter(minter), Some(scope)) => {
            match minter.mint(&scope).await {
                Ok(d) => *slot.lock() = Some(d),
                Err(e) => tracing::warn!(
                    world = %scope.world, scene = %scope.scene_id, error = %e,
                    "initial storage delegation mint failed; storage requests fail closed until renewal succeeds"
                ),
            }
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            renew_tx = Some(tx);
            // One loop per hosted scene, named after it; it ends with the
            // scene (its channel) or with the process (the jobs' root token).
            let shutdown = state.shutdown_token();
            let job = format!("scene-host-renewal:{}:{}", scope.world, scope.scene_id);
            let renewal =
                delegation::renewal_loop(Arc::clone(minter), scope, Arc::clone(&slot), rx);
            task = Some(tokio::spawn(async move {
                tokio::select! {
                    _ = renewal => {}
                    _ = shutdown.cancelled() => tracing::debug!(job, "storage delegation renewal stopped by shutdown"),
                }
            }));
        }
        (DelegationSource::Minter(_), None) => {
            tracing::warn!(
                "the local development scene has no deployment to scope a delegation to; storage requests fail closed"
            )
        }
        (DelegationSource::None, _) => {
            tracing::warn!(
                "SCENE_HOST_STORAGE_URL set without a delegation source; storage requests fail closed"
            )
        }
    }

    Some((
        StorageCtx {
            origin: origin.origin.clone(),
            allow_http_loopback: origin.allow_http_loopback,
            delegation: slot,
            renew_tx,
            http: state.fetch_http.clone(),
        },
        task,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_point_and_the_static_crdt_are_looked_up_by_file_name() {
        let files = vec![
            ("bin/game.js".to_string(), "bafycode".to_string()),
            ("main.crdt".to_string(), "bafycrdt".to_string()),
        ];
        assert_eq!(
            scene_files("bin/game.js", &files).unwrap(),
            ("bafycode".to_string(), Some("bafycrdt".to_string()))
        );
        assert_eq!(
            scene_files("bin/game.js", &files[..1]).unwrap(),
            ("bafycode".to_string(), None)
        );
        assert!(scene_files("index.js", &files).is_err());
        assert_eq!(code_text("\u{feff}const x = 1;".as_bytes()), "const x = 1;");
        let entity: SceneEntity = serde_json::from_value(serde_json::json!({
            "metadata": { "main": "game.js", "scene": { "base": "3,4" } },
            "content": [{ "file": "game.js", "hash": "bafycode-memo" }]
        }))
        .unwrap();
        assert_eq!(
            (entity.metadata.main.as_str(), entity.content.len()),
            ("game.js", 1)
        );
    }
}
