//! `--json`: the command's result as one JSON document on stdout, nothing
//! else there, for the commands that have a result; `start` prints one JSON
//! event per line instead.

#[cfg(unix)]
mod common {
    pub mod child;
    pub mod wait;
}

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_dcl-scene-cli");
const SCENE: &str =
    r#"{"main":"bin/index.js","runtimeVersion":"7","scene":{"parcels":["0,0"],"base":"0,0"}}"#;

/// A folder of this test alone. Its name has a random part, only this user
/// can read or write it, and the test fails if the name exists; the second
/// field removes the folder when the test ends.
struct Fixture(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl Fixture {
    fn empty(tag: &str) -> Self {
        let made = tempfile::Builder::new()
            .prefix(&format!("dcl-scene-cli-json-{tag}-"))
            .tempdir()
            .unwrap();
        Fixture(made.path().to_path_buf(), made)
    }

    fn new(tag: &str) -> Self {
        let f = Self::empty(tag);
        std::fs::create_dir_all(f.0.join("bin")).unwrap();
        std::fs::write(f.0.join("scene.json"), SCENE).unwrap();
        std::fs::write(f.0.join("bin/index.js"), "module.exports = {}\n").unwrap();
        f
    }
}

fn run(args: &[&str]) -> Output {
    run_in(&std::env::current_dir().unwrap(), args)
}

fn run_in(cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args).current_dir(cwd);
    for k in [
        "DCL_PRIVATE_KEY",
        "RUST_LOG",
        "DCL_SCENE_CLI_TARGET_SERVER",
        "CI",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

/// The whole of stdout parses as one document.
fn document(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

#[test]
fn export_static_reports_the_urns_and_entities_as_upstream_does() {
    let f = Fixture::new("export");
    let dir = f.0.display().to_string();
    let dest = f.0.join("out").display().to_string();
    let out = run(&[
        "export-static",
        "--dir",
        &dir,
        "--destination",
        &dest,
        "--skip-build",
        "--baseUrl",
        "https://static.example/scenes/",
        "--json",
    ]);
    let doc = document(&out);
    let urns = doc["scenesUrn"].as_array().unwrap();
    let entities = doc["entities"].as_array().unwrap();
    assert_eq!(urns.len(), 1);
    assert_eq!(entities.len(), 1);
    let entity = entities[0].as_str().unwrap();
    assert!(entity.starts_with("bafkrei"), "{doc}");
    assert_eq!(
        urns[0],
        format!("urn:decentraland:entity:{entity}?=&baseUrl=https://static.example/scenes/")
    );
    let destination = PathBuf::from(doc["destination"].as_str().unwrap());
    assert!(
        destination.join(entity).is_file(),
        "the entity file is at the destination"
    );
    let files = doc["entityFiles"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{doc}");
    assert_eq!(
        PathBuf::from(files[0].as_str().unwrap()),
        destination.join(entity),
        "one path for each entity, in the order of entities"
    );
    assert_eq!(doc["aboutFile"], Value::Null, "no realm was asked for");

    // The content files of each entity: the path in the scene and the hash,
    // which is the name of the file at the destination.
    let content = doc["contentFiles"].as_array().unwrap();
    assert_eq!(content.len(), 1, "one list for each entity: {doc}");
    let listed: Vec<(&str, &str)> = content[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            assert_eq!(c.as_object().unwrap().len(), 2, "{doc}");
            (c["file"].as_str().unwrap(), c["hash"].as_str().unwrap())
        })
        .collect();
    let stored: Value =
        serde_json::from_slice(&std::fs::read(destination.join(entity)).unwrap()).unwrap();
    let in_entity: Vec<(&str, &str)> = stored["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["file"].as_str().unwrap(), c["hash"].as_str().unwrap()))
        .collect();
    assert_eq!(listed, in_entity, "the list of the entity file: {doc}");
    assert!(listed.iter().any(|(file, _)| *file == "bin/index.js"));
    for (file, hash) in &listed {
        let source = std::fs::read(f.0.join(file)).unwrap();
        assert_eq!(std::fs::read(destination.join(hash)).unwrap(), source);
    }
    assert_eq!(doc.as_object().unwrap().len(), 6, "{doc}");

    // Without --json the text is on stdout and no document is.
    let text = run(&[
        "export-static",
        "--dir",
        &dir,
        "--destination",
        &dest,
        "--skip-build",
        "--baseUrl",
        "https://static.example/scenes/",
        "--timestamp",
        &stored["timestamp"].to_string(),
    ]);
    assert!(text.status.success());
    // The destination is in the scene folder, so the second export names each
    // file there that it does not pack; the three result lines follow.
    let stdout: String = String::from_utf8_lossy(&text.stdout)
        .split_inclusive('\n')
        .filter(|line| !line.ends_with(" skipped: it lives inside the destination\n"))
        .collect();
    let files = listed.len();
    let short: String = entity.chars().take(12).collect();
    assert_eq!(
        stdout,
        format!(
            "[1/1] Exported Untitled \u{2014} {files} files (0 new) and the entity {short}\u{2026}\n\
             Destination: {}\n  \u{2192} {}\n",
            destination.display(),
            urns[0].as_str().unwrap()
        )
    );
}

/// With a build, `export-static --json` still gives stdout to the document
/// alone: the output of a custom `scripts.build` goes to stderr.
#[cfg(unix)]
#[test]
fn export_static_keeps_the_output_of_a_build_off_the_document() {
    let f = Fixture::new("export-build");
    std::fs::write(
        f.0.join("package.json"),
        r#"{"name":"custom","private":true,"scripts":{"build":"echo build chatter; echo more chatter >&2"}}"#,
    )
    .unwrap();
    let dir = f.0.display().to_string();
    let dest = f.0.join("out").display().to_string();
    let out = run(&[
        "export-static",
        "--dir",
        &dir,
        "--destination",
        &dest,
        "--json",
    ]);
    let doc = document(&out);
    assert_eq!(doc["entities"].as_array().unwrap().len(), 1, "{doc}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("build chatter"), "{stderr}");
    assert!(stderr.contains("more chatter"), "{stderr}");
}

/// `--realmName` writes the realm's `about`; the document names that file.
#[test]
fn export_static_names_the_realm_file_it_wrote() {
    let f = Fixture::new("export-realm");
    let dir = f.0.display().to_string();
    let dest = f.0.join("out").display().to_string();
    let out = run(&[
        "export-static",
        "--dir",
        &dir,
        "--destination",
        &dest,
        "--skip-build",
        "--baseUrl",
        "https://static.example/scenes/",
        "--realmName",
        "demo",
        "--json",
    ]);
    let doc = document(&out);
    let destination = PathBuf::from(doc["destination"].as_str().unwrap());
    let about = PathBuf::from(doc["aboutFile"].as_str().unwrap());
    assert_eq!(about, destination.join("demo").join("about"), "{doc}");
    assert!(about.is_file(), "{doc}");
}

/// A template in a zip in the folder of `f`, with the given `package.json`.
fn template_zip(f: &Fixture, name: &str, package_json: &str) -> PathBuf {
    use std::io::Write;
    let path = f.0.join(format!("{name}.zip"));
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.add_directory("template-main/", opts).unwrap();
    for (rel, body) in [
        ("scene.json", SCENE),
        ("src/index.ts", "export function main() {}\n"),
        ("package.json", package_json),
    ] {
        zip.start_file(format!("template-main/{rel}"), opts)
            .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// `init --template --json`: the project folder and the packages its
/// package.json names that the vendored SDK did not install, with the range
/// the project asks for. Missing packages do not fail the command.
#[test]
fn init_from_a_template_lists_the_packages_the_vendored_sdk_lacks() {
    let f = Fixture::new("init-template");
    // One package the vendored SDK carries, two it does not, and one that
    // the two sections name with different ranges.
    let zip = template_zip(
        &f,
        "template",
        r#"{"name":"t","dependencies":{"@dcl/sdk":"7","@dcl-sdk/utils":"^1.2.0","left-pad":"1.3.0"},"devDependencies":{"left-pad":"^1.0.0","a-tool":"github:owner/a-tool"}}"#,
    );
    let project = f.0.join("project");
    let out = run(&[
        "init",
        "--dir",
        &project.display().to_string(),
        "--template",
        zip.to_str().unwrap(),
        "--json",
    ]);
    let doc = document(&out);
    assert_eq!(
        doc["missingDependencies"],
        serde_json::json!(["@dcl-sdk/utils", "a-tool", "left-pad"]),
        "{doc}"
    );
    assert_eq!(
        doc["missingPackages"],
        serde_json::json!([
            { "name": "@dcl-sdk/utils", "range": "^1.2.0" },
            { "name": "a-tool", "range": "github:owner/a-tool" },
            { "name": "left-pad", "range": "1.3.0" },
        ]),
        "sorted by name, the range of dependencies first: {doc}"
    );
    assert_eq!(
        PathBuf::from(doc["dir"].as_str().unwrap()),
        project.canonicalize().unwrap(),
        "{doc}"
    );
    assert_eq!(doc.as_object().unwrap().len(), 3, "{doc}");

    // Without --json the text names the packages, as before, and stdout
    // holds no document.
    let text = f.0.join("text");
    let out = run(&[
        "init",
        "--dir",
        &text.display().to_string(),
        "--template",
        zip.to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "the project depends on packages the vendored SDK does not carry: @dcl-sdk/utils, a-tool, left-pad \u{2014} npm install adds them\n"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains('{'), "{stdout}");

    // --skip-install installs nothing, so there is no list to give.
    let skipped = f.0.join("skipped");
    let out = run(&[
        "init",
        "--dir",
        &skipped.display().to_string(),
        "--template",
        zip.to_str().unwrap(),
        "--skip-install",
        "--json",
    ]);
    let doc = document(&out);
    assert_eq!(doc["missingDependencies"], Value::Null, "{doc}");
    assert_eq!(doc["missingPackages"], Value::Null, "{doc}");

    // A project that asks only for packages the vendored SDK carries.
    let zip = template_zip(
        &f,
        "template-full",
        r#"{"name":"t","dependencies":{"@dcl/sdk":"7"}}"#,
    );
    let full = f.0.join("full");
    let out = run(&[
        "init",
        "--dir",
        &full.display().to_string(),
        "--template",
        zip.to_str().unwrap(),
        "--json",
    ]);
    let doc = document(&out);
    assert_eq!(doc["missingDependencies"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["missingPackages"], serde_json::json!([]), "{doc}");
}

/// `init --node-modules-only --missing-only --json`: the scene folder, the
/// packages written and the packages that stayed, each with its version and
/// the version of the vendored archive. The warning is on stderr.
#[test]
fn init_missing_only_lists_the_packages_it_installed_and_kept() {
    let f = Fixture::new("init-missing-only");
    let sdk = f.0.join("node_modules/@dcl/sdk");
    std::fs::create_dir_all(&sdk).unwrap();
    std::fs::write(
        sdk.join("package.json"),
        r#"{"name":"@dcl/sdk","version":"7.0.0"}"#,
    )
    .unwrap();
    let dir = f.0.display().to_string();
    let args = [
        "init",
        "--dir",
        &dir,
        "--node-modules-only",
        "--missing-only",
        "--json",
    ];
    // --dry-run reports the same lists and writes nothing.
    let dry = [&args[..], &["--dry-run"]].concat();
    let report = document(&run(&dry));
    assert_eq!(report["dryRun"], true, "{report}");
    assert!(
        !f.0.join("node_modules/@dcl/ecs").exists(),
        "a dry run installs nothing"
    );

    let out = run(&args);
    let doc = document(&out);
    assert_eq!(doc["dryRun"], false, "{doc}");
    for key in ["dir", "installed", "kept", "stoppedExtraction"] {
        assert_eq!(report[key], doc[key], "{key}: {report}");
    }
    assert_eq!(doc["stoppedExtraction"], false, "{doc}");
    assert_eq!(
        PathBuf::from(doc["dir"].as_str().unwrap()),
        f.0.canonicalize().unwrap(),
        "{doc}"
    );
    let installed: Vec<&str> = doc["installed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    assert!(installed.contains(&"@dcl/ecs"), "{doc}");
    assert!(installed.contains(&"typescript"), "{doc}");
    assert!(!installed.contains(&"@dcl/sdk"), "{doc}");
    assert!(installed.is_sorted(), "{doc}");
    let kept = doc["kept"].as_array().unwrap();
    assert_eq!(kept.len(), 1, "{doc}");
    assert_eq!(kept[0]["name"], "@dcl/sdk", "{doc}");
    assert_eq!(kept[0]["version"], "7.0.0", "{doc}");
    let archive_version = kept[0]["archiveVersion"].as_str().unwrap();
    assert_ne!(archive_version, "7.0.0", "{doc}");
    assert_eq!(kept[0].as_object().unwrap().len(), 3, "{doc}");
    assert_eq!(doc.as_object().unwrap().len(), 5, "{doc}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "warning: node_modules keeps @dcl/sdk 7.0.0; the vendored SDK has @dcl/sdk {archive_version}\n"
        )
    );

    // The second run installs nothing and keeps them all.
    let doc = document(&run(&args));
    assert_eq!(doc["installed"], serde_json::json!([]), "{doc}");
    assert_eq!(
        doc["kept"].as_array().unwrap().len(),
        installed.len() + 1,
        "{doc}"
    );
    assert_eq!(doc["kept"][0]["name"], "@dcl/ecs", "{doc}");
    assert_eq!(
        doc["kept"][0]["version"], doc["kept"][0]["archiveVersion"],
        "{doc}"
    );

    // "Outdated", then "update": the dry run still reports the older SDK
    // with the two versions; `init --node-modules-only` replaces it, because
    // the folder has the vendored stand-in `@dcl/inspector`; the next dry run
    // reports the two versions as equal.
    let sdk = |doc: &Value| {
        let kept = doc["kept"].as_array().unwrap();
        kept.iter()
            .find(|p| p["name"] == "@dcl/sdk")
            .unwrap()
            .clone()
    };
    let report = document(&run(&dry));
    assert_eq!(report["installed"], serde_json::json!([]), "{report}");
    assert_eq!(sdk(&report)["version"], "7.0.0", "{report}");
    assert_eq!(sdk(&report)["archiveVersion"], archive_version, "{report}");
    let update = run(&["init", "--dir", &dir, "--node-modules-only"]);
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let report = document(&run(&dry));
    assert_eq!(report["installed"], serde_json::json!([]), "{report}");
    assert_eq!(sdk(&report)["version"], archive_version, "{report}");
    assert_eq!(sdk(&report)["archiveVersion"], archive_version, "{report}");

    // --dry-run belongs to --missing-only.
    let out = run(&["init", "--dir", &dir, "--node-modules-only", "--dry-run"]);
    assert_eq!(out.status.code(), Some(2), "a usage error");
}

/// `init --node-modules-only --json` without `--missing-only`: one document
/// with the keys of the `--missing-only` one. `installed` has the packages
/// written and `kept` is empty. Without `--json` stdout is the text line.
#[test]
fn init_node_modules_only_gives_the_document_of_the_full_install() {
    const OWN: &str = r#"{"name":"@dcl/sdk","version":"7.0.0"}"#;
    let f = Fixture::new("init-node-modules-only");
    let modules = f.0.join("node_modules");
    std::fs::create_dir_all(modules.join("@dcl/sdk")).unwrap();
    std::fs::write(modules.join("@dcl/sdk/package.json"), OWN).unwrap();
    let dir = f.0.display().to_string();
    let args = ["init", "--dir", &dir, "--node-modules-only", "--json"];

    // The install of another tool (no record) is kept: no package is written.
    let doc = document(&run(&args));
    assert_eq!(doc["installed"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["kept"], serde_json::json!([]), "{doc}");
    assert_eq!(
        std::fs::read_to_string(modules.join("@dcl/sdk/package.json")).unwrap(),
        OWN
    );
    assert_eq!(std::fs::read_dir(&modules).unwrap().count(), 1);

    // No node_modules: each vendored package is written.
    std::fs::remove_dir_all(&modules).unwrap();
    let out = run(&args);
    let doc = document(&out);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let mut keys = Vec::from_iter(doc.as_object().unwrap().keys().map(String::as_str));
    keys.sort();
    assert_eq!(
        keys,
        ["dir", "dryRun", "installed", "kept", "stoppedExtraction"]
    );
    assert_eq!(
        PathBuf::from(doc["dir"].as_str().unwrap()),
        f.0.canonicalize().unwrap(),
        "{doc}"
    );
    assert_eq!(doc["dryRun"], false, "{doc}");
    assert_eq!(doc["kept"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["stoppedExtraction"], false, "{doc}");
    let installed = doc["installed"].as_array().unwrap();
    let installed = Vec::from_iter(installed.iter().map(|name| name.as_str().unwrap()));
    for package in ["@dcl/sdk", "@dcl/ecs", "typescript"] {
        assert!(installed.contains(&package), "{package}: {doc}");
    }
    assert_eq!(installed.len(), 11, "{doc}");
    assert!(installed.is_sorted(), "{doc}");

    // Each package of the archive is now a kept package of `--missing-only`.
    let report = document(&run(&[&args[..], &["--missing-only", "--dry-run"]].concat()));
    assert_eq!(report["installed"], serde_json::json!([]), "{report}");
    let kept = report["kept"].as_array().unwrap();
    let kept = Vec::from_iter(kept.iter().map(|p| p["name"].as_str().unwrap()));
    assert_eq!(kept, installed, "{report}");

    // The second run writes nothing.
    let doc = document(&run(&args));
    assert_eq!(doc["installed"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["kept"], serde_json::json!([]), "{doc}");

    // Another SDK in a folder that has a record: the packages are written.
    std::fs::write(modules.join("@dcl/sdk/package.json"), OWN).unwrap();
    let doc = document(&run(&args));
    assert_eq!(doc["installed"].as_array().unwrap().len(), 11, "{doc}");

    // An empty record: the document says so, and the packages are written.
    std::fs::write(modules.join(".dcl-one-archive"), "").unwrap();
    let doc = document(&run(&args));
    assert_eq!(doc["stoppedExtraction"], true, "{doc}");
    assert_eq!(doc["installed"].as_array().unwrap().len(), 11, "{doc}");
    assert_eq!(document(&run(&args))["stoppedExtraction"], false);

    // Without --json: the text line and no document.
    let text = run(&args[..4]);
    assert!(text.status.success(), "{text:?}");
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(stdout.contains("nothing to do"), "{stdout}");
    assert!(!stdout.contains('{'), "{stdout}");
}

/// The full install stops with exit code 1 at a scope folder that is a link,
/// before it writes a file, and the error names the link. The folder has the
/// record of another archive, so the command has packages to replace.
#[cfg(unix)]
#[test]
fn init_node_modules_only_refuses_a_scope_folder_that_is_a_link() {
    let f = Fixture::new("init-node-modules-only-link");
    let target = f.0.join("elsewhere");
    std::fs::create_dir_all(target.join("sdk")).unwrap();
    std::fs::write(target.join("sdk/own.js"), "own").unwrap();
    std::fs::create_dir_all(f.0.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(&target, f.0.join("node_modules/@dcl")).unwrap();
    let record = f.0.join("node_modules/.dcl-one-archive");
    std::fs::write(&record, "0".repeat(64)).unwrap();
    let dir = f.0.display().to_string();
    let out = run(&["init", "--dir", &dir, "--node-modules-only", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "", "no document");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("node_modules/@dcl is a link"), "{stderr}");
    let entries = |dir: &Path| {
        let mut names = Vec::from_iter(
            std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name()),
        );
        names.sort();
        names
    };
    assert_eq!(
        entries(&f.0.join("node_modules")),
        [".dcl-one-archive", "@dcl"]
    );
    assert_eq!(std::fs::read_to_string(&record).unwrap(), "0".repeat(64));
    assert_eq!(entries(&target), ["sdk"]);
    assert_eq!(entries(&target.join("sdk")), ["own.js"]);
}

/// `--missing-only` keeps a package, not an entry: a folder with no
/// package.json is installed and named in `installed`. With an empty record
/// of the archive (an extraction that stopped) the document says so and no
/// package of the archive is kept. The dry run gives the same document and
/// the same warnings.
#[test]
fn init_missing_only_reports_an_entry_that_is_not_a_package_and_a_stopped_extraction() {
    let f = Fixture::new("init-missing-only-entry");
    let ecs = f.0.join("node_modules/@dcl/ecs");
    std::fs::create_dir_all(&ecs).unwrap();
    let dir = f.0.display().to_string();
    let args = [
        "init",
        "--dir",
        &dir,
        "--node-modules-only",
        "--missing-only",
        "--json",
    ];
    let dry = [&args[..], &["--dry-run"]].concat();
    let both = || {
        let (report, out) = (run(&dry), run(&args));
        assert!(out.status.success(), "{out:?}");
        let (stderr, doc) = (String::from_utf8_lossy(&out.stderr), document(&out));
        assert_eq!(String::from_utf8_lossy(&report.stderr), stderr);
        let report = document(&report);
        assert_eq!(report["dryRun"], true, "{report}");
        for key in ["installed", "kept", "stoppedExtraction"] {
            assert_eq!(report[key], doc[key], "{key}: {report}");
        }
        (doc, stderr.into_owned())
    };

    let (doc, stderr) = both();
    let first = doc["installed"].clone();
    assert_eq!(first.as_array().unwrap().len(), 11, "{doc}");
    assert_eq!(doc["kept"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["stoppedExtraction"], false, "{doc}");
    assert_eq!(
        stderr,
        "warning: node_modules/@dcl/ecs is not a package (no package.json with a version); \
         the vendored package replaces it\n"
    );
    assert!(ecs.join("package.json").is_file());

    let record = f.0.join("node_modules/.dcl-one-archive");
    std::fs::write(&record, "").unwrap();
    let (doc, stderr) = both();
    assert_eq!(doc["stoppedExtraction"], true, "{doc}");
    assert_eq!(doc["kept"], serde_json::json!([]), "{doc}");
    assert_eq!(
        doc["installed"], first,
        "each package of the archive is installed again"
    );
    assert!(
        stderr.starts_with("warning: node_modules/.dcl-one-archive is empty: ")
            && stderr.lines().count() == 1,
        "{stderr}"
    );
    assert_eq!(std::fs::read(&record).unwrap(), b"", "the record stays");
}

/// The default flags in a project folder: the destination is the project,
/// so the export would hold no file. The command fails and writes nothing.
#[test]
fn export_static_with_the_default_flags_is_refused_in_a_project_folder() {
    let f = Fixture::new("export-default");
    let out = run_in(&f.0, &["export-static", "--skip-build", "--json"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("holds the project"), "{stderr}");
    assert!(out.stdout.is_empty(), "no result document");
    let mut left: Vec<_> = std::fs::read_dir(&f.0)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(left, ["bin", "scene.json"], "nothing is written");
}

#[test]
fn a_dry_run_deploy_reports_the_entity_it_packed() {
    let f = Fixture::new("deploy");
    let dir = f.0.display().to_string();
    let out = run(&[
        "--json",
        "deploy",
        "--dir",
        &dir,
        "--skip-build",
        "--dry-run",
    ]);
    let doc = document(&out);
    assert!(
        doc["entityId"].as_str().unwrap().starts_with("bafkrei"),
        "{doc}"
    );
    assert!(
        doc["message"].as_str().unwrap().contains("dry run"),
        "{doc}"
    );
}

/// A custom `scripts.build` writes to stdout; under `--json` that output
/// joins stderr, so the document is still the whole of stdout.
#[cfg(unix)]
#[test]
fn a_custom_build_script_cannot_write_into_the_json_document() {
    let f = Fixture::new("custom-build");
    std::fs::write(
        f.0.join("package.json"),
        r#"{"name":"custom","private":true,"scripts":{"build":"echo build chatter; echo more chatter >&2"}}"#,
    )
    .unwrap();
    let dir = f.0.display().to_string();
    let out = run(&["--json", "deploy", "--dir", &dir, "--dry-run"]);
    let doc = document(&out);
    assert!(
        doc["entityId"].as_str().unwrap().starts_with("bafkrei"),
        "{doc}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("build chatter"), "{stderr}");
    assert!(stderr.contains("more chatter"), "{stderr}");
}

/// `deploy --programmatic` gives stdout to the lines a program parses: the
/// output of a custom `scripts.build` goes to stderr there too.
#[cfg(unix)]
#[test]
fn a_custom_build_script_cannot_write_into_the_programmatic_lines() {
    let f = Fixture::new("custom-build-programmatic");
    std::fs::write(
        f.0.join("package.json"),
        r#"{"name":"custom","private":true,"scripts":{"build":"echo build chatter"}}"#,
    )
    .unwrap();
    let dir = f.0.display().to_string();
    let out = run(&["deploy", "--dir", &dir, "--dry-run", "--programmatic"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(!lines.is_empty(), "no state line");
    for line in lines {
        let parsed: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
        assert_eq!(parsed["event"], "state", "{line}");
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("build chatter"), "{stderr}");
}

/// The flags of a preview that reaches nothing outside this machine and
/// opens no client, the way a parent program starts one.
#[cfg(unix)]
const START: [&str; 9] = [
    "start",
    "--port",
    "0",
    "--no-host",
    "--no-mcp",
    "--offline-comms",
    "--no-asset-bundles",
    "--explorer-alpha",
    "--no-client",
];

/// A scene whose `scripts.build` writes to stdout, then copies the source to
/// the output when the source says `ok` and fails when it does not.
#[cfg(unix)]
fn watched_scene(tag: &str) -> Fixture {
    let f = Fixture::new(tag);
    std::fs::write(
        f.0.join("package.json"),
        r#"{"name":"watched","private":true,"scripts":{"build":"echo build chatter && mkdir -p bin && grep -q ok src/index.ts && cp src/index.ts bin/index.js"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(f.0.join("src")).unwrap();
    std::fs::write(f.0.join("src/index.ts"), "// ok 1\n").unwrap();
    f
}

#[cfg(unix)]
struct Preview {
    child: std::process::Child,
    stdout: common::child::Tail,
    stderr: common::child::Tail,
}

#[cfg(unix)]
impl Preview {
    fn spawn(f: &Fixture, extra: &[&str]) -> Self {
        Self::spawn_on(f, &START, extra, None)
    }

    /// A run that launches clients: `path` is its whole `PATH`, the folder
    /// the system opener is looked up in.
    fn spawn_launching(f: &Fixture, extra: &[&str], path: &Path) -> Self {
        let no_client = START.len() - 1;
        Self::spawn_on(f, &START[..no_client], extra, Some(path))
    }

    fn spawn_on(f: &Fixture, base: &[&str], extra: &[&str], path: Option<&Path>) -> Self {
        let mut cmd = Command::new(BIN);
        cmd.args(base).arg("--dir").arg(&f.0).args(extra);
        for k in ["RUST_LOG", "CI"] {
            cmd.env_remove(k);
        }
        if let Some(path) = path {
            cmd.env("PATH", path);
        }
        Self::run(cmd)
    }

    /// Starts `cmd` with its two streams read while it runs.
    fn run(mut cmd: Command) -> Self {
        let mut child = cmd
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = common::child::Tail::spawn(child.stdout.take().unwrap());
        let stderr = common::child::Tail::spawn(child.stderr.take().unwrap());
        Preview {
            child,
            stdout,
            stderr,
        }
    }

    /// The first complete stdout line that is the event `name`.
    fn event(&self, name: &str) -> Value {
        let found = common::wait::until_blocking(common::wait::HANG_GUARD, || {
            let text = self.stdout.text();
            let whole = &text[..text.rfind('\n').map_or(0, |end| end + 1)];
            whole
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|event| event["event"] == name)
        });
        found.unwrap_or_else(|| {
            panic!(
                "no {name} event\nstdout: {}\nstderr: {}",
                self.stdout.text(),
                self.stderr.text()
            )
        })
    }
}

#[cfg(unix)]
impl Drop for Preview {
    fn drop(&mut self) {
        common::child::terminate(&mut self.child);
    }
}

/// `start --json`: stdout is JSON lines alone (`ready`, then one `rebuild`
/// for a save that built and one `build-error` for a save that did not), and
/// the text of the terminal form is on stderr.
#[cfg(unix)]
#[test]
fn start_prints_one_event_per_line_and_its_text_on_stderr() {
    let f = watched_scene("start-events");
    let preview = Preview::spawn(&f, &["--json"]);

    let ready = preview.event("ready");
    let port = ready["port"].as_u64().unwrap();
    assert_ne!(port, 0, "{ready}");
    assert_eq!(ready["url"], format!("http://127.0.0.1:{port}"), "{ready}");
    let deep_link = ready["deepLink"].as_str().unwrap();
    assert!(deep_link.starts_with("decentraland://"), "{ready}");
    assert!(ready["webUrl"].as_str().unwrap().starts_with("http"));
    assert!(!ready["token"].as_str().unwrap().is_empty(), "{ready}");
    assert_eq!(ready["dataLayerUrl"], Value::Null, "no --data-layer");
    assert_eq!(ready["editorUrl"], Value::Null, "no --data-layer");
    assert_eq!(
        ready["clientsOpened"],
        serde_json::json!([]),
        "--no-client opens nothing: {ready}"
    );

    let source = f.0.join("src/index.ts");
    std::fs::write(&source, "// ok 2\n").unwrap();
    let rebuild = preview.event("rebuild");
    let changed = rebuild["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 1, "{rebuild}");
    let saved = Path::new(changed[0].as_str().unwrap());
    assert!(saved.is_absolute(), "{rebuild}");
    assert!(saved.ends_with("src/index.ts"), "{rebuild}");
    let output = Path::new(rebuild["output"].as_str().unwrap());
    assert!(output.is_absolute(), "{rebuild}");
    assert!(output.ends_with("bin/index.js"), "{rebuild}");
    assert!(rebuild["durationMs"].is_u64(), "{rebuild}");
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "// ok 2\n",
        "the event comes after the output is written"
    );

    std::fs::write(&source, "// bad\n").unwrap();
    let failed = preview.event("build-error");
    assert!(!failed["message"].as_str().unwrap().is_empty(), "{failed}");
    assert!(failed["steps"].is_array(), "{failed}");
    assert!(failed.get("detail").is_some(), "{failed}");

    let stdout = preview.stdout.text();
    let whole = &stdout[..stdout.rfind('\n').map_or(0, |end| end + 1)];
    for line in whole.lines() {
        let event: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
        let name = event["event"].as_str().unwrap_or_default();
        assert!(
            ["ready", "rebuild", "build-error"].contains(&name),
            "{line}"
        );
    }
    let stderr = preview.stderr.text();
    assert!(stderr.contains("Preview server ready"), "{stderr}");
    assert!(
        stderr.contains(&format!("desktop:  {deep_link}")),
        "{stderr}"
    );
    assert!(stderr.contains("build chatter"), "{stderr}");
}

/// Without `--json`, `start` prints no event: the join block is on stdout,
/// with the `desktop:` row a parent program reads.
#[cfg(unix)]
#[test]
fn start_without_the_flag_prints_the_text_form_and_no_event() {
    let f = watched_scene("start-text");
    let preview = Preview::spawn(&f, &[]);
    assert!(
        preview
            .stdout
            .wait_for("desktop:  decentraland://", common::wait::HANG_GUARD),
        "stdout: {}\nstderr: {}",
        preview.stdout.text(),
        preview.stderr.text()
    );
    let stdout = preview.stdout.text();
    assert!(stdout.contains("Preview server ready"), "{stdout}");
    assert!(!stdout.contains("\"event\""), "{stdout}");
    assert!(
        !preview.stderr.text().contains("Preview server ready"),
        "the text form stays on stdout"
    );
}

/// `--skip-auth-screen` and `--multi-instance`: with neither, the deep link
/// ends with both parameters; with one, it has the parameter of that flag
/// alone. The `ready` event and the `desktop:` row give the same link, and
/// text after a standalone `--` is in it.
#[cfg(unix)]
#[test]
fn the_link_flags_of_start_change_the_deep_link() {
    let f = Fixture::new("start-link-flags");
    let link = |extra: &[&str]| {
        let mut args = vec!["--skip-build", "--no-watch", "--json", "--hub"];
        args.extend(extra);
        let preview = Preview::spawn(&f, &args);
        let ready = preview.event("ready");
        let link = ready["deepLink"].as_str().unwrap().to_string();
        assert!(
            preview
                .stderr
                .wait_for(&format!("desktop:  {link}\n"), common::wait::HANG_GUARD),
            "{}",
            preview.stderr.text()
        );
        link
    };
    let tail = |link: &str| link.split_once("&dclenv=org").unwrap().1.to_string();

    assert_eq!(
        tail(&link(&[])),
        "&hub=true&multi-instance=true&skip-auth-screen=true"
    );
    assert_eq!(
        tail(&link(&["--skip-auth-screen"])),
        "&hub=true&skip-auth-screen=true"
    );
    assert_eq!(
        tail(&link(&["--multi-instance"])),
        "&hub=true&multi-instance=true"
    );
    assert_eq!(
        tail(&link(&["--multi-instance", "--skip-auth-screen"])),
        "&hub=true&multi-instance=true&skip-auth-screen=true"
    );
    assert_eq!(
        tail(&link(&["--multi-instance", "--", "--debug"])),
        "&debug=true&hub=true&multi-instance=true"
    );
}

/// A `start --json` run that fails and exits; `port` is the value of `--port`.
#[cfg(unix)]
fn failed_start(dir: &Path, port: &str, extra: &[&str]) -> (Vec<Value>, String) {
    let dir = dir.display().to_string();
    let mut args = vec!["start", "--dir", &dir, "--port", port, "--json"];
    args.extend(&START[3..]);
    args.extend(extra);
    let out = run(&args);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    let events = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    (events, stderr)
}

/// The log lines of `--verbose` and `RUST_LOG` go to stderr: under `--json`
/// each line of stdout is an event.
#[cfg(unix)]
#[test]
fn the_log_lines_of_a_json_run_are_on_stderr() {
    let f = Fixture::new("start-log-lines");
    let empty = f.0.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let dir = empty.display().to_string();
    let mut args = vec!["--verbose", "start", "--dir", &dir, "--json"];
    args.extend(&START[1..]);
    let out = Command::new(BIN)
        .args(&args)
        .env("RUST_LOG", "trace")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    let events: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["code"], "no-scene");
    assert!(stderr.contains("TRACE"), "{stderr}");
}

/// A run that succeeds, with `RUST_LOG=info`: from its start to its stop each
/// line of stdout is an event, and the log lines of the build are on stderr.
#[cfg(unix)]
#[test]
fn the_log_lines_of_a_json_run_that_succeeds_are_on_stderr() {
    let f = scaffolded_scene("start-info-log");
    let mut cmd = Command::new(BIN);
    cmd.args(START).arg("--dir").arg(&f.0);
    cmd.args(["--json", "--skip-type-check"]);
    cmd.env("RUST_LOG", "info").env_remove("CI");
    let mut preview = Preview::run(cmd);
    preview.event("ready");
    let status = common::child::terminate(&mut preview.child);
    assert_eq!(status.code(), Some(0), "a stop on request");

    let stdout = String::from_utf8(preview.stdout.finish()).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or_else(|| panic!("{event}")))
        .collect();
    assert!(names.contains(&"ready"), "{stdout}");
    assert!(!names.contains(&"error"), "{stdout}");
    let stderr = String::from_utf8(preview.stderr.finish()).unwrap();
    let logged = stderr
        .lines()
        .find(|line| line.contains("loader stub saved "));
    let logged = logged.unwrap_or_else(|| panic!("no log line of the build on stderr: {stderr}"));
    assert!(logged.contains("INFO"), "{logged}");
}

/// `start --json` that fails and exits: one `error` event on stdout, with the
/// code of a cause the CLI knows, and the text on stderr as without the flag.
#[cfg(unix)]
#[test]
fn a_start_that_fails_prints_the_error_event() {
    let f = Fixture::new("start-error");
    let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = held.local_addr().unwrap().port().to_string();
    let (events, stderr) = failed_start(&f.0, &port, &["--skip-build", "--no-watch"]);
    assert_eq!(
        events,
        [serde_json::json!({
            "event": "error",
            "message": format!("port {port} is already in use"),
            "code": "port-in-use",
        })]
    );
    assert!(
        stderr.contains(&format!("Error: port {port} is already in use")),
        "{stderr}"
    );
    drop(held);

    let empty = f.0.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let (events, stderr) = failed_start(&empty, "0", &[]);
    assert_eq!(
        events,
        [serde_json::json!({
            "event": "error",
            "message": "this directory is not a Decentraland scene",
            "code": "no-scene",
        })]
    );
    assert!(
        stderr.contains("Error: this directory is not a Decentraland scene"),
        "{stderr}"
    );

    let (events, _) = failed_start(&f.0.join("missing"), "0", &[]);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["code"], "no-directory", "{events:?}");

    std::fs::write(f.0.join("scene.json"), "not json").unwrap();
    let (events, _) = failed_start(&f.0, "0", &[]);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["event"], "error", "{events:?}");
    assert_eq!(events[0]["code"], Value::Null, "no code for this cause");
    assert!(
        events[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("scene.json is not valid JSON"),
        "{events:?}"
    );

    let dir = f.0.display().to_string();
    let out = run(&["start", "--dir", &dir, "--port", "0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "no event without --json");

    let out = run(&["start", "--dir", &dir, "--json", "--no-such-flag"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "the command line does not parse"
    );
    assert!(out.stdout.is_empty(), "no event for a usage error");
}

/// With watch on, a first build that fails does not end the run: the events
/// are `build-error`, then `ready`, and a later save that builds gives
/// `rebuild`. With `--no-watch` the same failure ends the run with `error`.
#[cfg(unix)]
#[test]
fn a_first_build_that_fails_gives_build_error_then_ready_then_rebuild() {
    let f = watched_scene("start-first-build");
    let source = f.0.join("src/index.ts");
    std::fs::write(&source, "// bad\n").unwrap();
    std::fs::remove_dir_all(f.0.join("bin")).unwrap();

    let (events, _) = failed_start(&f.0, "0", &["--no-watch"]);
    let names: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["error"], "{events:?}");
    assert_eq!(events[0]["code"], Value::Null, "{events:?}");

    let preview = Preview::spawn(&f, &["--json"]);
    preview.event("ready");
    std::fs::write(&source, "// ok 2\n").unwrap();
    preview.event("rebuild");
    let stdout = preview.stdout.text();
    let names: Vec<String> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|event| event["event"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["build-error", "ready", "rebuild"], "{stdout}");
}

/// A scene from `init`: the built-in bundler builds it with the vendored SDK
/// and writes the scene chunk `bin/scene.js` beside the `main` file.
#[cfg(unix)]
fn scaffolded_scene(tag: &str) -> Fixture {
    let f = Fixture::empty(tag);
    let out = run(&["init", "--dir", f.0.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    f
}

/// The `output` of each `rebuild` event of the run, after `count` of them.
#[cfg(unix)]
fn rebuild_outputs(preview: &Preview, count: usize) -> Vec<String> {
    let found = common::wait::until_blocking(common::wait::HANG_GUARD, || {
        let text = preview.stdout.text();
        let whole = &text[..text.rfind('\n').map_or(0, |end| end + 1)];
        let outputs: Vec<String> = whole
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| event["event"] == "rebuild")
            .map(|event| event["output"].as_str().unwrap().to_string())
            .collect();
        (outputs.len() >= count).then_some(outputs)
    });
    found.unwrap_or_else(|| {
        panic!(
            "fewer than {count} rebuild events\nstdout: {}\nstderr: {}",
            preview.stdout.text(),
            preview.stderr.text()
        )
    })
}

/// The built-in bundler: the `output` of `rebuild` is the `main` file of
/// scene.json, as it is for a custom `scripts.build`, and not the scene chunk
/// the bundler wrote beside it.
#[cfg(unix)]
#[test]
fn a_rebuild_of_the_built_in_bundler_names_the_main_file() {
    let f = scaffolded_scene("start-built-in");
    let main = f.0.canonicalize().unwrap().join("bin/index.js");
    let preview = Preview::spawn(&f, &["--json", "--skip-type-check"]);
    preview.event("ready");
    assert!(
        f.0.join("bin/scene.js").is_file(),
        "the built-in bundler built this scene"
    );

    let source = f.0.join("src/index.ts");
    let code = std::fs::read_to_string(&source).unwrap();
    std::fs::write(&source, format!("{code}\nexport const saved = 1\n")).unwrap();
    let outputs = rebuild_outputs(&preview, 1);
    assert_eq!(outputs, [main.to_str().unwrap()]);
    assert!(main.is_file());
}

/// The built-in bundler after a first build that failed: the save that
/// repairs the scene and each later save give the same `output`.
#[cfg(unix)]
#[test]
fn the_rebuilds_after_a_failed_first_build_name_the_main_file() {
    let f = scaffolded_scene("start-built-in-retry");
    let main = f.0.canonicalize().unwrap().join("bin/index.js");
    let source = f.0.join("src/index.ts");
    let code = std::fs::read_to_string(&source).unwrap();
    std::fs::write(&source, "export function main() { const x = = 1 }\n").unwrap();

    let preview = Preview::spawn(&f, &["--json", "--skip-type-check"]);
    preview.event("build-error");
    preview.event("ready");
    std::fs::write(&source, &code).unwrap();
    rebuild_outputs(&preview, 1);
    std::fs::write(&source, format!("{code}\nexport const saved = 1\n")).unwrap();
    let outputs = rebuild_outputs(&preview, 2);
    for output in &outputs {
        assert_eq!(output, main.to_str().unwrap(), "{outputs:?}");
    }
}

/// The token of the run is in the `ready` event only: stderr, where the text
/// of the terminal form goes, does not have it, with `--talk` too. A stop on
/// request ends the run with exit code 0 and no `error` event.
#[cfg(unix)]
#[test]
fn the_token_of_a_json_run_is_not_on_stderr() {
    let f = Fixture::new("start-token");
    let preview = Preview::spawn(
        &f,
        &[
            "--json",
            "--skip-build",
            "--no-watch",
            "--talk",
            "--no-browser",
            "--player",
            "http://player.example/",
        ],
    );
    let ready = preview.event("ready");
    let token = ready["token"].as_str().unwrap();
    assert!(token.len() >= 16, "{ready}");
    assert!(
        preview.stderr.wait_for("Talk: ", common::wait::HANG_GUARD),
        "{}",
        preview.stderr.text()
    );
    let port = ready["port"].as_u64().unwrap();
    assert!(
        preview.stderr.wait_for(
            &format!("Talk: http://127.0.0.1:{port}/talk\n"),
            common::wait::HANG_GUARD
        ),
        "{}",
        preview.stderr.text()
    );
    assert!(
        !preview.stderr.text().contains(token),
        "the token is on stderr"
    );
    assert_eq!(
        preview.stdout.text().matches(token).count(),
        1,
        "the token is in the ready event alone"
    );

    let mut preview = preview;
    let status = common::child::terminate(&mut preview.child);
    assert_eq!(status.code(), Some(0), "a stop on request");
    let stdout = String::from_utf8(preview.stdout.finish()).unwrap();
    assert!(!stdout.contains(r#""event":"error""#), "{stdout}");
}

/// The name the CLI starts the system opener by on this platform.
#[cfg(unix)]
const OPENER: &str = if cfg!(target_os = "macos") {
    "open"
} else {
    "xdg-open"
};

#[cfg(unix)]
const LAUNCH: [&str; 5] = [
    "--skip-build",
    "--no-watch",
    "--skip-type-check",
    "--no-livekit",
    "--web",
];

/// The `client-failed` lines of a run, in the order of stdout.
#[cfg(unix)]
fn client_failures(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event"] == "client-failed")
        .collect()
}

/// `--explorer-alpha --web` with an opener that fails on the desktop link and
/// passes on the web link: both openers started, so both clients are in
/// `clientsOpened`, and the desktop one gets a `client-failed` line after
/// `ready` with the status of its opener. Each opener got the link of the
/// `ready` line, and the web one, which ended with status 0, gets no line.
#[cfg(unix)]
#[test]
fn an_opener_that_fails_gives_a_client_failed_line_after_ready() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("start-opener-fails");
    let bin = f.0.join("opener");
    std::fs::create_dir_all(&bin).unwrap();
    let opener = bin.join(OPENER);
    std::fs::write(
        &opener,
        "#!/bin/sh\ncase \"$1\" in\n  decentraland:*) printf '%s' \"$1\" > \"$0.desktop\"; exit 3 ;;\n  *) printf '%s' \"$1\" > \"$0.web\" ;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut args = vec!["--json"];
    args.extend(LAUNCH);
    let mut preview = Preview::spawn_launching(&f, &args, &bin);

    let ready = preview.event("ready");
    assert_eq!(
        ready["clientsOpened"],
        serde_json::json!(["desktop", "web"]),
        "{ready}"
    );
    let failed = preview.event("client-failed");
    assert_eq!(
        failed,
        serde_json::json!({
            "event": "client-failed",
            "client": "desktop",
            "message": format!("{OPENER} ended with exit status: 3"),
        })
    );
    let given = |client: &str, link: &Value| {
        let file = bin.join(format!("{OPENER}.{client}"));
        let link = link.as_str().unwrap().to_string();
        let got = common::wait::until_blocking(common::wait::HANG_GUARD, || {
            std::fs::read_to_string(&file)
                .ok()
                .filter(|got| *got == link)
        });
        assert!(got.is_some(), "the {client} opener did not get {link}");
    };
    given("desktop", &ready["deepLink"]);
    given("web", &ready["webUrl"]);

    let status = common::child::terminate(&mut preview.child);
    assert_eq!(status.code(), Some(0), "a stop on request");
    let stdout = String::from_utf8(preview.stdout.finish()).unwrap();
    let names: Vec<String> = stdout
        .lines()
        .map(|line| {
            let event: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
            event["event"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(names, ["ready", "client-failed"], "{stdout}");
    let stderr = preview.stderr.text();
    assert!(stderr.contains("Desktop client: "), "{stderr}");
    assert!(!stderr.contains("could not open"), "{stderr}");
}

/// An opener that a signal ends gives no `client-failed` line: the event is
/// for a launch that failed. The desktop opener ends by a signal; the web
/// opener waits until the CLI has read that end, then fails, so its line is
/// the only one.
#[cfg(unix)]
#[test]
fn an_opener_that_a_signal_ends_gives_no_client_failed_line() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("start-opener-signal");
    let bin = f.0.join("opener");
    std::fs::create_dir_all(&bin).unwrap();
    let opener = bin.join(OPENER);
    // `kill -0` finds the first opener until the CLI has waited for it.
    std::fs::write(
        &opener,
        "#!/bin/sh\ncase \"$1\" in\n  decentraland:*) echo $$ > \"$0.pid\"; kill -KILL $$ ;;\n  *) until [ -s \"$0.pid\" ]; do :; done; read -r first < \"$0.pid\"\n     while kill -0 \"$first\" 2>/dev/null; do :; done; exit 3 ;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut args = vec!["--json"];
    args.extend(LAUNCH);
    let mut preview = Preview::spawn_launching(&f, &args, &bin);

    let ready = preview.event("ready");
    assert_eq!(
        ready["clientsOpened"],
        serde_json::json!(["desktop", "web"]),
        "{ready}"
    );
    let failed = preview.event("client-failed");
    assert_eq!(failed["client"], "web", "{failed}");
    let status = common::child::terminate(&mut preview.child);
    assert_eq!(status.code(), Some(0), "a stop on request");
    let stdout = String::from_utf8(preview.stdout.finish()).unwrap();
    assert_eq!(client_failures(&stdout), [failed], "{stdout}");
}

/// No opener on `PATH`: no client is in `clientsOpened`, each launch gets a
/// `client-failed` line after `ready`, and the note of the text form names
/// the client. Without `--json` the note is on stdout and no event prints.
#[cfg(unix)]
#[test]
fn an_opener_that_does_not_start_gives_a_client_failed_line_and_a_note() {
    let f = Fixture::new("start-no-opener");
    let empty = f.0.join("no-opener");
    std::fs::create_dir_all(&empty).unwrap();
    let note = |client: &str| {
        format!("could not open {client} automatically \u{2014} open the URL above manually\n")
    };

    let mut args = vec!["--json"];
    args.extend(LAUNCH);
    let preview = Preview::spawn_launching(&f, &args, &empty);
    let ready = preview.event("ready");
    assert_eq!(ready["clientsOpened"], serde_json::json!([]), "{ready}");
    let failures = common::wait::until_blocking(common::wait::HANG_GUARD, || {
        Some(client_failures(&preview.stdout.text())).filter(|found| found.len() == 2)
    })
    .unwrap_or_else(|| panic!("stdout: {}", preview.stdout.text()));
    for (failure, client) in failures.iter().zip(["desktop", "web"]) {
        assert_eq!(failure["client"], client, "{failure}");
        let message = failure["message"].as_str().unwrap();
        assert!(
            message.starts_with(&format!("{OPENER} did not start: ")),
            "{failure}"
        );
    }
    let stdout = preview.stdout.text();
    assert!(
        stdout.find(r#""event":"ready""#) < stdout.find(r#""event":"client-failed""#),
        "{stdout}"
    );
    for client in ["the desktop client", "the web explorer"] {
        assert!(
            preview
                .stderr
                .wait_for(&note(client), common::wait::HANG_GUARD),
            "{}",
            preview.stderr.text()
        );
    }
    drop(preview);

    let preview = Preview::spawn_launching(&f, &LAUNCH, &empty);
    for client in ["the desktop client", "the web explorer"] {
        assert!(
            preview
                .stdout
                .wait_for(&note(client), common::wait::HANG_GUARD),
            "stdout: {}\nstderr: {}",
            preview.stdout.text(),
            preview.stderr.text()
        );
    }
    assert!(!preview.stdout.text().contains("\"event\""));
}
