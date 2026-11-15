//! `deploy --programmatic` end to end: the binary's two stdout lines and the
//! `/api/deploy` resource a program drives between them, against a target
//! that never answers so nothing can be published.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common {
    pub mod child;
    pub mod wait;
}
use common::child::{Tail, terminate, wait_bounded};
use common::wait::HANG_GUARD;

const BIN: &str = env!("CARGO_BIN_EXE_dcl-scene-cli");
const TOKEN: &str = "0123456789abcdef";
const SCENE: &str = r#"{"main":"bin/index.js","runtimeVersion":"7","scene":{"parcels":["0,0"],"base":"0,0"},"display":{"title":"Gather","description":"a square"}}"#;
/// Nothing listens here, so a signed upload can only fail and a wait for a
/// signature can only be cancelled.
const DEAD_TARGET: &str = "http://127.0.0.1:9";

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "dcl-scene-cli-deploy-api-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("scene.json"), SCENE).unwrap();
        std::fs::write(dir.join("bin/index.js"), "module.exports = {}\n").unwrap();
        Fixture(dir)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The child with both streams tailed; the linker wait is only a hang guard.
struct Run {
    child: Child,
    out: Tail,
    err: Tail,
}

fn spawn(dir: &Fixture, extra: &[&str], envs: &[(&str, &str)]) -> Run {
    spawn_as(&["--programmatic"], dir, extra, envs)
}

/// `mode` is the output contract under test: `--programmatic`'s two lines,
/// or `--json`'s single document.
fn spawn_as(mode: &[&str], dir: &Fixture, extra: &[&str], envs: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(BIN);
    cmd.args(["deploy", "--dir"])
        .arg(&dir.0)
        .args(["--skip-build", "--target-content", DEAD_TARGET])
        .args(mode)
        .args(extra);
    for k in [
        "DCL_PRIVATE_KEY",
        "RUST_LOG",
        "DCL_SCENE_CLI_TARGET_SERVER",
        "DCL_SCENE_CLI_LINKER_HOST",
        "DCL_SCENE_CLI_CATALYST_ROTATION",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("NO_COLOR", "1")
        .env("DCL_SCENE_CLI_LINKER_TIMEOUT_SECS", "25")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let out = Tail::spawn(child.stdout.take().unwrap());
    let err = Tail::spawn(child.stderr.take().unwrap());
    Run { child, out, err }
}

impl Drop for Run {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            terminate(&mut self.child);
        }
    }
}

impl Run {
    /// Stops the child and fails with everything it said.
    fn fail(&mut self, why: impl std::fmt::Display) -> ! {
        let status = wait_bounded(&mut self.child, Duration::from_millis(50))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "still running".to_string());
        if status == "still running" {
            terminate(&mut self.child);
        }
        panic!(
            "{why}\nchild: {status}\nstdout: {}\nstderr: {}",
            self.out.text(),
            self.err.text()
        );
    }

    /// Sends a request to the child's server; a refused connection is the
    /// child's failure, reported with its streams.
    async fn send(&mut self, req: reqwest::RequestBuilder) -> reqwest::Response {
        match req.send().await {
            Ok(resp) => resp,
            Err(e) => self.fail(format!("request failed: {e}")),
        }
    }

    /// Blocks until stdout carries a whole line holding `needle`, and parses it.
    fn line_with(&self, needle: &str) -> Value {
        assert!(
            self.out.wait_for(needle, HANG_GUARD),
            "stdout never carried {needle}: {}\nstderr: {}",
            self.out.text(),
            self.err.text()
        );
        let text = self.out.text();
        let line = text
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle} in {text}"));
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    /// The exit and both streams; the child is stopped when it outlives the guard.
    fn finish(self, guard: Duration) -> (bool, String, String) {
        let (code, out, err) = self.end(guard);
        (code == Some(0), out, err)
    }

    /// [`Run::finish`] with the exit code, `None` for an exit by a signal.
    fn end(mut self, guard: Duration) -> (Option<i32>, String, String) {
        let status = wait_bounded(&mut self.child, guard).unwrap_or_else(|| {
            let (out, err) = (self.out.text(), self.err.text());
            terminate(&mut self.child);
            panic!("the deploy outlived {guard:?}\nstdout: {out}\nstderr: {err}");
        });
        (
            status.code(),
            String::from_utf8_lossy(&self.out.finish()).into_owned(),
            String::from_utf8_lossy(&self.err.finish()).into_owned(),
        )
    }
}

/// Each line of stdout, which must be one JSON object.
fn json_lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HANG_GUARD)
        .build()
        .unwrap()
}

impl Run {
    /// Sends a request and reads its JSON answer, status first.
    async fn json(&mut self, req: reqwest::RequestBuilder) -> (u16, Value) {
        let resp = self.send(req).await;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }
}

/// Every chunk of a server-sent stream until it ends, or `guard` passes.
async fn drain_sse(resp: reqwest::Response, guard: Duration) -> (String, bool) {
    let mut resp = resp;
    let mut text = String::new();
    let ended = tokio::time::timeout(guard, async {
        while let Some(chunk) = resp.chunk().await.unwrap() {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await
    .is_ok();
    (text, ended)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_program_reads_the_ready_line_drives_the_resource_and_cancels() {
    let f = Fixture::new("drive");
    let mut run = spawn(&f, &["--deploy-token", TOKEN], &[]);
    let ready = run.line_with(r#""event":"ready""#);
    let api = ready["api"].as_str().unwrap().to_string();
    let entity_id = ready["entityId"].as_str().unwrap().to_string();
    assert_eq!(ready["deployToken"], TOKEN);
    assert!(entity_id.starts_with("bafkrei"), "{ready}");
    assert!(
        ready["url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/deploy#token={TOKEN}")),
        "{ready}"
    );
    assert!(api.ends_with("/api/deploy"), "{ready}");
    assert!(ready["deleteScenesFromWorldPayload"].is_null());
    let base = api.trim_end_matches("/api/deploy").to_string();
    let http = client();

    let (status, state) = run.json(http.get(&api)).await;
    assert_eq!(status, 200);
    assert_eq!(state["phase"], "awaiting-signature");
    assert_eq!(state["cancelRequested"], false);

    let (status, info) = run.json(http.get(format!("{base}/api/info"))).await;
    assert_eq!(status, 200);
    assert_eq!(
        info["rootCID"], entity_id,
        "the id to sign is the one minted"
    );
    assert_eq!(info["isWorld"], false);
    assert_eq!(info["title"], "Gather");
    assert_eq!(info["description"], "a square");
    assert_eq!(info["targetContent"], DEAD_TARGET);
    assert_eq!(info["parcels"], serde_json::json!(["0,0"]));

    let (status, files) = run.json(http.get(format!("{base}/api/files"))).await;
    assert_eq!(status, 200);
    let names: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"scene.json") && names.contains(&"bin/index.js"),
        "{files}"
    );

    let (status, body) = run
        .json(
            http.post(&api)
                .header("content-type", "application/json")
                .body("not json"),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["message"], "Invalid payload: the body must be JSON");
    let (status, body) = run
        .json(
            http.post(&api)
                .json(&serde_json::json!({ "address": "0x1" })),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["message"], "Invalid payload: address");

    let (status, body) = run.json(http.delete(&api)).await;
    assert_eq!(status, 403);
    assert_eq!(body["message"], "Invalid or missing deploy token");
    let (status, _) = run
        .json(
            http.delete(&api)
                .header("x-deploy-token", "0123456789abcdeF"),
        )
        .await;
    assert_eq!(status, 403);
    let (status, state) = run.json(http.get(&api)).await;
    assert_eq!(
        (status, state["phase"].as_str()),
        (200, Some("awaiting-signature"))
    );

    let stream = run
        .send(http.get(&api).header("accept", "text/event-stream"))
        .await;
    assert_eq!(stream.status(), 200);
    assert!(
        stream.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let (status, state) = run
        .json(http.delete(&api).header("x-deploy-token", TOKEN))
        .await;
    assert_eq!(status, 202, "{state}");
    assert_eq!(state["phase"], "cancelled");
    assert_eq!(state["cancelRequested"], true);
    assert!(
        state["error"].is_null(),
        "a cancel before the signature is no error"
    );
    let (events, ended) = drain_sse(stream, HANG_GUARD).await;
    assert!(ended, "the stream ends after the final state: {events}");
    let states: Vec<Value> = events
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert_eq!(
        states.first().unwrap()["phase"],
        "awaiting-signature",
        "{events}"
    );
    assert_eq!(states.last().unwrap()["phase"], "cancelled", "{events}");
    assert!(events.contains("event: state"), "{events}");

    let (ok, stdout, stderr) = run.finish(Duration::from_secs(15));
    assert!(!ok, "a cancelled run fails: {stdout}");
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["event"], "state");
    assert_eq!(last["phase"], "cancelled");
    assert_eq!(
        stdout.lines().count(),
        2,
        "ready and the final state only: {stdout}"
    );
    assert!(stderr.contains("Deployment cancelled"), "{stderr}");
    assert!(
        http.get(&api).send().await.is_err(),
        "the server is gone with the run"
    );
}

/// `--json` without `--programmatic`: stdout is the result document alone,
/// so the signing prompt goes to stderr, and the run ends with the signing
/// outcome instead of keeping the preview up for a Ctrl+C nobody sends.
#[tokio::test(flavor = "multi_thread")]
async fn a_json_run_prompts_on_stderr_and_ends_with_the_outcome() {
    let f = Fixture::new("json-prompt");
    let mut run = spawn_as(&["--json"], &f, &["--deploy-token", TOKEN], &[]);
    assert!(
        run.err.wait_for("/deploy#token=", HANG_GUARD),
        "stderr never carried the signing url: {}\nstdout: {}",
        run.err.text(),
        run.out.text()
    );
    let err = run.err.text();
    let url = err
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("http://") && l.contains("/deploy#token="))
        .unwrap_or_else(|| panic!("no signing url line in {err}"))
        .to_string();
    assert!(
        run.out.text().is_empty(),
        "stdout stays clear for the document: {}",
        run.out.text()
    );
    let api = format!("{}/api/deploy", url.split("/deploy#").next().unwrap());
    let http = client();
    let (status, state) = run
        .json(http.delete(&api).header("x-deploy-token", TOKEN))
        .await;
    assert_eq!(status, 202, "{state}");
    assert_eq!(state["phase"], "cancelled");
    let (ok, stdout, stderr) = run.finish(Duration::from_secs(15));
    assert!(!ok, "a cancelled run fails: {stdout}");
    assert!(
        stdout.trim().is_empty(),
        "no document for a deployment that did not happen: {stdout}"
    );
    assert!(stderr.contains("Sign the deployment"), "{stderr}");
    assert!(stderr.contains("cancelled"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_token_is_refused_before_any_server_starts() {
    let f = Fixture::new("short");
    let run = spawn(&f, &["--deploy-token", "short"], &[]);
    let (ok, stdout, stderr) = run.finish(Duration::from_secs(15));
    assert!(!ok);
    assert!(stderr.contains("at least 16 characters"), "{stderr}");
    let lines = json_lines(&stdout);
    assert_eq!(lines.len(), 2, "the final state and the error: {stdout}");
    assert_eq!(lines[0]["event"], "state", "{stdout}");
    assert_eq!(lines[0]["phase"], "failed", "{stdout}");
    assert_eq!(lines[1]["event"], "error", "{stdout}");
    assert_eq!(lines[1]["code"], Value::Null, "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_prints_the_final_state_and_no_ready_line() {
    let f = Fixture::new("headless");
    let key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let run = spawn(&f, &[], &[("DCL_PRIVATE_KEY", key)]);
    let (ok, stdout, stderr) = run.finish(HANG_GUARD);
    assert!(!ok);
    let lines = json_lines(&stdout);
    assert_eq!(lines.len(), 2, "the final state and the error: {stdout}");
    assert_eq!(lines[1]["event"], "error", "{stdout}");
    assert_eq!(lines[1]["code"], Value::Null, "{stdout}");
    assert!(
        lines[1]["message"]
            .as_str()
            .unwrap()
            .starts_with("could not reach the content server"),
        "{stdout}"
    );
    let last = &lines[0];
    assert_eq!(last["event"], "state");
    assert_eq!(last["phase"], "failed");
    assert_eq!(last["error"]["code"], "DEPLOY_FAILED");
    assert!(
        last["error"]["message"]
            .as_str()
            .unwrap()
            .contains("could not reach the content server"),
        "{last}"
    );
    assert!(
        stderr.contains("could not reach the content server"),
        "{stderr}"
    );
}

/// The signing server listens on IPv4 loopback. `--host`, or
/// `DCL_SCENE_CLI_LINKER_HOST`, selects another address, and the URLs name an
/// address that is not loopback. 127.0.0.2 is a second loopback address on
/// Linux: a listener on 127.0.0.1 does not answer there.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn the_signing_server_listens_on_loopback_unless_another_address_is_given() {
    let f = Fixture::new("host");
    let other = |port: u16| std::net::TcpStream::connect(("127.0.0.2", port)).is_ok();
    let ready = |extra: &[&str], envs: &[(&str, &str)]| {
        let run = spawn(&f, &[&["--port", "0"], extra].concat(), envs);
        let ready = run.line_with(r#""event":"ready""#);
        let port = ready["port"].as_u64().unwrap() as u16;
        let api = ready["api"].as_str().unwrap().to_string();
        let url = ready["url"].as_str().unwrap().to_string();
        (run, port, api, url)
    };

    let (_run, port, api, url) = ready(&[], &[]);
    assert_eq!(api, format!("http://localhost:{port}/api/deploy"));
    assert!(
        url.starts_with(&format!("http://localhost:{port}/deploy#token=")),
        "{url}"
    );
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
    assert!(!other(port), "the default address is 127.0.0.1 alone");

    let (_run, port, api, _) = ready(&["--host", "0.0.0.0"], &[]);
    assert_eq!(api, format!("http://localhost:{port}/api/deploy"));
    assert!(other(port), "--host 0.0.0.0 listens on every address");

    let (_run, port, api, url) = ready(&["--host", "127.0.0.2"], &[]);
    assert_eq!(api, format!("http://127.0.0.2:{port}/api/deploy"));
    assert!(
        url.starts_with(&format!("http://127.0.0.2:{port}/deploy#token=")),
        "{url}"
    );
    assert!(other(port));
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());

    let (_run, port, _, _) = ready(&[], &[("DCL_SCENE_CLI_LINKER_HOST", "0.0.0.0")]);
    assert!(other(port), "the variable selects the address too");
    let (_run, port, _, _) = ready(
        &["--host", "127.0.0.1"],
        &[("DCL_SCENE_CLI_LINKER_HOST", "0.0.0.0")],
    );
    assert!(!other(port), "the flag wins");
}

/// Under the signing URL, the note that names `DCL_SCENE_CLI_LINKER_HOST` is
/// there for a run that the variable can open. `--host 127.0.0.1` wins over
/// the variable, so that run does not print the note.
#[test]
fn the_note_on_another_device_is_absent_when_the_flag_keeps_loopback() {
    const NOTE: &str = "to sign from another device, re-run with DCL_SCENE_CLI_LINKER_HOST=0.0.0.0";
    let f = Fixture::new("host-note");
    for (extra, noted) in [(&[][..], true), (&["--host", "127.0.0.1"][..], false)] {
        let args = [&["--port", "0", "--no-browser"], extra].concat();
        let run = spawn_as(&[], &f, &args, &[]);
        // The last note under the URL.
        assert!(
            run.out.wait_for("open the URL manually", HANG_GUARD),
            "stdout: {}\nstderr: {}",
            run.out.text(),
            run.err.text()
        );
        assert_eq!(run.out.text().contains(NOTE), noted, "{}", run.out.text());
    }
}

/// `--host` with one address that is not loopback, while remote signing is
/// off: the signing routes would refuse each request to that address, so the
/// command line is refused with the step to take. Exit code 2, no event,
/// and no server starts.
#[test]
fn a_host_that_needs_remote_signing_is_a_usage_error() {
    let f = Fixture::new("host-refused");
    for envs in [&[][..], &[("DCL_SCENE_CLI_LINKER_HOST", "127.0.0.1")][..]] {
        let run = spawn(&f, &["--port", "0", "--host", "192.0.2.7"], envs);
        let (code, out, err) = run.end(HANG_GUARD);
        assert_eq!(code, Some(2), "{err}");
        assert_eq!(out, "", "a usage error prints no event");
        assert!(
            err.contains("--host 192.0.2.7 needs remote signing, and remote signing is off"),
            "{err}"
        );
        assert!(err.contains("\u{2192} try: pass --host 0.0.0.0"), "{err}");
        assert!(err.contains("DCL_SCENE_CLI_LINKER_HOST=192.0.2.7"), "{err}");
    }
}

/// An address of this machine that is not loopback. No packet is sent: a UDP
/// socket that names a destination (a documentation address) only selects
/// its source address.
fn address_of_this_machine() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("192.0.2.1:9").ok()?;
    let addr = probe.local_addr().ok()?.ip();
    (!addr.is_loopback() && !addr.is_unspecified()).then_some(addr)
}

/// Remote signing on an address of this machine that is not loopback: the
/// printed `api` names the address, and the signing routes answer a request
/// to it, for the variable alone and for the flag with the variable. Each
/// request goes to an address of this machine, with no proxy.
#[tokio::test(flavor = "multi_thread")]
async fn remote_signing_on_an_address_of_this_machine_answers_its_printed_url() {
    use std::io::Write;
    let Some(addr) = address_of_this_machine() else {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED remote_signing_on_an_address_of_this_machine_answers_its_printed_url: \
             this machine has no address that is not loopback"
        );
        return;
    };
    let f = Fixture::new("host-remote");
    let host = addr.to_string();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(HANG_GUARD)
        .build()
        .unwrap();
    for (extra, variable) in [
        (&[][..], host.as_str()),
        (&["--host", host.as_str()][..], "0.0.0.0"),
    ] {
        let mut run = spawn(
            &f,
            &[&["--port", "0", "--deploy-token", TOKEN], extra].concat(),
            &[("DCL_SCENE_CLI_LINKER_HOST", variable)],
        );
        let ready = run.line_with(r#""event":"ready""#);
        let port = ready["port"].as_u64().unwrap();
        let api = ready["api"].as_str().unwrap().to_string();
        let origin = std::net::SocketAddr::new(addr, port as u16);
        assert_eq!(api, format!("http://{origin}/api/deploy"));
        assert!(
            ready["url"]
                .as_str()
                .unwrap()
                .starts_with(&format!("http://{origin}/deploy#token=")),
            "{ready}"
        );

        let (status, state) = run.json(http.get(&api)).await;
        assert_eq!(status, 200, "the reading gate admits the address: {state}");
        assert_eq!(state["phase"], "awaiting-signature");
        let (status, body) = run
            .json(
                http.post(&api)
                    .header("origin", format!("http://{origin}"))
                    .json(&serde_json::json!({ "address": "0x1" })),
            )
            .await;
        assert_eq!(status, 400, "the signing gate admits the address: {body}");
        assert_eq!(body["message"], "Invalid payload: address");
        let (status, _) = run
            .json(http.delete(&api).header("x-deploy-token", TOKEN))
            .await;
        assert_eq!(status, 202);
        let (ok, _, err) = run.finish(HANG_GUARD);
        assert!(!ok, "a cancelled run: {err}");
    }
}

/// `--language` is accepted and changes nothing a program reads, and
/// `/api/catalyst-pointers` answers in the upstream shape: the target of
/// this run does not answer, so the list is empty.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_gives_a_language_and_reads_the_pointers() {
    let f = Fixture::new("language");
    let mut run = spawn(&f, &["--language", "es", "--deploy-token", TOKEN], &[]);
    let ready = run.line_with(r#""event":"ready""#);
    let base = ready["api"]
        .as_str()
        .unwrap()
        .trim_end_matches("/api/deploy")
        .to_string();
    let http = client();
    let (status, pointers) = run
        .json(http.get(format!("{base}/api/catalyst-pointers")))
        .await;
    assert_eq!(status, 200);
    assert_eq!(pointers, serde_json::json!({ "catalysts": [] }));
    let (status, _) = run
        .json(
            http.delete(format!("{base}/api/deploy"))
                .header("x-deploy-token", TOKEN),
        )
        .await;
    assert_eq!(status, 202);
    let (_, stdout, _) = run.finish(Duration::from_secs(15));
    assert_eq!(
        stdout.lines().count(),
        2,
        "ready and the final state only: {stdout}"
    );
}

/// A deploy that fails prints the `error` event of `start --json`: the last
/// line of stdout, with the code of a cause the CLI knows. `--programmatic`
/// prints its final state first. The text stays on stderr, and the port step
/// names `deploy`.
#[tokio::test(flavor = "multi_thread")]
async fn a_deploy_that_fails_prints_the_error_event() {
    let f = Fixture::new("error-event");
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port().to_string();
    let taken = serde_json::json!({
        "event": "error",
        "message": format!("port {port} is already in use"),
        "code": "port-in-use",
    });
    let args = ["--port", &port, "--deploy-token", TOKEN];

    let (code, stdout, stderr) = spawn_as(&["--json"], &f, &args, &[]).end(Duration::from_secs(15));
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(
        json_lines(&stdout),
        std::slice::from_ref(&taken),
        "{stdout}"
    );
    assert!(
        stderr.contains(&format!("Error: port {port} is already in use")),
        "{stderr}"
    );
    assert!(stderr.contains("dcl-scene-cli deploy --port "), "{stderr}");
    assert!(!stderr.contains("start --port"), "{stderr}");
    assert!(!stderr.contains(TOKEN), "{stderr}");

    let (code, stdout, stderr) = spawn(&f, &args, &[]).end(Duration::from_secs(15));
    assert_eq!(code, Some(1), "{stderr}");
    let lines = json_lines(&stdout);
    assert_eq!(lines.len(), 2, "the final state, then the error: {stdout}");
    assert_eq!(lines[0]["event"], "state", "{stdout}");
    assert_eq!(lines[0]["phase"], "failed", "{stdout}");
    assert_eq!(lines[0]["error"]["code"], "DEPLOY_FAILED", "{stdout}");
    assert_eq!(lines[1], taken, "{stdout}");
    assert!(!stdout.contains(TOKEN), "{stdout}");
    assert!(!stderr.contains(TOKEN), "{stderr}");

    // Without --json and --programmatic no line of stdout is an event.
    let (code, stdout, stderr) = spawn_as(&[], &f, &args, &[]).end(Duration::from_secs(15));
    assert_eq!(code, Some(1), "{stderr}");
    assert!(!stdout.contains(r#""event""#), "{stdout}");
    assert!(stderr.contains("dcl-scene-cli deploy --port "), "{stderr}");
    drop(held);

    let empty = Fixture(f.0.join("empty"));
    std::fs::create_dir_all(&empty.0).unwrap();
    let missing = Fixture(f.0.join("missing"));
    for (dir, code_word, message) in [
        (
            &empty,
            "no-scene",
            Some("this directory is not a Decentraland scene"),
        ),
        (&missing, "no-directory", None),
    ] {
        for mode in ["--json", "--programmatic"] {
            let (code, stdout, stderr) =
                spawn_as(&[mode], dir, &[], &[]).end(Duration::from_secs(15));
            assert_eq!(code, Some(1), "{stderr}");
            let lines = json_lines(&stdout);
            let last = lines.last().unwrap_or_else(|| panic!("no line: {stderr}"));
            assert_eq!(last["event"], "error", "{stdout}");
            assert_eq!(last["code"], code_word, "{stdout}");
            assert_eq!(last.as_object().unwrap().len(), 3, "{stdout}");
            if let Some(message) = message {
                assert_eq!(last["message"], message, "{stdout}");
            }
            assert_eq!(
                lines.len(),
                1 + usize::from(mode == "--programmatic"),
                "{stdout}"
            );
        }
    }

    // A command line that does not parse: exit code 2 and no event.
    let (code, stdout, _) =
        spawn_as(&["--json"], &f, &["--no-such-flag"], &[]).end(Duration::from_secs(15));
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty(), "no event for a usage error: {stdout}");
}

/// A stop by SIGTERM is not a failure of the run: no `error` event. A
/// programmatic run still prints its final state.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_deploy_stopped_by_a_signal_prints_no_error_event() {
    let f = Fixture::new("signal");
    let sigterm = |run: &Run| unsafe {
        libc::kill(run.child.id() as libc::pid_t, libc::SIGTERM);
    };

    // An answer of the server shows that the run waits for a signature, and
    // so for a signal.
    let http = client();
    let mut run = spawn(&f, &["--port", "0"], &[]);
    let ready = run.line_with(r#""event":"ready""#);
    let (status, _) = run.json(http.get(ready["api"].as_str().unwrap())).await;
    assert_eq!(status, 200);
    sigterm(&run);
    let (code, stdout, stderr) = run.end(Duration::from_secs(15));
    assert_ne!(code, Some(0), "{stderr}");
    let lines = json_lines(&stdout);
    let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();
    assert_eq!(events, ["ready", "state"], "{stdout}");
    assert_eq!(lines[1]["phase"], "failed", "{stdout}");

    let mut run = spawn_as(&["--json"], &f, &["--port", "0"], &[]);
    assert!(
        run.err.wait_for("/deploy#token=", HANG_GUARD),
        "stderr never carried the signing url: {}",
        run.err.text()
    );
    let err = run.err.text();
    let origin = err
        .lines()
        .filter(|l| l.contains("/deploy#token="))
        .find_map(|l| l.trim().strip_prefix("http://")?.split("/deploy#").next())
        .unwrap_or_else(|| panic!("no signing url line in {err}"))
        .to_string();
    let (status, _) = run
        .json(http.get(format!("http://{origin}/api/deploy")))
        .await;
    assert_eq!(status, 200);
    sigterm(&run);
    let (code, stdout, stderr) = run.end(Duration::from_secs(15));
    assert_ne!(code, Some(0), "{stderr}");
    assert!(stdout.is_empty(), "no document and no event: {stdout}");
}
