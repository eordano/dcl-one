//! The SDK7 scene build without the CLI around it: what `dcl-scene-cli build
//! --built-in --skip-type-check` does over the prebuilt SDK chunks, as a
//! library that also compiles to `wasm32-wasip1`. The sites publish flow runs
//! that wasm in a web worker over an in-memory WASI file system, so a project
//! opened in the browser publishes its own code rather than a stand-in.
//!
//! # The wasm build
//!
//! `cargo build --release -p dcl-scene-build --bin scene-build --target
//! wasm32-wasip1` from `sdk/`, with `RUSTFLAGS="--cfg tokio_unstable"`
//! (`deploy/nix/scene-build-wasm.nix` is the reproducible form):
//!
//! * `wasm32-wasip1`, single threaded. rolldown's `target_family = "wasm"`
//!   paths run its rayon and `spawn_blocking` work on the calling thread, so
//!   the module needs no threads, SharedArrayBuffer or cross-origin isolation.
//!   `wasm32-unknown-unknown` has no `std::fs`, which the next point rules out.
//! * `--cfg tokio_unstable`: rolldown_plugin_utils enables tokio's `fs`
//!   feature, which tokio refuses to compile for wasm without it. The scene
//!   build never reaches that code.
//! * Size, rust 1.98.1 and rolldown 1.2.11 with the SDK embedded: 14,270,599
//!   bytes release, 9,991,983 after `wasm-opt -Oz` (binaryen 129, 3,393,067
//!   gzipped, 2,280,724 brotli). It imports 23 `wasi_snapshot_preview1`
//!   functions and nothing else. Through the sites WASI shim it builds the
//!   sdk7-scene-template in about 90 ms once compiled, and its `bin/*` and
//!   `main.crdt` match the native dcl-scene-cli build byte for byte in
//!   production and modulo the root path in development sourcemaps
//!   (`scene-build.test.ts` in sites).
//!
//! Files go through `std::fs` on purpose. rolldown can bundle over its
//! in-memory `MemoryFileSystem`, but `TsConfig::Manual` resolves the tsconfig
//! `extends` chain through a resolver fixed to `OsFileSystem`, and every scene's
//! tsconfig extends `@dcl/sdk/types/tsconfig.ecs7.json`. So the host supplies
//! the file system instead: node's `node:wasi` in tests, the worker's WASI shim
//! (sites `creator-hub/scene-build/wasi.ts`) in the browser.
//!
//! The SDK is not an input: [`install_sdk`] writes the `@dcl/sdk` embedded at
//! compile time (dcl-scene-cli's vendored one, see `build.rs`) into a project
//! that has none, so a browser feeds only the project's files.
//!
//! # Parity
//!
//! Each step below names the dcl-scene-cli function it mirrors, and
//! `tests/parity.rs` builds dcl-scene-cli's golden fixtures with both and
//! compares every output. Left out, because a browser has no use or no way
//! for them: the tsc type check, `package.json` `scripts.build`, custom entry
//! points, the source-path SDK chunk (a non-prebuilt `@dcl/sdk` is refused) and
//! the node data-layer `main.crdt` fallback (reported as
//! [`MainCrdt::Unsupported`]).

use anyhow::{Context, Result, anyhow, bail};
use dcl_scene_authoring::composite_norm::CompositeNormalizer;
use dcl_scene_authoring::crdt_gen::{self, GenError};
use dcl_scene_authoring::{
    COMPOSITE_FILE_MAX_BYTES, SceneFiles, entity_names, find_composites, is_pruned_dir,
};
use rolldown::Bundler;
use rolldown_common::{
    BundlerOptions, BundlerTransformOptions, ChecksOptions, CodeSplittingMode, CommentsOptions,
    Either, InputItem, IsExternal, OutputFormat, Platform, RawMinifyOptions, ResolveOptions,
    SourceMapType, TsConfig,
};
use rolldown_utils::indexmap::FxIndexMap;
use rolldown_utils::pattern_filter::StringOrRegex;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/sdk_files.rs"));

/// The loader stub dcl-scene-cli writes as `main`; shared, not copied.
const LOADER_TEMPLATE: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/apps/dcl-scene-cli/src/templates/split-loader.js"
));

const SDK_DIR: &str = "node_modules/@dcl/sdk";
const SDK_CORE: &str = "node_modules/@dcl/sdk/prebuilt/core.js";
const SDK_SMART: &str = "node_modules/@dcl/sdk/prebuilt/smart.js";
const WORK_DIR: &str = ".dcl-one";

/// dcl-scene-cli `rolldown_backend::externals`' fixed head.
const ALWAYS_EXTERNAL: &[&str] = &["~system/*", "@dcl/inspector", "@dcl/inspector/*"];

/// dcl-scene-cli `split::scene_externals` over a prebuilt SDK: everything the
/// core chunk owns, the two generated `~sdk/*` modules, and asset-packs (the
/// smart-item chunk's).
const SCENE_EXTERNALS: &[&str] = &[
    "@dcl/sdk",
    "@dcl/sdk/*",
    "@dcl/ecs",
    "@dcl/ecs/*",
    "@dcl/ecs-math",
    "@dcl/ecs-math/*",
    "@dcl/react-ecs",
    "@dcl/react-ecs/*",
    "react",
    "react/*",
    "~sdk/all-composites",
    "~sdk/script-utils",
    "@dcl/asset-packs",
    "@dcl/asset-packs/*",
];

pub struct Options {
    pub production: bool,
    pub ignore_composite: bool,
}

/// What became of `main.crdt` (dcl-scene-cli `data_layer::regenerate_main_crdt`).
#[derive(Debug, PartialEq)]
pub enum MainCrdt {
    /// Regenerated from this many composites.
    Native(u64),
    /// No composite, or `ignore_composite`: left as it was.
    Skipped,
    /// The native encoder does not cover a component the scene uses;
    /// dcl-scene-cli falls back to the node data-layer, which has no browser
    /// build, so the project's own `main.crdt` (if any) is left as it was.
    Unsupported(String),
}

pub struct Report {
    /// scene.json `main`, scene-relative.
    pub main: String,
    /// Every file this build wrote that belongs in a deployment, scene-relative.
    pub outputs: Vec<String>,
    pub smart_items: bool,
    pub max_composite_entity: u32,
    pub main_crdt: MainCrdt,
    /// Imports rolldown left as a bare `require`: the split loader serves only
    /// `~system/*` and the SDK registry, so the scene throws when the module
    /// that holds one loads.
    pub unresolved: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn to_json(&self) -> Value {
        let main_crdt = match &self.main_crdt {
            MainCrdt::Native(n) => json!({ "native": n }),
            MainCrdt::Skipped => json!("skipped"),
            MainCrdt::Unsupported(why) => json!({ "unsupported": why }),
        };
        json!({
            "ok": true,
            "main": self.main,
            "outputs": self.outputs,
            "smartItems": self.smart_items,
            "maxCompositeEntity": self.max_composite_entity,
            "mainCrdt": main_crdt,
            "unresolved": self.unresolved,
            "warnings": self.warnings,
        })
    }
}

/// Write the embedded `@dcl/sdk` under `root/node_modules` unless the project
/// already has one. True when it wrote.
pub fn install_sdk(root: &Path) -> Result<bool> {
    if root.join(SDK_DIR).exists() {
        return Ok(false);
    }
    for (rel, bytes) in SDK_FILES {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(true)
}

/// Build the scene at `root` (an absolute path) in place. Blocking: rolldown
/// runs on a current-thread tokio runtime, the only kind wasm32-wasip1 has.
pub fn build(root: &Path, opts: &Options) -> Result<Report> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .context("starting the tokio runtime")?
        .block_on(build_async(root, opts))
}

async fn build_async(root: &Path, opts: &Options) -> Result<Report> {
    let mut warnings = Vec::new();
    let scene_json = load_scene_json(root)?;
    let main = main_output(root, &scene_json)?;
    let tsconfig = root.join("tsconfig.json");
    if !tsconfig.is_file() {
        bail!(
            "this scene has no tsconfig.json; create one containing {{ \"extends\": \"@dcl/sdk/types/tsconfig.ecs7.json\" }}"
        );
    }
    if !root.join(SDK_CORE).is_file() {
        bail!(
            "{SDK_DIR} has no prebuilt runtime chunk ({SDK_CORE}); this build covers the vendored SDK only (an npm-installed one needs dcl-scene-cli)"
        );
    }
    let outfile = root.join(&main);
    if let Some(dir) = outfile.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let scene = SceneDir::walk(root, &mut warnings);
    let names = match opts.ignore_composite {
        true => Default::default(),
        false => entity_names::collect(&scene),
    };

    // entrypoint::generate (split, generated entrypoint). Its script-utils.js
    // and split::write_generated's files are left out: the scene chunk keeps
    // `~sdk/script-utils` external, and the others feed the source-path SDK chunk.
    let work = root.join(WORK_DIR);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    write_new(&work.join(".gitignore"), "*\n")?;
    let user_entry = root.join("src/index.ts");
    let safe_entry = serde_json::to_string(&user_entry.display().to_string().replace('\\', "/"))?;
    write(
        &work.join("all-composites.js"),
        all_composites(&scene, opts.ignore_composite)?,
    )?;
    write(&work.join("sdk-boot.js"), SDK_BOOT)?;
    let entrypoint = work.join("entrypoint.ts");
    write(
        &entrypoint,
        entrypoint_code(&safe_entry, is_editor_scene(root)),
    )?;
    let max_composite_entity = match opts.ignore_composite {
        true => 0,
        false => scan_max_composite_entity(&scene),
    };

    let mut outputs = Vec::new();
    // entity_names::write: before bundling, since a scene may import it.
    if !names.is_empty() {
        let rendered = entity_names::render(&names);
        let path = root.join(entity_names::OUTPUT_PATH);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(rendered.as_str()) {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            write(&path, rendered)?;
        }
        outputs.push(entity_names::OUTPUT_PATH.to_string());
    }

    // build::emit_chunks over a prebuilt SDK.
    let sdk_rel = beside(&main, "sdk-runtime.js");
    let scene_rel = beside(&main, "scene.js");
    let smart_rel = beside(&main, "sdk-smart-items.js");
    copy(&root.join(SDK_CORE), &root.join(&sdk_rel))?;
    outputs.push(sdk_rel.clone());
    let unresolved = bundle(
        root,
        &entrypoint,
        &root.join(&scene_rel),
        &tsconfig,
        opts.production,
        &mut warnings,
    )
    .await?;
    outputs.push(scene_rel.clone());
    let scene_code = std::fs::read_to_string(root.join(&scene_rel)).unwrap_or_default();
    let smart_items = is_editor_scene(root)
        || scene_code.contains("require(\"@dcl/asset-packs")
        || scene_code.contains("require('@dcl/asset-packs");
    if smart_items {
        if !root.join(SDK_SMART).is_file() {
            bail!(
                "this scene uses smart items but {SDK_DIR} has no smart-item chunk ({SDK_SMART})"
            );
        }
        copy(&root.join(SDK_SMART), &root.join(&smart_rel))?;
        outputs.push(smart_rel.clone());
    } else {
        let _ = std::fs::remove_file(root.join(&smart_rel));
    }

    // split::Loader::write.
    let mp = scene_json
        .get("authoritativeMultiplayer")
        .and_then(Value::as_bool)
        == Some(true);
    let loader = LOADER_TEMPLATE
        .replace("__DCL_SCENE_CLI_CHUNK__", &sdk_rel)
        .replace(
            "__DCL_ONE_SMART_CHUNK__",
            if smart_items { &smart_rel } else { "" },
        )
        .replace("__DCL_ONE_SCENE_CHUNK__", &scene_rel)
        .replace(
            "__DCL_ONE_MAX_COMPOSITE_ENTITY__",
            &max_composite_entity.to_string(),
        )
        .replace("__DCL_ONE_MP__", if mp { "true" } else { "false" });
    let loader = match opts.production {
        true => dcl_scene_authoring::compact_loader(&loader),
        false => loader,
    };
    write(&outfile, loader)?;
    outputs.push(main.clone());

    // data_layer::regenerate_main_crdt, without the node fallback.
    let main_crdt = match opts.ignore_composite {
        true => MainCrdt::Skipped,
        false => match crdt_gen::generate(&scene) {
            Ok(None) => MainCrdt::Skipped,
            Ok(Some(generated)) => {
                write(&root.join("main.crdt"), &generated.bytes)?;
                outputs.push("main.crdt".to_string());
                MainCrdt::Native(generated.composites)
            }
            Err(GenError::Unsupported(why)) => {
                warnings.push(format!(
                    "main.crdt was not regenerated: {why} (dcl-scene-cli covers this scene through the node data-layer)"
                ));
                MainCrdt::Unsupported(why)
            }
            Err(GenError::Invalid(why)) => {
                bail!("main.crdt regeneration failed: {why}")
            }
        },
    };

    outputs.sort();
    outputs.dedup();
    Ok(Report {
        main,
        outputs,
        smart_items,
        max_composite_entity,
        main_crdt,
        unresolved,
        warnings,
    })
}

fn load_scene_json(root: &Path) -> Result<Value> {
    let path = root.join("scene.json");
    let raw = std::fs::read(&path).map_err(|e| {
        anyhow!(
            "this directory is not a Decentraland scene: {} ({e})",
            path.display()
        )
    })?;
    serde_json::from_slice(&raw).context("scene.json is not valid JSON")
}

/// dcl-scene-cli `Project::main_output`, plus the relative-path rule its custom
/// build applies: the bundle must land inside the scene.
fn main_output(root: &Path, scene_json: &Value) -> Result<String> {
    let main = scene_json
        .get("main")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if main.is_empty() {
        bail!("scene.json is missing \"main\"; add \"main\": \"bin/index.js\"");
    }
    if root.join(main).is_dir() {
        bail!("scene.json \"main\" points at \"{main}\", which is a directory");
    }
    if !main.ends_with(".js") {
        bail!("scene.json \"main\" must be a .js bundle path (got \"{main}\")");
    }
    if Path::new(main)
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("scene.json \"main\" must be a relative path inside the scene (got \"{main}\")");
    }
    Ok(main.to_string())
}

fn beside(main: &str, name: &str) -> String {
    match main.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_string(),
    }
}

/// dcl-scene-cli `Project::is_editor_scene`.
fn is_editor_scene(root: &Path) -> bool {
    let Ok(raw) = std::fs::read(root.join("assets/scene/main.composite")) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    json.get("components")
        .and_then(Value::as_array)
        .is_some_and(|comps| {
            comps.iter().any(|c| {
                c.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.starts_with("asset-packs::") && n != "asset-packs::Script")
            })
        })
}

/// dcl-scene-cli `entrypoint::write_all_composites`: only a root-level
/// `main.composite` is inlined.
fn all_composites(scene: &SceneDir, ignore: bool) -> Result<String> {
    let mut lines = Vec::new();
    if !ignore {
        let mut normalizer = CompositeNormalizer::new();
        for rel in find_composites(scene) {
            if rel != "main.composite" {
                continue;
            }
            let normalized = scene
                .read(&rel)
                .ok_or_else(|| anyhow!("cannot read {rel}"))
                .and_then(|raw| String::from_utf8(raw).map_err(anyhow::Error::from))
                .and_then(|raw| normalizer.normalize(&raw));
            match normalized {
                Ok(json) => lines.push(format!("'{rel}':{json}")),
                Err(err) => bail!(
                    "{rel} could not be loaded, so the scene would build with no content from the editor: {err:#}"
                ),
            }
        }
    }
    Ok(format!(
        "export const compositeFromLoader = {{{}}}",
        lines.join(",")
    ))
}

/// dcl-scene-cli `entrypoint::scan_max_composite_entity`.
fn scan_max_composite_entity(scene: &SceneDir) -> u32 {
    let mut max = 0u32;
    for rel in find_composites(scene) {
        let Some(raw) = scene.read(&rel) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        let comps = json
            .get("components")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for comp in comps {
            let Some(data) = comp.get("data").and_then(Value::as_object) else {
                continue;
            };
            for key in data.keys() {
                if let Ok(id) = key.parse::<u64>() {
                    max = max.max((id & 0xffff) as u32);
                }
            }
        }
    }
    max
}

/// dcl-scene-cli `rolldown_backend::run` for the scene chunk; returns the
/// unresolved imports.
async fn bundle(
    root: &Path,
    entry: &Path,
    outfile: &Path,
    tsconfig: &Path,
    production: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>> {
    let external = ALWAYS_EXTERNAL
        .iter()
        .chain(SCENE_EXTERNALS)
        .map(|p| external_pattern(p))
        .collect::<Result<Vec<_>>>()?;
    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("index".into()),
            import: entry.display().to_string(),
        }]),
        cwd: Some(root.to_path_buf()),
        file: Some(outfile.display().to_string()),
        code_splitting: Some(CodeSplittingMode::Bool(false)),
        format: Some(OutputFormat::Cjs),
        platform: Some(Platform::Browser),
        external: Some(IsExternal::StringOrRegex(external)),
        define: Some(defines(production)),
        resolve: Some(ResolveOptions {
            alias: Some(vec![]),
            ..Default::default()
        }),
        tsconfig: Some(TsConfig::Manual(tsconfig.to_path_buf())),
        transform: Some(BundlerTransformOptions {
            target: Some(Either::Left("es2020".to_string())),
            ..Default::default()
        }),
        checks: Some(ChecksOptions {
            invalid_annotation: Some(false),
            import_is_undefined: Some(false),
            ..Default::default()
        }),
        minify: Some(RawMinifyOptions::Bool(production)),
        // rolldown_backend::run: a production bundle keeps the legal comments only.
        comments: Some(CommentsOptions {
            legal: true,
            annotation: !production,
            jsdoc: !production,
        }),
        sourcemap: (!production).then_some(SourceMapType::Inline),
        ..Default::default()
    };
    let mut bundler =
        Bundler::new(options).map_err(|e| anyhow!("the bundler failed to initialize: {e:?}"))?;
    let output = bundler.write().await.map_err(|e| {
        anyhow!(
            "build failed \u{2014} rolldown reported errors:\n{}",
            e.into_vec()
                .iter()
                .map(|d| d.to_diagnostic().convert_to_string(false))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let mut unresolved = Vec::new();
    for warning in &output.warnings {
        match warning.kind().to_string().as_str() {
            "INVALID_ANNOTATION" | "IMPORT_IS_UNDEFINED" => {}
            "UNRESOLVED_IMPORT" => unresolved.push(warning.to_string()),
            kind => warnings.push(format!("rolldown {kind}: {warning}")),
        }
    }
    Ok(unresolved)
}

fn external_pattern(pattern: &str) -> Result<StringOrRegex> {
    if !pattern.contains('*') {
        return Ok(StringOrRegex::String(pattern.to_string()));
    }
    let mut rx = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => rx.push_str(".*"),
            c if "\\^$.|?+()[]{}".contains(c) => {
                rx.push('\\');
                rx.push(c);
            }
            c => rx.push(c),
        }
    }
    rx.push('$');
    StringOrRegex::new(rx, Some(&String::new()))
        .map_err(|e| anyhow!("external pattern {pattern}: {e}"))
}

fn defines(production: bool) -> FxIndexMap<String, String> {
    let (debug, env) = match production {
        true => ("false", "\"production\""),
        false => ("true", "\"development\""),
    };
    [
        ("document", "undefined"),
        ("window", "undefined"),
        ("DEBUG", debug),
        ("globalThis.DEBUG", debug),
        ("process.env.NODE_ENV", env),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn write(path: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn write_new(path: &Path, content: &str) -> Result<()> {
    match path.exists() {
        true => Ok(()),
        false => write(path, content),
    }
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    let bytes = std::fs::read(from).with_context(|| format!("reading {}", from.display()))?;
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write(to, bytes)
}

/// dcl-scene-cli `scene_files::SceneDir`, walked once: the build reads the
/// listing several times and only ever writes into pruned directories.
struct SceneDir {
    root: PathBuf,
    files: Vec<String>,
}

impl SceneDir {
    fn walk(root: &Path, warnings: &mut Vec<String>) -> Self {
        let mut files = Vec::new();
        walk(root, root, &mut files, warnings);
        SceneDir {
            root: root.to_path_buf(),
            files,
        }
    }
}

impl SceneFiles for SceneDir {
    fn list(&self) -> Vec<String> {
        self.files.clone()
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(rel)).ok()
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, warnings: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if !is_pruned_dir(&name) {
                walk(root, &path, out, warnings);
            }
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if name.ends_with(".composite")
            && !name.starts_with('.')
            && path.metadata().map(|m| m.len()).unwrap_or(0) > COMPOSITE_FILE_MAX_BYTES
        {
            warnings.push(format!(
                "composite '{rel}' exceeds the {COMPOSITE_FILE_MAX_BYTES}-byte cap; refusing to parse"
            ));
            continue;
        }
        out.push(rel);
    }
}

/// dcl-scene-cli `entrypoint::SDK_BOOT`.
const SDK_BOOT: &str = "import { engine, setCompositeProvider } from '@dcl/sdk/ecs'\n\
                        import { compositeProvider } from '@dcl/sdk/composite-provider'\n\
                        setCompositeProvider(engine, compositeProvider)\n";

/// dcl-scene-cli `entrypoint::entrypoint_code` for a split build. Its comments
/// are part of the output: a development build inlines this file's source
/// into the scene chunk's sourcemap.
fn entrypoint_code(safe_entry: &str, editor_scene: bool) -> String {
    let composite_fill = "import { compositeFromLoader as __sceneComposites } from './all-composites.js'\nObject.assign(compositeFromLoader, __sceneComposites)\n";
    let editor_block = if editor_scene {
        "\nimport { syncEntity } from '@dcl/sdk/network'\nimport players from '@dcl/sdk/players'\nimport { setCompositeProvider } from '@dcl/sdk/ecs'\nimport { initAssetPacks } from '@dcl/asset-packs/dist/scene-entrypoint'\nsetCompositeProvider(engine, compositeProvider)\ninitAssetPacks(engine, { syncEntity }, players)\n"
            .to_string()
    } else {
        "false".to_string()
    };
    format!(
        r#"// BEGIN AUTO GENERATED CODE "~sdk/scene-entrypoint"
"use strict";
import {{ engine, NetworkEntity }} from '@dcl/sdk/ecs'
import * as sdk from '@dcl/sdk'
import {{ compositeProvider }} from '@dcl/sdk/composite-provider'
import {{ compositeFromLoader }} from '~sdk/all-composites'
import {{ _initializeScripts }} from '~sdk/script-utils'
// Registers the composite provider, and must be imported BEFORE the scene.
// A scene that calls initAssetPacks() at module scope -- as the Creator Hub
// templates do -- reads that provider while its own module body runs, and until
// this existed the only thing registering it was `@dcl/sdk`'s body, which the
// bundler emits at the END of the chunk. So the editor logged "[asset-packs] No
// SDK composite provider registered; SPAWN_ENTITY cannot resolve composites"
// and placing a smart item resolved nothing. Upstream sdk-commands has the same
// ordering (logic/bundle.ts) and gets away with it only because upstream scenes
// never call initAssetPacks themselves.
//
// A separate module because neither cheaper fix works: statements here run after
// every import including the scene's, and the bundler does not honour moving
// `@dcl/sdk` earlier -- a side-effect import is shaken to a bare require and the
// re-export below then reads a `_dcl_sdk` binding that was never declared, while
// `export * from` is emitted at the end wherever it is written.
import './sdk-boot.js'
import * as entrypoint from {safe_entry}
{composite_fill}
{editor_block}

// `console.error(e)` drops the frames, and the host captures its own stack at
// the call site — this catch block. `e.stack` carries the throw site, so it is
// a superset of what upstream prints.
function __reportSceneError(e: any) {{
  console.error(e && e.stack ? e.stack : e)
}}

if ((entrypoint as any).main !== undefined) {{
  function _INTERNAL_startup_system() {{
    try {{
      _initializeScripts(engine)

      const maybePromise = (entrypoint as any).main()
      if (maybePromise && typeof maybePromise === 'object' && typeof (maybePromise as unknown as Promise<unknown>).then === 'function') {{
        maybePromise.catch(__reportSceneError)
      }}
    }} catch (e) {{
      __reportSceneError(e)
    }} finally {{
      engine.removeSystem(_INTERNAL_startup_system)
    }}
  }}
  engine.addSystem(_INTERNAL_startup_system, Infinity)
}}

export * from '@dcl/sdk'
export * from {safe_entry}
export * from '~sdk/script-utils'
"#
    )
}
