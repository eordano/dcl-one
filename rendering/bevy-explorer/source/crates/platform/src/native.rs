use async_tungstenite::{async_std::ConnectStream, WebSocketStream};
use bevy::{
    core_pipeline::{
        bloom::Bloom,
        tonemapping::{DebandDither, Tonemapping},
    },
    ecs::bundle::Bundle,
    pbr::ShadowFilteringMethod,
    render::view::{ColorGrading, ColorGradingGlobal, ColorGradingSection},
};
use futures_util::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream},
    StreamExt,
};
use serde::Serialize;
pub use tungstenite::client::IntoClientRequest;

pub(crate) use crate::devtools::Trace;

pub mod deeplink;
mod dm_history;
pub use dm_history::*;

pub struct WebSocket {
    inner: WebSocketStream<ConnectStream>,
}

impl WebSocket {
    pub async fn send(&mut self, item: tungstenite::Message) -> Result<(), anyhow::Error> {
        self.inner.send(item).await.map_err(|e| anyhow::anyhow!(e))
    }

    pub async fn next(&mut self) -> Option<Result<tungstenite::Message, anyhow::Error>> {
        self.inner
            .next()
            .await
            .map(|result| result.map_err(|e| anyhow::anyhow!(e)))
    }

    pub fn split(self) -> (WebSocketSplitSink, WebSocketSplitStream) {
        let (w, r) = self.inner.split();
        (
            WebSocketSplitSink { inner: w },
            WebSocketSplitStream { inner: r },
        )
    }
}

pub struct WebSocketSplitSink {
    inner: SplitSink<WebSocketStream<ConnectStream>, tungstenite::Message>,
}

impl WebSocketSplitSink {
    pub async fn send(&mut self, item: tungstenite::Message) -> Result<(), anyhow::Error> {
        self.inner.send(item).await.map_err(|e| anyhow::anyhow!(e))
    }
}

pub struct WebSocketSplitStream {
    inner: SplitStream<WebSocketStream<ConnectStream>>,
}

impl WebSocketSplitStream {
    pub async fn next(&mut self) -> Option<Result<tungstenite::Message, anyhow::Error>> {
        self.inner
            .next()
            .await
            .map(|result| result.map_err(|e| anyhow::anyhow!(e)))
    }
}

pub async fn websocket<R>(request: R) -> Result<WebSocket, anyhow::Error>
where
    R: IntoClientRequest + Unpin,
{
    let (stream, _response) = async_tungstenite::async_std::connect_async(request).await?;
    Ok(WebSocket { inner: stream })
}

pub trait ReqwestBuilderExt {}

pub fn compat<F>(f: F) -> async_compat::Compat<F> {
    async_compat::Compat::new(f)
}

pub fn project_directories() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("org", "decentraland", "BevyExplorer")
}

/// Writes the config file, whole or not at all: a failure is logged, and the file keeps what it
/// held. The file can hold a root key (`AppConfig::guest_root_key`), so a run that is killed
/// mid-write must not leave it truncated, and a failed write must not pass unseen.
pub fn write_config_file<T: Serialize>(config: &T) {
    let config_file = project_directories()
        .unwrap()
        .config_dir()
        .join("config.json");

    if let Some(folder) = config_file.parent() {
        if let Err(e) = std::fs::create_dir_all(folder) {
            bevy::log::error!("can't create {}: {e}", folder.display());
            return;
        }
    }
    if let Err(e) = write_atomically(
        &config_file,
        serde_json::to_string(config).unwrap().as_bytes(),
    ) {
        bevy::log::error!("can't write {}: {e}", config_file.display());
    }
}

mod client_logins;

fn login_queue_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub async fn client_logins_read() -> Result<String, String> {
    let dirs = project_directories().ok_or("no config directory")?;
    client_logins::read(dirs.config_dir(), login_queue_now())
}

pub async fn client_logins_write(json: String) -> Result<String, String> {
    let dirs = project_directories().ok_or("no config directory")?;
    client_logins::write(dirs.config_dir(), &json, login_queue_now())
}

/// `bytes` to `path` through a temporary file beside it that is renamed over it, so a reader (or
/// the next run after a kill) sees the old file or the new one, never a part. Nothing is synced to
/// disk, which would stall the frame that saves the settings: a killed process loses nothing it
/// wrote, a power cut can lose the latest write. The file is readable by its owner alone where
/// the platform has such modes (0600 on unix); on Windows it takes the user profile's access
/// rules, which exclude other users.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = std::path::PathBuf::from(temporary);
    // a leftover of a killed write; `create_new` below never writes through a file or link
    // that is already there
    let _ = std::fs::remove_file(&temporary);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    // closed before the rename
    let written = options
        .open(&temporary)
        .and_then(|mut file| file.write_all(bytes));
    match written.and_then(|()| std::fs::rename(&temporary, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temporary);
            Err(e)
        }
    }
}

#[derive(Default)]
pub struct AsyncRwLock<T>(tokio::sync::RwLock<T>);

impl<T> AsyncRwLock<T> {
    pub fn new(value: T) -> Self {
        Self(tokio::sync::RwLock::new(value))
    }

    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, T> {
        self.0.read().await
    }

    pub async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, T> {
        self.0.write().await
    }

    pub fn blocking_read(&self) -> tokio::sync::RwLockReadGuard<'_, T> {
        self.0.blocking_read()
    }

    pub fn blocking_write(&self) -> tokio::sync::RwLockWriteGuard<'_, T> {
        self.0.blocking_write()
    }

    pub fn try_read(
        &self,
    ) -> Result<tokio::sync::RwLockReadGuard<'_, T>, tokio::sync::TryLockError> {
        self.0.try_read()
    }

    pub fn try_write(
        &self,
    ) -> Result<tokio::sync::RwLockWriteGuard<'_, T>, tokio::sync::TryLockError> {
        self.0.try_write()
    }
}

pub fn platform_pointer_lock_requested() {}

/// Nothing outside the engine changes the grab natively.
pub fn platform_pointer_is_locked() -> Option<bool> {
    None
}

/// The OS language as a BCP-47 tag (`en-GB`).
pub fn system_language() -> Option<String> {
    sys_locale::get_locale()
}

pub fn default_camera_components() -> impl Bundle {
    (
        Tonemapping::AcesFitted,
        DebandDither::Enabled,
        ColorGrading {
            // ACES tonemapping with slightly deepened midtones; tune live with
            // /tonemap /exposure /gamma /saturation
            global: ColorGradingGlobal {
                exposure: 0.0,
                ..Default::default()
            },
            shadows: ColorGradingSection {
                gamma: 0.9,
                ..Default::default()
            },
            midtones: ColorGradingSection {
                gamma: 0.9,
                ..Default::default()
            },
            highlights: ColorGradingSection {
                gamma: 0.9,
                ..Default::default()
            },
        },
        Bloom {
            // tune live with /bloom
            intensity: 0.15,
            ..Bloom::OLD_SCHOOL
        },
        ShadowFilteringMethod::Gaussian,
    )
}

// Persist a scene's composite. The destination is the active scene's project root, which the
// caller resolves (via `IpfsIo::local_project_root`) and passes in `scene_target` as `{root, …}`;
// we write straight to `<root>/assets/scene/main.composite` and return the path. A remote/deployed
// scene has no local root (`root` is null), so saving is refused — clone it locally to edit.
pub async fn save_scene_composite(
    _scene_hash: String,
    bytes: Vec<u8>,
    scene_target: String,
) -> Result<String, String> {
    let Some(path) = local_composite_path(&scene_target) else {
        return Err(
            "save is only supported for a local scene — clone it locally before editing"
                .to_string(),
        );
    };
    std::fs::write(&path, &bytes)
        .map(|_| path.display().to_string())
        .map_err(|e| format!("write failed ({}): {e}", path.display()))
}

// The local project root carried in the `scene_target` JSON (`{root, …}`, resolved by the caller
// via `IpfsIo::local_project_root` — which correctly decodes the dev server's standard-base64
// `b64-<path>-<machineId>` hashes). None for a remote/deployed scene, where `root` is null.
pub fn local_scene_root(scene_target: &str) -> Option<std::path::PathBuf> {
    let target: serde_json::Value = serde_json::from_str(scene_target).ok()?;
    let root = target.get("root")?.as_str()?;
    Some(std::path::PathBuf::from(root))
}

// The local target path under the scene's project root, else None.
fn local_composite_path(scene_target: &str) -> Option<std::path::PathBuf> {
    Some(
        local_scene_root(scene_target)?
            .join("assets")
            .join("scene")
            .join("main.composite"),
    )
}

// Write bytes into a local scene project at `rel_path` (relative to the project root), creating
// parent dirs. Used to persist imported assets into the edited scene so it renders on a normal
// (non-live) load. Err for a remote (non-local) scene.
pub async fn write_scene_file(
    _scene_hash: &str,
    rel_path: &str,
    bytes: &[u8],
    scene_target: &str,
) -> Result<(), String> {
    let Some(root) = local_scene_root(scene_target) else {
        return Err("not a local scene".to_string());
    };
    let dest = root.join(rel_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(&dest, bytes).map_err(|e| format!("write {}: {e}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("platform_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn files(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_write_makes_the_file_and_leaves_nothing_beside_it() {
        let dir = directory("makes");
        let path = dir.join("config.json");
        write_atomically(&path, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
        assert_eq!(files(&dir), ["config.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_replaces_a_longer_file_whole() {
        let dir = directory("replaces");
        let path = dir.join("config.json");
        std::fs::write(&path, "x".repeat(10_000)).unwrap();
        write_atomically(&path, b"short").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
        assert_eq!(files(&dir), ["config.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_leftover_of_a_killed_write_is_replaced() {
        let dir = directory("leftover");
        let path = dir.join("config.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::write(dir.join("config.json.tmp"), "half a wri").unwrap();
        write_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(files(&dir), ["config.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_is_an_error_and_the_file_keeps_what_it_held() {
        let dir = directory("fails");
        let path = dir.join("config.json");
        std::fs::write(&path, "old").unwrap();
        // the temporary file's name is taken by a directory: nothing can be written there
        std::fs::create_dir(dir.join("config.json.tmp")).unwrap();
        assert!(write_atomically(&path, b"new").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");

        // and a folder that is not there is an error too, not a silent loss
        assert!(write_atomically(&dir.join("missing").join("config.json"), b"new").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = directory("mode");
        let path = dir.join("config.json");
        // a file that was world-readable is replaced by one that is not
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomically(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
