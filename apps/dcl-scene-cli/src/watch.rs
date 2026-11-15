use crate::build::{self, BuildOptions};
use crate::entrypoint;
use crate::esbuild::{self, EsbuildOptions};
use crate::live_reload::ReloadEvent;
use crate::scene::Project;
use crate::split;
use crate::ux::{self, TrySteps, UserError};
use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A batch closes `QUIET` after its last event (a save is several events a few
/// ms apart), and no later than `DEBOUNCE` after the first. Once the writer is
/// known to be done with every file in the batch (it closed the file, or
/// renamed the finished file into place), `SETTLE` is enough: the wait that is
/// left only gathers the other files of the same save.
const QUIET: Duration = Duration::from_millis(20);
const SETTLE: Duration = Duration::from_millis(4);
const DEBOUNCE: Duration = Duration::from_millis(100);

/// What the watcher's thread tells the batcher.
#[derive(Debug)]
enum Seen {
    /// The path changed: it joins the batch, and its writer may not be done.
    Changed(PathBuf),
    /// The path's writer is done with it. Linux reports this (close after
    /// write, rename into place); elsewhere a batch only ever closes on
    /// `QUIET`.
    Settled(PathBuf),
}

fn seen(event: notify::Event) -> Vec<Seen> {
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
    match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
            event.paths.into_iter().map(Seen::Settled).collect()
        }
        EventKind::Access(_) => Vec::new(),
        EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both)) => {
            let landed = event.paths.last().cloned();
            event
                .paths
                .into_iter()
                .map(Seen::Changed)
                .chain(landed.map(Seen::Settled))
                .collect()
        }
        _ => event.paths.into_iter().map(Seen::Changed).collect(),
    }
}

/// The next batch of relevant paths, `None` once the watcher is gone.
async fn next_batch_from(
    rx: &mut mpsc::UnboundedReceiver<Seen>,
    root: &Path,
) -> Option<Vec<PathBuf>> {
    loop {
        let mut batch = Vec::new();
        // Paths in the batch whose writer has not been seen to finish.
        let mut open: Vec<PathBuf> = Vec::new();
        let mut take = |seen: Seen, batch: &mut Vec<PathBuf>| match seen {
            Seen::Changed(p) => {
                if is_relevant(root, &p) {
                    if !open.contains(&p) {
                        open.push(p.clone());
                    }
                    batch.push(p);
                }
                !batch.is_empty() && open.is_empty()
            }
            Seen::Settled(p) => {
                open.retain(|o| o != &p);
                !batch.is_empty() && open.is_empty()
            }
        };
        let mut settled = take(rx.recv().await?, &mut batch);
        let deadline = tokio::time::Instant::now() + DEBOUNCE;
        loop {
            let wait = if settled { SETTLE } else { QUIET };
            let until = deadline.min(tokio::time::Instant::now() + wait);
            match tokio::time::timeout_at(until, rx.recv()).await {
                Ok(Some(seen)) => settled = take(seen, &mut batch),
                Ok(None) => return (!batch.is_empty()).then_some(batch),
                Err(_) => break,
            }
        }
        if !batch.is_empty() {
            return Some(batch);
        }
    }
}

pub struct FsWatcher {
    _watcher: std::sync::Arc<std::sync::Mutex<notify::RecommendedWatcher>>,
    rx: mpsc::UnboundedReceiver<Seen>,
    root: PathBuf,
}

fn watch_error(path: &Path, e: notify::Error) -> anyhow::Error {
    UserError::new(
        format!(
            "could not watch {} for changes (system watch limit reached?)",
            path.display()
        ),
        TrySteps::one("raise the limit: sudo sysctl fs.inotify.max_user_watches=524288")
            .and("or run dcl-scene-cli start --no-watch"),
    )
    .caused_by(e)
    .into()
}

/// The relevant files already under `dir`, for a directory that appeared
/// with files in it before its watch was in place.
fn relevant_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => relevant_files(root, &path, out),
            Ok(_) if is_relevant(root, &path) => out.push(path),
            _ => {}
        }
    }
}

impl FsWatcher {
    /// Watches the root's own entries and, recursively, each of its
    /// directories that can hold scene input. The ones [`is_relevant`] drops
    /// whole (`node_modules`, `bin`, dot-directories) are never walked: an
    /// installed `node_modules` is thousands of directories, and a watch on
    /// each is what a start would otherwise wait for.
    pub fn new(root: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<Seen>();
        let (dirs_tx, dirs_rx) = std::sync::mpsc::channel::<PathBuf>();
        let events = tx.clone();
        let top = root.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) {
                for path in event.paths.iter().filter(|p| p.parent() == Some(&top)) {
                    let _ = dirs_tx.send(path.clone());
                }
            }
            for seen in seen(event) {
                let _ = events.send(seen);
            }
        })
        .map_err(|e| {
            anyhow::Error::from(
                UserError::new(
                    "could not start the file watcher",
                    TrySteps::one(
                        "on Linux, raise the inotify limit: sudo sysctl fs.inotify.max_user_instances=512",
                    )
                    .and("to build once without watching, run dcl-scene-cli build"),
                )
                .caused_by(e),
            )
        })?;
        watcher
            .watch(root, RecursiveMode::NonRecursive)
            .map_err(|e| watch_error(root, e))?;
        for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let path = entry.path();
            if holds_scene_input(root, &path) {
                watcher
                    .watch(&path, RecursiveMode::Recursive)
                    .map_err(|e| watch_error(&path, e))?;
            }
        }
        let watcher = std::sync::Arc::new(std::sync::Mutex::new(watcher));
        // A directory that appears at the root later is watched from its own
        // thread: the watcher's callback cannot add a watch, and the batcher
        // is not polled while a build runs. The thread ends with the watcher,
        // whose callback holds the only sender.
        let weak = std::sync::Arc::downgrade(&watcher);
        let top = root.to_path_buf();
        std::thread::spawn(move || {
            while let Ok(dir) = dirs_rx.recv() {
                if !holds_scene_input(&top, &dir) {
                    continue;
                }
                let Some(watcher) = weak.upgrade() else {
                    return;
                };
                let watched = watcher
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .watch(&dir, RecursiveMode::Recursive);
                drop(watcher);
                if let Err(e) = watched {
                    ux::report_watch(&watch_error(&dir, e));
                    continue;
                }
                let mut found = Vec::new();
                relevant_files(&top, &dir, &mut found);
                for path in found {
                    let _ = tx.send(Seen::Changed(path.clone()));
                    let _ = tx.send(Seen::Settled(path));
                }
            }
        });
        Ok(Self {
            _watcher: watcher,
            rx,
            root: root.to_path_buf(),
        })
    }

    pub async fn next_batch(&mut self) -> Option<Vec<PathBuf>> {
        next_batch_from(&mut self.rx, &self.root).await
    }
}

/// Root entries whose whole subtree [`is_relevant`] drops.
fn ignored_at_root(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "bin")
}

/// A directory directly under `root` that can hold scene input.
fn holds_scene_input(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !ignored_at_root(n))
        && path.is_dir()
}

pub fn is_relevant(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let first = rel.components().next().and_then(|c| c.as_os_str().to_str());
    if first.is_some_and(ignored_at_root) {
        return false;
    }
    // Agent CLI configuration at the root is not scene input; the scene assistant writes and
    // restores `opencode.json` around each OpenCode turn, which must not rebuild the scene.
    if matches!(rel.to_str(), Some("opencode.json" | "opencode.jsonc")) {
        return false;
    }
    matches!(
        rel.to_str(),
        Some("package.json" | "scene.json" | "tsconfig.json")
    ) || is_model(path)
        || matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "ts" | "tsx"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "json"
                | "composite"
                | "png"
                | "jpg"
                | "jpeg"
                | "webp"
                | "gif"
                | "ktx2"
                | "mp3"
                | "ogg"
                | "wav"
                | "mp4"
                | "webm"
                | "bin"
        )
}

pub fn is_model(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("glb") || e.eq_ignore_ascii_case("gltf"))
}

/// Splits a batch into (model, removed) pairs and everything else. Deletion is
/// judged by what is on disk, not by the notify event kind: an atomic save and a
/// trash-style delete both end with the path present/absent, which is all the
/// explorer needs to pick UMT_CHANGE vs UMT_REMOVE.
fn partition_batch(paths: Vec<PathBuf>) -> (Vec<(PathBuf, bool)>, Vec<PathBuf>) {
    let (mut models, code): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| is_model(p));
    models.sort();
    models.dedup();
    let models = models
        .into_iter()
        .map(|p| {
            let removed = !p.exists();
            (p, removed)
        })
        .collect();
    (models, code)
}

/// `start --json`: one rebuild of the running preview, with the `main` file
/// of scene.json and the saved files that started it. Absolute paths.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RebuildEvent {
    event: &'static str,
    output: String,
    changed: Vec<String>,
    duration_ms: u64,
}

/// `output` has one meaning for each build kind: the file the client loads,
/// never a chunk the built-in bundler wrote beside it.
pub fn rebuild_event(project: &Project, changed: &[PathBuf], took: Duration) {
    let Ok(main) = project.main_output() else {
        return;
    };
    let mut changed: Vec<String> = changed.iter().map(|p| p.display().to_string()).collect();
    changed.sort();
    changed.dedup();
    ux::event(&RebuildEvent {
        event: "rebuild",
        output: project.root.join(main).display().to_string(),
        changed,
        duration_ms: took.as_millis() as u64,
    });
}

struct SplitState {
    sdk_opts: EsbuildOptions,
    registry: Vec<&'static str>,
    /// The `~sdk/script-utils` module the chunk was last built with: a composite
    /// edit can flip it between the stub and the real runtime with no registry
    /// key changing.
    script_utils: String,
    generated_dir: PathBuf,
}

/// Keep the custom path separate: constructing a built-in session writes a
/// loader, even when the caller requested --skip-build.
pub enum WatchSession {
    Builtin(Box<BuiltinWatchSession>),
    Custom {
        project: Project,
        opts: BuildOptions,
    },
}

impl WatchSession {
    pub async fn create(
        project: Project,
        opts: &BuildOptions,
        initial_build: bool,
        steps: &mut ux::Steps,
    ) -> Result<Self> {
        if let Some(script) = crate::build_script::selected(&project, opts)? {
            let project = Project::load(&project.root)?;
            if initial_build {
                // The step line is the narration; a second "completed" note would repeat it.
                let narrated = BuildOptions {
                    quiet: true,
                    ..opts.clone()
                };
                let started = std::time::Instant::now();
                let built = crate::build_script::run(project.clone(), &narrated, &script).await?;
                steps.done(format!(
                    "Build script saved {}",
                    crate::build_script::summary(&built, started.elapsed(), "")
                ));
            }
            return Ok(Self::Custom {
                project,
                opts: opts.clone(),
            });
        }
        Ok(Self::Builtin(Box::new(
            BuiltinWatchSession::create(project, opts, initial_build, steps).await?,
        )))
    }

    pub fn project(&self) -> &Project {
        match self {
            Self::Builtin(s) => s.project(),
            Self::Custom { project, .. } => project,
        }
    }

    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }

    pub async fn run(self, mut fs: FsWatcher, notify: impl Fn(ReloadEvent)) -> Result<()> {
        let (project, opts) = match self {
            Self::Builtin(s) => return s.run(fs, notify).await,
            Self::Custom { project, opts } => (project, opts),
        };
        // A script's output directory must be distinct from its sources. Ignore
        // it as well as bin/.dcl-one so generated chunks cannot trigger a loop.
        while let Some(mut batch) = fs.next_batch().await {
            let main = match Project::load(&project.root).and_then(|p| p.main_output()) {
                Ok(main) => project.root.join(main),
                Err(e) => {
                    ux::report_build(&e);
                    continue;
                }
            };
            let output_dir = main.parent().filter(|p| *p != project.root);
            batch.retain(|p| p != &main && !output_dir.is_some_and(|d| p.starts_with(d)));
            let (models, paths) = partition_batch(batch);
            if paths.is_empty() {
                for (path, removed) in models {
                    notify(ReloadEvent::Model { path, removed });
                }
                continue;
            }
            let _attempt = ux::build_attempt();
            // Reload package.json and scene.json on every build. Never fall back
            // to a built-in build when a script reports a failure.
            let narrated = BuildOptions {
                quiet: true,
                ..opts.clone()
            };
            // Said before the script runs: a custom build can take seconds, and its
            // silence would otherwise read as a save that was never noticed.
            if ux::custom_build_note()
                && let Ok(Some(script)) = crate::build_script::command(&project.root)
            {
                ux::note_clocked(format!("\u{25b8} custom build: {script}"));
            }
            let started = std::time::Instant::now();
            match build::build_as(&narrated, build::BuildMode::Preview).await {
                Ok(built) => {
                    ux::note_clocked(format!(
                        "\u{21bb} rebuilt {}",
                        crate::build_script::summary(&built, started.elapsed(), ux::RESTORE_DIM)
                    ));
                    rebuild_event(&built.project, &paths, started.elapsed());
                    notify(ReloadEvent::Scene)
                }
                Err(e) => ux::report_build(&e),
            }
        }
        Ok(())
    }
}

pub struct BuiltinWatchSession {
    project: Project,
    es_opts: EsbuildOptions,
    ignore_composite: bool,
    custom_entry_point: bool,
    split: SplitState,
    /// `Some` when the toolchain ships prebuilt chunks: the SDK chunk is copied
    /// rather than bundled, and the smart-item chunk is a separate file.
    prebuilt: Option<crate::prebuilt::Prebuilt>,
    loader: split::Loader,
    typecheck: build::BackgroundCheck,
    type_checking: bool,
    built_in: bool,
}

impl BuiltinWatchSession {
    pub async fn create(
        project: Project,
        opts: &BuildOptions,
        initial_build: bool,
        steps: &mut ux::Steps,
    ) -> Result<Self> {
        let mut staged = build::stage(&project, opts, &project.root)?;
        staged.loader.write()?;
        if initial_build {
            steps.done(format!(
                "Loader stub saved {}",
                ux::rel_to(&project.root, &staged.loader.outfile)
            ));
            let smart = build::emit_chunks(&project, &staged, &project.root, steps).await?;
            if smart {
                staged.loader.smart_installed = true;
                staged.loader.write()?;
            }
        }
        let build::Staged {
            generated,
            prebuilt,
            sdk_opts,
            scene_opts,
            loader,
        } = staged;
        let registry = split::registry_keys(&project);
        let script_utils = read_script_utils(&generated.dir);
        let mut session = Self {
            project,
            es_opts: scene_opts,
            ignore_composite: opts.ignore_composite,
            custom_entry_point: opts.custom_entry_point,
            split: SplitState {
                sdk_opts,
                registry,
                script_utils,
                generated_dir: generated.dir,
            },
            prebuilt,
            loader,
            typecheck: build::BackgroundCheck::default(),
            type_checking: !opts.skip_type_check,
            built_in: opts.built_in,
        };
        if session.type_checking && initial_build {
            session.typecheck.restart(session.project.clone());
        }
        Ok(session)
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    fn rewrite_loader_stub(&self) {
        if let Err(e) = self.loader.write() {
            ux::report_build(&e);
        }
    }

    /// The loader stub carries scene.json's `authoritativeMultiplayer`, and
    /// the flag can flip under a running preview (the /scene page's switch, a
    /// hand edit). A scene.json mid-edit keeps the flag it had.
    fn follow_mp_flag(&mut self, changed: &[PathBuf]) {
        let is_scene_json = |p: &PathBuf| {
            p.strip_prefix(&self.project.root)
                .is_ok_and(|rel| rel == Path::new("scene.json"))
        };
        if !changed.iter().any(is_scene_json) {
            return;
        }
        let Ok(fresh) = Project::load(&self.project.root) else {
            return;
        };
        let mp = crate::entrypoint::authoritative_multiplayer(&fresh);
        if mp != self.loader.mp {
            self.loader.mp = mp;
            self.rewrite_loader_stub();
        }
    }

    pub async fn run(mut self, mut fs: FsWatcher, notify: impl Fn(ReloadEvent)) -> Result<()> {
        while let Some(batch) = fs.next_batch().await {
            // A project may opt into a custom script while preview is running.
            // Check before touching generated files, including on a model save.
            let custom = if self.built_in {
                false
            } else {
                match crate::build_script::command(&self.project.root) {
                    Ok(script) => script.is_some(),
                    Err(e) => {
                        ux::report_build(&e);
                        continue;
                    }
                }
            };
            if custom {
                let opts = BuildOptions {
                    dir: self.project.root.clone(),
                    built_in: false,
                    production: self.es_opts.production,
                    ignore_composite: self.ignore_composite,
                    custom_entry_point: self.custom_entry_point,
                    skip_type_check: !self.type_checking,
                    out_root: None,
                    quiet: false,
                };
                let mut steps = ux::Steps::silent();
                let started = Instant::now();
                // Keep this session on failure so the next edit can retry.
                match WatchSession::create(self.project.clone(), &opts, true, &mut steps).await {
                    Ok(session) => {
                        rebuild_event(session.project(), &batch, started.elapsed());
                        notify(ReloadEvent::Scene);
                        return Box::pin(session.run(fs, notify)).await;
                    }
                    Err(e) => {
                        ux::report_build(&e);
                        continue;
                    }
                }
            }
            let (models, paths) = partition_batch(batch);
            for (model, removed) in &models {
                let verb = if *removed { "removed" } else { "update" };
                ux::note_clocked(format!(
                    "\u{21bb} model {verb} {}",
                    ux::rel_to(&self.project.root, model)
                ));
            }
            for (path, removed) in models {
                notify(ReloadEvent::Model { path, removed });
            }
            if paths.is_empty() {
                continue;
            }
            let _attempt = ux::build_attempt();
            let started = Instant::now();
            self.follow_mp_flag(&paths);
            let composites_changed = match regenerate_composites(
                &self.project,
                self.ignore_composite,
                self.custom_entry_point,
                &paths,
            )
            .await
            {
                Err(e) => {
                    ux::report_build(&watch_regen_error(
                        e,
                        "composite rebuild failed \u{2014} watching continues",
                    ));
                    continue;
                }
                Ok(Some(new_max)) => {
                    if new_max != self.loader.max_composite_entity {
                        self.loader.max_composite_entity = new_max;
                        self.rewrite_loader_stub();
                    }
                    true
                }
                Ok(None) => false,
            };
            if self.prebuilt.is_none() {
                refresh_sdk_chunk_cli(&self.project, &mut self.split, composites_changed).await;
            }
            if let Err(e) = esbuild::bundle(&self.project, &self.es_opts).await {
                ux::report_build(&e);
                continue;
            }
            tracing::info!(
                "rebuilt {} in {}",
                self.es_opts.outfile.display(),
                ux::fmt_elapsed(started.elapsed())
            );
            match build::install_smart_chunk(
                &self.project,
                self.prebuilt.as_ref(),
                &self.project.root,
                &self.loader.paths.scene,
                &self.loader.paths.smart,
            ) {
                Ok(now) if now != self.loader.smart_installed => {
                    self.loader.smart_installed = now;
                    self.rewrite_loader_stub();
                }
                Ok(_) => {}
                Err(e) => ux::report_build(&e),
            }
            ux::note_clocked(format!(
                "\u{21bb} rebuilt {} ({})",
                ux::rel_to(&self.project.root, &self.es_opts.outfile),
                ux::fmt_elapsed_tinted(started.elapsed(), ux::RESTORE_DIM)
            ));
            rebuild_event(&self.project, &paths, started.elapsed());
            notify(ReloadEvent::Scene);
            if self.type_checking {
                self.typecheck.restart(self.project.clone());
            }
        }
        Ok(())
    }
}

fn read_script_utils(generated_dir: &Path) -> String {
    std::fs::read_to_string(generated_dir.join("script-utils.js")).unwrap_or_default()
}

fn watch_regen_error(e: anyhow::Error, what: &str) -> anyhow::Error {
    UserError::new(
        what.to_string(),
        TrySteps::one("fix the file named below, then save any file to retry"),
    )
    .why(format!("{e:#}"))
    .into()
}

async fn regenerate_composites(
    project: &Project,
    ignore_composite: bool,
    custom_entry_point: bool,
    paths: &[PathBuf],
) -> Result<Option<u32>> {
    let touched = paths
        .iter()
        .any(|p| p.extension().and_then(|e| e.to_str()) == Some("composite"));
    if !touched {
        return Ok(None);
    }
    let generated = entrypoint::generate(project, ignore_composite, custom_entry_point, true)?;
    tracing::info!("composites changed, regenerated all-composites.js");
    if !ignore_composite {
        match crate::entity_names::write_if_changed(&project.root) {
            Ok(Some(n)) => tracing::info!(
                "composites changed, regenerated {} ({n} name(s))",
                dcl_scene_authoring::entity_names::OUTPUT_PATH
            ),
            Ok(None) => {}
            Err(e) => tracing::warn!(
                "could not write {}: {e}",
                dcl_scene_authoring::entity_names::OUTPUT_PATH
            ),
        }
    }
    if let Err(e) = crate::data_layer::regenerate_main_crdt(&project.root, ignore_composite).await {
        ux::report_build(&e);
    }
    Ok(Some(generated.max_composite_entity))
}

async fn refresh_sdk_chunk_cli(project: &Project, sp: &mut SplitState, composites_changed: bool) {
    let keys = split::registry_keys(project);
    let script_utils = match composites_changed {
        true => read_script_utils(&sp.generated_dir),
        false => sp.script_utils.clone(),
    };
    if keys == sp.registry && script_utils == sp.script_utils {
        return;
    }
    if let Err(e) = split::write_generated(project, &sp.generated_dir) {
        ux::report_build(&watch_regen_error(
            e,
            "sdk runtime entry rebuild failed \u{2014} watching continues",
        ));
        return;
    }
    match crate::sdk_cache::bundle(project, &sp.sdk_opts).await {
        Ok(_) => {
            sp.registry = keys;
            sp.script_utils = script_utils;
            tracing::info!(
                "sdk registry changed, rebuilt {}",
                sp.sdk_opts.outfile.display()
            );
            ux::note_clocked(format!(
                "\u{21bb} rebuilt {} (sdk registry changed)",
                ux::rel_to(&project.root, &sp.sdk_opts.outfile)
            ));
        }
        Err(e) => ux::report_build(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEBOUNCE, FsWatcher, QUIET, SETTLE, Seen, is_relevant, next_batch_from, partition_batch,
        seen,
    };
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    const ROOT: &str = "/proj";

    fn at(rel: &str) -> PathBuf {
        Path::new(ROOT).join(rel)
    }

    fn changed(rel: &str) -> Seen {
        Seen::Changed(at(rel))
    }

    fn settled(rel: &str) -> Seen {
        Seen::Settled(at(rel))
    }

    /// Feeds `script` (a pause, then what the watcher saw) on the paused
    /// clock and returns every batch with the time it closed at.
    async fn batches(script: Vec<(u64, Seen)>) -> Vec<(Duration, Vec<PathBuf>)> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        tokio::spawn(async move {
            for (pause, seen) in script {
                tokio::time::sleep(Duration::from_millis(pause)).await;
                tx.send(seen).unwrap();
            }
            // The watcher outlives the save: a closed channel would end the
            // batch at once and hide the window under test.
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let mut out = Vec::new();
        while let Some(batch) = next_batch_from(&mut rx, Path::new(ROOT)).await {
            out.push((started.elapsed(), batch));
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_file_builds_after_the_settle_window() {
        let got = batches(vec![
            (0, changed("src/index.ts")),
            (0, changed("src/index.ts")),
            (0, settled("src/index.ts")),
        ])
        .await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, SETTLE);
    }

    /// An editor that moves the old file aside, writes the new one and drops
    /// the backup: one save, one rebuild, although the file is briefly gone.
    #[tokio::test(start_paused = true)]
    async fn a_save_in_several_steps_is_one_batch() {
        let got = batches(vec![
            (0, changed("src/index.ts")),
            (0, changed("src/index.ts~")),
            (0, settled("src/index.ts~")),
            (15, changed("src/index.ts")),
            (2, changed("src/index.ts")),
            (1, settled("src/index.ts")),
            (1, changed("src/index.ts~")),
        ])
        .await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Duration::from_millis(19) + SETTLE);
    }

    /// Write to a temporary name, rename over the source.
    #[tokio::test(start_paused = true)]
    async fn an_atomic_save_is_one_batch() {
        let renamed = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Name(notify::event::RenameMode::Both),
        ))
        .add_path(at("src/.index.ts.swp"))
        .add_path(at("src/index.ts"));
        let mut script = vec![
            (0, changed("src/.index.ts.swp")),
            (0, settled("src/.index.ts.swp")),
            (3, changed("src/.index.ts.swp")),
            (0, changed("src/index.ts")),
            (0, settled("src/index.ts")),
        ];
        script.extend(seen(renamed).into_iter().map(|s| (0, s)));
        let got = batches(script).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Duration::from_millis(3) + SETTLE);
    }

    #[tokio::test(start_paused = true)]
    async fn files_saved_together_are_one_batch() {
        let got = batches(vec![
            (0, changed("src/a.ts")),
            (0, settled("src/a.ts")),
            (3, changed("src/b.ts")),
            (10, changed("src/b.ts")),
            (0, settled("src/b.ts")),
            (2, changed("scene.json")),
            (0, settled("scene.json")),
        ])
        .await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].1.len(), 4);
    }

    /// Without a close or rename to go by (macOS, Windows, a file still being
    /// written) the quiet window alone closes the batch, as it always did.
    #[tokio::test(start_paused = true)]
    async fn an_open_file_waits_out_the_quiet_window() {
        let got = batches(vec![
            (0, changed("src/a.ts")),
            (15, changed("src/a.ts")),
            (15, changed("src/a.ts")),
        ])
        .await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Duration::from_millis(30) + QUIET);

        let got = batches(vec![
            (0, changed("src/a.ts")),
            (0, settled("src/a.ts")),
            (2, changed("src/b.ts")),
        ])
        .await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Duration::from_millis(2) + QUIET);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_of_events_still_closes_at_the_debounce() {
        let got = batches((0..40).map(|_| (10, changed("assets/big.glb"))).collect()).await;
        assert!(got.len() > 1, "{got:?}");
        assert_eq!(got[0].0, Duration::from_millis(10) + DEBOUNCE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_alone_starts_nothing() {
        let got = batches(vec![
            (0, settled("src/a.ts")),
            (0, changed("README.md")),
            (0, settled("README.md")),
            (50, changed("src/a.ts")),
            (0, settled("src/a.ts")),
        ])
        .await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, Duration::from_millis(50) + SETTLE);
        assert_eq!(got[0].1, vec![at("src/a.ts")]);
    }

    #[test]
    fn reads_are_not_changes_and_a_close_after_write_settles() {
        use notify::event::{AccessKind, AccessMode};
        let on = |kind| seen(notify::Event::new(kind).add_path(at("src/a.ts")));
        assert!(on(notify::EventKind::Access(AccessKind::Open(AccessMode::Any))).is_empty());
        assert!(
            on(notify::EventKind::Access(AccessKind::Close(
                AccessMode::Read
            )))
            .is_empty()
        );
        assert!(matches!(
            on(notify::EventKind::Access(AccessKind::Close(
                AccessMode::Write
            )))[..],
            [Seen::Settled(_)]
        ));
        assert!(matches!(
            on(notify::EventKind::Remove(notify::event::RemoveKind::File))[..],
            [Seen::Changed(_)]
        ));
    }

    async fn next(fs: &mut FsWatcher) -> Option<Vec<PathBuf>> {
        tokio::time::timeout(Duration::from_secs(5), fs.next_batch())
            .await
            .ok()
            .flatten()
    }

    /// The real watcher: one write is one batch, and what lands in the
    /// directories that are not watched starts none.
    #[tokio::test]
    async fn one_write_to_a_source_is_one_batch() {
        let dir = crate::scene::Tmp::new("watch-one-write");
        dir.write("src/index.ts", "export {}");
        dir.write("node_modules/pkg/index.js", "");
        dir.write(".dcl-one/entrypoint.ts", "");
        let root = dir.0.canonicalize().unwrap();
        let mut fs = FsWatcher::new(&root).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "1").unwrap();
        std::fs::write(root.join(".dcl-one/entrypoint.ts"), "1").unwrap();
        std::fs::write(root.join("src/index.ts"), "export const a = 1").unwrap();
        let mut batch = next(&mut fs).await.expect("the write");
        batch.dedup();
        assert_eq!(batch, vec![root.join("src/index.ts")]);
        let more = tokio::time::timeout(Duration::from_millis(300), fs.next_batch()).await;
        assert!(more.is_err(), "a second batch for one write: {more:?}");
    }

    /// A directory made at the root after the start is watched like the ones
    /// found there, including the files it arrived with.
    #[tokio::test]
    async fn a_directory_added_at_the_root_is_watched() {
        let dir = crate::scene::Tmp::new("watch-new-dir");
        dir.write("scene.json", "{}");
        let root = dir.0.canonicalize().unwrap();
        let mut fs = FsWatcher::new(&root).unwrap();
        std::fs::create_dir_all(root.join("assets/models")).unwrap();
        std::fs::write(root.join("assets/models/tree.glb"), "glb").unwrap();
        let first = next(&mut fs).await.expect("the new directory's file");
        assert!(
            first.contains(&root.join("assets/models/tree.glb")),
            "{first:?}"
        );
        while tokio::time::timeout(Duration::from_millis(300), fs.next_batch())
            .await
            .is_ok()
        {}
        std::fs::write(root.join("assets/models/rock.glb"), "glb").unwrap();
        let later = next(&mut fs).await.expect("a later write in it");
        assert!(
            later.contains(&root.join("assets/models/rock.glb")),
            "{later:?}"
        );
    }

    #[test]
    fn partition_batch_flags_missing_models_as_removed() {
        let dir = crate::scene::Tmp::new("partition");
        let present = dir.0.join("tree.glb");
        std::fs::write(&present, b"glb").unwrap();
        let gone = dir.0.join("old.gltf");
        let (models, code) = partition_batch(vec![
            present.clone(),
            gone.clone(),
            present.clone(),
            dir.0.join("src/game.ts"),
        ]);
        assert_eq!(code, vec![dir.0.join("src/game.ts")]);
        assert_eq!(models, vec![(gone, true), (present, false)]);
    }

    fn under_root(rel: &str) -> bool {
        let root = Path::new("/proj");
        is_relevant(root, &root.join(rel))
    }

    #[test]
    fn build_output_and_tool_dirs_are_ignored_by_component() {
        assert!(!under_root("bin/index.js"));
        assert!(!under_root("bin/scene.js"));
        assert!(!under_root("node_modules/foo/bar.js"));
        assert!(!under_root(".dcl-one/all-composites.js"));
        assert!(!under_root(".git/hooks/pre-commit.ts"));
        assert!(!under_root("opencode.json"));
        assert!(!under_root(".cursor/mcp.json"));
        assert!(under_root("src/opencode.json"));
        assert!(!under_root(".dcl-optimized-assets/out/b64-x/mac/model.glb"));
        assert!(!under_root(
            ".dcl-optimized-assets/cache/content/deadbeef.gltf"
        ));
    }

    #[test]
    fn sources_that_share_a_prefix_are_still_watched() {
        assert!(under_root("bindings.ts"));
        assert!(under_root("binary/loader.ts"));
        assert!(under_root("node_modules_helper/x.ts"));
        assert!(under_root("src/game.ts"));
    }

    #[test]
    fn only_code_and_models_are_relevant() {
        assert!(under_root("scene.composite"));
        assert!(under_root("src/config.json"));
        assert!(under_root("assets/tree.glb"));
        assert!(under_root("assets/tree.GLTF"));
        assert!(under_root("src/tex.png"));
        assert!(under_root("assets/sky.PNG"));
        assert!(under_root("assets/music.ogg"));
        assert!(!under_root("README.md"));
    }
}
