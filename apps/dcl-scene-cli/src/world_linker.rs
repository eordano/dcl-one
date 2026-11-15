//! Browser signing for world-management requests: the ADR signed-fetch
//! payload (`method:path:timestamp:{}`) that world settings and permission
//! changes authenticate with, so granting a deploy key never requires
//! exporting the owner's private key.
//!
//! Unlike the deploy flow, a rejection here is usually "wrong wallet
//! connected", so 401/403 keeps the page alive and invites another attempt
//! instead of killing the CLI.

use crate::deploy::{caused, read_server_message, refusal};
use crate::linker::{LinkerOptions, finish, fmt_wait, signing_bind, spawn_browser};
use crate::ux::{self, TrySteps, UserError};
use crate::world::{WorldAction, browser_headers, signed_fetch_payload_meta};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    response::Html,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

pub struct WorldSignRequest {
    pub base: String,
    pub name: String,
    pub action: WorldAction,
}

/// The world flows answer the CLI with the outcome line alone.
pub(crate) type DoneSender = tokio::sync::oneshot::Sender<Result<String>>;

pub struct WorldLinkerState {
    req: WorldSignRequest,
    /// Payloads minted by /api/info and not yet spent; bounded so a page left
    /// reloading cannot grow it without limit.
    pending: Mutex<HashSet<String>>,
    done: Mutex<Option<DoneSender>>,
}

pub fn new_state(
    req: WorldSignRequest,
) -> (
    Arc<WorldLinkerState>,
    tokio::sync::oneshot::Receiver<Result<String>>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (
        Arc::new(WorldLinkerState {
            req,
            pending: Mutex::new(HashSet::new()),
            done: Mutex::new(Some(tx)),
        }),
        rx,
    )
}

pub fn router(state: Arc<WorldLinkerState>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/info", get(info))
        .route("/api/sign", post(sign))
        .with_state(state)
}

async fn page() -> Html<String> {
    document(PAGE)
}

/// A world page is one document with nothing to fetch: the ui3 elements, the
/// CLI tokens and the sheet both world pages share go ahead of its own rules.
pub(crate) fn document(page: &str) -> Html<String> {
    use crate::start::chrome::{TOKENS, ui3};
    let shared = format!("<style>{}{TOKENS}{}", ui3(), include_str!("world_page.css"));
    Html(page.replacen("<style>", &shared, 1))
}

async fn info(State(st): State<Arc<WorldLinkerState>>) -> Json<Value> {
    let r = &st.req;
    let path = r.action.path(&r.name);
    let method = r.action.method();
    let payload =
        signed_fetch_payload_meta(method, &path, crate::deploy::now_ms(), &r.action.metadata());
    remember_pending(&st.pending, payload.clone());
    let details = match &r.action {
        WorldAction::SettingsSet(update) => update.changed_fields(),
        WorldAction::SceneRemove {
            coordinate,
            entity_id,
        } => vec![
            format!("remove scene at {coordinate}"),
            format!("expected deployment={entity_id}"),
            "Other scenes remain published. No replacement scene will be uploaded.".to_string(),
        ],
        WorldAction::Permission {
            permission,
            address,
            revoke,
        } => vec![
            format!("permission={permission}"),
            format!("address={address}"),
            format!("operation={}", if *revoke { "revoke" } else { "grant" }),
        ],
        WorldAction::AccessSet(spec) => spec.details(),
        WorldAction::AccessCommunity { id, revoke } => vec![
            format!("community={id}"),
            format!("operation={}", if *revoke { "remove" } else { "add" }),
        ],
    };
    Json(json!({
        "world": r.name,
        "summary": r.action.summary(),
        "method": method.to_uppercase(),
        "path": path,
        "targetContent": r.base,
        "payload": payload,
        "details": details,
    }))
}

/// Bounded so a page left reloading cannot grow the set without limit.
pub(crate) fn remember_pending(pending: &Mutex<HashSet<String>>, payload: String) {
    let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
    if pending.len() > 32 {
        pending.clear();
    }
    pending.insert(payload);
}

#[derive(Deserialize)]
struct SignReq {
    address: String,
    signature: String,
    payload: String,
}

async fn sign(State(st): State<Arc<WorldLinkerState>>, Json(req): Json<SignReq>) -> Json<Value> {
    let known = st
        .pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&req.payload);
    if !known {
        return Json(stale_payload());
    }
    let r = &st.req;
    Json(
        submit(
            &st.done,
            &r.base,
            &r.name,
            &r.action,
            &req.address,
            &req.payload,
            &req.signature,
        )
        .await,
    )
}

pub(crate) fn stale_payload() -> Value {
    json!({
        "ok": false,
        "fatal": false,
        "error": "unknown or stale request — reload the page and sign again"
    })
}

/// Send a browser-signed action and turn the outcome into the page's answer.
/// A refusal that another wallet could fix (401/403, an expired signature)
/// keeps the CLI waiting; anything else resolves it with the error.
pub(crate) async fn submit(
    done: &Mutex<Option<DoneSender>>,
    base: &str,
    name: &str,
    action: &WorldAction,
    address: &str,
    payload: &str,
    signature: &str,
) -> Value {
    let headers = browser_headers(address, payload, signature);
    match action.send(base, name, headers).await {
        Ok((status, body)) if (200..300).contains(&status) => {
            action.print_body(&body);
            let message = action.success(name);
            finish(done, Ok(message.clone()));
            json!({ "ok": true, "message": message })
        }
        Ok((status, body)) => {
            let detail = body.trim();
            let tail = if detail.is_empty() {
                String::new()
            } else {
                format!("\n\n{detail}")
            };
            let expired = is_expired_signature(detail);
            let retryable = expired || status == 401 || status == 403;
            let error = if expired {
                format!(
                    "the signature expired before it reached the server \u{2014} it is only valid for about a minute.{tail}"
                )
            } else if retryable {
                format!(
                    "the worlds server refused this request (HTTP {status}) — {} is not allowed to {} on {}.{tail}",
                    address,
                    action.summary(),
                    name,
                )
            } else {
                format!("the worlds server rejected this request (HTTP {status}){tail}")
            };
            if !retryable {
                let err = UserError::new(
                    format!(
                        "the worlds server refused to {} (HTTP {status})",
                        action.summary()
                    ),
                    read_server_message(),
                );
                finish(done, Err(refusal(err, detail)));
            }
            json!({
                "ok": false,
                "fatal": !retryable,
                "expired": expired,
                "error": error,
            })
        }
        Err(e) => {
            let msg = format!("{e:#}");
            finish(done, Err(e));
            json!({ "ok": false, "fatal": true, "error": msg })
        }
    }
}

/// The worlds server answers an expired payload with an "Expired signature"
/// error on a plain 401; matching the body is what tells "sign again" from
/// "wrong wallet".
fn is_expired_signature(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("expired signature") || lower.contains("timestamp expiration")
}

fn timeout_error(timeout: std::time::Duration, url: &str, summary: &str) -> anyhow::Error {
    UserError::new(
        format!(
            "no signature arrived within {} \u{2014} request abandoned",
            fmt_wait(timeout)
        ),
        TrySteps::one("re-run the command and sign on the printed URL")
            .and("raise the wait with DCL_SCENE_CLI_LINKER_TIMEOUT_SECS=<seconds>")
            .and("for headless runs set DCL_PRIVATE_KEY or pass --sign-key <file>"),
    )
    .why(format!("the page at {url} was waiting to {summary}"))
    .into()
}

pub async fn run(req: WorldSignRequest, opts: LinkerOptions) -> Result<String> {
    let summary = req.action.summary();
    let world = req.name.clone();
    let (state, rx) = new_state(req);
    let app = router(state);
    serve(app, rx, &world, &summary, opts).await
}

/// Host a one-shot signing page: bind, print the URL, open the browser, and
/// resolve with the first outcome the page reports (or the timeout).
pub(crate) async fn serve(
    app: Router,
    rx: tokio::sync::oneshot::Receiver<Result<String>>,
    world: &str,
    summary: &str,
    opts: LinkerOptions,
) -> Result<String> {
    // The same reading of DCL_SCENE_CLI_LINKER_HOST as `deploy`; these
    // commands have no `--host`.
    let signing = signing_bind(None)?;
    let listener = tokio::net::TcpListener::bind((signing.addr, opts.port.unwrap_or(0)))
        .await
        .map_err(caused(
            match opts.port {
                Some(p) => format!("port {p} cannot be opened for the signing page"),
                None => "no port could be opened for the signing page".to_string(),
            },
            TrySteps::one("pass --port <free-port> or free the port and retry"),
        ))?;
    let bound = listener
        .local_addr()
        .context("reading the signing page port")?;
    let url = format!("{}/", crate::start::signing_origin(bound));
    println!();
    println!("Sign this change to {world} with your wallet in a browser:");
    println!("  {url}");
    ux::note(format!("the wallet must be allowed to {summary}"));
    if let Some(note) = signing.other_device_note() {
        ux::note(note);
    }
    if opts.open_browser {
        spawn_browser(&url);
    } else {
        ux::note("browser auto-open disabled \u{2014} open the URL manually");
    }
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = stopped.await;
    });
    let serve = std::future::IntoFuture::into_future(serve);
    tokio::pin!(serve);
    tokio::select! {
        r = &mut serve => {
            r.context("serving the signing page")?;
            Err(UserError::new(
                "the signing page stopped before a signature arrived",
                TrySteps::one("re-run the command"),
            )
            .into())
        }
        res = rx => {
            let _ = stop.send(());
            tokio::time::timeout(std::time::Duration::from_secs(5), &mut serve)
                .await
                .context("timed out draining the signing response")?
                .context("serving the signing response")?;
            match res {
                Ok(outcome) => outcome,
                Err(_) => Err(UserError::new(
                    "the signing flow ended without a result",
                    TrySteps::one("re-run the command"),
                )
                .into()),
            }
        },
        _ = tokio::time::sleep(opts.timeout) => Err(timeout_error(opts.timeout, &url, summary)),
    }
}

const PAGE: &str = r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>dcl-scene-cli world — sign</title>
<style>
  body{align-items:center}
  .sheet{max-width:720px}
  .kv{display:grid;grid-template-columns:130px 1fr;gap:var(--s-1-5) var(--s-3);font-size:var(--fs-13)}
  .kv b{color:var(--ink-6);font-weight:var(--fw-medium)}
  ul{margin:0;padding-left:var(--s-4);color:var(--ink-7);font-size:var(--fs-12)}
</style></head>
<body><div class="ui-card sheet">
  <h1>World change <span>·</span> signature required</h1>
  <p class="sub">Connect the wallet that owns this world (or holds the needed permission) and sign. The command line waits until you finish here.</p>
  <div class="kv">
    <b>World</b><code id="world">…</code>
    <b>Change</b><code id="summary">…</code>
    <b>Details</b><div><ul id="details"></ul></div>
    <b>Request</b><code id="req">…</code>
    <b>Server</b><code id="cs">…</code>
  </div>
  <div class="act"><button class="btn btn--primary btn--lg" id="go" disabled>Connect wallet &amp; sign</button></div>
  <div id="status"></div>
</div>
<script>
let INFO=null;
const $=id=>document.getElementById(id);
function show(cls,msg){const s=$("status");s.className=cls;s.style.display="block";s.textContent=msg;}
function render(){
  $("world").textContent=INFO.world;
  $("summary").textContent=INFO.summary;
  $("details").replaceChildren(...INFO.details.map(d=>{const li=document.createElement("li");li.textContent=d;return li;}));
  $("req").textContent=INFO.method+" "+INFO.path;
  $("cs").textContent=INFO.targetContent;
}
async function load(){
  try{
    INFO=await (await fetch("api/info")).json();
    if(INFO.error){show("err",INFO.error);return;}
    render();
    $("go").disabled=false;
  }catch(e){show("err","Could not load the request: "+e);}
}
// The server accepts a signed payload for only ~60s, and a wallet prompt can
// easily outlast that. So: connect first (slowest step, done once), then mint
// and sign back-to-back, and if the payload still ages out, mint a fresh one
// and ask again rather than sending the user back to the command line.
const MAX_SIGN_ATTEMPTS=3;
async function attempt(address,left){
  INFO=await (await fetch("api/info")).json();
  if(INFO.error){show("err",INFO.error);return false;}
  render();
  show("info","Signing with "+address+" … (approve promptly — the signature is valid for about a minute)");
  const signature=await window.ethereum.request({method:"personal_sign",params:[INFO.payload,address]});
  show("info","Sending to "+INFO.targetContent+" …");
  const r=await (await fetch("api/sign",{method:"POST",headers:{"content-type":"application/json"},
    body:JSON.stringify({address,signature,payload:INFO.payload})})).json();
  if(r.ok){show("ok","✓ "+r.message+"\n\nYou can close this tab; the command line has finished.");return true;}
  if(r.expired&&left>0){
    show("info","That signature expired before it landed. Signing again with a fresh timestamp — please approve as soon as the prompt appears.");
    return attempt(address,left-1);
  }
  if(r.expired){show("err","✗ "+r.error+"\n\nGave up after "+MAX_SIGN_ATTEMPTS+" attempts. Press the button and approve the wallet prompt immediately.");return false;}
  if(r.fatal){show("err","✗ "+r.error+"\n\nThe command line exited with this error — fix it and re-run.");return false;}
  show("err","✗ "+r.error+"\n\nSwitch to a wallet that is allowed to make this change and press the button again.");
  return false;
}
$("go").onclick=async()=>{
  const btn=$("go");
  try{
    if(!window.ethereum){show("err","No wallet found. Open this page in a browser with MetaMask (or another EIP-1193 wallet).");return;}
    btn.disabled=true;
    show("info","Requesting wallet…");
    const accounts=await window.ethereum.request({method:"eth_requestAccounts"});
    const address=accounts[0];
    const ok=await attempt(address,MAX_SIGN_ATTEMPTS-1);
    if(!ok)btn.disabled=false;
  }catch(e){show("err","✗ "+(e&&e.message?e.message:e));btn.disabled=false;}
};
load();
</script></body></html>
"##;

#[cfg(test)]
mod tests {
    #[test]
    fn the_page_is_built_from_the_ui3_elements() {
        crate::world_access::assert_built_from_ui3(super::PAGE, &["btn btn--primary btn--lg"]);
    }

    /// The signing page reads `DCL_SCENE_CLI_LINKER_HOST` as `deploy` does:
    /// a value that is not an IP address stops the command before a port
    /// opens.
    #[tokio::test]
    async fn the_signing_page_reads_the_variable_as_deploy_does() {
        let _env = crate::deploy::ENV_LOCK.lock().await;
        crate::set_test_env("DCL_SCENE_CLI_LINKER_HOST", "not-an-address");
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let served = super::serve(
            axum::Router::new(),
            rx,
            "example.dcl.eth",
            "publish",
            crate::linker::LinkerOptions {
                port: None,
                open_browser: false,
                timeout: std::time::Duration::from_secs(5),
                programmatic: false,
                host: None,
            },
        )
        .await;
        crate::remove_test_env("DCL_SCENE_CLI_LINKER_HOST");
        let err = format!("{:#}", served.unwrap_err());
        assert!(
            err.contains("DCL_SCENE_CLI_LINKER_HOST is not an IP address: not-an-address"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn standalone_signing_drains_the_response_before_returning() {
        let _env = crate::deploy::ENV_LOCK.lock().await;
        crate::remove_test_env("DCL_SCENE_CLI_LINKER_HOST");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let done = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let expected = "published".repeat(128 * 1024);
        let response = expected.clone();
        let app = axum::Router::new().route(
            "/done",
            axum::routing::get(move || {
                let done = done.clone();
                let response = response.clone();
                async move {
                    crate::linker::finish(&done, Ok("published".into()));
                    response
                }
            }),
        );
        let server = tokio::spawn(super::serve(
            app,
            rx,
            "example.dcl.eth",
            "publish",
            crate::linker::LinkerOptions {
                port: Some(port),
                open_browser: false,
                timeout: std::time::Duration::from_secs(5),
                programmatic: false,
                host: None,
            },
        ));
        let body = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut delay = std::time::Duration::from_millis(1);
            loop {
                match reqwest::get(format!("http://127.0.0.1:{port}/done")).await {
                    Ok(response) => break response.text().await.unwrap(),
                    Err(_) => {
                        tokio::time::sleep(delay).await;
                        delay = delay
                            .saturating_mul(2)
                            .min(std::time::Duration::from_millis(50));
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(body, expected);
        assert_eq!(server.await.unwrap().unwrap(), "published");
    }

    use super::*;
    use crate::world::SettingsUpdate;

    fn permission_request(base: &str) -> WorldSignRequest {
        WorldSignRequest {
            base: base.to_string(),
            name: "Test.dcl.eth".to_string(),
            action: WorldAction::Permission {
                permission: "deployment".to_string(),
                address: "0x1111111111111111111111111111111111111111".to_string(),
                revoke: false,
            },
        }
    }

    async fn serve(
        req: WorldSignRequest,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<Result<String>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (state, rx) = new_state(req);
        let app = router(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, rx, handle)
    }

    async fn get_info(base: &str) -> Value {
        reqwest::Client::new()
            .get(format!("{base}/api/info"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post_sign(base: &str, body: &Value) -> Value {
        reqwest::Client::new()
            .post(format!("{base}/api/sign"))
            .json(body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn info_describes_the_permission_change_and_mints_a_signable_payload() {
        let (base, _rx, handle) = serve(permission_request("http://127.0.0.1:9")).await;

        let page = reqwest::Client::new()
            .get(format!("{base}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status().as_u16(), 200);
        let body = page.text().await.unwrap();
        assert!(body.contains("personal_sign"));
        assert!(body.contains("api/info"));

        let info = get_info(&base).await;
        assert_eq!(info["world"], "Test.dcl.eth");
        assert_eq!(info["method"], "PUT");
        assert_eq!(
            info["path"],
            "/world/Test.dcl.eth/permissions/deployment/0x1111111111111111111111111111111111111111"
        );
        let payload = info["payload"].as_str().unwrap();
        assert!(payload.starts_with("put:/world/test.dcl.eth/permissions/deployment/"));
        assert_eq!(payload, payload.to_lowercase());
        handle.abort();
    }

    #[test]
    fn expiry_is_told_apart_from_a_permission_refusal() {
        let expired = r#"{"error":"Expired signature: signature timestamp: 1785345934007, timestamp expiration: 1785345994007, local timestamp: 1785346010626","message":"This endpoint requires a signed fetch request. See ADR-44."}"#;
        assert!(is_expired_signature(expired));
        assert!(is_expired_signature("expired signature"));
        assert!(!is_expired_signature(
            r#"{"error":"Not authorized","message":"wallet is not the owner"}"#
        ));
        assert!(!is_expired_signature(""));
    }

    #[tokio::test]
    async fn a_stale_payload_is_rejected_without_killing_the_cli() {
        let (base, _rx, handle) = serve(permission_request("http://127.0.0.1:9")).await;
        let stale = post_sign(
            &base,
            &json!({"address":"0x0","signature":"0x0","payload":"put:/nope:1:{}"}),
        )
        .await;
        assert_eq!(stale["ok"], false);
        assert_eq!(stale["fatal"], false);
        handle.abort();
    }

    #[tokio::test]
    async fn an_unreachable_server_is_fatal_and_resolves_the_cli() {
        let (base, rx, handle) = serve(permission_request("http://127.0.0.1:9")).await;
        let info = get_info(&base).await;
        let payload = info["payload"].as_str().unwrap().to_string();
        let signer = crate::random_test_wallet();
        let sig = signer.sign_message(payload.as_bytes()).unwrap();
        let resp = post_sign(
            &base,
            &json!({
                "address": signer.address(),
                "signature": sig,
                "payload": payload,
            }),
        )
        .await;
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["fatal"], true);
        assert!(rx.await.unwrap().is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn settings_updates_list_only_the_touched_fields() {
        let req = WorldSignRequest {
            base: "http://127.0.0.1:9".to_string(),
            name: "Test.dcl.eth".to_string(),
            action: WorldAction::SettingsSet(SettingsUpdate {
                title: Some("New Title".to_string()),
                single_player: Some(true),
                ..Default::default()
            }),
        };
        let (base, _rx, handle) = serve(req).await;
        let info = get_info(&base).await;
        let details: Vec<String> = info["details"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap().to_string())
            .collect();
        assert_eq!(details, vec!["title=New Title", "single_player=true"]);
        assert_eq!(info["path"], "/world/Test.dcl.eth/settings");
        handle.abort();
    }
}
