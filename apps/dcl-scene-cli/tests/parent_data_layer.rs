//! A parent process and the editor data layer of `start --data-layer --json`:
//! the parent reads the `ready` line from stdout, opens `dataLayerUrl` with
//! `token`, makes one call and stops the child. Every request is on loopback.
#![cfg(unix)]

mod common {
    pub mod child;
    pub mod testgate;
    pub mod wait;
}

use futures::{SinkExt, StreamExt};
use prost::Message as _;
use serde_json::Value;
use std::process::{Command, Stdio};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

const BIN: &str = env!("CARGO_BIN_EXE_dcl-scene-cli");
/// The origin of the page that opens the socket for the parent, as the
/// parent names it in `DCL_SCENE_CLI_ALLOWED_ORIGINS`. Nothing connects to it.
const EDITOR_ORIGIN: &str = "http://localhost:8734";
const OTHER_ORIGIN: &str = "http://localhost:8735";

// The messages of the `@dcl/rpc` protocol that one call needs,
// and the two of the data layer's `GetFile`.
const REQUEST: u32 = 1;
const RESPONSE: u32 = 2;
const CREATE_PORT: u32 = 5;
const CREATE_PORT_RESPONSE: u32 = 6;
const REQUEST_MODULE: u32 = 7;
const REQUEST_MODULE_RESPONSE: u32 = 8;
const REMOTE_ERROR_RESPONSE: u32 = 9;

fn identifier(kind: u32, number: u32) -> u32 {
    (kind << 27) | number
}

#[derive(Clone, PartialEq, prost::Message)]
struct Header {
    #[prost(fixed32, tag = "1")]
    message_identifier: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CreatePort {
    #[prost(fixed32, tag = "1")]
    message_identifier: u32,
    #[prost(string, tag = "4")]
    port_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CreatePortResponse {
    #[prost(fixed32, tag = "2")]
    port_id: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct RequestModule {
    #[prost(fixed32, tag = "1")]
    message_identifier: u32,
    #[prost(fixed32, tag = "2")]
    port_id: u32,
    #[prost(string, tag = "4")]
    module_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct RequestModuleResponse {
    #[prost(message, repeated, tag = "5")]
    procedures: Vec<ModuleProcedure>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ModuleProcedure {
    #[prost(fixed32, tag = "1")]
    procedure_id: u32,
    #[prost(string, tag = "2")]
    procedure_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Request {
    #[prost(fixed32, tag = "1")]
    message_identifier: u32,
    #[prost(fixed32, tag = "2")]
    port_id: u32,
    #[prost(fixed32, tag = "4")]
    procedure_id: u32,
    #[prost(bytes = "vec", tag = "6")]
    payload: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Response {
    #[prost(bytes = "vec", tag = "6")]
    payload: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct GetFileRequest {
    #[prost(string, tag = "1")]
    path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct GetFileResponse {
    #[prost(bytes = "vec", tag = "1")]
    content: Vec<u8>,
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn upgrade(url: &str, origin: Option<&str>) -> tungstenite::handshake::client::Request {
    let mut request = url.into_client_request().unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    request
}

/// The status of an upgrade the preview refuses.
async fn refusal(url: &str, origin: Option<&str>) -> u16 {
    match tokio_tungstenite::connect_async(upgrade(url, origin)).await {
        Err(tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(e) => panic!("the upgrade failed in another way: {e}"),
        Ok(_) => panic!("the upgrade was not refused"),
    }
}

/// Sends `message` and gives the answer of kind `kind` to message `number`.
async fn ask<T: prost::Message + Default>(
    socket: &mut Socket,
    message: impl prost::Message,
    kind: u32,
    number: u32,
) -> T {
    socket
        .send(Message::Binary(message.encode_to_vec().into()))
        .await
        .unwrap();
    loop {
        let frame = tokio::time::timeout(common::wait::HANG_GUARD, socket.next())
            .await
            .expect("an answer from the data layer")
            .expect("the socket stays open")
            .unwrap();
        let Message::Binary(bytes) = frame else {
            continue;
        };
        let id = Header::decode(&bytes[..]).unwrap().message_identifier;
        assert_ne!(
            id >> 27,
            REMOTE_ERROR_RESPONSE,
            "the data layer answered with an error: {}",
            String::from_utf8_lossy(&bytes)
        );
        if id == identifier(kind, number) {
            return T::decode(&bytes[..]).unwrap();
        }
    }
}

/// The argv of each child process of `parent`.
#[cfg(target_os = "linux")]
fn argv_of_children(parent: u32) -> Vec<String> {
    let is_child = |status: &str| {
        status
            .lines()
            .any(|line| line.strip_prefix("PPid:").map(str::trim) == Some(&parent.to_string()))
    };
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            let status = std::fs::read_to_string(dir.join("status")).ok()?;
            let argv = std::fs::read(dir.join("cmdline")).ok()?;
            is_child(&status).then(|| String::from_utf8_lossy(&argv).replace('\0', " "))
        })
        .collect()
}

/// `init` makes the scene, `start --data-layer --json --no-client` serves it,
/// and the `ready` line is all the parent needs: the data layer answers
/// `GetFile` through `dataLayerUrl` with `token`. The socket
/// refuses an upgrade with no token (401) and one from a page whose origin is
/// not in `DCL_SCENE_CLI_ALLOWED_ORIGINS` (403). The token is not in the argv
/// of a child process, and not on stderr, which the data-layer host of this
/// run writes to as well.
#[tokio::test]
async fn a_parent_reads_the_ready_line_and_calls_the_data_layer() {
    if dcl_scene_cli::build::find_node().is_none() {
        let _: Option<()> = common::testgate::unavailable(
            "node",
            "install node; the data-layer host of start --data-layer runs under it",
        );
        return;
    }
    // A folder with a random name that only this user can read or write.
    let folder = tempfile::tempdir().unwrap();
    let scene = folder.path().join("scene");
    let init = Command::new(BIN)
        .args(["init", "--yes", "--dir"])
        .arg(&scene)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let mut child = Command::new(BIN)
        .args([
            "start",
            "--port",
            "0",
            "--no-host",
            "--no-mcp",
            "--offline-comms",
            "--no-asset-bundles",
            "--no-livekit",
            "--data-layer",
            "--json",
            "--no-client",
            "--dir",
        ])
        .arg(&scene)
        .env_remove("RUST_LOG")
        .env_remove("CI")
        .env("NO_COLOR", "1")
        .env("DCL_SCENE_CLI_ALLOWED_ORIGINS", EDITOR_ORIGIN)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = common::child::Tail::spawn(child.stdout.take().unwrap());
    let stderr = common::child::Tail::spawn(child.stderr.take().unwrap());

    let ready = common::wait::until(common::wait::HANG_GUARD, async || {
        let text = stdout.text();
        let whole = &text[..text.rfind('\n').map_or(0, |end| end + 1)];
        whole
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["event"] == "ready")
    })
    .await;
    let Some(ready) = ready else {
        common::child::terminate(&mut child);
        panic!(
            "no ready event\nstdout: {}\nstderr: {}",
            stdout.text(),
            stderr.text()
        );
    };
    let port = ready["port"].as_u64().unwrap();
    let token = ready["token"].as_str().unwrap().to_string();
    let data_layer = ready["dataLayerUrl"].as_str().unwrap().to_string();
    assert_eq!(data_layer, format!("ws://127.0.0.1:{port}/data-layer"));
    assert_eq!(ready["clientsOpened"], serde_json::json!([]), "{ready}");
    let url = format!("{data_layer}?token={token}");

    assert_eq!(refusal(&data_layer, None).await, 401, "no token");
    assert_eq!(refusal(&url, Some(OTHER_ORIGIN)).await, 403, "other page");
    tokio_tungstenite::connect_async(upgrade(&url, None))
        .await
        .expect("the parent process itself, which sends no Origin");

    let (mut socket, _) = tokio_tungstenite::connect_async(upgrade(&url, Some(EDITOR_ORIGIN)))
        .await
        .expect("the page the parent named");
    let port_id = ask::<CreatePortResponse>(
        &mut socket,
        CreatePort {
            message_identifier: identifier(CREATE_PORT, 1),
            port_name: "parent".to_string(),
        },
        CREATE_PORT_RESPONSE,
        1,
    )
    .await
    .port_id;
    let module = ask::<RequestModuleResponse>(
        &mut socket,
        RequestModule {
            message_identifier: identifier(REQUEST_MODULE, 2),
            port_id,
            module_name: "DataService".to_string(),
        },
        REQUEST_MODULE_RESPONSE,
        2,
    )
    .await;
    let get_file = module
        .procedures
        .iter()
        .find(|procedure| procedure.procedure_name == "GetFile")
        .unwrap_or_else(|| panic!("no GetFile in {:?}", module.procedures))
        .procedure_id;
    let answer = ask::<Response>(
        &mut socket,
        Request {
            message_identifier: identifier(REQUEST, 3),
            port_id,
            procedure_id: get_file,
            payload: GetFileRequest {
                path: "scene.json".to_string(),
            }
            .encode_to_vec(),
        },
        RESPONSE,
        3,
    )
    .await;
    // The vendored data-layer host answers each file read with no content, so
    // the bytes of the answer are not compared with the file.
    GetFileResponse::decode(&answer.payload[..]).expect("the answer of GetFile");
    drop(socket);

    #[cfg(target_os = "linux")]
    {
        let argv = argv_of_children(child.id());
        assert!(
            argv.iter().any(|argv| argv.contains("data-layer-host")),
            "the data-layer host is a child process: {argv:?}"
        );
        assert!(
            !argv.iter().any(|argv| argv.contains(&token)),
            "the token is in an argv"
        );
    }

    let status = common::child::terminate(&mut child);
    assert_eq!(status.code(), Some(0), "a stop on request");
    let out = String::from_utf8(stdout.finish()).unwrap();
    for line in out.lines() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
    }
    assert!(!stderr.text().contains(&token), "the token is on stderr");
}
