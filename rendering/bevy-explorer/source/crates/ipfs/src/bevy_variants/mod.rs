//! Prototype: GPU-ready "Bevy variants" of the textures and models of scenes, wearables and
//! emotes, published by abgen.
//!
//! The variants base is `--bevy-variants-url`, `?bevyVariants=` or the `bevy_variants_url` config
//! field, else the asset-bundle generator's CDN for the base domain (`https://abgen-cdn.<base>`);
//! `off` in any of them turns the client off. For each scene, wearable and emote entity it loads, the
//! explorer fetches
//! `{base}/bevy/manifest/{entityId}.json` once (schema v3); it maps original content CIDs to the
//! paths of their variants, `bevy/{prefix}/{name}`, whose prefix letter is the variant's class
//! (`t` a BC KTX2 texture, `m` a `glb-br` model, `g` a GLB). A content file read whose
//! original CID has a variant of a class this client takes is answered with
//! `{base}/{variantPath}` instead and cached under a digest of that url, never under the
//! original's hash. abgen keeps a path's bytes fixed for the life of its generation, and the
//! manifest names no hash for them, so a variant is trusted by its path and rejected only by
//! what it decodes to. A texture variant is a BC KTX2 whose header names its BC format, handed
//! to the image loaders as it is; they unwrap it to the DDS they decode (`ktx2`), inside the
//! asset load. Anything else, at any step, reads the original. A variant that fails to decode
//! is marked bad for the session, its cached copy dropped, and its asset reloaded from the
//! original. Manifests and variants are fetched in a single attempt with short timeouts: the
//! original is their retry. An entity's files wait for its manifest for at
//! most [`MANIFEST_DEADLINE`] from the request, then read their originals.
//!
//! The formats taken are the [`SUPPORTED_FORMATS`] table: a manifest variant is loaded when its
//! `kind`/`format` pair has a row whose file types, GPU needs and decoder this client has: BC
//! KTX2 textures everywhere, and on web `glb-br` models, the original `.glb` byte for byte,
//! fetched as its Brotli copy (`{path}.br`, stored with `Content-Encoding: br`) which the
//! browser decodes. Native leaves `glb-br` to a later change (it has no Brotli decoder on its
//! http client) and loads the original model. `glb` embeds its textures as KTX2 under
//! `KHR_texture_basisu`, which the glTF loader rejects when a file requires it, and is ignored,
//! so its original loads.
//!
//! Preview mode, local scenes (`b64-` ids) and entities served from a loopback or file content
//! server never ask for a manifest; in preview mode that includes the wearables and emotes, so a
//! preview always shows the deployed files.

mod debug_panel;
pub mod ktx2;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex, OnceLock},
};

use bevy::{
    asset::{io::AssetReaderError, AssetLoadFailedEvent},
    image::CompressedImageFormats,
    prelude::*,
    render::renderer::RenderCapabilities,
    tasks::IoTaskPool,
};
use common::util::{TaskCompat, UrlLoopbackExt};
use serde::Deserialize;
use tokio::sync::watch;
use web_time::Duration;

use crate::{
    content_hash,
    download::FetchPolicy,
    ipfs_path::{CacheKey, IpfsPath},
    IpfsIo, IpfsResource, SharedBytes,
};

/// Image paths whose loader unwraps a BC KTX2 (`ktx2::bc_ktx2_dds`) before it decodes, so a
/// texture variant loads at the original path. A path without an extension goes to the last
/// image loader, which does too.
const TEXTURE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "ktx2"];
/// Every variant class the manifest schema (v3) names by a path's prefix letter, taken or not.
const SCHEMA_FORMATS: [&str; 3] = ["ktx2-bc", "glb-br", "glb"];
/// Model paths a `glb-br` variant stands in for: only a binary glTF, which is what it holds.
#[cfg(target_arch = "wasm32")]
const MODEL_EXTENSIONS: [&str; 1] = ["glb"];
const RECENT_FALLBACKS: usize = 20;

/// What a supported variant's bytes are handed to the loaders as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A BC KTX2 texture (BC1, BC5 or BC7, as its header says), which the image loaders unwrap
    /// to the DDS they decode.
    BcTexture,
    /// A `glb-br` model: the original `.glb`, fetched as its Brotli copy that the browser
    /// decodes, handed to the glTF loader of the original path. Web only.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    BrotliGlb,
}

/// A variant class this client takes: its `kind`/`format` (from the path's prefix letter), the
/// content files it may replace, what the GPU must support and what the bytes decode as.
#[derive(Debug)]
pub struct SupportedFormat {
    pub kind: &'static str,
    pub format: &'static str,
    /// Lower-case extensions of the files the variant may stand in for.
    pub extensions: &'static [&'static str],
    /// Whether a file without an extension may be one of them (an image: its loader guesses the
    /// format from the bytes).
    pub without_extension: bool,
    /// The GPU must decode BC textures.
    pub needs_bc: bool,
    pub decoded: Decoded,
}

impl SupportedFormat {
    fn accepts_path(&self, file_path: &str) -> bool {
        match extension(file_path) {
            Some(ext) => self.extensions.contains(&ext.as_str()),
            None => self.without_extension,
        }
    }
}

/// The variant classes this client takes. A variant is loaded only through a row here; the
/// manifest's order of preference decides between rows.
pub const SUPPORTED_FORMATS: &[SupportedFormat] = &[
    SupportedFormat {
        kind: "texture",
        format: "ktx2-bc",
        extensions: &TEXTURE_EXTENSIONS,
        without_extension: true,
        needs_bc: true,
        decoded: Decoded::BcTexture,
    },
    // web only: the browser decodes the Brotli copy; native has no decoder on its http client
    #[cfg(target_arch = "wasm32")]
    SupportedFormat {
        kind: "model",
        format: "glb-br",
        extensions: &MODEL_EXTENSIONS,
        without_extension: false,
        needs_bc: false,
        decoded: Decoded::BrotliGlb,
    },
];

/// The row for a manifest variant's `kind`/`format`, if this client takes it.
pub fn supported_format(variant: &Variant) -> Option<&'static SupportedFormat> {
    SUPPORTED_FORMATS
        .iter()
        .find(|row| row.kind == variant.kind && row.format == variant.format)
}

/// The formats this client takes and the schema's it does not, in the debug panel's words.
pub fn taken_formats() -> String {
    let taken: Vec<_> = SUPPORTED_FORMATS.iter().map(|row| row.format).collect();
    let not_taken: Vec<_> = SCHEMA_FORMATS
        .iter()
        .copied()
        .filter(|format| !taken.contains(format))
        .collect();
    // the formats grouped by what they are handed to the loaders as, in table order
    let mut groups: Vec<(Vec<&str>, &str)> = Vec::new();
    for row in SUPPORTED_FORMATS {
        let how = match row.decoded {
            Decoded::BcTexture => "BC1/BC5/BC7, unwrapped to DDS by the image loaders",
            Decoded::BrotliGlb => "Brotli, decoded by the browser",
        };
        match groups.iter_mut().find(|(_, group_how)| *group_how == how) {
            Some((formats, _)) => formats.push(row.format),
            None => groups.push((vec![row.format], how)),
        }
    }
    let mut words = groups
        .iter()
        .map(|(formats, how)| format!("{} ({how})", formats.join(", ")))
        .collect::<Vec<_>>()
        .join(", ");
    if !not_taken.is_empty() {
        words.push_str("; not ");
        words.push_str(&not_taken.join(", "));
    }
    words
}
/// How long an entity's files wait for its manifest, from the moment it was requested. A file
/// read after that goes to the original at once; a manifest that arrives later still serves
/// the files read after it. Long enough for a first connection to the service (DNS, TLS), and
/// short next to a cold scene load.
pub const MANIFEST_DEADLINE: Duration = Duration::from_secs(2);
/// How long a manifest request may wait for headers, or between body chunks, in its single
/// attempt. Its files stop waiting for it at [`MANIFEST_DEADLINE`] anyway; this bounds how long
/// a host that never answers holds a request slot.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(3);
/// A variant download: one attempt, with short timeouts, since the original is the retry.
const VARIANT_FETCH: FetchPolicy = FetchPolicy {
    headers_timeout: Duration::from_secs(5),
    idle_timeout: Duration::from_secs(5),
    max_attempts: 1,
};
/// Consecutive transport failures (manifest or variant) after which the service is treated as
/// down for the rest of the session, so a dead host costs a few requests, not one per file.
const BREAKER_FAILURES: u32 = 5;

/// One variant of an original: where it is served, and the class its path's prefix letter
/// names. The path also identifies it (failed variants, caches): abgen keeps a path's bytes
/// fixed for the life of its generation.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    /// Relative to the base: `bevy/{prefix}/{name}`.
    pub path: String,
    pub kind: String,
    pub format: String,
}

impl Variant {
    /// The variant at `path`, when it is `bevy/{prefix}/{name}` with plain segments and its
    /// prefix letter is a class the schema names; anything else is dropped.
    pub fn from_path(path: &str) -> Option<Variant> {
        if !is_variant_path(path) {
            return None;
        }
        let prefix = path.split('/').nth(1)?;
        let (kind, format) = match prefix.as_bytes().first()? {
            b't' => ("texture", "ktx2-bc"),
            b'm' => ("model", "glb-br"),
            b'g' => ("model", "glb"),
            _ => return None,
        };
        Some(Variant {
            path: path.to_owned(),
            kind: kind.to_owned(),
            format: format.to_owned(),
        })
    }
}

#[derive(Debug)]
pub struct Manifest {
    /// The generation each variant class was built from (`texture` → `t2`, …), for the debug
    /// panel; the client does not act on it.
    pub lanes: BTreeMap<String, String>,
    /// Variants by original content CID, in the manifest's order of preference.
    pub files: HashMap<String, Vec<Variant>>,
}

#[derive(Deserialize)]
struct ManifestVersion {
    v: u32,
}

#[derive(Deserialize)]
struct ManifestJson {
    #[serde(default)]
    lanes: BTreeMap<String, String>,
    #[serde(default)]
    files: HashMap<String, Vec<String>>,
}

/// The manifest schema version this client reads.
pub const MANIFEST_VERSION: u32 = 3;

/// A v3 manifest: `v`, `lanes`, and original content CID → variant paths. It is fetched at the
/// entity's own url, so it names no entity. Paths that are not `bevy/{prefix}/{name}` with plain
/// segments (a path becomes part of a url) or whose prefix letter names no class are dropped,
/// and so is an original left with none.
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    let version: ManifestVersion = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if version.v != MANIFEST_VERSION {
        return Err(format!("unsupported manifest version {}", version.v));
    }
    let json: ManifestJson = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let files = json
        .files
        .into_iter()
        .map(|(original, paths)| {
            let variants: Vec<_> = paths
                .iter()
                .filter_map(|path| Variant::from_path(path))
                .collect();
            (original, variants)
        })
        .filter(|(_, variants)| !variants.is_empty())
        .collect();
    Ok(Manifest {
        lanes: json.lanes,
        files,
    })
}

/// `bevy/{prefix}/{name}`: the prefix ASCII alphanumeric and not `manifest`, the name ASCII
/// alphanumeric or `_`. No dots, slashes or escapes can reach the url.
fn is_variant_path(path: &str) -> bool {
    let mut segments = path.split('/');
    let (Some("bevy"), Some(prefix), Some(name), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    path.len() <= 256
        && prefix != "manifest"
        && !prefix.is_empty()
        && prefix.bytes().all(|b| b.is_ascii_alphanumeric())
        && !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn extension(file_path: &str) -> Option<String> {
    Path::new(file_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
}

/// Whether a file of this type could be replaced by any supported format; others never wait on
/// a manifest.
pub fn could_have_variant(file_path: &str) -> bool {
    SUPPORTED_FORMATS
        .iter()
        .any(|row| row.accepts_path(file_path))
}

/// Whether this client can load `variant` at `file_path`: a supported format, for a file type
/// it may replace, on a GPU that has what it needs (`bc`: BC texture support).
pub fn supports(variant: &Variant, file_path: &str, bc: bool) -> bool {
    supported_format(variant)
        .is_some_and(|row| (bc || !row.needs_bc) && row.accepts_path(file_path))
}

/// The first variant this client supports that has not failed this session.
pub fn select_variant<'a>(
    variants: &'a [Variant],
    file_path: &str,
    bc: bool,
    is_bad: impl Fn(&str) -> bool,
) -> Option<&'a Variant> {
    variants
        .iter()
        .find(|variant| supports(variant, file_path, bc) && !is_bad(&variant.path))
}

/// Why an entity (scene, wearable or emote) gets no manifest request, if it doesn't.
pub fn scene_eligibility(
    entity: &str,
    contents_base_url: Option<&str>,
    preview: bool,
) -> Result<(), &'static str> {
    if preview {
        return Err("preview mode");
    }
    if entity.starts_with("b64-") {
        return Err("local scene (b64- id)");
    }
    if entity.is_empty() || !entity.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("entity id is not a content id");
    }
    let Some(url) = contents_base_url else {
        return Err("no content server");
    };
    let Ok(url) = url::Url::parse(url) else {
        return Err("unparseable content server url");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err("not an http content server");
    }
    if url.is_loopback() {
        return Err("loopback content server");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub enum ManifestState {
    Pending,
    Ineligible(&'static str),
    /// 404: abgen has no manifest for the entity (yet).
    None,
    Loaded(Arc<Manifest>),
    Failed(String),
}

#[derive(Clone, Debug)]
struct ServedVariant {
    scene: String,
    file: String,
    /// The variant's path, its identity.
    variant: String,
    format: String,
    /// Where its bytes came from, the key of their cached copy.
    url: String,
}

#[derive(Debug)]
struct Fallback {
    scene: String,
    file: String,
    reason: String,
}

#[derive(Default)]
struct Session {
    disabled: Option<String>,
    transport_failures: u32,
    bad: HashSet<String>,
    served: HashMap<PathBuf, ServedVariant>,
    downloaded: u64,
    cached: u64,
    /// [downloaded, from cache] by variant format.
    served_by_format: BTreeMap<String, [u64; 2]>,
    fallbacks: u64,
    recent: VecDeque<Fallback>,
}

impl Session {
    fn fallback(&mut self, scene: &str, file: &str, reason: String) {
        debug!("bevy variant fallback for `{file}` in {scene}: {reason}");
        self.fallbacks += 1;
        if self.recent.len() == RECENT_FALLBACKS {
            self.recent.pop_front();
        }
        self.recent.push_back(Fallback {
            scene: scene.to_owned(),
            file: file.to_owned(),
            reason,
        });
    }
}

#[derive(Default)]
pub struct BevyVariants {
    base: OnceLock<String>,
    /// The GPU's BC support, once the renderer has reported it.
    bc: OnceLock<bool>,
    manifests: Mutex<HashMap<String, watch::Receiver<ManifestState>>>,
    /// When each entity's manifest was requested, for [`MANIFEST_DEADLINE`].
    requested_at: Mutex<HashMap<String, web_time::Instant>>,
    session: Mutex<Session>,
}

impl BevyVariants {
    fn base(&self) -> Option<&str> {
        self.base.get().map(String::as_str)
    }

    fn bc(&self) -> bool {
        self.bc.get().copied().unwrap_or(false)
    }

    fn set_base(&self, url: &str) {
        let url = url.trim().trim_end_matches('/');
        match url::Url::parse(url) {
            Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {
                if self.base.set(url.to_owned()).is_ok() {
                    info!("bevy variants: on, from `{url}`");
                }
            }
            _ => warn!("bevy variants: ignoring `{url}`, not an http(s) url"),
        }
    }

    /// A new pending manifest for `entity`, or None when one was already requested.
    fn begin(&self, entity: &str) -> Option<watch::Sender<ManifestState>> {
        let mut manifests = self.manifests.lock().unwrap();
        if manifests.contains_key(entity) {
            return None;
        }
        let (sender, receiver) = watch::channel(ManifestState::Pending);
        manifests.insert(entity.to_owned(), receiver);
        self.requested_at
            .lock()
            .unwrap()
            .insert(entity.to_owned(), web_time::Instant::now());
        Some(sender)
    }

    /// How much of [`MANIFEST_DEADLINE`] an entity's files may still wait for its manifest.
    fn manifest_wait_left(&self, entity: &str) -> Duration {
        self.requested_at
            .lock()
            .unwrap()
            .get(entity)
            .map_or(MANIFEST_DEADLINE, |at| {
                MANIFEST_DEADLINE.saturating_sub(at.elapsed())
            })
    }

    fn manifest(&self, entity: &str) -> Option<watch::Receiver<ManifestState>> {
        self.manifests.lock().unwrap().get(entity).cloned()
    }

    fn disabled(&self) -> Option<String> {
        self.session.lock().unwrap().disabled.clone()
    }

    fn transport_ok(&self) {
        self.session.lock().unwrap().transport_failures = 0;
    }

    fn transport_failed(&self, reason: &str) {
        let mut session = self.session.lock().unwrap();
        session.transport_failures += 1;
        if session.transport_failures >= BREAKER_FAILURES && session.disabled.is_none() {
            warn!("bevy variants: off for this session, {BREAKER_FAILURES} failed requests ({reason})");
            session.disabled = Some(format!(
                "{BREAKER_FAILURES} consecutive failed requests, last: {reason}"
            ));
        }
    }

    fn is_bad(&self, variant: &str) -> bool {
        self.session.lock().unwrap().bad.contains(variant)
    }

    fn fail(&self, scene: &str, file: &str, variant: Option<&str>, reason: String) {
        let mut session = self.session.lock().unwrap();
        if let Some(variant) = variant {
            session.bad.insert(variant.to_owned());
        }
        session.fallback(scene, file, reason);
    }

    fn served(&self, path: &Path, served: ServedVariant, cached: bool) {
        let mut session = self.session.lock().unwrap();
        if cached {
            session.cached += 1;
        } else {
            session.downloaded += 1;
        }
        session
            .served_by_format
            .entry(served.format.clone())
            .or_default()[usize::from(cached)] += 1;
        session.served.insert(path.to_owned(), served);
    }

    /// Whether the asset at `path` was last read from a variant.
    pub fn is_served(&self, path: &Path) -> bool {
        self.session.lock().unwrap().served.contains_key(path)
    }

    /// Marks the variant served for `path` bad, if there was one, and returns the url of its
    /// cached copy: the asset should be reloaded (from its original), and the copy dropped.
    fn decode_failed(&self, path: &Path, error: &str) -> Option<String> {
        let mut session = self.session.lock().unwrap();
        let served = session.served.remove(path)?;
        session.bad.insert(served.variant.clone());
        session.fallback(
            &served.scene,
            &served.file,
            format!("{} variant failed to load: {error}", served.format),
        );
        Some(served.url)
    }
}

/// What the loader reads for a variant (a BC texture's KTX2, a model's own bytes), and whether
/// it came from the cache.
pub(crate) struct VariantRead {
    pub(crate) bytes: Vec<u8>,
    pub(crate) cached: bool,
}

fn describe_fetch_error(error: &platform::FetchError<anyhow::Error>) -> String {
    match error {
        platform::FetchError::Headers => "timed out awaiting headers".to_owned(),
        platform::FetchError::Stalled => "stalled".to_owned(),
        platform::FetchError::Send(e) => format!("request failed: {e}"),
        platform::FetchError::Status(status, _) => format!("status {status}"),
        platform::FetchError::Body(e) => format!("body stream error: {e}"),
    }
}

impl IpfsIo {
    /// Turns the variants client on for the session. Startup only: a second call is ignored.
    pub fn set_bevy_variants_base(&self, url: &str) {
        self.bevy_variants.set_base(url);
    }

    /// Whether the asset at `path` was last read from a variant (and so must not be
    /// reprocessed as if it were the original).
    pub fn served_bevy_variant(&self, path: &Path) -> bool {
        self.bevy_variants.is_served(path)
    }

    /// Drops the cached copy of the variant fetched from `url`: it was rejected, and must not be
    /// read again in a later session.
    async fn forget_variant(&self, url: &str) {
        #[cfg(not(target_arch = "wasm32"))]
        let forgotten = self
            .forget_disk_cache(&CacheKey::Url(content_hash::url_cache_name(url)))
            .await
            .map_err(|e| e.to_string());
        #[cfg(target_arch = "wasm32")]
        let forgotten = crate::web_cache::forget(url)
            .await
            .map_err(|e| format!("{e:?}"));
        if let Err(error) = forgotten {
            warn!("failed to drop the cached copy of `{url}`: {error}");
        }
    }

    /// Asks abgen for an entity's manifest (a scene, wearable or emote), once per session. A
    /// no-op when variants are off.
    pub fn request_bevy_variants(self: &Arc<Self>, entity: &str) {
        let Some(base) = self.bevy_variants.base().map(str::to_owned) else {
            return;
        };
        let Some(sender) = self.bevy_variants.begin(entity) else {
            return;
        };
        let io = self.clone();
        let entity = entity.to_owned();
        IoTaskPool::get()
            .spawn_compat(async move {
                let state = io.fetch_bevy_variants_manifest(&base, &entity).await;
                sender.send_replace(state);
            })
            .detach();
    }

    async fn fetch_bevy_variants_manifest(&self, base: &str, entity: &str) -> ManifestState {
        let contents = self.context.read().await.contents_base_url(entity);
        if let Err(reason) = scene_eligibility(entity, contents.as_deref(), self.is_preview) {
            return ManifestState::Ineligible(reason);
        }
        if let Some(reason) = self.bevy_variants.disabled() {
            return ManifestState::Failed(format!("variants off: {reason}"));
        }
        let url = format!("{base}/bevy/manifest/{entity}.json");
        let request = match self.client.get(&url).build() {
            Ok(request) => request,
            Err(e) => return ManifestState::Failed(e.to_string()),
        };
        match self
            .fetch_request_once(
                request,
                self.client.clone(),
                MANIFEST_TIMEOUT,
                MANIFEST_TIMEOUT,
                Some(entity.to_owned()),
            )
            .await
        {
            Ok(response) => {
                self.bevy_variants.transport_ok();
                match parse_manifest(&response.body) {
                    Ok(manifest) => ManifestState::Loaded(Arc::new(manifest)),
                    Err(e) => ManifestState::Failed(format!("invalid manifest: {e}")),
                }
            }
            Err(platform::FetchError::Status(status, _))
                if status == reqwest::StatusCode::NOT_FOUND =>
            {
                self.bevy_variants.transport_ok();
                ManifestState::None
            }
            Err(e) => {
                let reason = describe_fetch_error(&e);
                self.bevy_variants.transport_failed(&reason);
                ManifestState::Failed(reason)
            }
        }
    }

    /// The variant's bytes for an entity's content file, or None to read the original.
    pub(crate) async fn read_bevy_variant(
        self: &Arc<Self>,
        ipfs_path: &IpfsPath,
        path: &Path,
    ) -> Option<VariantRead> {
        let variants = &self.bevy_variants;
        let base = variants.base()?;
        let (scene, file) = (ipfs_path.scene_hash()?, ipfs_path.content_path()?);
        if !could_have_variant(file) {
            return None;
        }
        let mut manifest = variants.manifest(scene)?;
        let state = manifest.borrow().clone();
        let state = if matches!(state, ManifestState::Pending) {
            // wait for the manifest, but only until its deadline: a slow service must not
            // hold the entity's files back
            let left = variants.manifest_wait_left(scene);
            let arrived = if left.is_zero() {
                None
            } else {
                platform::within(left, async {
                    manifest
                        .wait_for(|state| !matches!(state, ManifestState::Pending))
                        .await
                        .ok()
                        .map(|state| state.clone())
                })
                .await
            };
            match arrived {
                Some(state) => state?,
                None => {
                    variants.fail(
                        scene,
                        file,
                        None,
                        format!(
                            "the manifest was not there within {}s",
                            MANIFEST_DEADLINE.as_secs()
                        ),
                    );
                    return None;
                }
            }
        } else {
            state
        };
        let ManifestState::Loaded(manifest) = state else {
            return None;
        };
        let original = self
            .context
            .read()
            .await
            .entities
            .get(scene)?
            .collection
            .hash(file)?
            .into_owned();
        let options = manifest.files.get(&original)?;
        if variants.disabled().is_some() {
            return None;
        }
        let bc = variants.bc();
        if !options.iter().any(|variant| supports(variant, file, bc)) {
            let formats: Vec<_> = options.iter().map(|v| v.format.as_str()).collect();
            // a format this file type would take on a GPU with BC textures
            let wants_bc = !bc && options.iter().any(|variant| supports(variant, file, true));
            let reason = if wants_bc {
                "the GPU has no BC texture support (or has not reported it yet)".to_owned()
            } else {
                format!(
                    "no variant format this file type takes ({})",
                    formats.join(", ")
                )
            };
            variants.fail(scene, file, None, reason);
            return None;
        }
        let variant = select_variant(options, file, bc, |path| variants.is_bad(path))?.clone();

        let decoded = supported_format(&variant)?.decoded;

        // the manifest names no hash for a variant, so its bytes are cached under a digest of
        // the url they came from, never under a content hash
        // a `glb-br` variant is fetched as its Brotli copy: a CDN does not choose it by
        // `Accept-Encoding`, so it is asked for by name and the browser decodes it
        let url = match decoded {
            Decoded::BcTexture => format!("{base}/{}", variant.path),
            Decoded::BrotliGlb => format!("{base}/{}.br", variant.path),
        };
        let key = CacheKey::Url(content_hash::url_cache_name(&url));
        let served = ServedVariant {
            scene: scene.to_owned(),
            file: file.to_owned(),
            variant: variant.path.clone(),
            format: variant.format.clone(),
            url: url.clone(),
        };

        #[cfg(not(target_arch = "wasm32"))]
        let cached = self.read_disk_cache(&key).await.ok().flatten();
        #[cfg(target_arch = "wasm32")]
        let cached = crate::web_cache::read(&url).await;
        let (fetched, from_cache) = match cached {
            Some(bytes) => (SharedBytes(Arc::new(bytes)), true),
            None => {
                let token = self.reqno.fetch_add(1, Ordering::SeqCst);
                let (download, task) = self.share_download_with(
                    key,
                    token,
                    path.to_owned(),
                    url.clone(),
                    Some(scene.to_owned()),
                    VARIANT_FETCH,
                );
                if let Some(task) = task {
                    IoTaskPool::get().spawn_compat(task).detach();
                }
                let mut result = download.result();
                let fetched = result
                    .wait_for(Option::is_some)
                    .await
                    .ok()
                    .and_then(|result| result.clone());
                match fetched {
                    Some(Ok(bytes)) => {
                        variants.transport_ok();
                        (bytes, false)
                    }
                    Some(Err(AssetReaderError::Io(e))) if e.kind() == ErrorKind::Interrupted => {
                        return None
                    }
                    Some(Err(e)) => {
                        let reason = e.to_string();
                        variants.transport_failed(&reason);
                        variants.fail(scene, file, Some(&variant.path), format!("fetch: {reason}"));
                        return None;
                    }
                    None => return None,
                }
            }
        };

        // checks that need no decoding; a copy that fails them is dropped from the cache too,
        // so a later session does not read it again
        let rejected = match decoded {
            // the loaders unwrap the KTX2 (`ktx2::bc_ktx2_dds`), inside the asset load; here
            // only its header is read, so a file that is not a BC KTX2 at all falls back
            // before it reaches them
            Decoded::BcTexture => ktx2::bc_format(&fetched)
                .err()
                .map(|reason| format!("invalid KTX2: {reason}")),
            // the browser hands over the decoded bytes, which are the original `.glb` byte for
            // byte: checked against the original's own content hash, a truncated or corrupt
            // copy falls back here, and one that passes is the original, so a glTF loader
            // failure on it is the original's own and needs no reload
            Decoded::BrotliGlb => {
                (!content_hash::matches_content_hash(&original, &fetched)).then(|| {
                    format!(
                        "{} bytes from the Brotli copy are not the original `{original}`",
                        fetched.len()
                    )
                })
            }
        };
        if let Some(reason) = rejected {
            self.forget_variant(&served.url).await;
            variants.fail(scene, file, Some(&variant.path), reason);
            return None;
        }
        let bytes = Arc::try_unwrap(fetched.0).unwrap_or_else(|shared| shared.as_ref().clone());
        variants.served(path, served, from_cache);
        Some(VariantRead {
            bytes,
            cached: from_cache,
        })
    }
}

pub(crate) struct BevyVariantsPlugin;

impl Plugin for BevyVariantsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, detect_gpu_formats);
        app.add_plugins(debug_panel::BevyVariantsDebugPanelPlugin);
    }

    /// After the asset plugins exist: an app without images has nothing to fall back.
    fn finish(&self, app: &mut App) {
        if app
            .world()
            .contains_resource::<Events<AssetLoadFailedEvent<Image>>>()
        {
            app.add_systems(Update, fall_back_from_failed_variants);
        }
    }
}

fn detect_gpu_formats(
    capabilities: Option<Res<RenderCapabilities>>,
    ipfs: Res<IpfsResource>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(capabilities) = capabilities else {
        return;
    };
    *done = true;
    let bc = CompressedImageFormats::from_features(capabilities.features())
        .contains(CompressedImageFormats::BC);
    let _ = ipfs.bevy_variants.bc.set(bc);
}

/// Reloads from its original each texture whose variant failed to load.
fn fall_back_from_failed_variants(
    mut images: EventReader<AssetLoadFailedEvent<Image>>,
    ipfs: Res<IpfsResource>,
    server: Res<AssetServer>,
) {
    for event in images.read() {
        if let Some(url) = ipfs
            .bevy_variants
            .decode_failed(event.path.path(), &event.error.to_string())
        {
            server.reload(event.path.without_label().into_owned());
            let io = ipfs.clone();
            IoTaskPool::get()
                .spawn_compat(async move { io.forget_variant(&url).await })
                .detach();
        }
    }
}
