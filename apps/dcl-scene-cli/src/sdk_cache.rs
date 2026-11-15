//! Reuse the source-built SDK chunk across builds: bundling it again costs
//! more than the rest of a build with nothing changed.
//!
//! The chunk is rebuilt unless a manifest written by the last successful
//! bundle still describes the tree. A manifest holds:
//!
//! - a digest of everything known before bundling: this binary, the
//!   production flag, the paths involved, the externals, the aliases with
//!   where they really point, the tsconfig chain and whether each of its
//!   `paths` targets exists;
//! - every file the bundler loaded. One under `node_modules` is compared by
//!   size and timestamps, any other (the generated registry entry, the
//!   composite slot, `~sdk/script-utils`) by content, since each build
//!   rewrites those;
//! - every `package.json` above a loaded file, by content: `exports`, `main`,
//!   `browser` and `sideEffects` change the bundle without touching a module;
//! - the package manager's record of the install ([`INSTALL_RECORDS`]), by
//!   content;
//! - the timestamps of every directory above a loaded file, of the
//!   `node_modules` beside each and of its scope directories, so a file or
//!   package that would now win a resolution is seen;
//! - the chunk itself, by size, timestamps and digest. The copy kept under
//!   `.dcl-one/` restores a chunk that was deleted or overwritten by the
//!   other profile.
//!
//! A timestamp only proves a file unchanged if it is older than the moment it
//! was recorded by more than the filesystem's resolution ([`RACY`]): a write
//! in the same tick leaves it as it was. So a manifest is not written while
//! any input is that young, and the chunk is compared by digest until it has
//! aged.
//!
//! Anything that cannot be read or resolved means no manifest, and the next
//! build bundles from source.

use crate::esbuild::{self, EsbuildOptions};
use crate::scene::Project;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Bumped when the manifest's meaning changes.
const FORMAT: u32 = 1;

/// What npm, pnpm and yarn keep in `node_modules` about what they installed.
const INSTALL_RECORDS: &[&str] = &[
    ".package-lock.json",
    ".modules.yaml",
    ".yarn-integrity",
    ".yarn-state.yml",
];

/// Coarser than any filesystem's timestamps (FAT's are two seconds apart).
const RACY: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Outcome {
    /// Bundled from source.
    Built,
    /// The chunk on disk is the one the manifest describes.
    Reused,
    /// The kept copy was put back where the chunk belongs.
    Restored,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct Manifest {
    fixed: String,
    files: Vec<Seen>,
    dirs: Vec<Seen>,
    out: Stat,
    out_hash: String,
    /// The chunk was seen with this digest after its timestamps had aged:
    /// from then on they vouch for it.
    settled: bool,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct Seen {
    path: String,
    is: Is,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
enum Is {
    Stat(Stat),
    Hash(String),
    Absent,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug, Clone)]
struct Stat {
    size: u64,
    mtime_ns: u128,
    /// Inode change time: a replacement that keeps size and mtime still
    /// moves it. Zero where the platform has none.
    ctime_ns: i128,
}

impl Stat {
    /// Was the file last touched before `moment`, by more than `margin`?
    fn older_than(&self, moment: SystemTime, margin: Duration) -> bool {
        let Ok(moment) = moment.duration_since(SystemTime::UNIX_EPOCH) else {
            return false;
        };
        let touched = i128::try_from(self.mtime_ns)
            .unwrap_or(i128::MAX)
            .max(self.ctime_ns);
        let limit = i128::try_from(moment.saturating_sub(margin).as_nanos()).unwrap_or(0);
        touched < limit
    }
}

fn stat(path: &Path) -> Option<Stat> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ns = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    #[cfg(unix)]
    let ctime_ns = {
        use std::os::unix::fs::MetadataExt;
        i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
    };
    #[cfg(not(unix))]
    let ctime_ns = 0;
    Some(Stat {
        // A directory's size is the filesystem's business, not its content's.
        size: if meta.is_dir() { 0 } else { meta.len() },
        mtime_ns,
        ctime_ns,
    })
}

pub(crate) fn hex(digest: impl AsRef<[u8]>) -> String {
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_file(path: &Path) -> Option<String> {
    Some(hex(Sha256::digest(std::fs::read(path).ok()?)))
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Where the manifest and the kept copy live: one slot per output path and
/// profile, so the dev and production chunks do not evict each other.
fn slot(project: &Project, opts: &EsbuildOptions) -> Result<(PathBuf, PathBuf)> {
    let dir = crate::scene::work_dir(&project.root)?;
    let mut h = Sha256::new();
    h.update(text(&opts.outfile));
    h.update([u8::from(opts.production)]);
    let name = &hex(h.finalize())[..12];
    Ok((
        dir.join(format!("sdk-chunk-{name}.json")),
        dir.join(format!("sdk-chunk-{name}.js")),
    ))
}

/// tsconfig.json and everything it extends, and where their `paths` point;
/// None when a link does not resolve.
fn tsconfig_chain(project: &Project, tsconfig: &Path) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut chain = Vec::new();
    let mut targets = Vec::new();
    let mut pending = vec![tsconfig.to_path_buf()];
    while let Some(current) = pending.pop() {
        if chain.contains(&current) {
            continue;
        }
        if chain.len() >= 16 {
            return None;
        }
        let doc =
            dcl_scene_authoring::jsjson::parse(&std::fs::read_to_string(&current).ok()?).ok()?;
        use dcl_scene_authoring::jsjson::JsValue;
        let extends: Vec<&str> = match doc.get("extends") {
            None => vec![],
            Some(JsValue::String(one)) => vec![one],
            Some(JsValue::Array(many)) => many.iter().map(|v| v.as_str()).collect::<Option<_>>()?,
            Some(_) => return None,
        };
        for next in extends {
            let target = match next.starts_with('.') || Path::new(next).is_absolute() {
                true => current.parent()?.join(next),
                false => project.root.join("node_modules").join(next),
            };
            let found = [
                target.clone(),
                PathBuf::from(format!("{}.json", target.display())),
                target.join("tsconfig.json"),
            ]
            .into_iter()
            .find(|p| p.is_file())?;
            pending.push(found);
        }
        targets.extend(path_targets(&doc, current.parent()?)?);
        chain.push(current);
    }
    Some((chain, targets))
}

/// Where a tsconfig's `compilerOptions.paths` point, each cut at its
/// wildcard: a target that is missing today takes over the specifier the
/// day it appears.
fn path_targets(doc: &dcl_scene_authoring::jsjson::JsValue, dir: &Path) -> Option<Vec<PathBuf>> {
    use dcl_scene_authoring::jsjson::JsValue;
    let Some(options) = doc.get("compilerOptions") else {
        return Some(vec![]);
    };
    let base = match options.get("baseUrl") {
        None => dir.to_path_buf(),
        Some(url) => dir.join(url.as_str()?),
    };
    let mut out = Vec::new();
    match options.get("paths") {
        None => {}
        Some(JsValue::Object(entries)) => {
            for (_, candidates) in entries {
                let JsValue::Array(candidates) = candidates else {
                    return None;
                };
                for candidate in candidates {
                    let candidate = candidate.as_str()?;
                    let fixed = candidate.split('*').next().unwrap_or(candidate);
                    out.push(base.join(fixed));
                }
            }
        }
        Some(_) => return None,
    }
    Some(out)
}

/// Everything that shapes the chunk and is known before bundling. None when
/// part of it cannot be read: the build then never reuses a chunk.
fn fixed_key(project: &Project, opts: &EsbuildOptions) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe_stat = stat(&exe)?;
    let mut parts: Vec<String> = vec![
        format!("format {FORMAT}"),
        format!("version {}", env!("CARGO_PKG_VERSION")),
        format!("exe {} {} {}", text(&exe), exe_stat.size, exe_stat.mtime_ns),
        format!("production {}", opts.production),
        format!("root {}", text(&project.root)),
        format!("outfile {}", text(&opts.outfile)),
        format!("entry {}", text(&opts.entrypoint)),
    ];
    let node_modules = project.root.join("node_modules");
    parts.push(format!(
        "node_modules {}",
        text(&std::fs::canonicalize(&node_modules).ok()?)
    ));
    for (name, target) in &opts.aliases {
        let real = std::fs::canonicalize(target).ok()?;
        parts.push(format!("alias {name} {} {}", text(target), text(&real)));
    }
    for external in &opts.externals {
        parts.push(format!("external {external}"));
    }
    let (chain, targets) = tsconfig_chain(project, &opts.tsconfig)?;
    for link in chain {
        parts.push(format!("tsconfig {} {}", text(&link), hash_file(&link)?));
    }
    for target in targets {
        let real = std::fs::canonicalize(&target).ok();
        parts.push(format!("path {} {real:?}", text(&target)));
    }
    let mut h = Sha256::new();
    for part in parts {
        h.update(part.as_bytes());
        h.update([0]);
    }
    Some(hex(h.finalize()))
}

/// What to remember about the files the bundler loaded.
fn snapshot(project: &Project, inputs: &[PathBuf]) -> Option<(Vec<Seen>, Vec<Seen>)> {
    let node_modules = project.root.join("node_modules");
    let mut files = Vec::new();
    let mut dirs = BTreeSet::new();
    let mut manifests = BTreeSet::new();
    let mut inputs: Vec<&PathBuf> = inputs.iter().collect();
    inputs.sort();
    inputs.dedup();
    for input in inputs {
        let installed = input.starts_with(&node_modules);
        files.push(Seen {
            path: text(input),
            is: match installed {
                true => Is::Stat(stat(input)?),
                false => Is::Hash(hash_file(input)?),
            },
        });
        if !installed {
            continue;
        }
        let mut dir = input.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(&node_modules)) {
            if !dirs.insert(d.to_path_buf()) {
                break;
            }
            manifests.insert(d.join("package.json"));
            dir = d.parent();
        }
    }
    // Every install rewrites its package manager's own record of the tree,
    // including one that only moves a package no loaded file sits under.
    for record in INSTALL_RECORDS {
        manifests.insert(node_modules.join(record));
    }
    for manifest in manifests {
        files.push(Seen {
            path: text(&manifest),
            is: match manifest.is_file() {
                true => Is::Hash(hash_file(&manifest)?),
                false => Is::Absent,
            },
        });
    }
    let mut seen_dirs = Vec::new();
    for dir in dirs {
        seen_dirs.push(seen_dir(&dir)?);
        if dir.file_name().is_some_and(|n| n == "node_modules") {
            seen_dirs.extend(scope_dirs(&dir)?);
            continue;
        }
        let nested = dir.join("node_modules");
        match nested.is_dir() {
            true => {
                seen_dirs.push(seen_dir(&nested)?);
                seen_dirs.extend(scope_dirs(&nested)?);
            }
            false => seen_dirs.push(Seen {
                path: text(&nested),
                is: Is::Absent,
            }),
        }
    }
    seen_dirs.sort_by(|a, b| a.path.cmp(&b.path));
    seen_dirs.dedup_by(|a, b| a.path == b.path);
    Some((files, seen_dirs))
}

fn seen_dir(dir: &Path) -> Option<Seen> {
    Some(Seen {
        path: text(dir),
        is: Is::Stat(stat(dir)?),
    })
}

/// The `@scope` directories of a `node_modules`: a package added to one
/// moves the scope's timestamp, not the `node_modules`'.
fn scope_dirs(node_modules: &Path) -> Option<Vec<Seen>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(node_modules).ok()? {
        let entry = entry.ok()?;
        if entry.file_name().to_string_lossy().starts_with('@') {
            out.push(seen_dir(&entry.path())?);
        }
    }
    Some(out)
}

fn still(seen: &Seen) -> bool {
    let path = Path::new(&seen.path);
    match &seen.is {
        Is::Stat(was) => stat(path).as_ref() == Some(was),
        Is::Hash(was) => hash_file(path).as_ref() == Some(was),
        Is::Absent => std::fs::symlink_metadata(path).is_err(),
    }
}

fn read_manifest(path: &Path) -> Option<Manifest> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Option<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(manifest).ok()?).ok()?;
    std::fs::rename(&tmp, path).ok()
}

/// Why the manifest does not allow reuse, or None when it does.
fn stale(manifest: &Manifest, fixed: &str) -> Option<String> {
    if manifest.fixed != fixed {
        return Some("the build inputs changed".to_string());
    }
    manifest
        .files
        .iter()
        .chain(&manifest.dirs)
        .find(|seen| !still(seen))
        .map(|seen| format!("{} changed", seen.path))
}

/// Put a valid chunk at `opts.outfile` without bundling, if the manifest allows.
fn reuse(manifest_path: &Path, kept: &Path, fixed: &str, outfile: &Path) -> Option<Outcome> {
    let mut manifest = read_manifest(manifest_path)?;
    if let Some(why) = stale(&manifest, fixed) {
        tracing::info!("sdk chunk not reused: {why}");
        return None;
    }
    if stat(outfile).as_ref() == Some(&manifest.out) {
        if manifest.settled {
            return Some(Outcome::Reused);
        }
        if hash_file(outfile).as_ref() == Some(&manifest.out_hash) {
            if manifest.out.older_than(SystemTime::now(), RACY) {
                manifest.settled = true;
                write_manifest(manifest_path, &manifest)?;
            }
            return Some(Outcome::Reused);
        }
    }
    if hash_file(kept)? != manifest.out_hash {
        tracing::info!("sdk chunk not reused: the kept copy does not match its manifest");
        return None;
    }
    std::fs::copy(kept, outfile).ok()?;
    manifest.out = stat(outfile)?;
    manifest.settled = false;
    write_manifest(manifest_path, &manifest)?;
    Some(Outcome::Restored)
}

/// Best-effort: a manifest that cannot be written only costs the next build
/// its shortcut.
fn record(
    project: &Project,
    (manifest_path, kept): (&Path, &Path),
    fixed: String,
    outfile: &Path,
    inputs: &[PathBuf],
    started: SystemTime,
    margin: Duration,
) -> Option<()> {
    let (files, dirs) = snapshot(project, inputs)?;
    // A file written while the bundler ran may have been read before the
    // write, and one written just before it can be written again without its
    // timestamps moving: either way the snapshot would not describe the
    // bytes the chunk was built from.
    let racy = files.iter().chain(&dirs).any(|seen| match &seen.is {
        Is::Stat(stat) => !stat.older_than(started, margin),
        _ => false,
    });
    if racy {
        tracing::info!("sdk chunk not cached: node_modules changed moments ago");
        return None;
    }
    let chunk = std::fs::read(outfile).ok()?;
    std::fs::write(kept, &chunk).ok()?;
    let manifest = Manifest {
        fixed,
        files,
        dirs,
        out: stat(outfile)?,
        out_hash: hex(Sha256::digest(&chunk)),
        settled: false,
    };
    write_manifest(manifest_path, &manifest)
}

/// Bundle the SDK chunk, or reuse the last one when nothing it was built
/// from has changed.
pub async fn bundle(project: &Project, opts: &EsbuildOptions) -> Result<Outcome> {
    bundle_with(project, opts, RACY).await
}

async fn bundle_with(
    project: &Project,
    opts: &EsbuildOptions,
    margin: Duration,
) -> Result<Outcome> {
    let (manifest_path, kept) = slot(project, opts)?;
    let fixed = fixed_key(project, opts);
    if let Some(outcome) = fixed
        .as_deref()
        .and_then(|fixed| reuse(&manifest_path, &kept, fixed, &opts.outfile))
    {
        return Ok(outcome);
    }
    let _ = std::fs::remove_file(&manifest_path);
    let started = SystemTime::now();
    let inputs = esbuild::bundle_tracked(project, opts).await?;
    let recorded = fixed.and_then(|fixed| {
        record(
            project,
            (&manifest_path, &kept),
            fixed,
            &opts.outfile,
            &inputs,
            started,
            margin,
        )
    });
    if recorded.is_none() {
        let _ = std::fs::remove_file(&kept);
    }
    Ok(Outcome::Built)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Tmp;

    struct Scene {
        tmp: Tmp,
        project: Project,
        opts: EsbuildOptions,
        inputs: Vec<PathBuf>,
        manifest: PathBuf,
        kept: PathBuf,
    }

    /// A scene tree with a bundled chunk and its recorded manifest; the
    /// bundler is stood in for by a fixed input list.
    fn scene(tag: &str) -> Scene {
        let tmp = Tmp::new(tag);
        tmp.write("scene.json", "{}");
        tmp.write(
            "tsconfig.json",
            r#"{ "extends": "@dcl/sdk/types/tsconfig.ecs7.json", "include": ["src/**/*.ts"] }"#,
        );
        tmp.write(
            "node_modules/@dcl/sdk/types/tsconfig.ecs7.json",
            r#"{ "compilerOptions": { "strict": true } }"#,
        );
        tmp.write(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
        );
        tmp.write("node_modules/@dcl/sdk/index.js", "export const a = 1\n");
        tmp.write(
            "node_modules/@dcl/sdk/internal/x.js",
            "export const x = 1\n",
        );
        tmp.write("node_modules/react/package.json", r#"{"name":"react"}"#);
        tmp.write("node_modules/react/index.js", "module.exports = {}\n");
        tmp.write(".dcl-one/sdk-runtime-entry.js", "module.exports = {}\n");
        tmp.write(".dcl-one/script-utils.js", "export const stub = 1\n");
        tmp.write("bin/sdk-runtime.js", "/* the chunk */\n");
        let root = tmp.0.clone();
        let project = Project {
            root: root.clone(),
            scene_json: serde_json::json!({}),
        };
        let opts = EsbuildOptions {
            production: false,
            entrypoint: root.join(".dcl-one/sdk-runtime-entry.js"),
            outfile: root.join("bin/sdk-runtime.js"),
            tsconfig: root.join("tsconfig.json"),
            aliases: vec![
                ("@dcl/sdk".to_string(), root.join("node_modules/@dcl/sdk")),
                (
                    "~sdk/script-utils".to_string(),
                    root.join(".dcl-one/script-utils.js"),
                ),
            ],
            externals: vec![],
        };
        let inputs = vec![
            root.join(".dcl-one/sdk-runtime-entry.js"),
            root.join(".dcl-one/script-utils.js"),
            root.join("node_modules/@dcl/sdk/index.js"),
            root.join("node_modules/@dcl/sdk/internal/x.js"),
            root.join("node_modules/react/index.js"),
        ];
        let (manifest, kept) = slot(&project, &opts).unwrap();
        let scene = Scene {
            tmp,
            project,
            opts,
            inputs,
            manifest,
            kept,
        };
        scene.record();
        scene
    }

    impl Scene {
        fn record(&self) {
            record(
                &self.project,
                (&self.manifest, &self.kept),
                fixed_key(&self.project, &self.opts).unwrap(),
                &self.opts.outfile,
                &self.inputs,
                SystemTime::now() + Duration::from_secs(3600),
                RACY,
            )
            .expect("the manifest records");
            self.tick();
        }

        /// Wait for the filesystem's clock to move, so a change made next
        /// cannot share a timestamp with what was just recorded. A real
        /// build gets the same guarantee from [`RACY`].
        fn tick(&self) {
            let probe = self.at("probe");
            std::fs::write(&probe, "").unwrap();
            let before = stat(&probe).unwrap();
            while stat(&probe).unwrap() == before {
                std::thread::sleep(Duration::from_millis(1));
                std::fs::write(&probe, "").unwrap();
            }
        }

        fn opts(&self) -> EsbuildOptions {
            EsbuildOptions {
                production: self.opts.production,
                entrypoint: self.opts.entrypoint.clone(),
                outfile: self.opts.outfile.clone(),
                tsconfig: self.opts.tsconfig.clone(),
                aliases: self.opts.aliases.clone(),
                externals: self.opts.externals.clone(),
            }
        }

        fn reuse(&self) -> Option<Outcome> {
            let fixed = fixed_key(&self.project, &self.opts)?;
            reuse(&self.manifest, &self.kept, &fixed, &self.opts.outfile)
        }

        /// Rewrite a file with other bytes of the same length, as an
        /// upgrade that happens to keep a file's size would.
        fn rewrite(&self, rel: &str, contents: &str) {
            let path = self.tmp.0.join(rel);
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                contents.len() as u64,
                "the test keeps {rel}'s size"
            );
            std::fs::write(path, contents).unwrap();
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.tmp.0.join(rel)
        }
    }

    #[test]
    fn an_untouched_tree_reuses_the_chunk() {
        let s = scene("sdk-cache-reuse");
        assert_eq!(s.reuse(), Some(Outcome::Reused));
        assert_eq!(s.reuse(), Some(Outcome::Reused));
    }

    #[test]
    fn generated_inputs_are_compared_by_content_not_by_time() {
        let s = scene("sdk-cache-generated");
        s.tmp
            .write(".dcl-one/sdk-runtime-entry.js", "module.exports = {}\n");
        assert_eq!(
            s.reuse(),
            Some(Outcome::Reused),
            "every build rewrites it with the same bytes"
        );
        s.tmp.write(
            ".dcl-one/sdk-runtime-entry.js",
            "module.exports = { '@dcl/sdk/server': 1 }\n",
        );
        assert_eq!(s.reuse(), None, "another registry is another chunk");
    }

    #[test]
    fn a_changed_script_utils_rebuilds() {
        let s = scene("sdk-cache-utils");
        s.tmp
            .write(".dcl-one/script-utils.js", "export const real = 1\n");
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn a_changed_module_rebuilds_even_at_the_same_size() {
        let s = scene("sdk-cache-module");
        s.rewrite(
            "node_modules/@dcl/sdk/internal/x.js",
            "export const x = 2\n",
        );
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn a_changed_package_manifest_rebuilds_even_at_the_same_size() {
        let s = scene("sdk-cache-package");
        s.rewrite(
            "node_modules/@dcl/sdk/package.json",
            r#"{"name":"@dcl/sdk","version":"7.0.1"}"#,
        );
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn a_removed_module_rebuilds() {
        let s = scene("sdk-cache-removed");
        std::fs::remove_file(s.at("node_modules/react/index.js")).unwrap();
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn a_file_that_could_shadow_a_resolution_rebuilds() {
        let s = scene("sdk-cache-shadow-file");
        s.tmp.write(
            "node_modules/@dcl/sdk/internal/x.ts",
            "export const x = 3\n",
        );
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn a_nested_node_modules_that_could_shadow_a_package_rebuilds() {
        let s = scene("sdk-cache-shadow-nested");
        s.tmp.write(
            "node_modules/@dcl/sdk/node_modules/react/index.js",
            "module.exports = { nested: true }\n",
        );
        assert_eq!(s.reuse(), None, "a nested node_modules appeared");
        s.record();
        assert_eq!(s.reuse(), Some(Outcome::Reused));
        s.tmp.write(
            "node_modules/@dcl/sdk/node_modules/@dcl/ecs/index.js",
            "export const nested = true\n",
        );
        assert_eq!(s.reuse(), None, "a scope appeared inside it");
        s.record();
        s.tmp.write(
            "node_modules/@dcl/sdk/node_modules/@dcl/ecs-math/index.js",
            "export const nested = true\n",
        );
        assert_eq!(s.reuse(), None, "a package appeared inside that scope");
    }

    #[test]
    fn a_newly_installed_package_rebuilds() {
        let s = scene("sdk-cache-install");
        s.tmp.write(
            "node_modules/@dcl/asset-packs/package.json",
            r#"{"name":"@dcl/asset-packs"}"#,
        );
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn the_fixed_key_follows_every_build_setting() {
        let s = scene("sdk-cache-fixed");
        let base = fixed_key(&s.project, &s.opts).unwrap();
        let with = |change: &dyn Fn(&mut EsbuildOptions)| {
            let mut opts = s.opts();
            change(&mut opts);
            fixed_key(&s.project, &opts).unwrap()
        };
        assert_eq!(with(&|_| {}), base);
        assert_ne!(with(&|o| o.production = true), base);
        assert_ne!(with(&|o| o.outfile = s.at("bin/other.js")), base);
        assert_ne!(with(&|o| o.externals.push("react".to_string())), base);
        assert_ne!(
            with(&|o| o.aliases[0].1 = s.at("node_modules/react")),
            base,
            "an alias that points elsewhere"
        );
        assert_ne!(
            with(&|o| {
                o.aliases.pop();
            }),
            base
        );

        s.tmp.write(
            "tsconfig.json",
            r#"{ "extends": "@dcl/sdk/types/tsconfig.ecs7.json", "compilerOptions": { "paths": { "react": ["x"] } } }"#,
        );
        let edited = fixed_key(&s.project, &s.opts).unwrap();
        assert_ne!(edited, base, "tsconfig.json itself");
        s.tmp.write(
            "node_modules/@dcl/sdk/types/tsconfig.ecs7.json",
            r#"{ "compilerOptions": { "strict": false } }"#,
        );
        assert_ne!(
            fixed_key(&s.project, &s.opts).unwrap(),
            edited,
            "a tsconfig it extends"
        );
    }

    #[test]
    fn a_tsconfig_path_target_that_appears_changes_the_fixed_key() {
        let s = scene("sdk-cache-paths");
        s.tmp.write(
            "tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": {
                "@dcl/asset-packs/*": ["node_modules/@dcl/inspector/node_modules/@dcl/asset-packs/*"]
            } } }"#,
        );
        let before = fixed_key(&s.project, &s.opts).unwrap();
        s.tmp.write(
            "node_modules/@dcl/inspector/node_modules/@dcl/asset-packs/dist/index.js",
            "export const nested = true\n",
        );
        assert_ne!(fixed_key(&s.project, &s.opts).unwrap(), before);
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn a_reinstall_that_rewrites_the_install_record_rebuilds() {
        let s = scene("sdk-cache-install-record");
        s.tmp
            .write("node_modules/.package-lock.json", r#"{"packages":{}}"#);
        assert_eq!(s.reuse(), None, "node_modules gained its record");
        s.record();
        assert_eq!(s.reuse(), Some(Outcome::Reused));
        s.rewrite("node_modules/.package-lock.json", r#"{"packages":[]}"#);
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn an_unreadable_tsconfig_chain_is_never_cached() {
        let s = scene("sdk-cache-chain");
        s.tmp
            .write("tsconfig.json", r#"{ "extends": "./missing.json" }"#);
        assert!(fixed_key(&s.project, &s.opts).is_none());
        s.tmp.write(
            "tsconfig.json",
            r#"{ "extends": ["./a.json", "@dcl/sdk/types/tsconfig.ecs7.json"] }"#,
        );
        assert!(
            fixed_key(&s.project, &s.opts).is_none(),
            "a.json is missing"
        );
        s.tmp.write("a.json", "{}");
        assert!(fixed_key(&s.project, &s.opts).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn an_alias_whose_link_moves_changes_the_fixed_key() {
        let s = scene("sdk-cache-symlink");
        s.tmp.write("store/a/index.js", "export const a = 1\n");
        s.tmp.write("store/b/index.js", "export const a = 1\n");
        let link = s.at("node_modules/linked");
        std::os::unix::fs::symlink(s.at("store/a"), &link).unwrap();
        let mut opts = s.opts();
        opts.aliases = vec![("linked".to_string(), link.clone())];
        let before = fixed_key(&s.project, &opts).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(s.at("store/b"), &link).unwrap();
        assert_ne!(fixed_key(&s.project, &opts).unwrap(), before);
    }

    #[test]
    fn a_missing_or_foreign_chunk_is_restored_from_the_kept_copy() {
        let s = scene("sdk-cache-restore");
        std::fs::remove_file(&s.opts.outfile).unwrap();
        assert_eq!(s.reuse(), Some(Outcome::Restored));
        assert_eq!(
            std::fs::read_to_string(&s.opts.outfile).unwrap(),
            "/* the chunk */\n"
        );
        assert_eq!(s.reuse(), Some(Outcome::Reused));

        std::fs::write(&s.opts.outfile, "/* another profile's chunk */\n").unwrap();
        assert_eq!(s.reuse(), Some(Outcome::Restored));
        assert_eq!(
            std::fs::read_to_string(&s.opts.outfile).unwrap(),
            "/* the chunk */\n"
        );
    }

    #[test]
    fn a_damaged_kept_copy_is_not_restored() {
        let s = scene("sdk-cache-damaged");
        std::fs::remove_file(&s.opts.outfile).unwrap();
        std::fs::write(&s.kept, "/* not chunk */\n").unwrap();
        assert_eq!(s.reuse(), None);
    }

    #[test]
    fn profiles_and_output_paths_keep_separate_slots() {
        let s = scene("sdk-cache-slots");
        let mut production = s.opts();
        production.production = true;
        assert_ne!(slot(&s.project, &production).unwrap().0, s.manifest);
        let mut release = s.opts();
        release.outfile = s.at(".dcl-one/release/bin/sdk-runtime.js");
        assert_ne!(slot(&s.project, &release).unwrap().0, s.manifest);
    }

    #[test]
    fn inputs_too_young_for_their_timestamps_to_mean_anything_are_not_recorded() {
        let s = scene("sdk-cache-racy");
        std::fs::remove_file(&s.manifest).unwrap();
        let record_at = |started: SystemTime| {
            record(
                &s.project,
                (&s.manifest, &s.kept),
                fixed_key(&s.project, &s.opts).unwrap(),
                &s.opts.outfile,
                &s.inputs,
                started,
                RACY,
            )
        };
        assert!(
            record_at(SystemTime::now() - Duration::from_secs(3600)).is_none(),
            "written after the bundle started"
        );
        assert!(
            record_at(SystemTime::now()).is_none(),
            "written just before it started"
        );
        assert!(!s.manifest.exists());
        assert!(record_at(SystemTime::now() + RACY + RACY).is_some());
    }

    #[test]
    fn a_young_chunk_is_compared_by_digest_until_it_settles() {
        let s = scene("sdk-cache-settle");
        let manifest = || read_manifest(&s.manifest).unwrap();
        assert_eq!(s.reuse(), Some(Outcome::Reused));
        assert!(!manifest().settled, "the chunk was written a moment ago");

        let now = SystemTime::now();
        assert!(!manifest().out.older_than(now, RACY));
        assert!(manifest().out.older_than(now + RACY + RACY, RACY));

        let mut settled = manifest();
        settled.settled = true;
        write_manifest(&s.manifest, &settled).unwrap();
        assert_eq!(s.reuse(), Some(Outcome::Reused));
        s.tick();
        std::fs::write(&s.opts.outfile, "/* THE chunk */\n").unwrap();
        assert_eq!(
            s.reuse(),
            Some(Outcome::Restored),
            "a settled chunk is still checked by its timestamps"
        );
        assert!(!manifest().settled);
    }

    /// The real bundler, on a tree small enough to read: what it reports as
    /// loaded is what decides the next build.
    #[cfg(feature = "rolldown")]
    #[tokio::test]
    async fn a_bundled_chunk_is_reused_until_a_module_it_was_built_from_changes() {
        let s = scene("sdk-cache-bundler");
        std::fs::remove_file(&s.manifest).unwrap();
        s.tmp.write(
            ".dcl-one/sdk-runtime-entry.js",
            "module.exports = { sdk: require('@dcl/sdk'), utils: require('~sdk/script-utils') }\n",
        );
        s.tmp.write(
            "node_modules/@dcl/sdk/index.js",
            "export { x } from './internal/x.js'\n",
        );
        s.tmp.write(
            "node_modules/@dcl/sdk/internal/x.js",
            "export const x = 'first'\n",
        );
        s.tick();
        let chunk = || std::fs::read_to_string(&s.opts.outfile).unwrap();
        let build = |opts: EsbuildOptions| {
            let project = s.project.clone();
            async move {
                bundle_with(&project, &opts, Duration::ZERO)
                    .await
                    .expect("the chunk bundles")
            }
        };

        assert_eq!(build(s.opts()).await, Outcome::Built);
        assert!(chunk().contains("'first'") || chunk().contains("\"first\""));
        let files = read_manifest(&s.manifest).expect("recorded").files;
        for rel in [
            ".dcl-one/sdk-runtime-entry.js",
            ".dcl-one/script-utils.js",
            "node_modules/@dcl/sdk/index.js",
            "node_modules/@dcl/sdk/internal/x.js",
            "node_modules/@dcl/sdk/package.json",
        ] {
            assert!(
                files.iter().any(|f| Path::new(&f.path) == s.at(rel)),
                "{rel} is an input"
            );
        }
        assert_eq!(build(s.opts()).await, Outcome::Reused);

        s.tick();
        s.rewrite(
            "node_modules/@dcl/sdk/internal/x.js",
            "export const x = 'again'\n",
        );
        s.tick();
        assert_eq!(build(s.opts()).await, Outcome::Built);
        assert!(chunk().contains("again") && !chunk().contains("first"));
        let development = chunk();

        std::fs::remove_file(&s.opts.outfile).unwrap();
        assert_eq!(build(s.opts()).await, Outcome::Restored);
        assert_eq!(chunk(), development);

        let mut production = s.opts();
        production.production = true;
        assert_eq!(build(production).await, Outcome::Built);
        assert_ne!(chunk(), development, "the production chunk took its place");
        assert_eq!(build(s.opts()).await, Outcome::Restored);
        assert_eq!(chunk(), development);

        s.tick();
        s.tmp.write(
            ".dcl-one/script-utils.js",
            "export const real = 'runtime'\n",
        );
        assert_eq!(build(s.opts()).await, Outcome::Built);
        assert!(chunk().contains("runtime"));
    }
}
