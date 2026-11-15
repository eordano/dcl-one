use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

// Only the unix-only provisioned-scene test reads the gate.
#[cfg(unix)]
mod common {
    pub mod env;
    pub mod testgate;
}

const BIN: &str = env!("CARGO_BIN_EXE_dcl-scene-cli");

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("dcl-scene-cli-initgate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fixture(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn dir_arg(&self) -> String {
        self.0.display().to_string()
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.0.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
    }

    fn json(&self, rel: &str) -> serde_json::Value {
        serde_json::from_str(&self.read(rel)).unwrap_or_else(|e| panic!("parsing {rel}: {e}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args).stdin(Stdio::null());
    for k in ["RUST_LOG", "NO_COLOR"] {
        cmd.env_remove(k);
    }
    cmd.output().unwrap()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const SCENE_FILES: &[&str] = &[
    "scene.json",
    "package.json",
    "tsconfig.json",
    "src/index.ts",
    ".gitignore",
    ".dclignore",
    "README.md",
    "images/scene-thumbnail.png",
];

#[test]
fn scene_scaffold_is_complete_and_loadable() {
    let f = Fixture::new("scene");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(stdout.contains("Scaffolded a scene project"), "{stdout}");
    assert!(stdout.contains("default scene project"), "{stdout}");
    assert!(!stdout.contains("npm install"), "{stdout}");
    assert!(
        stdout.contains("Installed node_modules from the vendored SDK"),
        "{stdout}"
    );
    assert!(stdout.contains("dcl-scene-cli start"), "{stdout}");
    for rel in SCENE_FILES {
        assert!(f.path().join(rel).is_file(), "missing {rel}");
    }
    let scene = f.json("scene.json");
    assert_eq!(scene["runtimeVersion"], "7");
    assert_eq!(scene["main"], "bin/index.js");
    assert_eq!(scene["scene"]["parcels"][0], "0,0");
    let title = scene["display"]["title"].as_str().unwrap();
    assert!(title.starts_with("dcl-scene-cli-initgate-scene"), "{title}");
    let pkg = f.json("package.json");
    for (name, version) in pkg["devDependencies"].as_object().unwrap() {
        let v = version.as_str().unwrap();
        assert_ne!(v, "latest", "{name} is unpinned");
        assert!(
            v.chars().next().unwrap().is_ascii_digit(),
            "{name}={v} is not an exact pin"
        );
    }
    for script in ["start", "build", "deploy"] {
        assert!(
            pkg["scripts"][script].is_string(),
            "missing script {script}"
        );
    }
    assert!(
        f.path()
            .join("node_modules/@dcl/sdk/package.json")
            .is_file()
    );
    assert!(
        f.path()
            .join("node_modules/typescript/lib/tsc.js")
            .is_file()
    );
    assert!(f.read(".dclignore").lines().any(|l| l == "node_modules"));
    assert!(f.read(".gitignore").lines().any(|l| l == "bin/"));
    let png = std::fs::read(f.path().join("images/scene-thumbnail.png")).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let project = dcl_scene_cli::scene::Project::load(f.path()).expect("scaffold must load");
    assert_eq!(project.main_output().unwrap(), "bin/index.js");
    assert_eq!(project.parcels(), vec!["0,0".to_string()]);
    assert!(dcl_scene_cli::scene::min_cli_warning(f.path()).is_none());
}

#[test]
fn refuses_a_non_empty_directory_with_next_steps() {
    let f = Fixture::new("nonempty");
    std::fs::write(f.path().join("existing.txt"), "hi").unwrap();
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    let error_lines: Vec<&str> = err.lines().filter(|l| l.starts_with("Error: ")).collect();
    assert_eq!(error_lines.len(), 1, "stderr: {err}");
    assert!(error_lines[0].contains("not empty"), "stderr: {err}");
    assert!(err.contains("\u{2192} try: "), "stderr: {err}");
    assert!(err.contains("--yes"), "stderr: {err}");
    assert!(!err.contains('\u{1b}'), "ANSI leaked: {err}");
    assert!(!f.path().join("scene.json").exists());
}

#[test]
fn yes_scaffolds_into_a_non_empty_directory() {
    let f = Fixture::new("yes");
    std::fs::write(f.path().join("existing.txt"), "hi").unwrap();
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir, "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(f.path().join("scene.json").is_file());
    assert_eq!(f.read("existing.txt"), "hi");
}

#[test]
fn smart_wearable_scaffold_has_wearable_skeleton() {
    let f = Fixture::new("sw");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir, "--project", "smart-wearable"]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("Scaffolded a smart wearable project"),
        "{stdout}"
    );
    assert!(stdout.contains("thumbnail.png"), "{stdout}");
    let wearable = f.json("wearable.json");
    let id = wearable["id"].as_str().unwrap();
    assert_eq!(id.len(), 36);
    assert_eq!(id.as_bytes()[14], b'4');
    assert_eq!(wearable["data"]["category"], "eyewear");
    assert_eq!(wearable["rarity"], "mythic");
    let rep = &wearable["data"]["representations"][0];
    assert_eq!(rep["mainFile"], "model.glb");
    assert_eq!(rep["contents"][0], "model.glb");
    assert_eq!(rep["bodyShapes"].as_array().unwrap().len(), 2);
    let scene = f.json("scene.json");
    assert_eq!(scene["isPortableExperience"], true);
    assert_eq!(scene["runtimeVersion"], "7");
    assert_eq!(scene["scene"]["parcels"].as_array().unwrap().len(), 100);
    assert_eq!(scene["scene"]["base"], "0,0");
    let pkg = f.json("package.json");
    assert!(pkg["scripts"]["pack"].is_string());
    assert!(f.read("README.md").contains("model.glb"));
    let glb = std::fs::read(f.path().join("model.glb")).unwrap();
    assert_eq!(&glb[..4], b"glTF", "model.glb is a glTF binary");
    let png = std::fs::read(f.path().join("thumbnail.png")).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(&png[16..24], &[0, 0, 1, 0, 0, 0, 1, 0], "256x256");
    let second = Fixture::new("sw2");
    let dir2 = second.dir_arg();
    let out2 = run(&["init", "--dir", &dir2, "--project", "smart-wearable"]);
    assert!(out2.status.success());
    assert_ne!(second.json("wearable.json")["id"], wearable["id"]);
}

#[test]
fn init_creates_a_missing_target_directory() {
    let f = Fixture::new("mkdir");
    let nested = f.path().join("brand/new/scene");
    let dir = nested.display().to_string();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(nested.join("scene.json").is_file());
    let scene: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(nested.join("scene.json")).unwrap()).unwrap();
    assert_eq!(scene["display"]["title"], "scene");
}

/// `node` on PATH, or the gate's refusal: the vendored toolchain type-checks
/// through it.
#[cfg(unix)]
fn node_available() -> Option<()> {
    match dcl_scene_cli::build::find_node() {
        Some(_) => Some(()),
        None => common::testgate::unavailable("node", "no node binary on PATH"),
    }
}

#[cfg(unix)]
fn run_with_path(args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args).stdin(Stdio::null()).env_remove("RUST_LOG");
    cmd.output().unwrap()
}

/// The scaffold builds with nothing but the vendored node_modules: SDK
/// chunk, scene chunk, loader stub and a passing type check.
#[cfg(unix)]
#[test]
fn scene_scaffold_builds_with_the_vendored_toolchain() {
    let Some(()) = node_available() else { return };
    let f = Fixture::new("vendored-build");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let out = run_with_path(&["build", "--dir", &dir]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "build failed\nstdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    assert!(stdout.contains("SDK chunk installed"), "{stdout}");
    assert!(stdout.contains("Scene chunk saved"), "{stdout}");
    assert!(stdout.contains("Type check passed"), "{stdout}");
    for (rel, min) in [
        ("bin/sdk-runtime.js", 100_000usize),
        ("bin/index.js", 1_000),
        ("bin/scene.js", 100),
    ] {
        let bytes = std::fs::read(f.path().join(rel)).unwrap();
        assert!(
            bytes.len() > min,
            "{rel} suspiciously small: {} bytes",
            bytes.len()
        );
    }
}

/// A scene tsconfig from before the template moved to `bundler`:
/// `"moduleResolution": "node"`, which the vendored TypeScript 6 deprecates
/// as a value (TS5107, reported as `node10`). The build repeats the check
/// with `--ignoreDeprecations 6.0` and says which value needed it.
#[cfg(unix)]
#[test]
fn scene_with_a_deprecated_module_resolution_builds_with_the_vendored_toolchain() {
    let Some(()) = node_available() else { return };
    let f = Fixture::new("vendored-node10");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let tsconfig = f.read("tsconfig.json").replace(
        "\"moduleResolution\": \"bundler\"",
        "\"moduleResolution\": \"node\"",
    );
    assert!(
        tsconfig.contains("\"moduleResolution\": \"node\""),
        "{tsconfig}"
    );
    std::fs::write(f.path().join("tsconfig.json"), tsconfig).unwrap();
    let out = run_with_path(&["build", "--dir", &dir]);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(
        out.status.success(),
        "build failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("Type check passed"), "{stdout}");
    assert!(
        stderr.contains("--ignoreDeprecations 6.0") && stderr.contains("moduleResolution=node10"),
        "{stderr}"
    );

    // The next check passes the flag at once, having learnt it is needed,
    // and says the same.
    let source = f.read("src/index.ts");
    std::fs::write(
        f.path().join("src/index.ts"),
        format!("{source}\n// edited\n"),
    )
    .unwrap();
    let out = run_with_path(&["build", "--dir", &dir]);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Type check passed ("), "{stdout}");
    assert!(
        stderr.contains("--ignoreDeprecations 6.0") && stderr.contains("moduleResolution=node10"),
        "{stderr}"
    );
    // What it learnt still reports this scene's type errors.
    std::fs::write(
        f.path().join("src/index.ts"),
        format!("{source}\nexport const wrong: number = 'text'\n"),
    )
    .unwrap();
    let out = run_with_path(&["build", "--dir", &dir]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("TS2322") && !stderr.contains("TS5107"),
        "{stderr}"
    );

    // A tsconfig that no longer needs the flag is checked without it.
    let fixed = f.read("tsconfig.json").replace(
        "\"moduleResolution\": \"node\"",
        "\"moduleResolution\": \"bundler\"",
    );
    std::fs::write(f.path().join("tsconfig.json"), fixed).unwrap();
    std::fs::write(f.path().join("src/index.ts"), source).unwrap();
    let out = run_with_path(&["build", "--dir", &dir]);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Type check passed ("), "{stdout}");
    assert!(!stderr.contains("ignoreDeprecations"), "{stderr}");
}

/// The upstream template's tsconfig sets `baseUrl`, which the vendored
/// TypeScript 6 deprecates (TS5101): the build knows from the tsconfig and
/// says so, on the first check and on later ones.
#[cfg(unix)]
#[test]
fn scene_with_base_url_builds_with_the_vendored_toolchain() {
    let Some(()) = node_available() else { return };
    let f = Fixture::new("vendored-baseurl");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let tsconfig = f.read("tsconfig.json").replacen(
        "\"compilerOptions\": {",
        "\"compilerOptions\": {\n    \"baseUrl\": \".\",",
        1,
    );
    assert!(tsconfig.contains("\"baseUrl\""), "{tsconfig}");
    std::fs::write(f.path().join("tsconfig.json"), &tsconfig).unwrap();
    let source = f.read("src/index.ts");
    for edit in ["", "\n// edited\n"] {
        std::fs::write(f.path().join("src/index.ts"), format!("{source}{edit}")).unwrap();
        let out = run_with_path(&["build", "--dir", &dir]);
        let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
        assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
        assert!(stdout.contains("Type check passed ("), "{stdout}");
        assert!(
            stderr.contains("tsconfig uses baseUrl, which TypeScript 6.")
                && stderr.contains("--ignoreDeprecations 6.0"),
            "{stderr}"
        );
    }
    std::fs::write(
        f.path().join("src/index.ts"),
        format!("{source}\nexport const wrong: number = 'text'\n"),
    )
    .unwrap();
    let out = run_with_path(&["build", "--dir", &dir]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("TS2322") && !stderr.contains("TS5101"),
        "{stderr}"
    );

    std::fs::write(
        f.path().join("tsconfig.json"),
        tsconfig.replace("\"baseUrl\": \".\",", ""),
    )
    .unwrap();
    std::fs::write(f.path().join("src/index.ts"), source).unwrap();
    let out = run_with_path(&["build", "--dir", &dir]);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stderr.contains("ignoreDeprecations"), "{stderr}");
}

/// init, then code-to-composite: it refuses without `--yes`, converts the
/// scaffold to a composite the build runs from, and a second run changes nothing.
#[cfg(unix)]
#[test]
fn code_to_composite_converts_the_scaffold_and_survives_a_second_run() {
    let Some(()) = node_available() else { return };
    let f = Fixture::new("code-to-composite");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let out = run_with_path(&["code-to-composite", "--dir", &dir]);
    assert!(
        !out.status.success(),
        "no terminal and no --yes must refuse"
    );
    assert!(stderr_of(&out).contains("--yes"), "{}", stderr_of(&out));
    assert!(!f.path().join("assets/scene/main.composite").exists());

    let out = run_with_path(&["code-to-composite", "--dir", &dir, "-y"]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "conversion failed\nstdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    assert!(stdout.contains("1 entity in 3 component types"), "{stdout}");
    assert!(stdout.contains("Commented out 1 source file"), "{stdout}");
    assert!(
        stdout.contains("converted to the visual-editor format"),
        "{stdout}"
    );
    let composite = f.json("assets/scene/main.composite");
    assert_eq!(composite["version"], 1);
    let names: Vec<&str> = composite["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["core::Transform", "core::MeshRenderer", "core-schema::Name"],
        "{composite}"
    );
    let raw = f.read("assets/scene/main.composite");
    assert!(
        raw.contains("\"Box\""),
        "the cube is named after its mesh: {raw}"
    );
    assert!(raw.contains("\"box\""), "{raw}");
    assert!(
        std::fs::metadata(f.path().join("main.crdt")).unwrap().len() > 0,
        "main.crdt is written"
    );
    let index = f.read("src/index.ts");
    assert_eq!(
        index
            .matches(dcl_scene_cli::code_to_composite::COMMENT_MARKER)
            .count(),
        1,
        "{index}"
    );
    assert!(index.contains("// export function main() {"), "{index}");
    assert!(index.ends_with("export {}\n"), "{index}");
    assert!(f.read("assets/scene/entity-names.ts").contains("Box"));
    assert!(
        f.path().join("bin/index.js").exists(),
        "rebuilt from the composite"
    );

    let out = run_with_path(&["code-to-composite", "--dir", &dir, "--yes"]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "second conversion failed\nstdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    assert!(stdout.contains("Commented out 0 source files"), "{stdout}");
    let again = f.json("assets/scene/main.composite");
    assert_eq!(again, composite, "a converted scene round-trips unchanged");
    assert_eq!(f.read("src/index.ts"), index, "sources are commented once");
}

/// Only the build from the composites reads `assets/broken.composite`: it
/// fails after the sources were commented out, and they come back as they were.
#[cfg(unix)]
#[test]
fn code_to_composite_restores_the_sources_when_the_rebuild_fails() {
    let Some(()) = node_available() else { return };
    let f = Fixture::new("code-to-composite-restore");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let index = f.read("src/index.ts");
    std::fs::create_dir_all(f.path().join("assets")).unwrap();
    std::fs::write(f.path().join("assets/broken.composite"), "not json").unwrap();
    let out = run_with_path(&["code-to-composite", "--dir", &dir, "--yes"]);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(!out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Commented out 1 source file"), "{stdout}");
    assert!(stderr.contains("src/ is restored"), "{stderr}");
    assert_eq!(f.read("src/index.ts"), index);
}

/// A model whose glTF names one dependency plainly, one that is not there,
/// one that is a folder, one with `..` and one by URL: the sibling moves with
/// the model, the other four are left as they are and reported, and nothing
/// lands where the `..` path would point. A model whose glTF lists are not
/// lists of objects moves alone: its dependencies cannot be read.
#[cfg(unix)]
#[test]
fn code_to_composite_copies_only_gltf_dependencies_inside_the_model_folder() {
    let Some(()) = node_available() else { return };
    let (f, stdout, stderr) = convert_scene(
        "code-to-composite-gltf",
        &[
            (
                "models/evil.gltf",
                r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"evil.bin","byteLength":4},{"uri":"gone.bin","byteLength":4},{"uri":"folder.bin","byteLength":4},{"uri":"../secret.bin","byteLength":4},{"uri":"https://cdn.example/far.bin","byteLength":4}]}"#,
            ),
            ("models/evil.bin", "next"),
            ("models/folder.bin/inner", "inner"),
            ("secret.bin", "keep"),
            (
                "models/odd.gltf",
                r#"{"asset":{"version":"2.0"},"images":[null],"buffers":[{"uri":"odd.bin","byteLength":4}]}"#,
            ),
            ("models/odd.bin", "odd"),
        ],
        r#"import { engine, GltfContainer, Transform } from '@dcl/sdk/ecs'
import { Vector3 } from '@dcl/sdk/math'

export function main() {
  for (const src of ['models/evil.gltf', 'models/odd.gltf']) {
    const model = engine.addEntity()
    Transform.create(model, { position: Vector3.create(8, 0, 8) })
    GltfContainer.create(model, { src })
  }
}
"#,
    );
    assert!(
        stdout.contains("models/evil.gltf -> assets/scene/Models/evil/evil.gltf"),
        "{stdout}"
    );
    assert!(
        f.path()
            .join("assets/scene/Models/evil/evil.gltf")
            .is_file()
    );
    assert!(
        f.path().join("assets/scene/Models/evil/evil.bin").is_file(),
        "a plain sibling moves with its model"
    );
    assert!(
        !f.path().join("assets/scene/Models/secret.bin").exists(),
        "a .. dependency is not copied to where its path lands"
    );
    assert!(f.path().join("secret.bin").is_file());
    assert!(
        !f.path()
            .join("assets/scene/Models/evil/folder.bin")
            .exists(),
        "a dependency that is a folder is not copied"
    );
    assert!(
        stdout.contains("models/odd.gltf -> assets/scene/Models/odd/odd.gltf"),
        "{stdout}"
    );
    assert!(
        !f.path().join("assets/scene/Models/odd/odd.bin").exists(),
        "no dependency is read from lists that are not lists of objects"
    );
    for left in [
        "asset not found, its path is left as is: models/gone.bin",
        "asset not found, its path is left as is: models/folder.bin",
        "models/evil.gltf: ../secret.bin",
        "models/evil.gltf: https://cdn.example/far.bin",
    ] {
        assert!(stderr.contains(left), "{left} not reported:\n{stderr}");
    }
    assert!(
        f.read("assets/scene/main.composite")
            .contains("assets/scene/Models/evil/evil.gltf"),
        "the component path follows the model"
    );
}

/// A scene that leaves a timer running does not keep the conversion: the run
/// ends after the first frame. What the scene prints through `console` is not
/// read as the result of the run.
#[cfg(unix)]
#[test]
fn code_to_composite_ends_when_the_scene_leaves_a_timer_running() {
    let Some(()) = node_available() else { return };
    let (f, stdout, _) = convert_scene(
        "code-to-composite-timer",
        &[],
        r#"import { engine, MeshRenderer, Transform } from '@dcl/sdk/ecs'
import { Vector3 } from '@dcl/sdk/math'

export function main() {
  const cube = engine.addEntity()
  Transform.create(cube, { position: Vector3.create(8, 1, 8) })
  MeshRenderer.setBox(cube)
  console.log('{"ok":true,"entities":99}')
  setInterval(() => console.log('{"ok":true,"entities":98}'), 10)
}
"#,
    );
    assert!(stdout.contains("1 entity in 3 component types"), "{stdout}");
    assert!(stdout.contains("Commented out 1 source file"), "{stdout}");
    assert!(
        f.read("src/index.ts")
            .contains(dcl_scene_cli::code_to_composite::COMMENT_MARKER)
    );
    assert!(f.path().join("assets/scene/main.composite").is_file());
}

/// A scaffold whose `src/index.ts` is `index` and that holds `files`, converted,
/// with what the conversion wrote to stdout and stderr.
#[cfg(unix)]
fn convert_scene(tag: &str, files: &[(&str, &str)], index: &str) -> (Fixture, String, String) {
    let f = scene_with(tag, files, index);
    let out = run_with_path(&["code-to-composite", "--dir", &f.dir_arg(), "--yes"]);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(
        out.status.success(),
        "conversion failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    (f, stdout, stderr)
}

/// A scaffold whose `src/index.ts` is `index` and that holds `files`.
#[cfg(unix)]
fn scene_with(tag: &str, files: &[(&str, &str)], index: &str) -> Fixture {
    let f = Fixture::new(tag);
    let out = run(&["init", "--dir", &f.dir_arg()]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    for (rel, content) in files.iter().chain(&[("src/index.ts", index)]) {
        let path = f.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    f
}

/// A link at an asset's destination, or a linked folder above it, is not
/// written through: the conversion stops, names the destination and leaves
/// the folder outside the scene as it was.
#[cfg(unix)]
#[test]
fn code_to_composite_refuses_a_destination_behind_a_link() {
    let Some(()) = node_available() else { return };
    let outside = Fixture::new("code-to-composite-link-outside");
    let f = scene_with(
        "code-to-composite-link",
        &[("images/logo.png", "logo"), ("sounds/hit.mp3", "hit")],
        r#"import { AudioSource, engine, Material } from '@dcl/sdk/ecs'

export function main() {
  const texture = Material.Texture.Common({ src: 'images/logo.png' })
  Material.setBasicMaterial(engine.addEntity(), { texture })
  AudioSource.create(engine.addEntity(), { audioClipUrl: 'sounds/hit.mp3' })
}
"#,
    );
    let assets = f.path().join("assets/scene");
    std::fs::create_dir_all(assets.join("Images")).unwrap();
    let file_link = assets.join("Images/logo.png");
    std::os::unix::fs::symlink(outside.path().join("planted.png"), &file_link).unwrap();
    let folder_link = assets.join("Audio");
    for (link, refused) in [
        (&file_link, "assets/scene/Images/logo.png is a link"),
        (
            &folder_link,
            "assets/scene/Audio/hit.mp3 is in a folder that links out of the scene",
        ),
    ] {
        let out = run_with_path(&["code-to-composite", "--dir", &f.dir_arg(), "--yes"]);
        let stderr = stderr_of(&out);
        assert!(!out.status.success(), "{}", stdout_of(&out));
        assert!(
            stderr.contains(refused),
            "{refused} not reported:\n{stderr}"
        );
        assert_eq!(
            std::fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing is written through {}",
            link.display()
        );
        std::fs::remove_file(link).unwrap();
        std::os::unix::fs::symlink(outside.path(), &folder_link).unwrap();
    }
}

/// The composite and `main.crdt` follow the same rule: a link at either is
/// not written through, the conversion stops and names it, and neither is
/// written.
#[cfg(unix)]
#[test]
fn code_to_composite_refuses_to_write_its_results_through_a_link() {
    let Some(()) = node_available() else { return };
    let outside = Fixture::new("code-to-composite-result-link-outside");
    let f = scene_with(
        "code-to-composite-result-link",
        &[],
        "export function main() {}\n",
    );
    for result in ["assets/scene/main.composite", "main.crdt"] {
        let link = f.path().join(result);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.path().join("planted"), &link).unwrap();
        let out = run_with_path(&["code-to-composite", "--dir", &f.dir_arg(), "--yes"]);
        let stderr = stderr_of(&out);
        assert!(!out.status.success(), "{}", stdout_of(&out));
        assert!(stderr.contains(&format!("{result} is a link")), "{stderr}");
        assert_eq!(
            std::fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing is written through {result}"
        );
        std::fs::remove_file(&link).unwrap();
        assert!(!f.path().join("assets/scene/main.composite").exists());
    }
}

#[cfg(unix)]
fn composite_values(f: &Fixture, component: &str) -> Vec<String> {
    let composite = f.json("assets/scene/main.composite");
    let found = composite["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == component)
        .unwrap_or_else(|| panic!("no {component} in {composite}"));
    let mut values: Vec<(u64, String)> = found["data"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(entity, value)| (entity.parse().unwrap(), value.to_string()))
        .collect();
    values.sort();
    values.into_iter().map(|(_, value)| value).collect()
}

/// Two files with one name in different folders keep their own bytes and
/// their own reference, the first in path order under the plain name whatever
/// the code uses first; a third with the bytes of the first shares its copy,
/// and a file already at a destination is kept.
#[cfg(unix)]
#[test]
fn code_to_composite_gives_assets_with_the_same_name_different_destinations() {
    let Some(()) = node_available() else { return };
    let gltf = r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"tree.bin","byteLength":6}]}"#;
    let (f, stdout, _) = convert_scene(
        "code-to-composite-same-name",
        &[
            ("images/a/logo.png", "logo a"),
            ("images/b/logo.png", "logo b"),
            ("images/c/logo.png", "logo a"),
            ("models/x/tree.gltf", gltf),
            ("models/x/tree.bin", "tree x"),
            ("models/y/tree.gltf", gltf),
            ("models/y/tree.bin", "tree y"),
            ("sounds/hit.mp3", "newer hit"),
            ("assets/scene/Audio/hit.mp3", "old hit"),
        ],
        r#"import { AudioSource, engine, GltfContainer, Material } from '@dcl/sdk/ecs'

export function main() {
  for (const folder of ['b', 'a', 'c']) {
    const texture = Material.Texture.Common({ src: `images/${folder}/logo.png` })
    Material.setBasicMaterial(engine.addEntity(), { texture })
  }
  GltfContainer.create(engine.addEntity(), { src: 'models/x/tree.gltf' })
  GltfContainer.create(engine.addEntity(), { src: 'models/y/tree.gltf' })
  AudioSource.create(engine.addEntity(), { audioClipUrl: 'sounds/hit.mp3' })
}
"#,
    );
    for (rel, content) in [
        ("Images/logo.png", "logo a"),
        ("Images/b-logo.png", "logo b"),
        ("Models/tree/tree.gltf", gltf),
        ("Models/tree/tree.bin", "tree x"),
        ("Models/y-tree/y-tree.gltf", gltf),
        ("Models/y-tree/tree.bin", "tree y"),
        ("Audio/hit.mp3", "old hit"),
        ("Audio/sounds-hit.mp3", "newer hit"),
    ] {
        assert_eq!(f.read(&format!("assets/scene/{rel}")), content, "{rel}");
    }
    assert!(
        stdout.contains("images/b/logo.png -> assets/scene/Images/b-logo.png"),
        "{stdout}"
    );
    assert!(!f.path().join("assets/scene/Images/c-logo.png").exists());
    for (component, files) in [
        (
            "core::Material",
            ["Images/b-logo.png", "Images/logo.png", "Images/logo.png"].as_slice(),
        ),
        (
            "core::GltfContainer",
            &["Models/tree/tree.gltf", "Models/y-tree/y-tree.gltf"],
        ),
        ("core::AudioSource", &["Audio/sounds-hit.mp3"]),
    ] {
        let values = composite_values(&f, component);
        assert_eq!(values.len(), files.len(), "{component}: {values:?}");
        for (value, file) in values.iter().zip(files) {
            assert!(
                value.contains(&format!("\"assets/scene/{file}\"")),
                "{component}: {file} not in {value}"
            );
        }
    }
    let components = ["core::Material", "core::GltfContainer", "core::AudioSource"];
    let first = components.map(|c| composite_values(&f, c));
    let out = run_with_path(&["code-to-composite", "--dir", &f.dir_arg(), "--yes"]);
    let stdout = stdout_of(&out);
    assert!(out.status.success(), "{stdout}\n{}", stderr_of(&out));
    assert!(
        !stdout.contains(" -> "),
        "a second run moves nothing: {stdout}"
    );
    assert_eq!(components.map(|c| composite_values(&f, c)), first);
}

/// One file under several references is one source: it gets one destination.
#[cfg(unix)]
#[test]
fn code_to_composite_gives_a_file_referenced_twice_one_destination() {
    let Some(()) = node_available() else { return };
    let (f, _, _) = convert_scene(
        "code-to-composite-one-file",
        &[("images/logo.png", "logo")],
        r#"import { engine, Material } from '@dcl/sdk/ecs'

export function main() {
  for (const src of ['images/logo.png', 'images/logo.png', './images/logo.png']) {
    Material.setBasicMaterial(engine.addEntity(), { texture: Material.Texture.Common({ src }) })
  }
}
"#,
    );
    let copied: Vec<_> = std::fs::read_dir(f.path().join("assets/scene/Images"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(copied, ["logo.png"]);
    assert_eq!(f.read("assets/scene/Images/logo.png"), "logo");
    let values = composite_values(&f, "core::Material");
    assert_eq!(values.len(), 3, "{values:?}");
    for value in &values {
        assert!(
            value.contains("\"assets/scene/Images/logo.png\""),
            "{value}"
        );
    }
}

/// init, then pack, with no edits in between: the zip the builder takes
/// carries the manifest, the placeholder assets and the production bundle.
#[cfg(unix)]
#[test]
fn smart_wearable_scaffold_packs_end_to_end() {
    let Some(()) = node_available() else { return };
    let f = Fixture::new("vendored-pack");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir, "--project", "smart-wearable"]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let out = run_with_path(&["pack", "--dir", &dir]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "pack failed\nstdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    let zip = std::fs::File::open(f.path().join("smart-wearable.zip")).unwrap();
    let mut archive = zip::ZipArchive::new(zip).unwrap();
    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().unwrap().to_string())
        .collect();
    names.sort();
    for required in [
        "bin/index.js",
        "bin/scene.js",
        "bin/sdk-runtime.js",
        "model.glb",
        "scene.json",
        "thumbnail.png",
        "wearable.json",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "{required} missing from {names:?}"
        );
    }
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("src/") || n.starts_with("node_modules/")),
        "{names:?}"
    );
    let total: u64 = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().size())
        .sum();
    assert!(
        total < 2_097_152,
        "the scaffold must pack under the 2 MiB limit, got {total}"
    );
    assert!(
        !f.path().join("bin/index.js").exists(),
        "pack builds into the release tree, not the working tree"
    );
}

// Unix only: the borrowed node_modules is linked in by symlink.
#[cfg(unix)]
#[test]
#[ignore = "needs DCL_SCENE_CLI_TEST_SCENE: an installed scene checkout to borrow node_modules from; see docs/testing.md"]
fn init_scene_is_immediately_buildable_with_provisioned_node_modules() {
    let Some(src) = common::env::require_env("DCL_SCENE_CLI_TEST_SCENE") else {
        return;
    };
    let src = PathBuf::from(src);
    assert!(
        src.join("node_modules/@dcl/sdk").is_dir(),
        "DCL_SCENE_CLI_TEST_SCENE has no installed @dcl/sdk"
    );
    let f = Fixture::new("buildable");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    std::fs::remove_dir_all(f.path().join("node_modules")).unwrap();
    std::os::unix::fs::symlink(src.join("node_modules"), f.path().join("node_modules")).unwrap();
    let out = run(&["build", "--dir", &dir]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "build failed\nstdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    assert!(
        stdout.contains("SDK chunk installed") || stdout.contains("SDK chunk saved"),
        "{stdout}"
    );
    assert!(stdout.contains("Scene chunk saved"), "{stdout}");
    assert!(stdout.contains("Type check passed"), "{stdout}");
    for (rel, min) in [
        ("bin/sdk-runtime.js", 100_000usize),
        ("bin/index.js", 1_000),
        ("bin/scene.js", 100),
    ] {
        let bytes = std::fs::read(f.path().join(rel)).unwrap();
        assert!(
            bytes.len() > min,
            "{rel} suspiciously small: {} bytes",
            bytes.len()
        );
    }
}

#[test]
fn px_template_scaffold_is_a_portable_experience() {
    let f = Fixture::new("px");
    let dir = f.dir_arg();
    let out = run(&[
        "init",
        "--dir",
        &dir,
        "--project",
        "px-template",
        "--skip-install",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let scene = f.json("scene.json");
    assert_eq!(scene["isPortableExperience"], true);
    assert_eq!(scene["requiredPermissions"], serde_json::json!([]));
    assert_eq!(scene["scene"]["parcels"], serde_json::json!(["0,0"]));
    assert!(f.path().join("src/index.ts").is_file());
    assert!(
        !f.path().join("images").exists(),
        "a portable experience has no thumbnail folder"
    );
    assert!(
        !f.path().join("node_modules").exists(),
        "--skip-install leaves node_modules out"
    );
    assert!(
        stdout_of(&out).contains("Skipped node_modules"),
        "{}",
        stdout_of(&out)
    );
}

#[test]
fn scene_template_is_an_alias_for_scene() {
    let f = Fixture::new("scene-alias");
    let dir = f.dir_arg();
    let out = run(&[
        "init",
        "--dir",
        &dir,
        "--project",
        "scene-template",
        "--skip-install",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(
        f.json("scene.json")["isPortableExperience"],
        serde_json::Value::Null
    );
    assert!(f.path().join("images/scene-thumbnail.png").is_file());
}

#[test]
fn library_scaffold_exports_an_engine_taking_init() {
    let f = Fixture::new("library");
    let dir = f.dir_arg();
    let out = run(&[
        "init",
        "--dir",
        &dir,
        "--project",
        "library",
        "--skip-install",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let index = f.read("src/index.ts");
    assert!(
        index.contains("export function initLibrary(scene: IEngine)"),
        "{index}"
    );
    assert!(
        index.contains("export function main()"),
        "previewable as a scene: {index}"
    );
    let package = f.json("package.json");
    assert_eq!(package["types"], "dist/index.d.ts");
    assert!(
        package["scripts"]["build:lib"]
            .as_str()
            .unwrap()
            .contains("--single src/index.ts --emitDeclaration")
    );
    let tsconfig = f.json("tsconfig.json");
    assert_eq!(tsconfig["compilerOptions"]["declarationDir"], "dist");
    assert!(stdout_of(&out).contains("build:lib"), "{}", stdout_of(&out));
}

/// `build --single` on the library scaffold: the type check writes the
/// declarations and each entry bundles beside its source.
#[cfg(unix)]
#[test]
fn library_scaffold_bundles_single_files_with_declarations() {
    let Some(()) = node_available() else { return };
    let f = Fixture::new("library-single");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir, "--project", "library"]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let out = run_with_path(&[
        "build",
        "--built-in",
        "--dir",
        &dir,
        "--single",
        "src/**/*.ts",
        "--emitDeclaration",
    ]);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "build failed\nstdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    assert!(stdout.contains("declarations emitted"), "{stdout}");
    let bundle = f.read("src/index.js");
    assert!(bundle.contains("initLibrary"), "{bundle}");
    assert!(
        bundle.contains("require(\"@dcl/sdk/ecs\")") || bundle.contains("require('@dcl/sdk/ecs')"),
        "the SDK stays external: {}",
        &bundle[..bundle.len().min(600)]
    );
    let dts = f.read("dist/index.d.ts");
    assert!(dts.contains("export declare function initLibrary"), "{dts}");
    std::fs::remove_dir_all(f.path().join("dist")).unwrap();
    let out = run_with_path(&[
        "build",
        "--built-in",
        "--dir",
        &dir,
        "--single",
        "src/**/*.ts",
        "--emitDeclaration",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(
        f.path().join("dist/index.d.ts").is_file(),
        "unchanged sources and no dist: the declarations are written again"
    );
    assert!(
        !f.path().join("bin/index.js").exists(),
        "--single does not build the scene"
    );

    // A pattern that matches nothing is an error, not an empty success.
    let out = run_with_path(&["build", "--built-in", "--dir", &dir, "--single", "lib/*.ts"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("matches no .ts"),
        "{}",
        stderr_of(&out)
    );
}

fn template_zip(top: &str, files: &[(&str, &str)]) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "dcl-scene-cli-template-{top}-{}.zip",
        std::process::id()
    ));
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.add_directory(format!("{top}/"), opts).unwrap();
    for (rel, body) in files {
        use std::io::Write;
        zip.start_file(format!("{top}/{rel}"), opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// `--template` with a local zip: the one top-level folder's contents become
/// the project, and dependencies the vendored SDK lacks are named.
#[test]
fn template_zip_unpacks_its_top_level_folder() {
    let f = Fixture::new("template-zip");
    let dir = f.dir_arg();
    let zip = template_zip(
        "my-template-main",
        &[
            (
                "scene.json",
                r#"{"main":"bin/index.js","scene":{"parcels":["0,0"],"base":"0,0"}}"#,
            ),
            ("src/index.ts", "export function main() {}\n"),
            ("src/nested/util.ts", "export const x = 1\n"),
            (
                "package.json",
                r#"{"name":"t","dependencies":{"@dcl/sdk":"7","@dcl-sdk/utils":"1"}}"#,
            ),
        ],
    );
    let out = run(&["init", "--dir", &dir, "--template", zip.to_str().unwrap()]);
    let _ = std::fs::remove_file(&zip);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "stdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    assert_eq!(f.read("src/nested/util.ts"), "export const x = 1\n");
    assert!(f.path().join("scene.json").is_file());
    assert!(
        !f.path().join("my-template-main").exists(),
        "the top folder is stripped"
    );
    assert!(
        f.path()
            .join("node_modules/@dcl/sdk/package.json")
            .is_file()
    );
    assert!(stdout.contains("Unpacked 4 files"), "{stdout}");
    assert!(
        stdout.contains("@dcl-sdk/utils"),
        "missing dependency named: {stdout}"
    );
    assert!(
        !stdout.contains("@dcl/sdk,"),
        "vendored packages are not missing: {stdout}"
    );
}

/// A template's own `node_modules` stays in the zip: the packages that later
/// commands run under node come from the vendored archive alone.
#[test]
fn template_zip_node_modules_is_not_unpacked() {
    let f = Fixture::new("template-node-modules");
    let host = "node_modules/@dcl/inspector";
    let zip = template_zip(
        "planted-main",
        &[
            ("scene.json", "{}"),
            (
                &format!("{host}/package.json"),
                r#"{"name":"@dcl/inspector","main":"planted.js"}"#,
            ),
            (&format!("{host}/planted.js"), ""),
            ("node_modules/.dcl-one-archive", "planted"),
            ("Node_Modules/planted/index.js", ""),
        ],
    );
    let out = run(&[
        "init",
        "--dir",
        &f.dir_arg(),
        "--template",
        zip.to_str().unwrap(),
    ]);
    let _ = std::fs::remove_file(&zip);
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "stdout: {stdout}\nstderr: {}",
        stderr_of(&out)
    );
    assert!(stdout.contains("Unpacked 1 file into"), "{stdout}");
    let vendored = std::fs::File::open(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/vendor/node_modules.zip"
    ))
    .unwrap();
    let mut package = String::new();
    std::io::Read::read_to_string(
        &mut zip::ZipArchive::new(vendored)
            .unwrap()
            .by_name(&format!("{host}/package.json"))
            .unwrap(),
        &mut package,
    )
    .unwrap();
    assert!(!package.contains("planted.js"));
    assert_eq!(f.read(&format!("{host}/package.json")), package);
    assert!(!f.path().join(host).join("planted.js").exists());
    assert!(!f.path().join("Node_Modules").exists());
    assert_eq!(
        f.read("node_modules/.dcl-one-archive").len(),
        64,
        "the vendored archive is recorded"
    );
}

#[test]
fn template_zip_must_hold_exactly_one_top_level_folder() {
    let f = Fixture::new("template-flat");
    let dir = f.dir_arg();
    let path = std::env::temp_dir().join(format!("dcl-scene-cli-flat-{}.zip", std::process::id()));
    {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("scene.json", opts).unwrap();
        zip.write_all(b"{}").unwrap();
        zip.start_file("src/index.ts", opts).unwrap();
        zip.write_all(b"").unwrap();
        zip.finish().unwrap();
    }
    let out = run(&["init", "--dir", &dir, "--template", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("exactly one top-level folder"),
        "{}",
        stderr_of(&out)
    );

    let out = run(&[
        "init",
        "--dir",
        &dir,
        "--template",
        "/nonexistent/template.zip",
    ]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("neither a file nor an http(s) URL"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn github_repo_url_must_name_a_repository() {
    let f = Fixture::new("github-bad");
    let dir = f.dir_arg();
    for url in [
        "https://gitlab.com/a/b",
        "https://github.com/onlyowner",
        "https://github.com/a/b/blob/main/x",
    ] {
        let out = run(&["init", "--dir", &dir, "--github-repo", url]);
        assert!(!out.status.success(), "{url} accepted");
        assert!(
            stderr_of(&out).contains("not a GitHub repository URL"),
            "{url}: {}",
            stderr_of(&out)
        );
    }
}

/// The version of `package` in the vendored archive.
fn vendored_version(package: &str) -> String {
    let vendored = std::fs::File::open(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/vendor/node_modules.zip"
    ))
    .unwrap();
    let mut manifest = String::new();
    std::io::Read::read_to_string(
        &mut zip::ZipArchive::new(vendored)
            .unwrap()
            .by_name(&format!("node_modules/{package}/package.json"))
            .unwrap(),
        &mut manifest,
    )
    .unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    manifest["version"].as_str().unwrap().to_string()
}

/// `init --node-modules-only --missing-only` adds the packages a scene's
/// `node_modules` lacks and names, on stderr, each one it kept at another
/// version. `--dry-run` names the absent packages and writes nothing. Without
/// `--missing-only` the command replaces the kept package, because the
/// folder now has the vendored stand-in `@dcl/inspector`.
#[test]
fn missing_only_keeps_an_installed_sdk_and_names_its_version() {
    const OWN: &str = r#"{"name":"@dcl/sdk","version":"7.0.0"}"#;
    let f = Fixture::new("missing-only");
    let sdk = f.path().join("node_modules/@dcl/sdk");
    std::fs::create_dir_all(&sdk).unwrap();
    std::fs::write(sdk.join("package.json"), OWN).unwrap();
    std::fs::write(sdk.join("own.js"), "own").unwrap();
    let pin = vendored_version("@dcl/sdk");
    let warning = format!(
        "warning: node_modules keeps @dcl/sdk 7.0.0; the vendored SDK has @dcl/sdk {pin}\n"
    );
    let missing_only = [
        "init",
        "--dir",
        &f.dir_arg(),
        "--node-modules-only",
        "--missing-only",
    ];

    let out = run(&[&missing_only[..], &["--dry-run"]].concat());
    let stdout = stdout_of(&out);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), warning);
    assert!(
        stdout.starts_with("[1/1] node_modules does not have @dcl/ecs, @dcl/inspector, "),
        "{stdout}"
    );
    assert!(
        stdout.ends_with(", ws — nothing installed (--dry-run)\n"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_dir(f.path().join("node_modules/@dcl"))
            .unwrap()
            .count(),
        1,
        "a dry run installs nothing"
    );

    let out = run(&missing_only);
    let (stdout, stderr) = (stdout_of(&out), stderr_of(&out));
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        stderr, warning,
        "one line for the one package at another version"
    );
    assert!(
        stdout.starts_with("[1/1] Installed @dcl/ecs, @dcl/inspector, @dcl/js-runtime, "),
        "{stdout}"
    );
    assert!(
        stdout
            .ends_with("ws in node_modules from the vendored SDK; kept @dcl/sdk — no npm needed\n"),
        "{stdout}"
    );
    assert_eq!(f.read("node_modules/@dcl/sdk/package.json"), OWN);
    assert_eq!(f.read("node_modules/@dcl/sdk/own.js"), "own");
    assert!(
        !sdk.join("prebuilt").exists(),
        "nothing is written into a kept package"
    );
    assert_eq!(
        f.json("node_modules/@dcl/ecs/package.json")["version"],
        vendored_version("@dcl/ecs")
    );
    assert!(!f.path().join("node_modules/.dcl-one-archive").exists());

    let out = run(&missing_only);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), warning);
    assert_eq!(
        stdout_of(&out),
        "[1/1] node_modules already has each package of the vendored SDK — nothing installed\n"
    );
    assert_eq!(f.read("node_modules/@dcl/sdk/package.json"), OWN);

    let out = run(&["init", "--dir", &f.dir_arg(), "--node-modules-only"]);
    let stdout = stdout_of(&out);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).is_empty(), "{}", stderr_of(&out));
    assert!(
        stdout.starts_with("[1/1] Reinstalled @dcl/ecs, "),
        "{stdout}"
    );
    assert!(
        stdout.ends_with(&format!(
            " in node_modules from the vendored SDK (@dcl/sdk 7.0.0 replaced by @dcl/sdk {pin}) — no npm needed\n"
        )),
        "{stdout}"
    );
    assert_eq!(f.json("node_modules/@dcl/sdk/package.json")["version"], pin);
    assert!(!sdk.join("own.js").exists());
    assert_eq!(f.read("node_modules/.dcl-one-archive").len(), 64);

    let out = run(&missing_only);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).is_empty(), "every version is the archive's");
}

/// A link in the place of a package, that does not lead to a package: the
/// command names it, exits 1 and writes nothing, with `--dry-run` too. An
/// empty folder in the place of a package is replaced, with a warning.
#[cfg(unix)]
#[test]
fn missing_only_refuses_a_link_and_replaces_an_entry_that_is_not_a_package() {
    let f = Fixture::new("missing-only-link");
    let scope = f.path().join("node_modules/@dcl");
    std::fs::create_dir_all(scope.join("sdk")).unwrap();
    std::os::unix::fs::symlink(f.path().join("absent"), scope.join("ecs")).unwrap();
    let missing_only = [
        "init",
        "--dir",
        &f.dir_arg(),
        "--node-modules-only",
        "--missing-only",
    ];
    for dry_run in [&["--dry-run"][..], &[]] {
        let out = run(&[&missing_only[..], dry_run].concat());
        let stderr = stderr_of(&out);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(stdout_of(&out).is_empty(), "{}", stdout_of(&out));
        assert!(
            stderr.starts_with(
                "Error: node_modules/@dcl/ecs is a link, and this command does not replace a link\n"
            ),
            "{stderr}"
        );
        assert!(
            stderr.contains("→ try: remove node_modules/@dcl/ecs, then run the command again\n"),
            "{stderr}"
        );
        assert!(
            std::fs::symlink_metadata(scope.join("ecs"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(std::fs::read_dir(scope.join("sdk")).unwrap().count(), 0);
        assert!(!f.path().join("node_modules/typescript").exists());
    }

    std::fs::remove_file(scope.join("ecs")).unwrap();
    let out = run(&missing_only);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stderr,
        "warning: node_modules/@dcl/sdk is not a package (no package.json with a version); \
         the vendored package replaces it\n"
    );
    assert!(
        stdout_of(&out).starts_with("[1/1] Installed @dcl/ecs, "),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        f.json("node_modules/@dcl/sdk/package.json")["version"],
        vendored_version("@dcl/sdk")
    );
}

#[test]
fn missing_only_is_a_mode_of_node_modules_only() {
    let f = Fixture::new("missing-only-alone");
    let dir = f.dir_arg();
    let out = run(&["init", "--dir", &dir, "--missing-only"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("--node-modules-only"),
        "{}",
        stderr_of(&out)
    );
    let out = run(&[
        "init",
        "--dir",
        &dir,
        "--node-modules-only",
        "--missing-only",
        "--template",
        "t.zip",
    ]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("cannot be used with"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(
        std::fs::read_dir(f.path()).unwrap().count(),
        0,
        "nothing is written"
    );
}
