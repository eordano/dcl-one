use crate::ux::{self, TrySteps, UserError, write_error};
use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ProjectKind {
    /// `scene-template` is upstream's name for it.
    #[value(alias = "scene-template")]
    Scene,
    SmartWearable,
    /// A scene manifest flagged `isPortableExperience`.
    PxTemplate,
    /// An SDK library a scene imports, previewable as a scene.
    Library,
}

impl ProjectKind {
    fn label(self) -> &'static str {
        match self {
            ProjectKind::Scene => "scene",
            ProjectKind::SmartWearable => "smart wearable",
            ProjectKind::PxTemplate => "portable experience",
            ProjectKind::Library => "library",
        }
    }
}

pub struct InitOptions {
    pub dir: PathBuf,
    pub project: Option<ProjectKind>,
    pub yes: bool,
    pub node_modules_only: bool,
}

/// `init --template` / `--github-repo`: a project unpacked from elsewhere.
pub enum RemoteSource {
    /// A zip by http(s) URL or local path, with one top-level folder.
    Zip(String),
    /// `https://github.com/<owner>/<repo>` or `…/tree/<branch>/<path>`.
    GitHub(String),
}

pub struct RemoteInitOptions {
    pub dir: PathBuf,
    pub yes: bool,
    pub skip_install: bool,
    pub source: RemoteSource,
}

pub struct FileSpec {
    pub rel: &'static str,
    pub body: Vec<u8>,
}

const SCENE_SCENE_JSON: &str = include_str!("templates/init/scene/scene.json");
const SCENE_PACKAGE_JSON: &str = include_str!("templates/init/scene/package.json");
const SCENE_TSCONFIG: &str = include_str!("templates/init/scene/tsconfig.json");
const SCENE_INDEX_TS: &str = include_str!("templates/init/scene/index.ts");
const SCENE_GITIGNORE: &str = include_str!("templates/init/scene/gitignore");
const SCENE_DCLIGNORE: &str = include_str!("templates/init/scene/dclignore");
const SCENE_README: &str = include_str!("templates/init/scene/README.md");
const SCENE_THUMBNAIL: &[u8] = include_bytes!("templates/init/scene/scene-thumbnail.png");
const SW_WEARABLE_JSON: &str = include_str!("templates/init/smart-wearable/wearable.json");
const SW_SCENE_JSON: &str = include_str!("templates/init/smart-wearable/scene.json");
const SW_PACKAGE_JSON: &str = include_str!("templates/init/smart-wearable/package.json");
const SW_INDEX_TS: &str = include_str!("templates/init/smart-wearable/index.ts");
const SW_README: &str = include_str!("templates/init/smart-wearable/README.md");
const SW_MODEL_GLB: &[u8] = include_bytes!("templates/init/smart-wearable/model.glb");
const SW_THUMBNAIL: &[u8] = include_bytes!("templates/init/smart-wearable/thumbnail.png");
const PX_SCENE_JSON: &str = include_str!("templates/init/px/scene.json");
const LIB_PACKAGE_JSON: &str = include_str!("templates/init/library/package.json");
const LIB_TSCONFIG: &str = include_str!("templates/init/library/tsconfig.json");
const LIB_INDEX_TS: &str = include_str!("templates/init/library/index.ts");
const LIB_README: &str = include_str!("templates/init/library/README.md");
const VENDORED_NODE_MODULES: &[u8] = include_bytes!("vendor/node_modules.zip");

const INSTALLED_NOTE: &str = "Installed node_modules from the vendored SDK — no npm needed";
const KEPT_NOTE: &str = "node_modules was installed by another tool and is kept — nothing to do (remove node_modules, then run this command again, to get the vendored SDK)";

/// The `@dcl/sdk` the vendored node_modules ships: the scene scaffold's pin,
/// which `blob_tracks_the_scaffold_pin_and_carries_the_ecs_fixes` holds equal
/// to the archive's manifest.
fn vendored_sdk_version() -> String {
    serde_json::from_str::<serde_json::Value>(SCENE_PACKAGE_JSON)
        .ok()
        .and_then(|scaffold| {
            scaffold["devDependencies"]["@dcl/sdk"]
                .as_str()
                .map(str::to_owned)
        })
        .expect("the scene scaffold pins @dcl/sdk")
}

/// The version of `package` installed under `root`, from its manifest.
fn installed_version(root: &Path, package: &str) -> Option<String> {
    manifest_version(
        &std::fs::read(root.join("node_modules").join(package).join("package.json")).ok()?,
    )
}

/// The `version` of a package.json.
fn manifest_version(raw: &[u8]) -> Option<String> {
    let manifest: serde_json::Value = serde_json::from_slice(raw).ok()?;
    manifest["version"].as_str().map(str::to_owned)
}

pub fn init(opts: &InitOptions) -> Result<()> {
    init_with(opts, false)
}

/// [`init`], or with `skip_install` the same scaffold without node_modules
/// (`--skip-install`).
pub fn init_with(opts: &InitOptions, skip_install: bool) -> Result<()> {
    if opts.node_modules_only {
        return install_node_modules(&opts.dir).map(drop);
    }
    let root = prepare_dir(&opts.dir, opts.yes)?;
    let kind = resolve_kind(opts.project)?;
    let title = project_title(&root);
    let files = scaffold_files(kind, &title);
    for f in &files {
        write_file(&root, f.rel, &f.body)?;
    }
    let mut steps = ux::Steps::new(3);
    steps.done(format!(
        "Scaffolded a {} project in {} ({} files)",
        kind.label(),
        display_dir(&opts.dir),
        files.len()
    ));
    steps.done(node_modules_step(&root, skip_install)?);
    steps.done("Next steps:");
    if opts.dir != Path::new(".") {
        ux::note(format!("  cd {}", display_dir(&opts.dir)));
    }
    ux::note("  dcl-scene-cli start");
    match kind {
        ProjectKind::Scene | ProjectKind::PxTemplate => {
            ux::note("  dcl-scene-cli deploy   when you are ready to publish");
        }
        ProjectKind::Library => {
            ux::note(
                "  npm run build:lib      bundles src/index.ts beside its source and writes dist/*.d.ts",
            );
        }
        ProjectKind::SmartWearable => {
            ux::note(
                "  replace the placeholder model.glb and thumbnail.png (256x256, transparent background) — wearable.json references them",
            );
            ux::note(
                "  dcl-scene-cli pack     to produce smart-wearable.zip when you are ready to publish",
            );
        }
    }
    Ok(())
}

/// `init --node-modules-only` without `--missing-only`: what
/// [`refresh_vendored_node_modules`] did. `installed` has the packages
/// written, and none when the command wrote nothing. `kept` is empty: this
/// mode does not list the packages of a folder that it keeps.
pub fn install_node_modules(dir: &Path) -> Result<NodeModulesInstall> {
    let root = existing_dir(dir)?;
    let stopped_extraction = extraction_stopped(&root);
    let pin = vendored_sdk_version();
    let (note, installed) = match refresh_vendored_node_modules(&root)? {
        Refresh::Extracted(packages) => (INSTALLED_NOTE.to_string(), packages),
        Refresh::Replaced {
            installed,
            packages,
        } => (
            format!(
                "Reinstalled {} in node_modules from the vendored SDK ({installed} replaced by @dcl/sdk {pin}) — no npm needed",
                packages.join(", ")
            ),
            packages,
        ),
        Refresh::Current => (
            format!("node_modules already holds the vendored @dcl/sdk {pin} — nothing to do"),
            Vec::new(),
        ),
        Refresh::Kept => (KEPT_NOTE.to_string(), Vec::new()),
    };
    ux::Steps::new(1).done(note);
    Ok(NodeModulesInstall {
        dir: root,
        dry_run: false,
        installed,
        kept: Vec::new(),
        stopped_extraction,
    })
}

/// The folder `init --node-modules-only` installs into.
fn existing_dir(dir: &Path) -> Result<PathBuf> {
    dunce::canonicalize(dir).map_err(|e| {
        UserError::new(
            format!("cannot resolve the target directory {}", dir.display()),
            TrySteps::one("run from inside the scene, or pass --dir <scene>"),
        )
        .caused_by(e)
        .into()
    })
}

fn node_modules_step(root: &Path, skip_install: bool) -> Result<&'static str> {
    Ok(match skip_install {
        true => "Skipped node_modules (--skip-install)",
        false => match install_vendored_node_modules(root)? {
            Some(_) => INSTALLED_NOTE,
            None => "Kept the existing node_modules",
        },
    })
}

/// A package.json dependency that node_modules does not have.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct MissingDependency {
    pub name: String,
    /// What package.json asks for: a version range, a tag, a URL or a path.
    /// `None` when the value is not a string.
    pub range: Option<String>,
}

/// What `init --template` or `--github-repo` made.
pub struct RemoteInit {
    /// The project folder.
    pub root: PathBuf,
    /// package.json dependencies the vendored SDK did not install, sorted by
    /// name; `None` under `--skip-install`, which leaves no node_modules to
    /// compare with.
    pub missing: Option<Vec<MissingDependency>>,
}

pub async fn init_remote(opts: &RemoteInitOptions) -> Result<RemoteInit> {
    let root = prepare_dir(&opts.dir, opts.yes)?;
    let mut steps = ux::Steps::new(3);
    let (bytes, inner, label) = match &opts.source {
        RemoteSource::Zip(from) => (fetch_zip(from).await?, None, from.clone()),
        RemoteSource::GitHub(url) => {
            let repo = GitHubRef::parse(url)?;
            (repo.fetch_zip().await?, repo.path.clone(), url.clone())
        }
    };
    steps.done(format!(
        "Fetched {label} ({})",
        crate::deploy::human_size(bytes.len() as u64)
    ));
    let written = unpack_template(&bytes, &root, inner.as_deref())?;
    steps.done(format!(
        "Unpacked {written} file{} into {}",
        crate::build::plural(written as u64),
        display_dir(&opts.dir)
    ));
    steps.done(node_modules_step(&root, opts.skip_install)?);
    let missing = (!opts.skip_install).then(|| missing_dependencies(&root));
    if let Some(missing) = missing.as_ref().filter(|missing| !missing.is_empty()) {
        let names: Vec<&str> = missing.iter().map(|m| m.name.as_str()).collect();
        ux::note(format!(
            "the project depends on packages the vendored SDK does not carry: {} \u{2014} npm install adds them",
            names.join(", ")
        ));
    }
    ux::note("Next steps:");
    if opts.dir != Path::new(".") {
        ux::note(format!("  cd {}", display_dir(&opts.dir)));
    }
    ux::note("  dcl-scene-cli start");
    Ok(RemoteInit { root, missing })
}

/// A local zip, or one downloaded from an http(s) URL.
async fn fetch_zip(from: &str) -> Result<Vec<u8>> {
    if from.starts_with("http://") || from.starts_with("https://") {
        return download(from).await;
    }
    let path = Path::new(from);
    if path.is_file() {
        return std::fs::read(path).with_context(|| format!("reading {}", path.display()));
    }
    Err(UserError::new(
        format!("--template {from} is neither a file nor an http(s) URL"),
        TrySteps::one("pass a zip with one top-level folder: a path, or a URL such as https://github.com/<owner>/<repo>/archive/refs/heads/main.zip"),
    )
    .into())
}

/// The most a template download may hold, and the most it may unpack to.
const DOWNLOAD_LIMIT: u64 = 256 * 1024 * 1024;
const UNPACKED_LIMIT: u64 = 1024 * 1024 * 1024;
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// The longest silence of a download in flight.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn too_large(what: &str, limit: u64) -> anyhow::Error {
    UserError::new(
        format!("{what} is larger than {} MB", limit / (1024 * 1024)),
        TrySteps::one("use a smaller template, or download and unzip it yourself"),
    )
    .into()
}

/// An https URL stays https through every redirect; a stalled server and a
/// body past [`DOWNLOAD_LIMIT`] end the download.
async fn download(url: &str) -> Result<Vec<u8>> {
    let https = url.starts_with("https://");
    if !https {
        ux::note_absent(format!(
            "{url} is plain http: the template can be changed on the network, and its scripts.build runs on the next build"
        ));
    }
    let client = reqwest::Client::builder()
        .user_agent("dcl-scene-cli")
        .https_only(https)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .context("building the http client")?;
    let response = client.get(url).send().await.map_err(|e| {
        UserError::new(
            format!("could not download {url}"),
            TrySteps::one("check the URL and the network connection"),
        )
        .caused_by(e)
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(UserError::new(
            format!("{url} answered HTTP {}", status.as_u16()),
            TrySteps::one("check the URL: a GitHub zip is https://github.com/<owner>/<repo>/archive/refs/heads/<branch>.zip"),
        )
        .into());
    }
    let mut response = response;
    let mut bytes = Vec::new();
    if response
        .content_length()
        .is_some_and(|n| n > DOWNLOAD_LIMIT)
    {
        return Err(too_large(url, DOWNLOAD_LIMIT));
    }
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading the download from {url}"))?
    {
        if (bytes.len() + chunk.len()) as u64 > DOWNLOAD_LIMIT {
            return Err(too_large(url, DOWNLOAD_LIMIT));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// `https://github.com/<owner>/<repo>[/tree/<branch>[/<path>]]`.
struct GitHubRef {
    owner: String,
    repo: String,
    branch: Option<String>,
    path: Option<String>,
}

impl GitHubRef {
    fn parse(url: &str) -> Result<Self> {
        let bad = || {
            UserError::new(
                format!("{url} is not a GitHub repository URL"),
                TrySteps::one(
                    "expected https://github.com/<owner>/<repo>, or https://github.com/<owner>/<repo>/tree/<branch>/<path> for one folder of it",
                ),
            )
        };
        let rest = url
            .trim()
            .trim_end_matches('/')
            .strip_prefix("https://github.com/")
            .or_else(|| {
                url.trim()
                    .trim_end_matches('/')
                    .strip_prefix("http://github.com/")
            })
            .ok_or_else(bad)?;
        let mut parts = rest.splitn(5, '/');
        let owner = parts.next().filter(|s| !s.is_empty()).ok_or_else(bad)?;
        let repo = parts
            .next()
            .filter(|s| !s.is_empty())
            .map(|r| r.trim_end_matches(".git"))
            .ok_or_else(bad)?;
        let (branch, path) = match (parts.next(), parts.next(), parts.next()) {
            (None, _, _) => (None, None),
            (Some("tree"), Some(branch), path) if !branch.is_empty() => (
                Some(branch.to_string()),
                path.map(|p| p.trim_matches('/').to_string())
                    .filter(|p| !p.is_empty()),
            ),
            _ => return Err(bad().into()),
        };
        Ok(GitHubRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            branch,
            path,
        })
    }

    /// The archive of the branch; a URL without one tries main, then master.
    fn zip_urls(&self) -> Vec<String> {
        let branches: Vec<&str> = match &self.branch {
            Some(b) => vec![b.as_str()],
            None => vec!["main", "master"],
        };
        branches
            .into_iter()
            .map(|b| {
                format!(
                    "https://codeload.github.com/{}/{}/zip/refs/heads/{b}",
                    self.owner, self.repo
                )
            })
            .collect()
    }

    async fn fetch_zip(&self) -> Result<Vec<u8>> {
        let mut last = None;
        for url in self.zip_urls() {
            match download(&url).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("at least one archive URL"))
    }
}

/// Unpacks the zip's one top-level folder (or `inner` within it) into `root`,
/// without its `node_modules`; the number of files written.
fn unpack_template(bytes: &[u8], root: &Path, inner: Option<&str>) -> Result<usize> {
    unpack_template_within(bytes, root, inner, UNPACKED_LIMIT)
}

/// As [`unpack_template`], refusing once the files written pass `limit`
/// bytes: the sizes a zip declares are not trusted.
fn unpack_template_within(
    bytes: &[u8],
    root: &Path,
    inner: Option<&str>,
    limit: u64,
) -> Result<usize> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        UserError::new(
            "the template is not a zip archive",
            TrySteps::one("--template takes a zip; a GitHub branch is https://github.com/<owner>/<repo>/archive/refs/heads/<branch>.zip"),
        )
        .caused_by(e)
    })?;
    let names: Vec<String> = archive
        .file_names()
        .map(|n| n.map(|n| n.into_owned()))
        .collect::<std::result::Result<_, _>>()
        .context("reading the zip's file names")?;
    let tops: std::collections::BTreeSet<&str> = names
        .iter()
        .filter(|n| !n.starts_with("__MACOSX/"))
        .map(|n| n.split('/').next().unwrap_or(n.as_str()))
        .collect();
    let [top] = tops.iter().copied().collect::<Vec<_>>()[..] else {
        return Err(UserError::new(
            format!(
                "the template zip must hold exactly one top-level folder; this one has {}",
                tops.len()
            ),
            TrySteps::one("zip the project folder itself, not the files inside it"),
        )
        .into());
    };
    let prefix = match inner {
        Some(path) => format!("{top}/{path}/"),
        None => format!("{top}/"),
    };
    let mut written = 0;
    let mut unpacked = 0u64;
    for name in &names {
        let Some(rel) = name.strip_prefix(&prefix) else {
            continue;
        };
        if rel.is_empty() || rel.ends_with('/') {
            continue;
        }
        let rel_path = Path::new(rel);
        if rel_path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(anyhow::anyhow!(
                "the zip entry {name} would escape the project folder"
            ));
        }
        // Later commands run the packages in node_modules under node: only
        // the vendored archive supplies them, never a template.
        let top = rel_path.iter().next();
        if top.is_some_and(|top| top.eq_ignore_ascii_case("node_modules")) {
            continue;
        }
        let dest = root.join(rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| write_error(parent, e))?;
        }
        let mut entry = archive
            .by_name(name)
            .with_context(|| format!("reading {name} from the zip"))?;
        let mut out = std::fs::File::create(&dest).map_err(|e| write_error(&dest, e))?;
        let room = limit - unpacked;
        unpacked += std::io::copy(&mut std::io::Read::take(&mut entry, room + 1), &mut out)
            .map_err(|e| write_error(&dest, e))?;
        if unpacked > limit {
            return Err(too_large("the unpacked template", limit));
        }
        written += 1;
    }
    if written == 0 {
        return Err(UserError::new(
            match inner {
                Some(path) => format!("the repository has no files under {path}"),
                None => "the template zip holds no files".to_string(),
            },
            TrySteps::one("check the folder path in the URL"),
        )
        .into());
    }
    Ok(written)
}

/// package.json dependencies with no folder under node_modules, each once
/// and sorted by name. A package that `dependencies` and `devDependencies`
/// both name has the range of `dependencies`.
fn missing_dependencies(root: &Path) -> Vec<MissingDependency> {
    let Ok(bytes) = std::fs::read(root.join("package.json")) else {
        return Vec::new();
    };
    let Ok(package) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let mut missing = std::collections::BTreeMap::new();
    for deps in ["dependencies", "devDependencies"]
        .iter()
        .filter_map(|section| package[section].as_object())
    {
        for (name, range) in deps {
            let installed = root
                .join("node_modules")
                .join(name)
                .join("package.json")
                .is_file();
            if !installed {
                missing
                    .entry(name.clone())
                    .or_insert_with(|| range.as_str().map(str::to_owned));
            }
        }
    }
    missing
        .into_iter()
        .map(|(name, range)| MissingDependency { name, range })
        .collect()
}

static RESTORE_NODE_MODULES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// `--skip-install`: a build never writes node_modules.
pub fn set_restore_node_modules(on: bool) {
    RESTORE_NODE_MODULES.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// A scene with no node_modules at all (a fresh clone, a copied folder) gets
/// the vendored SDK before the built-in build looks for it, and a line saying
/// so; so does one whose extraction stopped half way. Any other node_modules
/// is never touched, whatever it lacks: that is an install someone made, and
/// the missing-package error names the fix.
pub fn restore_missing_node_modules(root: &Path) -> Result<()> {
    if !RESTORE_NODE_MODULES.load(std::sync::atomic::Ordering::Relaxed)
        || install_vendored_node_modules(root)?.is_none()
    {
        return Ok(());
    }
    ux::note(
        "node_modules was missing or incomplete \u{2014} restored it from the vendored SDK (--skip-install keeps a build from doing this)",
    );
    let version = |path: PathBuf, keys: &[&str]| -> Option<String> {
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        keys.iter()
            .find_map(|k| json.pointer(k)?.as_str().map(str::to_string))
    };
    let vendored = version(
        root.join("node_modules/@dcl/sdk/package.json"),
        &["/version"],
    );
    let asked = version(
        root.join("package.json"),
        &["/devDependencies/@dcl~1sdk", "/dependencies/@dcl~1sdk"],
    );
    if let (Some(vendored), Some(asked)) = (vendored, asked)
        && asked != vendored
    {
        ux::note(format!(
            "package.json asks for @dcl/sdk {asked}; the vendored SDK is {vendored} \u{2014} run npm install to build with {asked} instead"
        ));
    }
    Ok(())
}

type VendoredArchive = zip::ZipArchive<std::io::Cursor<&'static [u8]>>;

fn open_vendored_archive() -> Result<VendoredArchive> {
    zip::ZipArchive::new(std::io::Cursor::new(VENDORED_NODE_MODULES))
        .context("opening the vendored node_modules")
}

/// Extracts the archive where there is no node_modules, or one whose
/// extraction stopped half way (an empty record), and gives the packages
/// written.
fn install_vendored_node_modules(root: &Path) -> Result<Option<Vec<String>>> {
    if root.join("node_modules").exists() && !extraction_stopped(root) {
        return Ok(None);
    }
    extract_vendored_archive(root, open_vendored_archive()?).map(Some)
}

/// An empty record: an extraction started in this node_modules and did not
/// reach its end, so a package of the archive that is there can lack files.
fn extraction_stopped(root: &Path) -> bool {
    std::fs::read(root.join(ARCHIVE_MARKER)).is_ok_and(|record| record.is_empty())
}

/// Where an extraction records the archive it wrote: the SHA-256 of
/// `node_modules.zip`, which another archive changes and the pin does not.
/// The record is empty while the extraction runs.
const ARCHIVE_MARKER: &str = "node_modules/.dcl-one-archive";

fn vendored_archive_sha256() -> String {
    use sha2::Digest as _;
    crate::sdk_cache::hex(sha2::Sha256::digest(VENDORED_NODE_MODULES))
}

/// Writes the archive's packages in place of any that are there and keeps
/// everything else in node_modules. Gives the packages written, sorted.
///
/// A package that is a link is replaced, the link itself. A scope folder
/// that is a link is never written through, and neither is a `node_modules`
/// or a record that is a link: each ends the extraction with an error before
/// any write.
fn extract_vendored_archive(root: &Path, mut archive: VendoredArchive) -> Result<Vec<String>> {
    let modules = root.join("node_modules");
    let marker = root.join(ARCHIVE_MARKER);
    let packages = archive_packages(&archive);
    for entry in ["node_modules", ARCHIVE_MARKER] {
        refuse_link(&root.join(entry), entry, ARCHIVE_MARKER)?;
    }
    for package in &packages {
        refuse_scope_link(&modules, package)?;
    }
    std::fs::create_dir_all(&modules)
        .and_then(|()| std::fs::write(&marker, ""))
        .context("starting the vendored archive's record")?;
    for package in &packages {
        remove_entry(&modules.join(package))
            .with_context(|| format!("removing node_modules/{package}"))?;
    }
    archive
        .extract(root)
        .context("extracting the vendored node_modules")?;
    std::fs::write(marker, vendored_archive_sha256())
        .context("recording the vendored archive in node_modules")?;
    Ok(Vec::from_iter(packages))
}

/// What `init --node-modules-only` found and did.
enum Refresh {
    /// No node_modules, or one whose extraction stopped half way: the archive
    /// was extracted. Names the packages written.
    Extracted(Vec<String>),
    /// node_modules held another `@dcl/sdk` (or none), or the record of
    /// another archive, or no record and the stand-in `@dcl/inspector` (an
    /// install of a CLI that wrote no record): the archive's packages were
    /// replaced; everything else in node_modules was kept. `packages` names
    /// the replaced ones.
    Replaced {
        installed: String,
        packages: Vec<String>,
    },
    /// node_modules already holds this archive's packages.
    Current,
    /// No record and no stand-in `@dcl/inspector`: another tool (npm, pnpm)
    /// installed node_modules, and nothing in it was changed.
    Kept,
}

/// The version suffix of the vendored `@dcl/inspector`, a stand-in with no
/// editor UI. Only this CLI installs it, so it shows an install of this CLI
/// where node_modules has no record.
const STAND_IN_INSPECTOR: &str = "-dcl-one-sdk-shim";

fn refresh_vendored_node_modules(root: &Path) -> Result<Refresh> {
    if let Some(packages) = install_vendored_node_modules(root)? {
        return Ok(Refresh::Extracted(packages));
    }
    let marker = root.join(ARCHIVE_MARKER);
    if !marker.exists()
        && !installed_version(root, "@dcl/inspector")
            .is_some_and(|version| version.ends_with(STAND_IN_INSPECTOR))
    {
        return Ok(Refresh::Kept);
    }
    let installed = installed_version(root, "@dcl/sdk");
    let same_sdk = installed == Some(vendored_sdk_version());
    if same_sdk && std::fs::read_to_string(&marker).ok() == Some(vendored_archive_sha256()) {
        return Ok(Refresh::Current);
    }
    let packages = extract_vendored_archive(root, open_vendored_archive()?)?;
    Ok(Refresh::Replaced {
        installed: match installed {
            Some(version) if same_sdk => format!("@dcl/sdk {version} of another install"),
            Some(version) => format!("@dcl/sdk {version}"),
            None => "no @dcl/sdk".to_string(),
        },
        packages,
    })
}

/// A package that `init --node-modules-only --missing-only` found in
/// node_modules and did not change.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptPackage {
    pub name: String,
    /// From its package.json.
    pub version: String,
    /// The version this dcl-scene-cli installs.
    pub archive_version: Option<String>,
}

/// What `init --node-modules-only` did, with `--missing-only`
/// ([`install_missing_node_modules`]) or without ([`install_node_modules`]):
/// the `--json` document.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeModulesInstall {
    pub dir: PathBuf,
    /// `--dry-run`: nothing was written.
    pub dry_run: bool,
    /// The packages written, sorted. With `--missing-only`: each package
    /// that node_modules did not have, an entry that is not a package
    /// included, and with `dry_run` the packages that a run without it
    /// writes. Without: each package of the archive, or none when the
    /// command wrote nothing.
    pub installed: Vec<String>,
    /// The packages that were there and stay as they are, sorted. Empty
    /// without `--missing-only`.
    pub kept: Vec<KeptPackage>,
    /// The record of the archive was empty when the command started
    /// ([`extraction_stopped`]): no package of the archive counts as there,
    /// and `installed` has them all.
    pub stopped_extraction: bool,
}

/// Where [`install_missing_node_modules`] prepares the packages it adds. Each
/// package then moves to its place in one rename, so node_modules never
/// holds a part of one. Each run that is not a dry run removes the folder,
/// the one that a stopped run left too.
const MISSING_STAGING: &str = ".dcl-one-missing";

/// `init --node-modules-only --missing-only`: writes each package of the
/// archive that node_modules does not have. A package is there when its
/// entry has a package.json that can be read and gives a `version` text: it
/// stays as it is, with a warning on stderr when its version is not the
/// archive's. An entry that is not that (an empty folder, a folder with no
/// such file, a file) is installed from the archive as an absent package is,
/// with a warning on stderr.
///
/// A link is never replaced and never written through: an entry that is a
/// link and does not lead to a package, and a scope folder that is a link
/// above a package to write, end the run with an error before any write.
///
/// The folder stays an install that someone made: this never writes, changes
/// or removes [`ARCHIVE_MARKER`], so no build replaces a kept package because
/// of this run. When the record shows an extraction that stopped, no package
/// of the archive counts as there: each is installed again, the record stays
/// empty, and the next build does that extraction.
///
/// `dry_run` (`--dry-run`) reads and reports the same and writes nothing.
pub fn install_missing_node_modules(dir: &Path, dry_run: bool) -> Result<NodeModulesInstall> {
    let root = existing_dir(dir)?;
    let modules = root.join("node_modules");
    let stopped_extraction = extraction_stopped(&root);
    let mut archive = open_vendored_archive()?;
    let (mut installed, mut kept, mut not_packages) = (Vec::new(), Vec::new(), Vec::new());
    for name in archive_packages(&archive) {
        let place = modules.join(&name);
        let entry = match std::fs::symlink_metadata(&place) {
            Ok(entry) => Some(entry),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("looking for node_modules/{name}"));
            }
        };
        let version = installed_version(&root, &name)
            .filter(|version| !version.is_empty() && !stopped_extraction);
        let Some(version) = version else {
            if entry
                .as_ref()
                .is_some_and(|entry| entry.file_type().is_symlink())
            {
                return Err(refused_link(&name, stopped_extraction));
            }
            refuse_scope_link(&modules, &name)?;
            if entry.is_some() {
                not_packages.push(name.clone());
            }
            installed.push(name);
            continue;
        };
        kept.push(KeptPackage {
            archive_version: archive_version(&mut archive, &name),
            name,
            version,
        });
    }
    if !dry_run {
        let staging = modules.join(MISSING_STAGING);
        let written = match installed.is_empty() {
            true => Ok(()),
            false => stage_missing_packages(&modules, &staging, archive, &installed),
        };
        let cleared = remove_entry(&staging).context("removing the staging folder");
        written.and(cleared)?;
    }
    if stopped_extraction {
        ux::note_stderr(format!(
            "warning: {ARCHIVE_MARKER} is empty: an extraction of the vendored SDK stopped, so each vendored package is installed again; a build or init --node-modules-only does that extraction"
        ));
    } else {
        for name in &not_packages {
            ux::note_stderr(format!(
                "warning: node_modules/{name} is not a package (no package.json with a version); the vendored package replaces it"
            ));
        }
    }
    for package in kept
        .iter()
        .filter(|p| Some(&p.version) != p.archive_version.as_ref())
    {
        ux::note_stderr(format!(
            "warning: node_modules keeps {} {}; the vendored SDK has {}",
            package.name,
            package.version,
            match &package.archive_version {
                Some(version) => format!("{} {version}", package.name),
                None => format!("{} (no version in its package.json)", package.name),
            }
        ));
    }
    let kept_names = Vec::from_iter(kept.iter().map(|p| p.name.as_str())).join(", ");
    let installed_names = installed.join(", ");
    ux::Steps::new(1).done(match (installed.is_empty(), kept.is_empty()) {
        (true, _) => {
            "node_modules already has each package of the vendored SDK — nothing installed"
                .to_string()
        }
        (false, _) if dry_run => format!(
            "node_modules does not have {installed_names} — nothing installed (--dry-run)"
        ),
        (false, true) => format!(
            "Installed {installed_names} in node_modules from the vendored SDK — no npm needed"
        ),
        (false, false) => format!(
            "Installed {installed_names} in node_modules from the vendored SDK; kept {kept_names} — no npm needed"
        ),
    });
    Ok(NodeModulesInstall {
        dir: root,
        dry_run,
        installed,
        kept,
        stopped_extraction,
    })
}

/// The version of `package` in the archive, from its manifest.
fn archive_version(archive: &mut VendoredArchive, package: &str) -> Option<String> {
    use std::io::Read;
    let mut raw = Vec::new();
    let mut manifest = archive
        .by_name(&format!("node_modules/{package}/package.json"))
        .ok()?;
    manifest.read_to_end(&mut raw).ok()?;
    manifest_version(&raw)
}

/// A package link that `--missing-only` would have to replace. After an
/// extraction that stopped, `init --node-modules-only` does that extraction
/// and replaces the link.
fn refused_link(name: &str, stopped_extraction: bool) -> anyhow::Error {
    let remove = TrySteps::one(format!(
        "remove node_modules/{name}, then run the command again"
    ));
    let (steps, why) = match stopped_extraction {
        true => (
            remove.and("or run dcl-scene-cli init --node-modules-only, which replaces each vendored package"),
            "an extraction of the vendored SDK stopped in this node_modules, so the command installs each vendored package again",
        ),
        false => (
            remove,
            "the link does not lead to a package: a package.json with a version",
        ),
    };
    UserError::new(
        format!("node_modules/{name} is a link, and this command does not replace a link"),
        steps,
    )
    .why(why)
    .into()
}

/// Writes the archive below `staging` as an extraction writes it below a
/// scene, then moves each of `packages` to node_modules. An entry that is in
/// the place of a package moves into `staging` first, as it is, and goes with
/// that folder: nothing is written into it or through it.
fn stage_missing_packages(
    modules: &Path,
    staging: &Path,
    mut archive: VendoredArchive,
    packages: &[String],
) -> Result<()> {
    remove_entry(staging).context("removing an old staging folder")?;
    let staged = staging.join("node_modules");
    std::fs::create_dir_all(&staged).context("making the staging folder")?;
    archive
        .extract(staging)
        .context("extracting the vendored node_modules")?;
    let make_parent = |path: &Path| path.parent().map_or(Ok(()), std::fs::create_dir_all);
    for name in packages {
        let place = modules.join(name);
        let old = staging.join("replaced").join(name);
        make_parent(&place)
            .and_then(|()| match std::fs::symlink_metadata(&place) {
                Ok(_) => make_parent(&old).and_then(|()| std::fs::rename(&place, &old)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            })
            .and_then(|()| std::fs::rename(staged.join(name), &place))
            .with_context(|| format!("writing node_modules/{name}"))?;
    }
    Ok(())
}

/// The packages the archive installs under node_modules: `<name>` or
/// `@<scope>/<name>`, each the whole directory the archive would write.
fn archive_packages(archive: &VendoredArchive) -> std::collections::BTreeSet<String> {
    archive
        .file_names()
        .filter_map(|name| name.ok())
        .filter_map(|name| {
            let rel = name.strip_prefix("node_modules/")?;
            let mut parts = rel.split('/');
            let first = parts.next().filter(|s| !s.is_empty())?;
            Some(match first.starts_with('@') {
                true => format!("{first}/{}", parts.next().filter(|s| !s.is_empty())?),
                false => first.to_string(),
            })
        })
        .collect()
}

/// A scope folder that is a link, above the package `name` to write.
fn refuse_scope_link(modules: &Path, name: &str) -> Result<()> {
    let Some((scope, _)) = name.split_once('/') else {
        return Ok(());
    };
    refuse_link(
        &modules.join(scope),
        &format!("node_modules/{scope}"),
        &format!("node_modules/{name}"),
    )
}

/// `path` that is a link, at or above `target`, which the command has to
/// write. `shown` and `target` are the paths as the error names them.
fn refuse_link(path: &Path, shown: &str, target: &str) -> Result<()> {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    if !is_link {
        return Ok(());
    }
    Err(UserError::new(
        format!("{shown} is a link, and this command does not replace a link"),
        TrySteps::one(format!("remove {shown}, then run the command again")),
    )
    .why(format!(
        "the command has to write {target}, and it does not write through a link"
    ))
    .into())
}

fn remove_entry(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    match meta.is_dir() {
        true => std::fs::remove_dir_all(path),
        false => std::fs::remove_file(path),
    }
}

/// Is a full `@dcl/inspector` (with the editor UI) present? The base blob ships
/// a stand-in that only implements the crdt dump, so the UI bundle decides.
pub fn has_full_inspector(root: &Path) -> bool {
    root.join("node_modules/@dcl/inspector/public/index.html")
        .is_file()
}

fn prepare_dir(dir: &Path, yes: bool) -> Result<PathBuf> {
    if dir.is_file() {
        return Err(UserError::new(
            format!(
                "the target path {} is a file, not a directory",
                dir.display()
            ),
            TrySteps::one("pass a directory to --dir, or run init from inside an empty folder"),
        )
        .into());
    }
    std::fs::create_dir_all(dir).map_err(|e| {
        UserError::new(
            format!("cannot create the target directory {}", dir.display()),
            TrySteps::one("check write permission on the parent directory"),
        )
        .caused_by(e)
    })?;
    let root = dunce::canonicalize(dir)
        .with_context(|| format!("resolving target dir {}", dir.display()))?;
    let mut entries: Vec<String> = std::fs::read_dir(&root)
        .map_err(|e| {
            UserError::new(
                format!("cannot read the target directory {}", root.display()),
                TrySteps::one("check read permission on the directory"),
            )
            .caused_by(e)
        })?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    if !entries.is_empty() && !yes {
        entries.sort();
        return Err(UserError::new(
            "the target directory is not empty",
            TrySteps::one(
                "run init in a fresh folder: mkdir my-scene && dcl-scene-cli init --dir my-scene",
            )
            .and(
                "or pass --yes to scaffold here anyway (files with template names get overwritten)",
            ),
        )
        .why(format!(
            "{} contains {} entries (first: {})",
            root.display(),
            entries.len(),
            entries[0]
        ))
        .into());
    }
    Ok(root)
}

fn resolve_kind(flag: Option<ProjectKind>) -> Result<ProjectKind> {
    if let Some(kind) = flag {
        return Ok(kind);
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && !ux::ci() {
        return prompt_kind();
    }
    ux::note(
        "no --project given and no terminal to ask (or CI) — scaffolding the default scene project (pass --project scene|smart-wearable|px-template|library to choose)",
    );
    Ok(ProjectKind::Scene)
}

fn prompt_kind() -> Result<ProjectKind> {
    println!("What would you like to create?");
    println!("  1) scene           a standard Decentraland scene (default)");
    println!("  2) smart-wearable  a wearable with its own portable-experience code");
    println!("  3) px-template     a portable experience without a wearable");
    println!("  4) library         an SDK library scenes import, previewable as a scene");
    print!("Choose [1-4] (enter = 1): ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the project kind answer")?;
    parse_kind_choice(line.trim())
}

fn parse_kind_choice(answer: &str) -> Result<ProjectKind> {
    match answer.to_ascii_lowercase().as_str() {
        "" | "1" | "scene" | "scene-template" => Ok(ProjectKind::Scene),
        "2" | "smart-wearable" | "smart wearable" | "wearable" => Ok(ProjectKind::SmartWearable),
        "3" | "px-template" | "px" | "portable experience" => Ok(ProjectKind::PxTemplate),
        "4" | "library" | "lib" => Ok(ProjectKind::Library),
        other => Err(UserError::new(
            format!("\"{other}\" is not a project kind"),
            TrySteps::one("answer 1 (scene), 2 (smart-wearable), 3 (px-template) or 4 (library)")
                .and("or skip the prompt: dcl-scene-cli init --project scene"),
        )
        .into()),
    }
}

pub fn scaffold_files(kind: ProjectKind, title: &str) -> Vec<FileSpec> {
    let slug = project_slug(title);
    let description = match kind {
        ProjectKind::Scene => "A new Decentraland scene.",
        ProjectKind::SmartWearable => "A new Decentraland smart wearable.",
        ProjectKind::PxTemplate => "A new Decentraland portable experience.",
        ProjectKind::Library => "A new Decentraland SDK library.",
    };
    let sub = |template: &str| {
        template
            .replace("{{TITLE}}", title)
            .replace("{{DESCRIPTION}}", description)
            .replace("{{SLUG}}", &slug)
    };
    let file = |rel, body: String| FileSpec {
        rel,
        body: body.into_bytes(),
    };
    let raw = |rel, body: &str| file(rel, body.to_string());
    let (head, index, readme, tsconfig): (Vec<FileSpec>, _, _, _) = match kind {
        ProjectKind::Scene => (
            vec![
                file("scene.json", sub(SCENE_SCENE_JSON)),
                file("package.json", sub(SCENE_PACKAGE_JSON)),
            ],
            SCENE_INDEX_TS,
            SCENE_README,
            SCENE_TSCONFIG,
        ),
        ProjectKind::PxTemplate => (
            vec![
                file("scene.json", sub(PX_SCENE_JSON)),
                file("package.json", sub(SCENE_PACKAGE_JSON)),
            ],
            SCENE_INDEX_TS,
            SCENE_README,
            SCENE_TSCONFIG,
        ),
        ProjectKind::Library => (
            vec![
                file("scene.json", sub(SCENE_SCENE_JSON)),
                file("package.json", sub(LIB_PACKAGE_JSON)),
            ],
            LIB_INDEX_TS,
            LIB_README,
            LIB_TSCONFIG,
        ),
        ProjectKind::SmartWearable => (
            vec![
                file(
                    "wearable.json",
                    sub(SW_WEARABLE_JSON).replace("{{ID}}", &uuid_v4()),
                ),
                file(
                    "scene.json",
                    sub(SW_SCENE_JSON).replace("{{PARCELS}}", &parcel_grid(10, 10)),
                ),
                file("package.json", sub(SW_PACKAGE_JSON)),
            ],
            SW_INDEX_TS,
            SW_README,
            SCENE_TSCONFIG,
        ),
    };
    let mut files = head;
    files.extend([
        raw("tsconfig.json", tsconfig),
        file("src/index.ts", sub(index)),
        raw(".gitignore", SCENE_GITIGNORE),
        raw(".dclignore", SCENE_DCLIGNORE),
        file("README.md", sub(readme)),
    ]);
    match kind {
        ProjectKind::Scene | ProjectKind::Library => files.push(FileSpec {
            rel: "images/scene-thumbnail.png",
            body: SCENE_THUMBNAIL.to_vec(),
        }),
        ProjectKind::PxTemplate => {}
        ProjectKind::SmartWearable => files.extend([
            FileSpec {
                rel: "model.glb",
                body: SW_MODEL_GLB.to_vec(),
            },
            FileSpec {
                rel: "thumbnail.png",
                body: SW_THUMBNAIL.to_vec(),
            },
        ]),
    }
    files
}

fn write_file(root: &Path, rel: &str, body: &[u8]) -> Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| write_error(&path, e))?;
    }
    std::fs::write(&path, body).map_err(|e| write_error(&path, e))?;
    Ok(())
}

fn display_dir(dir: &Path) -> String {
    match dir == Path::new(".") {
        true => "the current directory".to_string(),
        false => dir.display().to_string(),
    }
}

pub fn project_title(root: &Path) -> String {
    let raw = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " ._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    match cleaned.trim().trim_matches('-').trim() {
        "" => "my-scene".to_string(),
        t => t.to_string(),
    }
}

pub fn project_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in name.trim().to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c);
        } else {
            pending_dash = true;
        }
    }
    match slug.is_empty() {
        true => "new-scene".to_string(),
        false => slug,
    }
}

fn parcel_grid(cols: u32, rows: u32) -> String {
    (0..rows)
        .flat_map(|y| (0..cols).map(move |x| format!("\"{x},{y}\"")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn uuid_v4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap counts the bytes written, whatever sizes the zip declares.
    #[test]
    fn a_template_that_unpacks_past_the_limit_is_refused() {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for name in ["t/scene.json", "t/big.bin"] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(&[b'x'; 600]).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let fits = crate::deploy::TempTree::new("init-unpack-fits");
        assert_eq!(
            unpack_template_within(&bytes, &fits.0, None, 1200).unwrap(),
            2
        );
        let over = crate::deploy::TempTree::new("init-unpack-over");
        let err = unpack_template_within(&bytes, &over.0, None, 1000).unwrap_err();
        let shown = ux::render(&err, false, false);
        assert!(shown.contains("the unpacked template is larger"), "{shown}");
    }

    #[test]
    fn slug_matches_the_ported_projectslug() {
        assert_eq!(project_slug("My Awesome Scene"), "my-awesome-scene");
        assert_eq!(project_slug("  spaced  out  "), "spaced-out");
        assert_eq!(project_slug("??"), "new-scene");
        assert_eq!(project_slug(""), "new-scene");
        assert_eq!(project_slug("-Already-Slugged-"), "already-slugged");
    }

    #[test]
    fn title_sanitizes_shell_hostile_names() {
        assert_eq!(project_title(Path::new("/tmp/my-scene")), "my-scene");
        assert_eq!(project_title(Path::new("/tmp/a\"b\\c")), "a-b-c");
        assert_eq!(project_title(Path::new("/")), "my-scene");
    }

    #[test]
    fn kind_choice_accepts_numbers_names_and_default() {
        assert_eq!(parse_kind_choice("").unwrap(), ProjectKind::Scene);
        assert_eq!(parse_kind_choice("1").unwrap(), ProjectKind::Scene);
        assert_eq!(parse_kind_choice("Scene").unwrap(), ProjectKind::Scene);
        assert_eq!(parse_kind_choice("2").unwrap(), ProjectKind::SmartWearable);
        assert_eq!(
            parse_kind_choice("wearable").unwrap(),
            ProjectKind::SmartWearable
        );
        assert_eq!(parse_kind_choice("3").unwrap(), ProjectKind::PxTemplate);
        assert_eq!(parse_kind_choice("px").unwrap(), ProjectKind::PxTemplate);
        assert_eq!(parse_kind_choice("4").unwrap(), ProjectKind::Library);
        assert_eq!(parse_kind_choice("library").unwrap(), ProjectKind::Library);
        assert!(parse_kind_choice("5").is_err());
        assert!(parse_kind_choice("quest").is_err());
    }

    #[test]
    fn parcel_grid_is_the_full_10x10() {
        let grid = parcel_grid(10, 10);
        assert!(grid.starts_with("\"0,0\", \"1,0\""));
        assert!(grid.ends_with("\"9,9\""));
        assert_eq!(grid.matches(',').count(), 199);
    }

    #[test]
    fn uuid_v4_shape() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        for i in [8, 13, 18, 23] {
            assert_eq!(id.as_bytes()[i], b'-');
        }
    }

    #[test]
    fn scene_scaffold_substitutes_every_placeholder() {
        for f in scaffold_files(ProjectKind::Scene, "Test Scene") {
            let body = String::from_utf8_lossy(&f.body).into_owned();
            assert!(!body.contains("{{"), "{} still has a placeholder", f.rel);
        }
    }

    #[test]
    fn wearable_scaffold_substitutes_every_placeholder() {
        for f in scaffold_files(ProjectKind::SmartWearable, "Test Wearable") {
            let body = String::from_utf8_lossy(&f.body).into_owned();
            assert!(!body.contains("{{"), "{} still has a placeholder", f.rel);
        }
    }

    /// Each of these is a silent failure if a blob rebuild drops it: without
    /// `host.js` `start --data-layer` cannot boot, a descriptor short of 22
    /// methods is a `TypeError` that kills the whole rpc connection, and
    /// without `component-schemas.json` the engine drops every crdt message
    /// for a component it does not know.
    #[test]
    fn blob_carries_the_data_layer_host() {
        let cursor = std::io::Cursor::new(VENDORED_NODE_MODULES);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let names: Vec<String> = archive
            .file_names()
            .map(|n| n.unwrap().to_string())
            .collect();
        for rel in [
            "node_modules/@dcl/inspector/host.js",
            "node_modules/@dcl/inspector/engine.js",
            "node_modules/@dcl/inspector/engine-to-composite.js",
            "node_modules/@dcl/inspector/data-layer.gen.js",
            "node_modules/@dcl/inspector/component-schemas.json",
            "node_modules/@dcl/inspector/minimal-composite.json",
            "node_modules/@dcl/rpc/dist/index.js",
            "node_modules/@dcl/rpc/dist/codegen.js",
            "node_modules/@dcl/rpc/dist/push-channel.js",
            "node_modules/@dcl/rpc/dist/transports/WebSocket.js",
            "node_modules/mitt/dist/mitt.js",
        ] {
            assert!(names.iter().any(|n| n == rel), "blob is missing {rel}");
        }
        assert!(
            !names
                .iter()
                .any(|n| n.ends_with(".gen.ts") || n.ends_with(".proto")),
            "the descriptor source leaked into the blob"
        );

        use std::io::Read;
        let mut descriptor = String::new();
        archive
            .by_name("node_modules/@dcl/inspector/data-layer.gen.js")
            .unwrap()
            .read_to_string(&mut descriptor)
            .unwrap();
        assert_eq!(
            descriptor.matches("requestStream:").count(),
            22,
            "the DataService descriptor must declare all 22 methods"
        );
    }

    /// The tree has no record and the stand-in `@dcl/inspector`: a CLI that
    /// wrote no record installed it, and the command replaces its packages.
    #[test]
    fn node_modules_only_replaces_another_sdk_and_keeps_the_rest() {
        let t = crate::scene::Tmp::new("init-refresh");
        t.write(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
        );
        t.write(
            "node_modules/@dcl/inspector/package.json",
            r#"{"name":"@dcl/inspector","version":"0.0.0-dcl-one-sdk-shim"}"#,
        );
        t.write("node_modules/@dcl/sdk/stale.js", "");
        t.write("node_modules/my-lib/index.js", "");
        match refresh_vendored_node_modules(&t.0).unwrap() {
            Refresh::Replaced {
                installed,
                packages,
            } => {
                assert_eq!(installed, "@dcl/sdk 7.0.0");
                let archive = archive_packages(&open_vendored_archive().unwrap());
                assert_eq!(packages, Vec::from_iter(archive));
            }
            _ => panic!("another @dcl/sdk is replaced"),
        }
        assert_eq!(
            installed_version(&t.0, "@dcl/sdk"),
            Some(vendored_sdk_version())
        );
        assert!(!t.0.join("node_modules/@dcl/sdk/stale.js").exists());
        assert!(t.0.join("node_modules/my-lib/index.js").exists());
        assert!(matches!(
            refresh_vendored_node_modules(&t.0).unwrap(),
            Refresh::Current
        ));
        // An extraction that stopped half way left an empty record: a build
        // or `init` does it again; node_modules without one stays as it is.
        std::fs::write(t.0.join(ARCHIVE_MARKER), "").unwrap();
        t.write("node_modules/@dcl/sdk/stale.js", "");
        restore_missing_node_modules(&t.0).unwrap();
        assert!(!t.0.join("node_modules/@dcl/sdk/stale.js").exists());
        assert!(t.0.join("node_modules/my-lib/index.js").exists());
        assert_eq!(
            std::fs::read_to_string(t.0.join(ARCHIVE_MARKER)).unwrap(),
            vendored_archive_sha256()
        );
        std::fs::remove_file(t.0.join(ARCHIVE_MARKER)).unwrap();
        t.write("node_modules/@dcl/sdk/stale.js", "");
        restore_missing_node_modules(&t.0).unwrap();
        assert!(t.0.join("node_modules/@dcl/sdk/stale.js").exists());
        let fresh = crate::scene::Tmp::new("init-refresh-fresh");
        assert!(matches!(
            refresh_vendored_node_modules(&fresh.0).unwrap(),
            Refresh::Extracted(_)
        ));
    }

    /// A node_modules with no record and no stand-in `@dcl/inspector` is the
    /// install of another tool: the command changes no byte of it, whatever
    /// `@dcl/sdk` it has.
    #[test]
    fn node_modules_only_keeps_an_install_of_another_tool() {
        fn tree(dir: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries = Vec::from_iter(std::fs::read_dir(dir).unwrap());
            entries.sort_by_key(|entry| entry.as_ref().unwrap().path());
            for entry in entries {
                let path = entry.unwrap().path();
                match path.is_dir() {
                    true => tree(&path, files),
                    false => files.push((path.clone(), std::fs::read(path).unwrap())),
                }
            }
        }
        let pin = vendored_sdk_version();
        for (tag, sdk, inspector) in [
            ("npm", pin.as_str(), Some("7.46.1")),
            ("npm-old", "7.0.0", Some("7.46.1")),
            ("npm-bare", "7.0.0", None),
        ] {
            let t = crate::scene::Tmp::new(&format!("init-refresh-{tag}"));
            let manifest = |name: &str, version: &str| {
                t.write(
                    &format!("node_modules/{name}/package.json"),
                    format!(r#"{{"name":"{name}","version":"{version}"}}"#),
                );
            };
            manifest("@dcl/sdk", sdk);
            manifest("protobufjs", "7.5.4");
            manifest("typescript", "5.9.3");
            if let Some(version) = inspector {
                manifest("@dcl/inspector", version);
                t.write("node_modules/@dcl/inspector/public/index.html", "editor");
            }
            let mut before = Vec::new();
            tree(&t.0, &mut before);
            assert!(matches!(
                refresh_vendored_node_modules(&t.0).unwrap(),
                Refresh::Kept
            ));
            let mut after = Vec::new();
            tree(&t.0, &mut after);
            assert_eq!(before, after, "{tag}");
            assert_eq!(has_full_inspector(&t.0), inspector.is_some());
        }
    }

    #[test]
    fn node_modules_only_follows_the_archive_under_an_unchanged_pin() {
        let t = crate::scene::Tmp::new("init-refresh-archive");
        let pin = vendored_sdk_version();
        let marker = t.0.join(ARCHIVE_MARKER);
        let sentinel = t.0.join("node_modules/@dcl/sdk/sentinel.js");
        assert!(matches!(
            refresh_vendored_node_modules(&t.0).unwrap(),
            Refresh::Extracted(_)
        ));
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            vendored_archive_sha256()
        );

        std::fs::write(&sentinel, "").unwrap();
        assert!(matches!(
            refresh_vendored_node_modules(&t.0).unwrap(),
            Refresh::Current
        ));
        assert!(sentinel.exists(), "this archive's packages are left alone");

        // Another archive under the same pin, then the same packages with no
        // record: the vendored `@dcl/inspector` is the stand-in, so this is
        // the install of a CLI that wrote no record.
        for other in [Some("0".repeat(64)), None] {
            match other {
                Some(hash) => std::fs::write(&marker, hash).unwrap(),
                None => std::fs::remove_file(&marker).unwrap(),
            }
            std::fs::write(&sentinel, "").unwrap();
            match refresh_vendored_node_modules(&t.0).unwrap() {
                Refresh::Replaced { installed, .. } => {
                    assert_eq!(installed, format!("@dcl/sdk {pin} of another install"))
                }
                _ => panic!("another archive's packages are replaced"),
            }
            assert!(!sentinel.exists());
            assert_eq!(
                std::fs::read_to_string(&marker).unwrap(),
                vendored_archive_sha256()
            );
        }

        t.write(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
        );
        match refresh_vendored_node_modules(&t.0).unwrap() {
            Refresh::Replaced { installed, .. } => assert_eq!(installed, "@dcl/sdk 7.0.0"),
            _ => panic!("another @dcl/sdk is replaced, whatever the record says"),
        }
        assert_eq!(installed_version(&t.0, "@dcl/sdk"), Some(pin));
    }

    /// The install replaces a package that is a link, the link itself, and
    /// writes through no scope folder that is a link: it stops before a write
    /// and names the link. A build that would repair node_modules stops in
    /// the same way.
    #[cfg(unix)]
    #[test]
    fn node_modules_only_refuses_a_scope_folder_that_is_a_link() {
        use std::os::unix::fs::symlink as link;
        let elsewhere = crate::scene::Tmp::new("init-link-target");
        elsewhere.write("scope/sdk/own.js", "own");
        elsewhere.write("ecs/own.js", "own");
        let target_is_as_it_was = || {
            let files = |rel: &str| std::fs::read_dir(elsewhere.0.join(rel)).unwrap().count();
            assert_eq!(
                (files("scope"), files("scope/sdk"), files("ecs")),
                (1, 1, 1)
            );
        };

        let t = crate::scene::Tmp::new("init-scope-link");
        let modules = t.0.join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        link(elsewhere.0.join("scope"), modules.join("@dcl")).unwrap();
        // The record of another archive: the command has packages to replace.
        let other = "0".repeat(64);
        std::fs::write(t.0.join(ARCHIVE_MARKER), &other).unwrap();
        let refused = |err: anyhow::Error| {
            let text = ux::render(&err, false, false);
            assert!(text.contains("node_modules/@dcl is a link"), "{text}");
            assert!(text.contains("remove node_modules/@dcl"), "{text}");
            assert!(!modules.join("typescript").exists(), "no file is written");
            target_is_as_it_was();
        };
        refused(refresh_vendored_node_modules(&t.0).err().unwrap());
        assert_eq!(
            std::fs::read_to_string(t.0.join(ARCHIVE_MARKER)).unwrap(),
            other
        );
        // An empty record makes a build do the extraction: the same stop,
        // and the record stays empty.
        std::fs::write(t.0.join(ARCHIVE_MARKER), "").unwrap();
        refused(restore_missing_node_modules(&t.0).unwrap_err());
        assert!(std::fs::read(t.0.join(ARCHIVE_MARKER)).unwrap().is_empty());

        let t = crate::scene::Tmp::new("init-package-link");
        std::fs::create_dir_all(t.0.join("node_modules/@dcl")).unwrap();
        std::fs::write(t.0.join(ARCHIVE_MARKER), &other).unwrap();
        let place = t.0.join("node_modules/@dcl/ecs");
        link(elsewhere.0.join("ecs"), &place).unwrap();
        assert!(matches!(
            refresh_vendored_node_modules(&t.0).unwrap(),
            Refresh::Replaced { .. }
        ));
        assert!(!std::fs::symlink_metadata(&place).unwrap().is_symlink());
        assert!(place.join("package.json").is_file());
        target_is_as_it_was();
    }

    /// The record is not written through a link: a `node_modules` or a record
    /// that is a link stops the install before a write and the error names
    /// it. What the link leads to stays as it was.
    #[cfg(unix)]
    #[test]
    fn node_modules_only_refuses_a_record_or_a_node_modules_that_is_a_link() {
        use std::os::unix::fs::symlink as link;
        let other = "0".repeat(64);
        let elsewhere = crate::scene::Tmp::new("init-record-link-target");
        elsewhere.write("record", &other);
        elsewhere.write("modules/.dcl-one-archive", &other);
        let refused = |root: &Path, name: &str| {
            let err = refresh_vendored_node_modules(root).err().unwrap();
            let text = ux::render(&err, false, false);
            assert!(text.contains(&format!("{name} is a link")), "{text}");
            assert!(text.contains(&format!("remove {name}")), "{text}");
            assert_eq!(
                Vec::from_iter(tree(&elsewhere.0)),
                [
                    (
                        "modules/.dcl-one-archive".to_string(),
                        other.clone().into_bytes()
                    ),
                    ("record".to_string(), other.clone().into_bytes()),
                ],
                "the target of the link is as it was"
            );
        };

        // The record of another archive behind a link: there are packages
        // to replace, and the record would be the first write.
        let t = crate::scene::Tmp::new("init-record-link");
        std::fs::create_dir_all(t.0.join("node_modules")).unwrap();
        link(elsewhere.0.join("record"), t.0.join(ARCHIVE_MARKER)).unwrap();
        refused(&t.0, ARCHIVE_MARKER);
        assert!(!t.0.join("node_modules/typescript").exists());

        // node_modules itself is a link, to a folder and to nothing.
        let t = crate::scene::Tmp::new("init-modules-link");
        link(elsewhere.0.join("modules"), t.0.join("node_modules")).unwrap();
        refused(&t.0, "node_modules");
        std::fs::remove_file(t.0.join("node_modules")).unwrap();
        link(elsewhere.0.join("absent"), t.0.join("node_modules")).unwrap();
        refused(&t.0, "node_modules");
        assert!(!elsewhere.0.join("absent").exists());
        let err = restore_missing_node_modules(&t.0).unwrap_err();
        assert!(ux::render(&err, false, false).contains("node_modules is a link"));
    }

    /// Every file below `dir`, by its path from there, with its bytes.
    fn tree(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        let mut folders = vec![dir.to_path_buf()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(&folder).unwrap().flatten() {
                let path = entry.path();
                match path.is_dir() {
                    true => folders.push(path),
                    false => {
                        let rel = path.strip_prefix(dir).unwrap().display().to_string();
                        files.insert(rel, std::fs::read(&path).unwrap());
                    }
                }
            }
        }
        files
    }

    fn all_archive_packages() -> Vec<String> {
        Vec::from_iter(archive_packages(&open_vendored_archive().unwrap()))
    }

    #[test]
    fn missing_only_installs_the_absent_packages_and_changes_no_other() {
        let t = crate::scene::Tmp::new("init-missing");
        t.write(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
        );
        t.write("node_modules/@dcl/sdk/own.js", "own");
        t.write(
            "node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"5.0.0"}"#,
        );
        t.write("node_modules/typescript/lib/own.js", "own");
        t.write("node_modules/my-lib/index.js", "");
        let modules = t.0.join("node_modules");
        let own = || {
            (
                tree(&modules.join("@dcl/sdk")),
                tree(&modules.join("typescript")),
            )
        };
        let before = own();

        let made = install_missing_node_modules(&t.0, false).unwrap();
        assert_eq!(made.dir, dunce::canonicalize(&t.0).unwrap());
        let mut expected = all_archive_packages();
        expected.retain(|name| name != "@dcl/sdk" && name != "typescript");
        assert_eq!(expected.len(), 9);
        assert_eq!(made.installed, expected);
        let compiler = archive_version(&mut open_vendored_archive().unwrap(), "typescript");
        assert!(compiler.is_some() && compiler.as_deref() != Some("5.0.0"));
        assert_eq!(
            made.kept,
            [
                KeptPackage {
                    name: "@dcl/sdk".to_string(),
                    version: "7.0.0".to_string(),
                    archive_version: Some(vendored_sdk_version()),
                },
                KeptPackage {
                    name: "typescript".to_string(),
                    version: "5.0.0".to_string(),
                    archive_version: compiler,
                },
            ]
        );
        assert!(!made.stopped_extraction);
        assert_eq!(before, own(), "a package that was there has the same bytes");
        assert!(modules.join("my-lib/index.js").exists());
        assert!(!modules.join(MISSING_STAGING).exists());
        assert!(!t.0.join(ARCHIVE_MARKER).exists(), "no record is written");

        // An added package is the one a full extraction writes.
        let full = crate::scene::Tmp::new("init-missing-full");
        refresh_vendored_node_modules(&full.0).unwrap();
        for package in &made.installed {
            assert_eq!(
                tree(&modules.join(package)),
                tree(&full.0.join("node_modules").join(package)),
                "{package}"
            );
        }

        // A second run finds nothing to install. A build changes nothing.
        let again = install_missing_node_modules(&t.0, false).unwrap();
        assert!(again.installed.is_empty(), "{:?}", again.installed);
        assert_eq!(again.kept.len(), made.installed.len() + 2);
        restore_missing_node_modules(&t.0).unwrap();
        assert_eq!(before, own());
        assert_eq!(
            installed_version(&t.0, "@dcl/sdk").as_deref(),
            Some("7.0.0")
        );
        assert!(!t.0.join(ARCHIVE_MARKER).exists());
    }

    /// An entry is a package with a package.json that gives a version text.
    /// An entry that is not one is replaced as an absent package is
    /// installed; a dry run reports the same and changes nothing.
    #[test]
    fn missing_only_installs_over_an_entry_that_is_not_a_package() {
        let t = crate::scene::Tmp::new("init-missing-not-package");
        let modules = t.0.join("node_modules");
        t.write("node_modules/typescript/lib/own.js", "no manifest");
        t.write(
            "node_modules/@dcl/ecs/package.json",
            r#"{"name":"@dcl/ecs"}"#,
        );
        t.write("node_modules/long/package.json", r#"{"version":""}"#);
        t.write("node_modules/@dcl/sdk", "a file");
        std::fs::create_dir_all(modules.join("@dcl/js-runtime")).unwrap();
        t.write("node_modules/my-lib/index.js", "");
        let before = tree(&t.0);
        let report = install_missing_node_modules(&t.0, true).unwrap();
        assert_eq!(report.installed, all_archive_packages());
        assert!(report.kept.is_empty(), "{:?}", report.kept);
        assert_eq!(tree(&t.0), before, "a dry run writes no file");

        let made = install_missing_node_modules(&t.0, false).unwrap();
        assert_eq!(made.installed, report.installed);
        assert!(made.kept.is_empty() && !made.stopped_extraction);
        let full = crate::scene::Tmp::new("init-missing-not-package-full");
        refresh_vendored_node_modules(&full.0).unwrap();
        for package in &made.installed {
            assert_eq!(
                tree(&modules.join(package)),
                tree(&full.0.join("node_modules").join(package)),
                "{package}"
            );
        }
        assert!(modules.join("my-lib/index.js").exists());
        assert!(!modules.join(MISSING_STAGING).exists());
        assert!(!t.0.join(ARCHIVE_MARKER).exists(), "no record is written");
    }

    /// A link is not replaced and not written through: the run stops before
    /// a write, with the dry run too, and names the link.
    #[cfg(unix)]
    #[test]
    fn missing_only_refuses_a_link_that_is_not_a_package() {
        use std::os::unix::fs::symlink as link;
        let refused = |t: &crate::scene::Tmp, name: &str| {
            let modules = t.0.join("node_modules");
            for dry_run in [true, false] {
                let err = install_missing_node_modules(&t.0, dry_run).unwrap_err();
                let text = ux::render(&err, false, false);
                assert!(
                    text.contains(&format!("node_modules/{name} is a link")),
                    "{text}"
                );
                assert!(
                    text.contains(&format!("remove node_modules/{name}")),
                    "{text}"
                );
                assert!(!text.contains("--node-modules-only"), "{text}");
            }
            assert!(
                std::fs::symlink_metadata(modules.join(name))
                    .unwrap()
                    .is_symlink()
            );
            for written in ["@dcl/sdk", "typescript", MISSING_STAGING] {
                assert!(!modules.join(written).exists(), "{written} is not written");
            }
        };

        // A link to nothing, and a link to a folder that is not a package.
        let t = crate::scene::Tmp::new("init-missing-link");
        let elsewhere = crate::scene::Tmp::new("init-missing-link-target");
        elsewhere.write("ecs/own.js", "own");
        std::fs::create_dir_all(t.0.join("node_modules/@dcl")).unwrap();
        let place = t.0.join("node_modules/@dcl/ecs");
        link(elsewhere.0.join("absent"), &place).unwrap();
        refused(&t, "@dcl/ecs");
        std::fs::remove_file(&place).unwrap();
        link(elsewhere.0.join("ecs"), &place).unwrap();
        refused(&t, "@dcl/ecs");
        assert_eq!(tree(&elsewhere.0).len(), 1, "the target has its one file");

        // A link to a package is a package that is there: it stays.
        elsewhere.write("ecs/package.json", r#"{"version":"7.0.0"}"#);
        let made = install_missing_node_modules(&t.0, false).unwrap();
        assert_eq!(made.kept[0].name, "@dcl/ecs");
        assert!(std::fs::symlink_metadata(&place).unwrap().is_symlink());
        assert_eq!(tree(&elsewhere.0).len(), 2);

        // After an extraction that stopped no package counts as there, so
        // the link is refused again. The error names the full install, which
        // replaces a package link.
        let marker = t.0.join(ARCHIVE_MARKER);
        std::fs::write(&marker, "").unwrap();
        let sdk = tree(&t.0.join("node_modules/@dcl/sdk"));
        for dry_run in [true, false] {
            let err = install_missing_node_modules(&t.0, dry_run).unwrap_err();
            let text = ux::render(&err, false, false);
            assert!(text.contains("node_modules/@dcl/ecs is a link"), "{text}");
            assert!(text.contains("init --node-modules-only"), "{text}");
        }
        assert_eq!(tree(&t.0.join("node_modules/@dcl/sdk")), sdk);
        assert_eq!(tree(&elsewhere.0).len(), 2);
        assert!(std::fs::read(&marker).unwrap().is_empty());

        // A scope folder that is a link, above a package to write.
        let t = crate::scene::Tmp::new("init-missing-scope-link");
        std::fs::create_dir_all(t.0.join("node_modules")).unwrap();
        std::fs::create_dir_all(elsewhere.0.join("scope")).unwrap();
        link(elsewhere.0.join("scope"), t.0.join("node_modules/@dcl")).unwrap();
        refused(&t, "@dcl");
        assert_eq!(tree(&elsewhere.0).len(), 2);
    }

    /// The document of the full install: the packages written, none kept,
    /// and the empty record that the command found.
    #[test]
    fn node_modules_only_reports_the_packages_it_wrote() {
        let t = crate::scene::Tmp::new("init-full-report");
        let archive = all_archive_packages();

        let made = install_node_modules(&t.0).unwrap();
        assert_eq!(made.dir, dunce::canonicalize(&t.0).unwrap());
        assert_eq!(made.installed, archive);
        assert!(made.kept.is_empty() && !made.dry_run && !made.stopped_extraction);

        // This archive's packages are there: nothing is written.
        let again = install_node_modules(&t.0).unwrap();
        assert!(again.installed.is_empty(), "{:?}", again.installed);
        assert!(again.kept.is_empty() && !again.stopped_extraction);

        // Another SDK in a folder that has a record: each package is written.
        t.write(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
        );
        let replaced = install_node_modules(&t.0).unwrap();
        assert_eq!(replaced.installed, archive);
        assert!(replaced.kept.is_empty() && !replaced.stopped_extraction);

        // An empty record: the document says so, and the extraction runs.
        std::fs::write(t.0.join(ARCHIVE_MARKER), "").unwrap();
        let repaired = install_node_modules(&t.0).unwrap();
        assert!(repaired.stopped_extraction && !repaired.dry_run);
        assert_eq!(repaired.installed, archive);
        assert!(repaired.kept.is_empty());
        assert_eq!(
            std::fs::read_to_string(t.0.join(ARCHIVE_MARKER)).unwrap(),
            vendored_archive_sha256()
        );

        // The install of another tool (no record, no stand-in
        // `@dcl/inspector`) is kept: the command writes no package.
        let other = crate::scene::Tmp::new("init-full-report-other");
        other.write(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
        );
        let before = tree(&other.0);
        let kept = install_node_modules(&other.0).unwrap();
        assert!(kept.installed.is_empty(), "{:?}", kept.installed);
        assert!(kept.kept.is_empty() && !kept.stopped_extraction);
        assert_eq!(tree(&other.0), before);
    }

    /// The record of an extraction belongs to the full install: a
    /// `--missing-only` run neither makes one nor changes one, and removes
    /// the staging folder that a stopped run left.
    #[test]
    fn missing_only_leaves_the_archive_record_as_it_is() {
        let t = crate::scene::Tmp::new("init-missing-record");
        let marker = t.0.join(ARCHIVE_MARKER);
        let own = t.0.join("node_modules/@dcl/sdk/own.js");

        let made = install_missing_node_modules(&t.0, false).unwrap();
        assert!(made.kept.is_empty(), "{:?}", made.kept);
        assert_eq!(
            installed_version(&t.0, "@dcl/sdk"),
            Some(vendored_sdk_version())
        );
        assert!(!marker.exists(), "a new node_modules gets no record");
        std::fs::write(&own, "").unwrap();
        restore_missing_node_modules(&t.0).unwrap();
        assert!(own.exists(), "a build leaves a folder without a record");

        // A stopped run left its staging folder. A dry run keeps it; a run
        // removes it, with a package to install and with none.
        let staging = t.0.join("node_modules").join(MISSING_STAGING);
        let leave_staging = || {
            t.write(
                &format!("node_modules/{MISSING_STAGING}/node_modules/@dcl/ecs/part.js"),
                "",
            )
        };
        leave_staging();
        let report = install_missing_node_modules(&t.0, true).unwrap();
        assert!(report.installed.is_empty(), "{:?}", report.installed);
        assert!(staging.exists(), "a dry run removes nothing");
        let made = install_missing_node_modules(&t.0, false).unwrap();
        assert!(made.installed.is_empty(), "{:?}", made.installed);
        assert!(
            !staging.exists(),
            "a run with nothing to install removes it"
        );
        assert!(own.exists());
        for record in ["another archive".to_string(), vendored_archive_sha256()] {
            leave_staging();
            std::fs::write(&marker, &record).unwrap();
            std::fs::remove_dir_all(t.0.join("node_modules/@dcl/ecs")).unwrap();
            let made = install_missing_node_modules(&t.0, false).unwrap();
            assert_eq!(made.installed, ["@dcl/ecs"]);
            assert!(!made.stopped_extraction);
            assert!(!t.0.join("node_modules/@dcl/ecs/part.js").exists());
            assert!(!t.0.join("node_modules").join(MISSING_STAGING).exists());
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), record);
            assert!(own.exists());
        }

        // An empty record shows an extraction that stopped half way: a
        // package of the archive that is there can lack files, so not one
        // counts as there. The dry run reports the same; the run writes each
        // package again and not the record.
        std::fs::write(&marker, "").unwrap();
        let ecs = t.0.join("node_modules/@dcl/ecs");
        let manifest = std::fs::read(ecs.join("package.json")).unwrap();
        std::fs::remove_dir_all(&ecs).unwrap();
        std::fs::create_dir_all(&ecs).unwrap();
        std::fs::write(ecs.join("package.json"), manifest).unwrap();
        t.write("node_modules/my-lib/index.js", "");
        let before = tree(&t.0);
        let report = install_missing_node_modules(&t.0, true).unwrap();
        assert!(report.stopped_extraction);
        assert_eq!(report.installed, all_archive_packages());
        assert!(report.kept.is_empty(), "{:?}", report.kept);
        assert_eq!(tree(&t.0), before, "a dry run writes no file");

        let made = install_missing_node_modules(&t.0, false).unwrap();
        assert!(made.stopped_extraction);
        assert_eq!(made.installed, report.installed);
        assert_eq!(made.kept, report.kept);
        assert!(
            !own.exists(),
            "each package of the archive is written again"
        );
        assert!(tree(&ecs).len() > 1, "the package has its files");
        assert!(t.0.join("node_modules/my-lib/index.js").exists());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "");
        assert!(!t.0.join("node_modules").join(MISSING_STAGING).exists());
        // With the empty record, a build does that extraction.
        restore_missing_node_modules(&t.0).unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            vendored_archive_sha256()
        );
    }

    /// What a parent program needs for "outdated" and "update": the dry run
    /// reports an older `@dcl/sdk` with the two versions and writes nothing,
    /// and `init --node-modules-only` then replaces that package, but not in
    /// the install of another tool.
    #[test]
    fn a_dry_run_reports_an_older_sdk_and_node_modules_only_replaces_it() {
        let empty = crate::scene::Tmp::new("init-dry-run-empty");
        let report = install_missing_node_modules(&empty.0, true).unwrap();
        assert!(report.dry_run && report.kept.is_empty());
        assert!(report.installed.contains(&"@dcl/sdk".to_string()));
        assert!(!empty.0.join("node_modules").exists(), "nothing is made");

        let t = crate::scene::Tmp::new("init-dry-run");
        t.write(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
        );
        t.write("node_modules/my-lib/index.js", "");
        let modules = t.0.join("node_modules");
        let older = KeptPackage {
            name: "@dcl/sdk".to_string(),
            version: "7.0.0".to_string(),
            archive_version: Some(vendored_sdk_version()),
        };
        let before = tree(&t.0);
        let report = install_missing_node_modules(&t.0, true).unwrap();
        assert_eq!(report.kept, [older]);
        assert!(report.installed.contains(&"@dcl/ecs".to_string()));
        assert_eq!(tree(&t.0), before, "a dry run writes no file");
        assert!(!modules.join(MISSING_STAGING).exists());
        assert!(!modules.join("@dcl/ecs").exists());

        // The run that writes gives the same two lists.
        let made = install_missing_node_modules(&t.0, false).unwrap();
        assert!(!made.dry_run);
        assert_eq!(made.installed, report.installed);
        assert_eq!(made.kept, report.kept);
        let report = install_missing_node_modules(&t.0, true).unwrap();
        assert!(report.installed.is_empty(), "{:?}", report.installed);
        assert!(
            report.kept.contains(&made.kept[0]),
            "the SDK is still older"
        );

        // The run installed the stand-in `@dcl/inspector`, so the folder is
        // an install of this CLI: the full install replaces the older SDK.
        let full_install = |dir: &Path| {
            let opts = InitOptions {
                dir: dir.to_path_buf(),
                project: None,
                yes: false,
                node_modules_only: true,
            };
            init_with(&opts, false).unwrap();
            let report = install_missing_node_modules(dir, true).unwrap();
            assert!(report.installed.is_empty(), "{:?}", report.installed);
            let sdk = report.kept.into_iter().find(|p| p.name == "@dcl/sdk");
            sdk.unwrap()
        };
        let sdk = full_install(&t.0);
        assert_eq!(Some(&sdk.version), sdk.archive_version.as_ref());
        assert_eq!(sdk.version, vendored_sdk_version());
        assert!(modules.join("my-lib/index.js").exists());

        // A full `@dcl/inspector` that another tool installed is a kept
        // package. The folder has no record and no stand-in, so the full
        // install keeps it too, and the SDK stays older.
        let other = crate::scene::Tmp::new("init-dry-run-other");
        for name in ["@dcl/sdk", "@dcl/inspector"] {
            other.write(
                &format!("node_modules/{name}/package.json"),
                r#"{"version":"7.0.0"}"#,
            );
        }
        let made = install_missing_node_modules(&other.0, false).unwrap();
        assert_eq!(made.kept.len(), 2, "{:?}", made.kept);
        let before = tree(&other.0);
        assert_eq!(full_install(&other.0).version, "7.0.0");
        assert_eq!(tree(&other.0), before);
    }

    /// The packages that `init --node-modules-only` replaces; the README
    /// names the same eleven.
    #[test]
    fn archive_packages_are_the_top_level_node_modules_entries() {
        let packages = archive_packages(&open_vendored_archive().unwrap());
        assert_eq!(
            Vec::from_iter(packages),
            [
                "@dcl/ecs",
                "@dcl/inspector",
                "@dcl/js-runtime",
                "@dcl/rpc",
                "@dcl/sdk",
                "@protobufjs/utf8",
                "long",
                "mitt",
                "protobufjs",
                "typescript",
                "ws",
            ]
        );
    }

    /// Both scaffold pins must name the blob's @dcl line (a manifest naming an
    /// older one would have the next npm install downgrade it), and the blob's
    /// @dcl/ecs must carry the fixes the line brought: the byteLength-scoped
    /// DataView (upstream #1460), the renderer-reserved entity-id guard (#1544)
    /// and the eight-byte network delete body the bevy engine's strict framing
    /// needs (#1595 — an overlay of ours on 7.27.0, upstream's own code on the
    /// auth-server line). The runtime chunk must also carry the peer-trust gate
    /// (`patch_sdk_peer_trust()` in scripts/blob_overlays.py), or every
    /// unflagged serverless-multiplayer scene stops syncing.
    #[test]
    fn blob_tracks_the_scaffold_pin_and_carries_the_ecs_fixes() {
        use std::io::Read;
        let scene: serde_json::Value = serde_json::from_str(SCENE_PACKAGE_JSON).unwrap();
        let pin = scene["devDependencies"]["@dcl/sdk"]
            .as_str()
            .unwrap()
            .to_string();
        for (name, raw) in [
            ("scene", SCENE_PACKAGE_JSON),
            ("smart-wearable", SW_PACKAGE_JSON),
        ] {
            let scaffold: serde_json::Value = serde_json::from_str(raw).unwrap();
            for dep in ["@dcl/sdk", "@dcl/js-runtime"] {
                assert_eq!(
                    scaffold["devDependencies"][dep], pin,
                    "the {name} scaffold's {dep} is off the blob's line"
                );
            }
        }

        let cursor = std::io::Cursor::new(VENDORED_NODE_MODULES);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let mut read = |rel: &str| {
            let mut s = String::new();
            archive
                .by_name(rel)
                .unwrap_or_else(|e| panic!("blob is missing {rel}: {e}"))
                .read_to_string(&mut s)
                .unwrap();
            s
        };
        for pkg in ["@dcl/sdk", "@dcl/ecs", "@dcl/js-runtime"] {
            let manifest: serde_json::Value =
                serde_json::from_str(&read(&format!("node_modules/{pkg}/package.json"))).unwrap();
            assert_eq!(
                manifest["version"], pin,
                "{pkg} is not on the scaffold's line"
            );
        }

        let byte_buffer = read("node_modules/@dcl/ecs/dist-cjs/serialization/ByteBuffer/index.js");
        assert_eq!(
            byte_buffer
                .matches("new DataView(this._buffer.buffer, this._buffer.byteOffset, this._buffer.byteLength)")
                .count(),
            2,
            "ByteBuffer must scope both DataViews to the buffer's byteLength"
        );
        assert!(
            !byte_buffer.contains("new DataView(this._buffer.buffer, oldOffset)"),
            "ByteBuffer still rebuilds its view from the pre-growth offset"
        );

        let entity = read("node_modules/@dcl/ecs/dist-cjs/engine/entity.js");
        assert!(
            entity.contains("function isReservedEntity("),
            "entity container lacks the renderer-reserved id guard"
        );
        assert!(
            entity.contains("number >= reservedStaticEntities && version < exports.MAX_U16"),
            "entity container still recycles renderer-reserved numbers"
        );

        let core = read("node_modules/@dcl/sdk/prebuilt/core.js");
        assert_eq!(
            core.matches(
                "new DataView(this._buffer.buffer,this._buffer.byteOffset,this._buffer.byteLength)"
            )
            .count(),
            2,
            "the prebuilt runtime chunk was not built from the fixed ByteBuffer"
        );

        let net_delete = read(
            "node_modules/@dcl/ecs/dist-cjs/serialization/crdt/network/deleteEntityNetwork.js",
        );
        assert!(
            net_delete.contains(
                "buf.writeUint32(types_1.CRDT_MESSAGE_HEADER_LENGTH + DeleteEntityNetwork.MESSAGE_HEADER_LENGTH);"
            ),
            "DeleteEntityNetwork.write does not declare the eight-byte body it writes (upstream #1595)"
        );
        assert!(
            !net_delete.contains("CRDT_MESSAGE_HEADER_LENGTH + 4"),
            "DeleteEntityNetwork.write still declares a 12-byte record"
        );
        let write = chunk_net_delete_write(&core);
        assert!(
            write.contains("writeUint32(8+") && write.contains(".MESSAGE_HEADER_LENGTH),"),
            "the prebuilt runtime chunk frames a network entity delete short: {write}"
        );
        assert!(
            !write.contains("writeUint32(12),"),
            "the prebuilt runtime chunk was built from the unpatched @dcl/ecs: {write}"
        );

        assert!(
            core.matches("__dclOneAuthoritative").count() >= 2,
            "the prebuilt runtime chunk lacks the peer-trust gate: an unflagged scene \
             would trust CRDT only from 'authoritative-server' (patch_sdk_peer_trust)"
        );
        assert!(
            core.contains("is only available on server-side scenes"),
            "the prebuilt runtime chunk carries no @dcl/sdk/server"
        );
    }

    /// The minified `DeleteEntityNetwork.write`. Rolldown mangles every local
    /// name, so the anchors are the two things it keeps: the namespace's
    /// `MESSAGE_HEADER_LENGTH=8` (its siblings set 4, 12, 16 or 20) and the
    /// string literal `read` throws; the write function sits between them.
    fn chunk_net_delete_write(core: &str) -> &str {
        let end = core
            .find("DeleteEntityNetwork tried to read another message type")
            .expect("core.js lacks the DeleteEntityNetwork namespace");
        let start = core[..end]
            .rfind("MESSAGE_HEADER_LENGTH=8;")
            .expect("core.js lacks DeleteEntityNetwork.MESSAGE_HEADER_LENGTH");
        &core[start..end]
    }
}
