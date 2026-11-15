use crate::ux::{self, TrySteps, UserError};
use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const DRIVER_TEMPLATE: &str = include_str!("templates/data-layer-host.mjs");
/// How the host gets the secret of its launch, and the header an upgrade to
/// it carries the secret in (`admission` in the driver).
const SECRET_ENV: &str = "DCL_SCENE_CLI_DATA_LAYER_SECRET";
const SECRET_HEADER: &str = "x-dcl-data-layer-secret";
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const DUMP_TIMEOUT: Duration = Duration::from_secs(120);

/// A launch of the host: its loopback port (0 while it restarts) and the
/// secret made for that launch, which only the relay sends.
#[derive(Clone, Default)]
pub struct Host {
    pub port: u16,
    secret: String,
}

impl Host {
    /// The one connection the host admits: no `Origin`, the secret in a header.
    pub async fn connect(&self) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let mut request = format!("ws://127.0.0.1:{}/", self.port).into_client_request()?;
        request
            .headers_mut()
            .insert(SECRET_HEADER, self.secret.parse()?);
        Ok(tokio_tungstenite::connect_async(request).await?.0)
    }
}

#[derive(Clone)]
pub struct DataLayerState {
    pub host_rx: watch::Receiver<Host>,
    /// The editor's browser bundle, if installed. `None` is normal: the data
    /// layer still serves the protocol and 404s `/inspector/*`; treating it as
    /// fatal made the vendored host unreachable, since a real `@dcl/inspector`
    /// wins at `require()` time.
    pub public_dir: Option<PathBuf>,
}

/// The editor UI for this scene, if one is installed. `Ok(None)` is not an
/// error; a `DCL_ONE_INSPECTOR_DIR` that names a non-inspector build is.
pub fn locate_inspector_public(root: &Path) -> Result<Option<PathBuf>> {
    if let Ok(dir) = std::env::var("DCL_ONE_INSPECTOR_DIR") {
        let d = PathBuf::from(&dir);
        for candidate in [d.join("public"), d.clone()] {
            if candidate.join("index.html").is_file() {
                return Ok(Some(candidate));
            }
        }
        return Err(UserError::new(
            "DCL_ONE_INSPECTOR_DIR does not contain an inspector build",
            TrySteps::one(
                "point it at an @dcl/inspector package dir (one containing public/index.html)",
            )
            .and("or unset it to use the scene's own node_modules"),
        )
        .why(format!("no index.html under {dir} or {dir}/public"))
        .into());
    }
    let mut dir = Some(root);
    while let Some(d) = dir {
        let candidate = d.join("node_modules/@dcl/inspector/public");
        if candidate.join("index.html").is_file() {
            return Ok(Some(candidate));
        }
        dir = d.parent();
    }
    Ok(None)
}

pub fn inject_config(html: &str, config_json: &str) -> String {
    html.replace(
        "const config = '$CONFIG'",
        &format!("const config = '{config_json}'"),
    )
}

pub fn inspector_config_json(ws_url: &str) -> String {
    json!({ "dataLayerRpcWsUrl": ws_url }).to_string()
}

/// The vendored blob stores the big bundles `.gz` (11 MB instead of 73 MB); an
/// npm-installed `@dcl/inspector` has them plain, so plain wins.
pub enum Asset {
    Plain(PathBuf),
    Gzipped(PathBuf),
}

pub fn resolve_asset(public_dir: &Path, rel: &str) -> Option<Asset> {
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    let base = dunce::canonicalize(public_dir).unwrap_or_else(|_| public_dir.to_path_buf());
    let found = match dunce::canonicalize(public_dir.join(rel)) {
        Ok(p) => Asset::Plain(p),
        Err(_) => Asset::Gzipped(dunce::canonicalize(public_dir.join(format!("{rel}.gz"))).ok()?),
    };
    let path = match &found {
        Asset::Plain(p) | Asset::Gzipped(p) => p,
    };
    path.starts_with(&base).then_some(found)
}

/// Does the client take gzip? `identity` and `gzip;q=0` both mean no.
pub fn accepts_gzip(accept_encoding: Option<&str>) -> bool {
    let Some(value) = accept_encoding else {
        return false;
    };
    value.split(',').any(|part| {
        let mut bits = part.split(';');
        let coding = bits.next().unwrap_or("").trim();
        if !coding.eq_ignore_ascii_case("gzip") && coding != "*" {
            return false;
        }
        !bits.any(|p| {
            let p = p.trim();
            p.strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q <= 0.0)
        })
    })
}

pub fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

pub fn inspector_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript",
        "css" => "text/css",
        "map" | "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn write_driver(root: &Path) -> Result<PathBuf> {
    crate::scene::write_driver(root, "data-layer-host.mjs", DRIVER_TEMPLATE)
}

fn node_for_data_layer() -> Result<PathBuf> {
    crate::build::require_node(
        "the visual editor data layer",
        "to preview without the editor, drop --data-layer",
    )
}

struct Driver {
    child: tokio::process::Child,
    _stdin: Option<tokio::process::ChildStdin>,
    host: Host,
}

async fn launch(node: &Path, driver: &Path, root: &Path) -> Result<Driver> {
    let secret = hex::encode(rand::random::<[u8; 32]>());
    let mut child = tokio::process::Command::new(node)
        .arg(driver)
        .arg(root)
        .arg("serve")
        .env(SECRET_ENV, &secret)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            anyhow::Error::from(
                UserError::new(
                    "could not start the data-layer host (node)",
                    TrySteps::one("check node runs: node --version")
                        .and("to preview without the editor, drop --data-layer"),
                )
                .caused_by(e),
            )
        })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("driver stdout missing")?;
    let stderr = child.stderr.take().context("driver stderr missing")?;
    let mut err_lines = BufReader::new(stderr).lines();
    let (err_tx, mut err_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Ok(Some(line)) = err_lines.next_line().await {
            tracing::debug!(target: "data_layer", "{line}");
            let _ = err_tx.send(line);
        }
    });
    let mut out_lines = BufReader::new(stdout).lines();
    let ready = tokio::time::timeout(READY_TIMEOUT, async {
        while let Ok(Some(line)) = out_lines.next_line().await {
            tracing::debug!(target: "data_layer", "{line}");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("ready").and_then(|r| r.as_bool()) == Some(true)
                && let Some(port) = v.get("port").and_then(|p| p.as_u64())
            {
                return Some(port as u16);
            }
        }
        None
    })
    .await;
    tokio::spawn(async move {
        while let Ok(Some(line)) = out_lines.next_line().await {
            tracing::debug!(target: "data_layer", "{line}");
        }
    });
    match ready {
        Ok(Some(port)) => Ok(Driver {
            child,
            _stdin: stdin,
            host: Host { port, secret },
        }),
        _ => {
            let _ = child.kill().await;
            let mut tail = Vec::new();
            while let Ok(line) = err_rx.try_recv() {
                tail.push(line);
            }
            let why = if tail.is_empty() {
                "the driver exited before reporting its port".to_string()
            } else {
                tail.join("\n")
            };
            Err(UserError::new(
                "the data-layer host did not come up",
                TrySteps::one(
                    "run dcl-scene-cli init --node-modules-only \u{2014} the host, @dcl/rpc and ws are vendored",
                )
                .and("or npm install in the scene (@dcl/inspector, @dcl/rpc and ws must resolve)")
                .and("re-run with --verbose for the full driver log"),
            )
            .why(why)
            .into())
        }
    }
}

pub async fn spawn(root: &Path) -> Result<watch::Receiver<Host>> {
    let node = node_for_data_layer()?;
    let driver = write_driver(root)?;
    let mut current = launch(&node, &driver, root).await?;
    let (tx, rx) = watch::channel(current.host.clone());
    tracing::info!("data-layer host ready on 127.0.0.1:{}", current.host.port);
    let root = root.to_path_buf();
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            let status = current.child.wait().await;
            if tx.is_closed() {
                return;
            }
            let _ = tx.send(Host::default());
            ux::report_watch(
                &UserError::new(
                    "the visual-editor data layer stopped \u{2014} restarting it",
                    TrySteps::one(
                        "reload the editor page after it reconnects (unsaved edits may be lost)",
                    )
                    .and("re-run with --verbose to capture why it exited"),
                )
                .why(match status {
                    Ok(s) => format!("driver exited with {s}"),
                    Err(e) => format!("driver wait failed: {e}"),
                })
                .into(),
            );
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            loop {
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, Duration::from_secs(30));
                match launch(&node, &driver, &root).await {
                    Ok(next) => {
                        current = next;
                        if tx.send(current.host.clone()).is_err() {
                            return;
                        }
                        tracing::info!(
                            "data-layer host restarted on 127.0.0.1:{}",
                            current.host.port
                        );
                        break;
                    }
                    Err(e) => ux::report_watch(&e),
                }
            }
        }
    });
    Ok(rx)
}

pub async fn dump_crdt(root: &Path) -> Result<u64> {
    let node = node_for_data_layer()?;
    let driver = write_driver(root)?;
    let _progress = crate::ux::Slow::start("regenerating main.crdt");
    let out = tokio::time::timeout(
        DUMP_TIMEOUT,
        tokio::process::Command::new(&node)
            .arg(&driver)
            .arg(root)
            .arg("dump-crdt")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        anyhow::Error::from(UserError::new(
            "main.crdt regeneration timed out",
            TrySteps::one("re-run with --verbose and check the composite files"),
        ))
    })?
    .map_err(|e| {
        anyhow::Error::from(
            UserError::new(
                "could not run the main.crdt regeneration (node)",
                TrySteps::one("check node runs: node --version"),
            )
            .caused_by(e),
        )
    })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(UserError::new(
            "main.crdt regeneration failed \u{2014} keeping the existing main.crdt",
            TrySteps::one("check the composite files named below")
                .and("run npm install if @dcl/inspector cannot be resolved"),
        )
        .why(format!("{}{}", stdout.trim(), stderr.trim()))
        .into());
    }
    let summary = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .unwrap_or_default();
    if summary.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(UserError::new(
            "some composites could not be instanced \u{2014} main.crdt may be incomplete",
            TrySteps::one("fix the composite errors below, then rebuild"),
        )
        .why(stderr.trim().to_string())
        .into());
    }
    Ok(summary
        .get("composites")
        .and_then(|v| v.as_u64())
        .unwrap_or(0))
}

/// Which generator produced main.crdt; the node fallback is slower and needs
/// `@dcl/inspector` resolvable, so the build reports it.
pub enum CrdtRegen {
    /// Generated in-process, from this many composite files.
    Native(u64),
    /// The node summary carries no trustworthy composite count, so none is
    /// reported rather than a zero that reads as "nothing included".
    NodeDataLayer,
}

/// Re-run the node data-layer and shout if its bytes differ from the native
/// ones; a `DCL_ONE_CRDT_VERIFY=1` soak that reports nothing is the gate for
/// deleting the node fallback. The driver only writes `<root>/main.crdt`, so
/// that path is borrowed and restored afterwards.
async fn shadow_verify(root: &Path, native: &[u8]) {
    let main_crdt = root.join("main.crdt");
    let outcome = match dump_crdt(root).await {
        Ok(_) => tokio::fs::read(&main_crdt)
            .await
            .map_err(anyhow::Error::from),
        Err(e) => Err(e),
    };
    match outcome {
        Ok(reference) => {
            if let Some(difference) = crate::crdt_gen::describe_difference(native, &reference, root)
            {
                tracing::error!(
                    "DCL_ONE_CRDT_VERIFY: native main.crdt differs from the node data-layer's — {difference}"
                );
            } else {
                tracing::info!(
                    "DCL_ONE_CRDT_VERIFY: native main.crdt matches the node data-layer byte for byte"
                );
            }
        }
        Err(e) => tracing::error!("DCL_ONE_CRDT_VERIFY: the node data-layer could not run ({e})"),
    }
    if let Err(e) = tokio::fs::write(&main_crdt, native).await {
        tracing::error!("DCL_ONE_CRDT_VERIFY: could not restore the native main.crdt ({e})");
    }
}

pub async fn regenerate_main_crdt(
    root: &Path,
    ignore_composite: bool,
) -> Result<Option<CrdtRegen>> {
    if ignore_composite {
        return Ok(None);
    }
    let progress = crate::ux::Slow::start("regenerating main.crdt");
    let generated = crate::crdt_gen::generate(root);
    progress.finish();
    match generated {
        Ok(None) => Ok(None),
        Ok(Some(generated)) => {
            tokio::fs::write(root.join("main.crdt"), &generated.bytes)
                .await
                .context("writing main.crdt")?;
            tracing::info!(
                "main.crdt regenerated natively from {} composite(s)",
                generated.composites
            );
            if std::env::var("DCL_ONE_CRDT_VERIFY").as_deref() == Ok("1") {
                shadow_verify(root, &generated.bytes).await;
            }
            Ok(Some(CrdtRegen::Native(generated.composites)))
        }
        Err(dcl_scene_authoring::crdt_gen::GenError::Unsupported(why)) => {
            tracing::info!(
                "native main.crdt generation does not cover this scene ({why}); using the node data-layer"
            );
            let n = dump_crdt(root).await?;
            tracing::info!("main.crdt regenerated from {n} composite(s)");
            Ok(Some(CrdtRegen::NodeDataLayer))
        }
        Err(dcl_scene_authoring::crdt_gen::GenError::Invalid(why)) => Err(UserError::new(
            "main.crdt regeneration failed \u{2014} keeping the existing main.crdt",
            TrySteps::one("check the composite files named below"),
        )
        .why(why)
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_gen::TestDir;

    #[test]
    fn config_injection_rewrites_only_the_assignment() {
        let html = "<script>const config = '$CONFIG'\nif (config !== '$CONFIG') { globalThis.InspectorConfig = JSON.parse(config) }</script>";
        let injected = inject_config(html, &inspector_config_json("ws://x/data-layer"));
        assert!(
            injected.contains("const config = '{\"dataLayerRpcWsUrl\":\"ws://x/data-layer\"}'")
        );
        assert!(
            injected.contains("if (config !== '$CONFIG')"),
            "the sentinel comparison must stay untouched or the config never loads: {injected}"
        );
    }

    #[test]
    fn locate_walks_up_to_find_the_inspector_public_dir() {
        let t = TestDir::new("datalayer-locate");
        t.write(
            "node_modules/@dcl/inspector/public/index.html",
            "<html>$CONFIG</html>",
        );
        t.write("ws/member/scene.json", "{}");
        let found = locate_inspector_public(&t.0.join("ws/member")).unwrap();
        assert_eq!(found, Some(t.0.join("node_modules/@dcl/inspector/public")));
        assert_eq!(
            locate_inspector_public(Path::new("/nonexistent-dcl1")).unwrap(),
            None
        );
    }

    #[test]
    fn driver_template_is_embedded_and_mode_aware() {
        assert!(DRIVER_TEMPLATE.contains("createDataLayerHost"));
        assert!(DRIVER_TEMPLATE.contains("dump-crdt"));
        assert!(DRIVER_TEMPLATE.contains("DataServiceDefinition"));
    }

    #[test]
    fn configured_inspector_overrides_the_scene_shim_and_reports_broken_configuration() {
        let t = TestDir::new("datalayer-override");
        t.write("scene/package.json", "{}");
        t.write(
            "scene/node_modules/@dcl/inspector/package.json",
            r#"{"main":"index.js"}"#,
        );
        t.write(
            "scene/node_modules/@dcl/inspector/index.js",
            "exports.dumpEngineToCrdtCommands=()=>Buffer.from('scene shim')",
        );
        t.write(
            "scene/node_modules/@dcl/ecs/dist-cjs.js",
            "exports.Engine=()=>({});exports.Composite={};exports.EntityMappingMode={}",
        );
        t.write("host/package.json", r#"{"main":"index.js"}"#);
        t.write(
            "host/index.js",
            "exports.dumpEngineToCrdtCommands=()=>Buffer.from('configured inspector')",
        );
        let root = t.0.join("scene");
        let driver = write_driver(&root).unwrap();
        let run = |configured: bool| {
            let mut command = std::process::Command::new(node_for_data_layer().unwrap());
            command
                .arg(&driver)
                .arg(&root)
                .arg("dump-crdt")
                .env_remove("DCL_ONE_INSPECTOR_DIR");
            if configured {
                command.env("DCL_ONE_INSPECTOR_DIR", t.0.join("host"));
            }
            command.output().unwrap()
        };
        for (configured, expected) in [(false, "scene shim"), (true, "configured inspector")] {
            let output = run(configured);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(root.join("main.crdt")).unwrap(),
                expected
            );
        }
        t.write(
            "host/index.js",
            "throw new Error('configured inspector broken')",
        );
        let output = run(true);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("configured inspector broken"));
    }

    /// The stand-in `ws` that the tests below serve with. Like the real one,
    /// it asks `verifyClient` about each upgrade; `ANSWER` is replaced.
    const WS_STAND_IN: &str = "const http=require('node:http');\
        exports.WebSocketServer=class extends require('node:events'){\
        constructor(o){super();\
        this.s=http.createServer().on('upgrade',(q,s)=>ANSWER)\
        .listen(o.port,o.host,()=>this.emit('listening'))}\
        address(){return this.s.address()}}";

    /// A scene whose `@dcl/inspector` is `inspector`, with stand-ins for the
    /// `@dcl/rpc` and `ws` that the host serves it with. With `verifies`
    /// false the `ws` admits every upgrade, as one without `verifyClient`.
    fn scene_with_host_packages(t: &TestDir, inspector: &str, verifies: bool) -> PathBuf {
        let asks =
            "o.verifyClient({req:q},(ok,code)=>s.end(`HTTP/1.1 ${ok?101:code} x\\r\\n\\r\\n`))";
        let admits = "s.end('HTTP/1.1 101 x\\r\\n\\r\\n')";
        let ws = WS_STAND_IN.replace("ANSWER", if verifies { asks } else { admits });
        t.write("scene/package.json", "{}");
        for (module, source) in [
            ("@dcl/inspector/index.js", inspector),
            (
                "@dcl/rpc/index.js",
                "exports.createRpcServer=()=>({setHandler(){},attachTransport(){}})",
            ),
            ("@dcl/rpc/dist/codegen.js", "exports.registerService=()=>{}"),
            (
                "@dcl/rpc/dist/transports/WebSocket.js",
                "exports.WebSocketTransport=(ws)=>ws",
            ),
            ("ws/index.js", ws.as_str()),
        ] {
            t.write(&format!("scene/node_modules/{module}"), source);
        }
        t.0.join("scene")
    }

    /// A stand-in `@dcl/inspector` that drives the fs interface the host hands
    /// it: every method on every path of `OUTSIDE` must throw, and work inside
    /// the scene must not.
    #[cfg(unix)]
    const FS_PROBE_INSPECTOR: &str = r#"
const nodeFs = require('fs')
exports.DataServiceDefinition = {}
exports.createDataLayerHost = async (fs) => {
  const passed = []
  for (const p of JSON.parse(process.env.OUTSIDE)) {
    const calls = {
      readFile: () => fs.readFile(p),
      writeFile: () => fs.writeFile(p, Buffer.from('written')),
      rm: () => fs.rm(p),
      rmdir: () => fs.rmdir(p),
      readdir: () => fs.readdir(p),
      stat: () => fs.stat(p)
    }
    for (const [name, call] of Object.entries(calls)) {
      await call().then(() => passed.push(`${name} ${p}`), () => {})
    }
    if (await fs.existFile(p)) passed.push(`existFile ${p}`)
  }
  await fs.rmdir('../link-in').then(() => passed.push('rmdir ../link-in'), () => {})
  let inside = 'ok'
  try {
    await fs.writeFile('assets/new/a.txt', Buffer.from('in'))
    const absolute = fs.join(fs.cwd(), 'assets/new/a.txt')
    for (const p of ['assets/new/a.txt', './assets/x/../new/a.txt', absolute, 'inner-link/new/a.txt']) {
      if (String(await fs.readFile(p)) !== 'in') throw new Error(`read ${p}`)
    }
    if (!(await fs.existFile('assets/new/a.txt'))) throw new Error('existFile')
    if ((await fs.stat('assets/new/a.txt')).size !== 2) throw new Error('stat')
    for (const p of ['', '.', './', fs.cwd()]) {
      if (!(await fs.readdir(p)).some((e) => e.name === 'assets' && e.isDirectory)) throw new Error(`readdir ${p}`)
    }
    if ((await fs.readdir('assets/new'))[0].name !== 'a.txt') throw new Error('readdir')
    await fs.rm('assets/new/a.txt')
    await fs.rmdir('assets/new')
    if (nodeFs.existsSync('assets/new')) throw new Error('rmdir')
    await fs.rmdir('inner-link')
    if (nodeFs.lstatSync('inner-link', { throwIfNoEntry: false })) throw new Error('rmdir of a link')
  } catch (e) {
    inside = String(e)
  }
  nodeFs.writeFileSync('../report.json', JSON.stringify({ passed, inside }))
  return {}
}
"#;

    #[cfg(unix)]
    #[test]
    fn the_fs_interface_refuses_every_path_outside_the_scene() {
        let t = TestDir::new("datalayer-confine");
        let root = scene_with_host_packages(&t, FS_PROBE_INSPECTOR, true);
        t.write("scene/assets/keep.txt", "keep");
        t.write("outside/secret.txt", "secret");
        let outside = t.0.join("outside");
        let link = |target: &Path, name: &str| {
            std::os::unix::fs::symlink(target, root.join(name)).unwrap();
        };
        link(&outside, "link-dir");
        link(&outside.join("secret.txt"), "link-file");
        link(&outside.join("made.txt"), "link-nowhere");
        link(&root.join("assets"), "inner-link");
        link(&root.join("assets"), "../link-in");
        let secret = outside.join("secret.txt").display().to_string();
        let escapes = [
            "..",
            "../outside",
            "../outside/secret.txt",
            "assets/../../outside/secret.txt",
            "../outside/made.txt",
            outside.to_str().unwrap(),
            secret.as_str(),
            "link-dir",
            "link-dir/secret.txt",
            "link-dir/made.txt",
            "link-file",
            "link-nowhere",
        ];
        let output = std::process::Command::new(node_for_data_layer().unwrap())
            .arg(write_driver(&root).unwrap())
            .arg(&root)
            .arg("serve")
            .env_remove("DCL_ONE_INSPECTOR_DIR")
            .env(SECRET_ENV, "secret")
            .env("OUTSIDE", json!(escapes).to_string())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ready: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            ready["ready"], true,
            "the host serves and ends with its stdin"
        );
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(t.0.join("report.json")).unwrap()).unwrap();
        assert_eq!(report, json!({ "passed": [], "inside": "ok" }));
        let left: Vec<_> = std::fs::read_dir(&outside)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["secret.txt"], "the folder outside the scene");
        assert!(t.0.join("link-in").is_symlink(), "a link outside the scene");
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
            "secret"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("assets/keep.txt")).unwrap(),
            "keep"
        );
    }

    /// The packages of the archive, whose `ws` makes the server. The CLI's
    /// relay (`start::editor::proxy_data_layer`) connects as here: the secret
    /// of the launch and no Origin. Anything a browser sends carries an
    /// Origin, and a program of another user does not have the secret.
    #[tokio::test]
    async fn the_host_admits_only_the_secret_of_its_launch_without_an_origin() {
        use tokio_tungstenite::tungstenite;
        let t = TestDir::new("datalayer-admission");
        t.write("scene/package.json", "{}");
        t.write(
            "scene/scene.json",
            r#"{"main":"bin/index.js","scene":{"parcels":["0,0"],"base":"0,0"}}"#,
        );
        let root = t.0.join("scene");
        crate::init::restore_missing_node_modules(&root).unwrap();
        let driver = write_driver(&root).unwrap();
        let launched = launch(&node_for_data_layer().unwrap(), &driver, &root)
            .await
            .unwrap();
        let host = &launched.host;
        assert_eq!(host.secret.len(), 64, "32 random bytes, in hex");
        host.connect().await.expect("the CLI's relay connects");
        let wrong = "0".repeat(64);
        let own_origin = format!("http://127.0.0.1:{}", host.port);
        let mut refused = vec![
            vec![],
            vec![(SECRET_HEADER, wrong.as_str())],
            vec![(SECRET_HEADER, &host.secret[..8])],
        ];
        for origin in [
            "https://example.com",
            "http://localhost:8000",
            own_origin.as_str(),
            "null",
        ] {
            refused.push(vec![
                (SECRET_HEADER, host.secret.as_str()),
                ("origin", origin),
            ]);
        }
        for headers in refused {
            let mut request = format!("ws://127.0.0.1:{}/", host.port)
                .into_client_request()
                .unwrap();
            for (name, value) in &headers {
                request.headers_mut().insert(*name, value.parse().unwrap());
            }
            match tokio_tungstenite::connect_async(request).await {
                Err(tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status().as_u16(), 403, "{headers:?}")
                }
                other => panic!("{headers:?} was not refused: {other:?}"),
            }
        }
    }

    /// The host does not start without the secret, nor on a `ws` that does
    /// not ask `verifyClient`.
    #[test]
    fn the_host_stops_without_its_secret_or_on_a_server_that_admits_every_upgrade() {
        let t = TestDir::new("datalayer-no-admission");
        let inspector =
            "exports.createDataLayerHost=async()=>({});exports.DataServiceDefinition={}";
        let root = scene_with_host_packages(&t, inspector, false);
        let driver = write_driver(&root).unwrap();
        for (secret, why) in [
            (None, "DCL_SCENE_CLI_DATA_LAYER_SECRET is not set"),
            (Some("secret"), "does not refuse an upgrade without"),
        ] {
            let mut host = std::process::Command::new(node_for_data_layer().unwrap());
            host.args([&driver, &root])
                .arg("serve")
                .env_remove("DCL_ONE_INSPECTOR_DIR")
                .env_remove(SECRET_ENV)
                .envs(secret.map(|secret| (SECRET_ENV, secret)))
                .stdin(Stdio::null());
            let output = host.output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{why}: the host started");
            assert!(stderr.contains(why), "{why}: {stderr}");
            assert!(output.stdout.is_empty(), "{why}: a port was reported");
        }
    }

    #[test]
    fn resolve_prefers_a_plain_asset_and_falls_back_to_the_gzipped_one() {
        let t = TestDir::new("datalayer-resolve");
        t.write("public/index.html", "<html></html>");
        t.write("public/bundle.js.gz", "not really gzip");
        t.write("public/plain.js", "console.log(1)");
        let dir = t.0.join("public");
        assert!(matches!(
            resolve_asset(&dir, "plain.js"),
            Some(Asset::Plain(_))
        ));
        assert!(matches!(
            resolve_asset(&dir, "bundle.js"),
            Some(Asset::Gzipped(_))
        ));
        t.write("public/bundle.js", "plain wins");
        assert!(matches!(
            resolve_asset(&dir, "bundle.js"),
            Some(Asset::Plain(_))
        ));
        assert!(resolve_asset(&dir, "missing.js").is_none());
    }

    #[test]
    fn resolve_refuses_paths_that_escape_the_public_dir() {
        let t = TestDir::new("datalayer-escape");
        t.write("public/index.html", "<html></html>");
        t.write("secret.txt", "nope");
        let dir = t.0.join("public");
        assert!(resolve_asset(&dir, "../secret.txt").is_none());
        assert!(resolve_asset(&dir, "a/../../secret.txt").is_none());
        assert!(resolve_asset(&dir, "../secret.txt.gz").is_none());
    }

    #[test]
    fn accepts_gzip_reads_the_q_value() {
        assert!(accepts_gzip(Some("gzip, deflate, br")));
        assert!(accepts_gzip(Some("br;q=1.0, gzip;q=0.8")));
        assert!(accepts_gzip(Some("GZIP")));
        assert!(accepts_gzip(Some("*")));
        assert!(!accepts_gzip(None));
        assert!(!accepts_gzip(Some("identity")));
        assert!(!accepts_gzip(Some("deflate, gzip;q=0")));
        assert!(!accepts_gzip(Some("")));
    }

    #[test]
    fn gunzip_round_trips_the_blobs_own_encoding() {
        use std::io::Write;
        let payload = b"globalThis.InspectorConfig = {}\n".repeat(64);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&payload).unwrap();
        let gz = enc.finish().unwrap();
        assert!(gz.len() < payload.len(), "the fixture must actually shrink");
        assert_eq!(gunzip(&gz).unwrap(), payload);
        assert!(gunzip(b"not gzip at all").is_err());
    }

    #[test]
    fn mime_table_covers_the_inspector_bundle() {
        assert_eq!(
            inspector_mime(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            inspector_mime(Path::new("bundle.js")),
            "application/javascript"
        );
        assert_eq!(inspector_mime(Path::new("bundle.css")), "text/css");
        assert_eq!(
            inspector_mime(Path::new("bundle.js.map")),
            "application/json"
        );
        assert_eq!(
            inspector_mime(Path::new("x.bin")),
            "application/octet-stream"
        );
    }
}
