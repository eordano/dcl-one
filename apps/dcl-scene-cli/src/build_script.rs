//! The package.json build contract, shared by build, preview/watch and deploy.
use crate::{
    build::{BuildOptions, Built},
    scene::Project,
    ux,
};
use anyhow::{Context, Result, bail};
use std::path::{Component, Path};
use std::process::Stdio;

const ACTIVE: &str = "DCL_SCENE_CLI_BUILD_SCRIPT";
/// Set to 1 when the caller narrates the build itself (the preview's step line and its
/// `\u{21bb} rebuilt` line), so a script can keep a passing build silent.
const QUIET: &str = "DCL_SCENE_CLI_QUIET";

/// `bin/index.js (256.5kb, 1.10 sec)`: what a caller that narrates the build says about it.
pub fn summary(built: &Built, elapsed: std::time::Duration, restore: &str) -> String {
    let size = std::fs::metadata(&built.outfile)
        .map(|m| m.len())
        .unwrap_or(0);
    format!(
        "{} ({}, {})",
        ux::rel_to(&built.project.root, &built.outfile),
        ux::fmt_bytes(size),
        ux::fmt_elapsed_tinted(elapsed, restore)
    )
}

pub fn command(root: &Path) -> Result<Option<String>> {
    let file = root.join("package.json");
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    let package: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", file.display()))?;
    let Some(value) = package.get("scripts").and_then(|s| s.get("build")) else {
        return Ok(None);
    };
    let script = value
        .as_str()
        .context("package.json scripts.build must be a nonempty string")?
        .trim();
    if script.is_empty() {
        bail!("package.json scripts.build must be a nonempty string");
    }
    if toolchain_build(script).is_some() {
        return Ok(None);
    }
    Ok(Some(script.to_owned()))
}

/// What a `scripts.build` that is the SDK build itself asks for, applied on
/// top of the command line's options. A preview build takes only the two
/// flags that shape the entrypoint: `--production` is `build`'s and
/// `deploy`'s, never `start`'s (a minified bundle without sourcemaps is no
/// preview).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScriptFlags {
    pub production: bool,
    pub ignore_composite: bool,
    pub custom_entry_point: bool,
    /// Upstream build flags this pipeline has no equivalent for.
    pub ignored: Vec<String>,
}

impl ScriptFlags {
    pub fn apply(&self, opts: &BuildOptions, preview: bool) -> BuildOptions {
        BuildOptions {
            production: opts.production || (self.production && !preview),
            ignore_composite: opts.ignore_composite || self.ignore_composite,
            custom_entry_point: opts.custom_entry_point || self.custom_entry_point,
            ..opts.clone()
        }
    }
}

/// The options of a preview or a watcher: the script's `--ignoreComposite`
/// and `--customEntryPoint` apply, its `--production` does not.
pub fn preview_options(opts: &BuildOptions) -> BuildOptions {
    script_flags(&opts.dir).apply(opts, true)
}

/// The flags of this project's `scripts.build` when it is the SDK build.
pub fn script_flags(root: &Path) -> ScriptFlags {
    std::fs::read(root.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|package| package["scripts"]["build"].as_str().map(str::to_owned))
        .and_then(|script| toolchain_build(&script))
        .unwrap_or_default()
}

const RUNNERS: &[&str] = &["npx", "pnpx", "bunx", "yarn", "pnpm", "npm", "bun"];
const RUNNER_VERBS: &[&str] = &["exec", "dlx", "x"];
const BUILD_TOOLS: &[&str] = &["dcl-scene-cli", "sdk-commands", "@dcl/sdk-commands"];

/// A `scripts.build` that is the SDK build — this binary or upstream's
/// `sdk-commands build`, with or without a package runner in front — and the
/// flags it carries. Such a script never spawns: the scaffold lists
/// `@dcl/sdk-commands` as a dependency, and running it from `node_modules`
/// would build with upstream's bundler instead of this one. Anything else
/// (another tool, a shell pipeline) is a custom build and runs as written.
pub fn toolchain_build(script: &str) -> Option<ScriptFlags> {
    let mut words = script.split_whitespace().peekable();
    while words
        .peek()
        .is_some_and(|w| RUNNERS.contains(w) || RUNNER_VERBS.contains(w))
    {
        words.next();
    }
    let tool = words.next()?;
    let tool = match tool.rsplit_once('@') {
        Some((name, version)) if !name.is_empty() && !version.contains('/') => name,
        _ => tool,
    };
    if !BUILD_TOOLS.contains(&tool) || words.next()? != "build" {
        return None;
    }
    let mut flags = ScriptFlags::default();
    while let Some(word) = words.next() {
        match word {
            "--production" | "-p" => flags.production = true,
            "--ignoreComposite" | "--ignore-composite" => flags.ignore_composite = true,
            "--customEntryPoint" | "--custom-entry-point" => flags.custom_entry_point = true,
            "--built-in" | "--skip-install" => {}
            "--single" | "--dir" => {
                words.next()?;
                flags.ignored.push(word.to_owned());
            }
            other if other.starts_with('-') => flags.ignored.push(other.to_owned()),
            _ => return None,
        }
    }
    Some(flags)
}

pub fn selected(project: &Project, opts: &BuildOptions) -> Result<Option<String>> {
    if opts.built_in {
        return Ok(None);
    }
    command(&project.root)
}

pub async fn run(project: Project, opts: &BuildOptions, script: &str) -> Result<Built> {
    if std::env::var_os(ACTIVE).is_some() {
        bail!(
            "recursive package.json scripts.build invocation; use dcl-scene-cli build --built-in inside the build script"
        );
    }
    let main = project.main_output()?;
    if Path::new(&main)
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("custom build scene.json main must be a relative path inside the project");
    }
    let outfile = project.root.join(&main);
    let mut paths = vec![project.root.join("node_modules/.bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    #[cfg(windows)]
    let mut child = {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.args(["/d", "/s", "/c", script]);
        c
    };
    #[cfg(not(windows))]
    let mut child = {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", script]);
        c
    };
    if !opts.quiet {
        ux::note(format!("scripts.build: {script}"));
    }
    let started = std::time::Instant::now();
    let status = child
        .current_dir(&project.root)
        .env("PATH", std::env::join_paths(paths)?)
        .env(ACTIVE, &project.root)
        .env(
            "DCL_SCENE_CLI_PRODUCTION",
            if opts.production { "1" } else { "0" },
        )
        .env("DCL_SCENE_CLI_MAIN", &main)
        .env(QUIET, if opts.quiet { "1" } else { "0" })
        .stdin(Stdio::inherit())
        // --json and deploy --programmatic keep stdout for the lines a
        // program parses: the script's own output joins stderr.
        .stdout(match ux::quiet() || ux::events() {
            true => Stdio::from(std::io::stderr()),
            false => Stdio::inherit(),
        })
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await
        .context("starting package.json scripts.build")?;
    if !status.success() {
        return Err(ux::UserError::new(
            format!("package.json scripts.build failed ({status})"),
            ux::TrySteps::one(
                "fix what the build script reported above; its output is the whole story",
            )
            .and("run it on its own to iterate faster: npm run build"),
        )
        .why("the SDK ran no build of its own, and a preview keeps serving the last good bundle")
        .into());
    }
    let metadata = std::fs::metadata(&outfile)
        .with_context(|| format!("scripts.build succeeded but did not produce {main}"))?;
    if !metadata.is_file() || metadata.len() == 0 {
        bail!("scripts.build must produce a nonempty bundle at {main}");
    }
    if !dunce::canonicalize(&outfile)?.starts_with(&project.root) {
        bail!("scripts.build output {main} resolves outside the project");
    }
    let built = Built { project, outfile };
    if !opts.quiet {
        ux::note(format!(
            "built {}",
            summary(&built, started.elapsed(), ux::RESTORE_DIM)
        ));
    }
    Ok(built)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> BuildOptions {
        BuildOptions {
            built_in: false,
            dir: std::path::PathBuf::from("scene"),
            production: false,
            ignore_composite: false,
            custom_entry_point: false,
            skip_type_check: false,
            out_root: None,
            quiet: true,
        }
    }

    #[test]
    fn a_preview_takes_the_entrypoint_flags_of_the_script_but_not_production() {
        let flags =
            toolchain_build("sdk-commands build -p --ignoreComposite --customEntryPoint").unwrap();
        assert!(flags.production && flags.ignore_composite && flags.custom_entry_point);
        let built = flags.apply(&opts(), false);
        assert!(built.production && built.ignore_composite && built.custom_entry_point);
        let preview = flags.apply(&opts(), true);
        assert!(
            !preview.production,
            "start never minifies on the script's say"
        );
        assert!(preview.ignore_composite && preview.custom_entry_point);
        let mut explicit = opts();
        explicit.production = true;
        assert!(
            flags.apply(&explicit, true).production,
            "the caller's own flag stands"
        );
    }

    /// `build --watch` and `start` read the script of the folder they build.
    #[test]
    fn a_watcher_reads_the_flags_of_the_project_script() {
        let t = crate::deploy::TempTree::new("watch-script-flags");
        t.write(
            "package.json",
            r#"{"scripts":{"build":"sdk-commands build --production --ignoreComposite --customEntryPoint"}}"#,
        );
        let mut watched = opts();
        watched.dir = t.0.clone();
        let watched = preview_options(&watched);
        assert!(watched.ignore_composite && watched.custom_entry_point);
        assert!(!watched.production);
        assert!(!preview_options(&opts()).custom_entry_point, "no script");
    }
}
