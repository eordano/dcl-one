//! `code-to-composite`: run the scene's code once under node against a mock
//! renderer, keep what the visual editor can edit and write it as
//! `assets/scene/main.composite` + `main.crdt`; the code under `src/` is then
//! commented out under upstream's marker and the scene rebuilt from its
//! composite, so `start` and `deploy` see the converted scene right away.

use crate::build::{self, BuildOptions};
use crate::scene::Project;
use crate::ux::{self, TrySteps, UserError};
use crate::workspace::Workspace;
use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub struct CodeToCompositeOptions {
    pub dir: PathBuf,
    /// Convert without the confirmation prompt.
    pub yes: bool,
}

/// Upstream's marker: a file carrying it is already converted and is left
/// alone. Kept byte-equal with `@dcl/sdk-commands`, so a scene converted by
/// either tool is recognised by the other.
pub const COMMENT_MARKER: &str = "COMMENTED BY \"@dcl/sdk-commands code-to-composite\" COMMAND";

const DRIVER_TEMPLATE: &str = include_str!("templates/code-to-composite.mjs");

/// The driver's summary line.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Converted {
    /// The one required key: a line without it is not the summary.
    ok: bool,
    #[serde(default)]
    pub entities: u64,
    #[serde(default)]
    pub components: u64,
    #[serde(default)]
    pub ui_entities: u64,
    #[serde(default)]
    pub assets: Vec<Moved>,
    #[serde(default)]
    pub missing_assets: Vec<String>,
    /// `<model>: <uri>` for a glTF dependency that is not a relative path
    /// inside the model's folder (a URL, an absolute path, `..`): left as is.
    #[serde(default)]
    pub skipped_dependencies: Vec<String>,
}

#[derive(Debug, PartialEq, serde::Deserialize)]
pub struct Moved {
    pub from: String,
    pub to: String,
}

fn ies(n: u64) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

pub async fn code_to_composite(opts: &CodeToCompositeOptions) -> Result<()> {
    let ws = Workspace::load(&opts.dir)?;
    if !confirm(opts.yes)? {
        ux::note("Operation cancelled by user.");
        return Ok(());
    }
    for (i, project) in ws.projects.iter().enumerate() {
        if let Some(header) = ws.member_header(i) {
            ux::note(header);
        }
        if project.root.join("wearable.json").is_file() {
            tracing::warn!(
                "{} is a smart wearable, not a scene — skipping",
                project.root.display()
            );
            continue;
        }
        convert(project).await?;
    }
    Ok(())
}

fn confirm(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    tracing::warn!(
        "This command will modify your scene code by commenting it out and creating composite/CRDT files."
    );
    for line in [
        "  - every file under src/ is kept, commented out line by line",
        "  - referenced models, images, audio and video are copied under assets/scene/",
        "  - the scene is rebuilt from assets/scene/main.composite afterwards",
        "  - without version control, undoing this is cumbersome",
    ] {
        ux::note(line);
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && !ux::ci()) {
        return Err(UserError::new(
            "code-to-composite needs a confirmation and there is no terminal to ask (or CI)",
            TrySteps::one("re-run with --yes (-y) to convert without the prompt"),
        )
        .into());
    }
    print!("Do you want to continue? [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the confirmation")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

async fn convert(project: &Project) -> Result<()> {
    let root = &project.root;
    let mut steps = ux::Steps::new(4);

    build::build(&build_options(root, true)).await?;
    steps.done("Scene built without its composite");

    let converted = run_driver(root).await?;
    steps.done(format!(
        "Scene run once: {} entit{} in {} component type{} captured",
        converted.entities,
        ies(converted.entities),
        converted.components,
        build::plural(converted.components),
    ));
    for Moved { from, to } in &converted.assets {
        ux::note(format!("  {from} -> {to}"));
    }
    for missing in &converted.missing_assets {
        tracing::warn!("asset not found, its path is left as is: {missing}");
    }
    for skipped in &converted.skipped_dependencies {
        tracing::warn!(
            "glTF dependency left as is, not a relative path inside the model's folder: {skipped}"
        );
    }
    if converted.ui_entities > 0 {
        ux::note(format!(
            "  {} UI entit{} left to code (the editor does not place react-ecs UI)",
            converted.ui_entities,
            ies(converted.ui_entities)
        ));
    }

    let originals = comment_sources(root)?;
    steps.done(format!(
        "Commented out {} source file{} under src/",
        originals.len(),
        build::plural(originals.len() as u64)
    ));

    if let Err(e) = build::build(&build_options(root, false)).await {
        match restore_sources(&originals) {
            true => tracing::warn!(
                "the scene does not build from its composite: src/ is restored, assets/scene/main.composite and main.crdt are kept"
            ),
            false => tracing::warn!(
                "the scene does not build from its composite: assets/scene/main.composite and main.crdt are kept"
            ),
        }
        return Err(e);
    }
    steps.done("Scene rebuilt from assets/scene/main.composite");

    ux::note_good("Scene converted to the visual-editor format");
    ux::note("  open it with dcl-scene-cli start --data-layer, or in the Creator Hub");
    ux::note(
        "  uncomment parts of src/ to keep them dynamic; assets/scene/entity-names.ts names the entities",
    );
    Ok(())
}

fn build_options(root: &Path, ignore_composite: bool) -> BuildOptions {
    BuildOptions {
        built_in: true,
        dir: root.to_path_buf(),
        production: false,
        ignore_composite,
        custom_entry_point: false,
        skip_type_check: false,
        out_root: None,
        quiet: true,
    }
}

async fn run_driver(root: &Path) -> Result<Converted> {
    let node = build::require_node(
        "code-to-composite",
        "the scene has to run once under node to capture what its code creates",
    )?;
    let driver = crate::scene::write_driver(root, "code-to-composite.mjs", DRIVER_TEMPLATE)?;
    let _progress = ux::Slow::start("running the scene");
    let out = tokio::process::Command::new(&node)
        .arg(&driver)
        .arg(root)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
            anyhow::Error::from(
                UserError::new(
                    "could not run the scene (node)",
                    TrySteps::one("check node runs: node --version"),
                )
                .caused_by(e),
            )
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let output = || {
        format!("{}\n{}", stdout.trim(), stderr.trim())
            .trim()
            .to_string()
    };
    if !out.status.success() {
        return Err(UserError::new(
            "the scene failed while running under node — src/ is unchanged, files already copied to assets/scene/ are kept",
            TrySteps::one(
                "fix the error below, or guard code that needs a real explorer behind a system",
            )
            .and("run dcl-scene-cli init --node-modules-only if @dcl/inspector or @dcl/ecs cannot be resolved"),
        )
        .why(output())
        .into());
    }
    if ux::verbose() && !stderr.trim().is_empty() {
        for line in stderr.lines() {
            ux::note(format!("  scene: {line}"));
        }
    }
    stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Converted>(l.trim()).ok())
        .filter(|c| c.ok)
        .ok_or_else(|| {
            UserError::new(
                "the scene ran but reported no result",
                TrySteps::one("re-run with --verbose"),
            )
            .why(output())
            .into()
        })
}

/// Every `.ts`, `.tsx`, `.js` and `.jsx` under `src/` except declarations
/// and anything under a `node_modules`, sorted. Upstream globs the same set.
fn source_files(src: &Path) -> Vec<PathBuf> {
    if !src.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(src)
        .standard_filters(false)
        .filter_entry(|e| e.file_name() != "node_modules")
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            matches!(ext, "ts" | "tsx" | "js" | "jsx") && !name.ends_with(".d.ts")
        })
        .collect();
    files.sort();
    files
}

/// The file commented out line by line under upstream's header; None when it
/// already carries the marker. `export {}` keeps it a module, so the type
/// check still accepts it (and the entrypoint's `import *` of it).
fn comment_out(content: &str) -> Option<String> {
    if content.contains(COMMENT_MARKER) {
        return None;
    }
    const RULE: &str =
        "// ============================================================================\n";
    let mut out = format!(
        "{RULE}// {COMMENT_MARKER}\n//\n\
         // This code has been migrated to main.composite and main.crdt files.\n\
         // The scene can now be edited visually using the Creator Hub app.\n//\n\
         // To restore code-based workflow: Uncomment the code below\n\
         // For hybrid workflow: Uncomment specific parts you want to keep dynamic\n//\n\
         // Learn more: https://docs.decentraland.org/creator\n{RULE}//\n"
    );
    for line in content.split('\n') {
        out.push_str("// ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("export {}\n");
    Some(out)
}

/// Comments out the sources; returns each file it changed with the content
/// the file had. Every file is read before the first write, so a file that
/// cannot be read leaves `src/` as it was.
fn comment_sources(root: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut originals = Vec::new();
    let mut commented = Vec::new();
    for path in source_files(&root.join("src")) {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if let Some(out) = comment_out(&content) {
            originals.push((path, content));
            commented.push(out);
        }
    }
    write_sources(&originals, &commented)?;
    Ok(originals)
}

/// Writes `commented` over the files of `originals`, one for one. A write
/// that fails ends the run with its error, after the files written so far
/// get the content they had.
fn write_sources(originals: &[(PathBuf, String)], commented: &[String]) -> Result<()> {
    for (i, ((path, _), out)) in originals.iter().zip(commented).enumerate() {
        if let Err(e) = std::fs::write(path, out) {
            restore_sources(&originals[..=i]);
            return Err(ux::write_error(path, e));
        }
    }
    Ok(())
}

/// Gives each file the content it had. Every file is tried; false when a
/// file does not have that content afterwards, with a warning that names
/// each such file.
fn restore_sources(originals: &[(PathBuf, String)]) -> bool {
    let lost: Vec<String> = originals
        .iter()
        .filter(|(path, content)| {
            std::fs::write(path, content).is_err()
                && std::fs::read_to_string(path).ok().as_ref() != Some(content)
        })
        .map(|(path, _)| path.display().to_string())
        .collect();
    if !lost.is_empty() {
        tracing::warn!("could not restore these source files: {}", lost.join(", "));
    }
    lost.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_gen::TestDir;

    #[test]
    fn commenting_prefixes_every_line_once_and_keeps_a_module() {
        let src = "import { engine } from '@dcl/sdk/ecs'\n\nexport function main() {\n  engine.addEntity()\n}\n";
        let out = comment_out(src).unwrap();
        assert!(out.starts_with("// ====="), "{out}");
        assert!(out.contains(&format!("// {COMMENT_MARKER}\n")));
        for line in src.lines() {
            assert!(
                out.contains(&format!("// {line}\n")),
                "missing {line:?} in {out}"
            );
        }
        assert!(out.ends_with("// \nexport {}\n"), "{out}");
        assert!(
            !out.lines()
                .any(|l| !l.starts_with("//") && l != "export {}"),
            "{out}"
        );
        assert!(
            comment_out(&out).is_none(),
            "a converted file is left alone"
        );
    }

    #[test]
    fn source_files_take_ts_tsx_js_jsx_but_not_declarations_or_node_modules() {
        let t = TestDir::new("c2c-sources");
        for rel in [
            "src/index.ts",
            "src/ui/panel.tsx",
            "src/legacy/helper.js",
            "src/legacy/widget.jsx",
            "src/types/globals.d.ts",
            "src/node_modules/dep/index.ts",
            "src/data/table.json",
            "src/notes.md",
        ] {
            t.write(rel, "x");
        }
        let found: Vec<String> = source_files(&t.0.join("src"))
            .iter()
            .map(|p| {
                p.strip_prefix(&t.0)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            found,
            [
                "src/index.ts",
                "src/legacy/helper.js",
                "src/legacy/widget.jsx",
                "src/ui/panel.tsx",
            ]
        );
        assert!(source_files(&t.0.join("nope")).is_empty());
    }

    #[test]
    fn the_summary_line_is_read_whole() {
        let c: Converted = serde_json::from_str(
            r#"{"ok":true,"entities":3,"components":4,"uiEntities":1,
                "assets":[{"from":"models/a.glb","to":"assets/scene/Models/a/a.glb"}],
                "missingAssets":["sounds/x.mp3"],"skippedDependencies":["models/a.glb: ../x.bin"]}"#,
        )
        .unwrap();
        assert_eq!((c.entities, c.components, c.ui_entities), (3, 4, 1));
        assert_eq!(
            c.assets,
            [Moved {
                from: "models/a.glb".to_string(),
                to: "assets/scene/Models/a/a.glb".to_string()
            }]
        );
        assert_eq!(c.missing_assets, ["sounds/x.mp3"]);
        assert_eq!(c.skipped_dependencies, ["models/a.glb: ../x.bin"]);
        let empty: Converted = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert_eq!(empty.entities, 0);
        assert!(empty.assets.is_empty());
        // A JSON line that the scene printed has no `ok`: it is not the summary.
        for line in ["{}", r#"{"entities":3}"#, r#"{"ok":"yes"}"#] {
            assert!(serde_json::from_str::<Converted>(line).is_err(), "{line}");
        }
    }

    /// A source that cannot be read stops the command before a write, and a
    /// write that fails gives the files written before it their content back.
    #[test]
    fn a_failed_read_or_write_leaves_the_sources_as_they_were() {
        let t = TestDir::new("c2c-comment");
        t.write("src/a.ts", "export const a = 1\n");
        t.write("src/z.ts", [0xff_u8, 0xfe]);
        let a = t.0.join("src/a.ts");
        let err = comment_sources(&t.0).unwrap_err();
        assert!(format!("{err:#}").contains("z.ts"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "export const a = 1\n");

        let originals = [
            (a.clone(), "export const a = 1\n".to_string()),
            (t.0.join("src/absent/b.ts"), "b\n".to_string()),
        ];
        let commented = ["// a\n".to_string(), "// b\n".to_string()];
        let err = write_sources(&originals, &commented).unwrap_err();
        assert!(format!("{err:#}").contains("b.ts"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "export const a = 1\n");
        assert!(
            !restore_sources(&originals),
            "b.ts has no content to get back"
        );
        assert!(restore_sources(&originals[..1]));
    }
}
