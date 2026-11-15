pub(crate) mod ansi;
mod assistant;
mod assistant_bridge;
mod assistant_history;
mod assistant_provider;
mod assistant_utterances;
pub(crate) mod chrome;
mod content_cache;
mod deploy_api;
mod deploy_page;
mod deploy_rights;
mod deploy_status;
mod edit;
mod editor;
mod external_debug;
mod external_debug_state;
mod host_slot;
mod http;
mod land_picker;
mod landing;
mod project_assets;
mod project_bridge;
pub(crate) mod proxy;
pub(crate) mod scene_logs;
mod storage_page;
mod ui_designer;

use crate::build::{self, BuildOptions};
use crate::data_layer::{self, DataLayerState};
use crate::joinblock::{self, JoinBlock, QrMode};
use crate::linker::Published;
use crate::live_reload::{self, ReloadEvent, ReloadFrame};
use crate::netinfo::{self, Iface, IfaceClass};
use crate::play_embed;
use crate::scene::{Project, b64_hash, machine_id};
use crate::shutdown::shutdown_signal;
use crate::ux::{self, TrySteps, UserError};
use crate::watch::{FsWatcher, WatchSession};
use crate::workspace::Workspace;
use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use editor::{data_layer_ws, inspector_asset, inspector_index, inspector_redirect, mobile_preview};
use http::{
    about, contents, entities_active, entities_scene, feature_flags, get_scene_adapter,
    preview_origin, preview_wearable, preview_wearables, root, scene_id_for, scene_json, scenes,
    signed_login,
};
use proxy::{
    catalyst_proxy, lambdas_contracts_servers, lambdas_explore_realms, world_about, world_content,
};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// `joinblock` advertises the `/world/…` mirror only when this holds: the
/// mirror answers 501 otherwise.
pub fn world_base_configured() -> bool {
    proxy::world_base().is_some()
}

pub struct StartOptions {
    pub dir: PathBuf,
    /// None picks 8000, or the next free port when 8000 is taken.
    pub port: Option<u16>,
    pub skip_build: bool,
    pub skip_type_check: bool,
    pub no_watch: bool,
    pub ignore_composite: bool,
    pub offline_comms: bool,
    /// Comms (and so voice) on a LiveKit SFU elsewhere (`--livekit-url`).
    pub livekit: Option<crate::livekit::Livekit>,
    /// Run a livekit-server of this preview's own when `livekit` names none:
    /// the default; `--no-livekit` keeps the built-in ws-room.
    pub embedded_livekit: bool,
    /// The realm room of that embedded server.
    pub livekit_room: String,
    pub mobile: bool,
    pub ab_sidecar: bool,
    /// Forward `local-ab=true` in the desktop deep link (tracks `ab_sidecar`):
    /// the client then fetches `{realm}/optimized-assets`, which this server
    /// proxies to the sidecar. Naming the sidecar via `optimized-assets-url`
    /// instead does not work — the launcher drops that param.
    pub local_ab: bool,
    pub mcp: bool,
    /// Let a non-loopback peer press Deploy. Off by default: the button signs
    /// with the hosting machine's wallet and the port is otherwise
    /// unauthenticated.
    pub allow_remote_deploy: bool,
    /// Already defaulted by the caller, so the deep link and the log reader
    /// agree on the port.
    pub mcp_port: u16,
    pub source_context: SourceContext,
    /// Raw tokens after a standalone `--`, forwarded into the deep link as
    /// query params.
    pub explorer_params: Vec<String>,
    /// `--multi-instance` and `--skip-auth-screen`: see `link_defaults`.
    pub multi_instance: bool,
    pub skip_auth_screen: bool,
    pub data_layer: bool,
    pub tunnel: Option<String>,
    pub tunnel_token: Option<String>,
    /// `--public-url`: the address a reverse proxy or a custom domain serves
    /// this preview at.
    pub public_url: Option<String>,
    /// Skip the authoritative-server isolate a scene.json
    /// `authoritativeMultiplayer: true` attaches (upstream parity).
    pub no_host: bool,
    /// `--talk`: hands-free listening in the web explorer's Scene assistant.
    pub talk: Option<Talk>,
    /// `--dclenv`: `org` or `zone`, for the deep links and the hosted web
    /// explorer.
    pub dclenv: String,
    /// `--realm`: what the desktop deep links name instead of this preview.
    pub realm: Option<String>,
    /// `--position`: where the join links land instead of scene.json's base.
    pub position: Option<(i64, i64)>,
    /// `--explorer-alpha`: open the desktop deep link once the banner prints.
    pub launch_desktop: bool,
    /// `--web`: open the hosted web explorer once the banner prints.
    pub launch_web: bool,
}

/// `--talk`: the web explorer that will be opened is let through the
/// `/api/project/*` origin gate for this run, the banner prints the URL that
/// lands in it with the Scene assistant open, and `open_browser` opens that
/// URL once (`--no-browser` turns it off).
#[derive(Clone, Debug, Default)]
pub struct Talk {
    /// `--player`: the web explorer to use, over `DCL_SCENE_CLI_WEB_EXPLORER`.
    pub player: Option<String>,
    pub open_browser: bool,
}

/// The web explorer `--talk` opens when neither `--player` nor
/// `DCL_SCENE_CLI_WEB_EXPLORER` names one and the binary carries no player of
/// its own: the build's site player (`play_embed::site_player`), since the
/// HUD that carries the Scene assistant lives there, not on the join
/// block's default player. Empty when the build names no site, which
/// `--talk` refuses with the flag and variable to set.
fn talk_web_explorer() -> String {
    play_embed::site_player().unwrap_or_default()
}

/// The origin `--talk` lets through the `/api/project/*` gate, set once per
/// run before the server takes requests. It reaches that gate only: the
/// data-layer upgrade keeps to `DCL_SCENE_CLI_ALLOWED_ORIGINS`.
static TALK_ORIGIN: OnceLock<String> = OnceLock::new();

/// `--public-url`, parsed: what a request for that host is answered as when
/// the proxy in front says nothing about the scheme or the path it serves.
static PUBLIC_URL: OnceLock<PublicUrl> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicUrl {
    https: bool,
    /// `host[:port]`, lowercase, the scheme's default port left out.
    authority: String,
    /// The path the proxy serves the preview under: empty, or `/p` with no
    /// trailing slash.
    prefix: String,
}

impl PublicUrl {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let bad = |why: &str| {
            UserError::new(
                format!("--public-url {raw:?} is not an address a preview can be served at"),
                TrySteps::one("pass the address people open, scheme included: --public-url https://preview.example.com")
                    .and("a path prefix is allowed (https://example.com/preview); a query or a fragment is not"),
            )
            .why(why.to_string())
        };
        let url = url::Url::parse(raw.trim()).map_err(|e| bad(&e.to_string()))?;
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            other => return Err(bad(&format!("the scheme is {other}, not http or https")).into()),
        };
        let Some(host) = url.host_str() else {
            return Err(bad("it names no host").into());
        };
        if url.query().is_some() || url.fragment().is_some() {
            return Err(bad("it carries a query or a fragment").into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(bad("it carries credentials").into());
        }
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        }
        .to_ascii_lowercase();
        Ok(PublicUrl {
            https,
            authority,
            prefix: url.path().trim_end_matches('/').to_string(),
        })
    }

    pub(crate) fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}{}", self.authority, self.prefix)
    }

    /// Whether a request names this address: `X-Forwarded-Host` when a proxy
    /// sent one, else `Host`, compared without the scheme's default port.
    fn names(&self, headers: &HeaderMap) -> bool {
        let default_port = if self.https { ":443" } else { ":80" };
        forwarded_host(headers)
            .or_else(|| {
                headers
                    .get(header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_string)
            })
            .is_some_and(|host| {
                let host = host.trim().to_ascii_lowercase();
                host.strip_suffix(default_port).unwrap_or(&host) == self.authority
            })
    }
}

/// The `--public-url` a request arrived for, if it did.
fn public_url_of(headers: &HeaderMap) -> Option<&'static PublicUrl> {
    PUBLIC_URL.get().filter(|public| public.names(headers))
}

/// The site the embedded player's HUD reads its services from
/// (`play_embed::PlayConfig`): the origin of the player `--player` or
/// `DCL_SCENE_CLI_WEB_EXPLORER` names when that is not a loopback address —
/// a self-hoster pointing at their own site gets its places and events —
/// else the build's site, if any. The embedded player itself is on
/// 127.0.0.1 and names no site.
fn play_site(flag: Option<&str>, env: Option<&str>) -> String {
    [flag, env]
        .into_iter()
        .flatten()
        .filter_map(origin_of_url)
        .find(|origin| !is_local_origin(origin))
        .or_else(play_embed::default_site)
        .unwrap_or_default()
}

/// Whether an origin is this machine: a loopback or unspecified address
/// (127.0.0.2 and 0.0.0.0 included), `localhost` or a `.localhost` name —
/// the same reading of a host as the `/api/project` gate's.
fn is_local_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => false,
    }
}

/// The `--talk` player: the flag, else the player this binary embeds
/// (`play_embed::local_player`, same-origin with the project API and a
/// secure context on 127.0.0.1), else the environment, else
/// [`talk_web_explorer`]; blank values count as absent.
fn talk_player(flag: Option<&str>, embedded: Option<&str>, env: Option<&str>) -> String {
    [flag, embedded, env]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map_or_else(talk_web_explorer, str::to_string)
        .trim_end_matches('/')
        .to_string()
}

/// The web join URL with `assistant=1`, which opens the Scene assistant in
/// the HUD; ahead of any fragment the player URL carried. An `assistant`
/// pair the player URL brought along is dropped rather than kept ahead of
/// ours, since the HUD reads the first one. None when the player is not an
/// http(s) URL.
fn talk_url(player: &str, realm: &str, position: (i64, i64)) -> Option<String> {
    let joined = joinblock::web_join_url(player, realm, position);
    if joined.is_empty() {
        return None;
    }
    let (base, fragment) = match joined.split_once('#') {
        Some((base, fragment)) => (base.to_string(), Some(fragment.to_string())),
        None => (joined, None),
    };
    let (path, query) = base.split_once('?').unwrap_or((&base, ""));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some("assistant"))
        .collect();
    let mut url = format!("{path}?{}&assistant=1", kept.join("&"));
    if let Some(fragment) = fragment {
        url.push('#');
        url.push_str(&fragment);
    }
    Some(url)
}

/// The origin an http(s) URL's requests carry, in the exact form
/// `project_bridge::allowed` compares (`scheme://host[:port]`, lowercase, no
/// default port).
fn origin_of_url(url: &str) -> Option<String> {
    let url = url::Url::parse(url.trim()).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

/// The editor origins the `/api/project/*` gate accepts: the comma list in
/// `DCL_SCENE_CLI_ALLOWED_ORIGINS` plus the `--talk` origin, each once.
fn editor_origins(env: Option<&str>, talk: Option<&str>) -> Vec<String> {
    let mut origins: Vec<String> = env
        .unwrap_or_default()
        .split(',')
        .chain(talk)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut seen = std::collections::HashSet::new();
    origins.retain(|o| seen.insert(o.clone()));
    origins
}

/// Extra source lines quoted either side of the line a scene error points at.
#[derive(Clone, Copy, Default)]
pub struct SourceContext {
    pub before: u32,
    pub after: u32,
}

impl SourceContext {
    /// `--error-source-lines-context` sets both sides; the per-side flags win
    /// over it. Defaults to 0: the line that threw is the answer.
    pub fn resolve(context: Option<u32>, before: Option<u32>, after: Option<u32>) -> Self {
        SourceContext {
            before: before.or(context).unwrap_or(0),
            after: after.or(context).unwrap_or(0),
        }
    }
}

pub(crate) struct AppState {
    /// Readers take a snapshot; `set_scene_json` / `refresh_scene_json` are
    /// the only writers (the landing page's editors rewrite scene.json).
    projects: RwLock<Vec<Project>>,
    project_watch: bool,
    assistant: assistant::Assistant,
    external_debug: external_debug_state::DebugState,
    machine: String,
    reload_tx: broadcast::Sender<ReloadFrame>,
    offline_comms: bool,
    /// Set: `/about` hands out `signed-login:` and the signed endpoints mint
    /// LiveKit tokens; unset: mini-comms ws-rooms on this server. Read it
    /// through [`AppState::voice`], which a hosted scene overrides.
    /// Set once: at startup for a server named on the command line, or when
    /// the built-in one, started beside the initial build, has answered.
    livekit: OnceLock<crate::livekit::Livekit>,
    /// True while the built-in livekit-server is still starting; see
    /// [`AppState::voice_settled`].
    voice_starting: tokio::sync::watch::Receiver<bool>,
    /// `--no-host`: scene.json's `authoritativeMultiplayer` attaches nothing.
    no_host: bool,
    /// The first project's server isolate; [`host_slot`] keeps it on the flag.
    host: Mutex<Option<crate::host::Isolate>>,
    port: u16,
    data_layer: Option<DataLayerState>,
    entity_cache: Mutex<HashMap<PathBuf, (Instant, Value)>>,
    /// The first project's storage database, opened once and kept here
    /// instead of per request, so no request pays to open, migrate and drop it.
    storage_db: Mutex<Option<(PathBuf, Arc<crate::storage::Db>)>>,
    /// The sidecar's own address, set once abgen reports ready: what
    /// `/optimized-assets/*` forwards to, never what a deep link names (see
    /// `local_ab`).
    optimized_assets_url: OnceLock<String>,
    /// Whether deep links carry `local-ab=true`. A link must then NOT also
    /// name the sidecar: the explorer treats `optimized-assets-url` as an
    /// override of the realm-derived base and the two would cancel out.
    local_ab: bool,
    mcp: bool,
    mcp_port: u16,
    allow_remote_deploy: bool,
    /// `--position`: where the hosted `/play` fallback lands, as the printed
    /// join links do, instead of scene.json's base parcel.
    position: Option<(i64, i64)>,
    /// Publish stops before signing or uploading. Only the test suite sets
    /// it: every gate test works by BREAKING a gate, so without this a test
    /// would fall through to a real deploy against a real content server.
    deploy_dry_run: bool,
    explorer_params: Vec<String>,
    /// The parameters that the printed deep link and the first visit of the
    /// landing page turn on.
    link_defaults: Vec<&'static str>,
    /// Shown on the landing page.
    recent_requests: Mutex<VecDeque<(String, u16, Instant)>>,
    deploy: deploy_page::DeployState,
    /// The run's capability token: every mutating route wants it as
    /// `Authorization: Bearer` on top of the loopback and origin gates, so
    /// an origin on the allow-list is not enough to write. A page gets it
    /// from the landing page's `<meta>` (served to top-level navigations
    /// on a loopback Host only, see [`page_token`]) or from the `#token=`
    /// fragment of a URL `start` printed; [`bearer_ok`] checks it.
    token: String,
    /// `--talk`'s player URL without the token; `/talk` redirects a
    /// top-level navigation to it with the `#token=` fragment, so the
    /// browser opener's argv never carries the token.
    talk_url: OnceLock<String>,
}

impl AppState {
    /// Everything off; each mode sets the fields it needs.
    fn new(projects: Vec<Project>, port: u16, reload_tx: broadcast::Sender<ReloadFrame>) -> Self {
        AppState {
            projects: RwLock::new(projects),
            project_watch: false,
            assistant: assistant::Assistant::default(),
            external_debug: external_debug_state::DebugState::default(),
            machine: machine_id(),
            reload_tx,
            offline_comms: false,
            livekit: OnceLock::new(),
            voice_starting: tokio::sync::watch::channel(false).1,
            no_host: false,
            host: Mutex::new(None),
            port,
            data_layer: None,
            entity_cache: Mutex::new(HashMap::new()),
            storage_db: Mutex::new(None),
            optimized_assets_url: OnceLock::new(),
            local_ab: false,
            mcp: false,
            mcp_port: joinblock::DEFAULT_EXPLORER_MCP_PORT,
            allow_remote_deploy: false,
            position: None,
            deploy_dry_run: false,
            explorer_params: Vec::new(),
            link_defaults: landing::DEFAULT_ON.to_vec(),
            recent_requests: Mutex::new(VecDeque::new()),
            deploy: deploy_page::DeployState::default(),
            token: mint_token(),
            talk_url: OnceLock::new(),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Vec<Project>> {
        self.projects.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// A snapshot, not a guard: no handler holds the lock across an await.
    fn projects(&self) -> Vec<Project> {
        self.read().clone()
    }

    fn first_project(&self) -> Option<Project> {
        self.read().first().cloned()
    }

    fn set_scene_json(&self, root: &Path, scene_json: Value) {
        let mut projects = self
            .projects
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(project) = projects.iter_mut().find(|p| p.root == root) {
            project.scene_json = scene_json;
        }
    }

    /// Re-read scene.json off disk on every watch batch, keeping the last good
    /// copy through a mid-edit syntax error.
    fn refresh_scene_json(&self, root: &Path) {
        if let Ok(bytes) = std::fs::read(root.join("scene.json"))
            && let Ok(scene_json) = serde_json::from_slice(&bytes)
        {
            self.set_scene_json(root, scene_json);
        }
    }
}

async fn scene_route(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    landing::scene_page(&st, &headers, peer.ip().is_loopback())
}

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use serde_json::json;

    /// The per-test differences are field mutations at the call site.
    pub(crate) fn state(projects: Vec<Project>) -> AppState {
        let mut st = AppState::new(projects, 0, broadcast::channel(4).0);
        st.machine = "test-machine".to_string();
        st.offline_comms = true;
        st.local_ab = true;
        st.mcp = true;
        st.deploy_dry_run = true;
        st
    }

    pub(crate) struct Tmp(pub(crate) PathBuf);

    impl Tmp {
        pub(crate) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "dcl-scene-cli-start-{tag}-{}-{:x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A one-file scene at `<dir>/<name>` whose `bin/index.js` holds `body`.
    pub(crate) fn scene(dir: &Path, name: &str, parcels: &[&str], body: &str) -> Project {
        let scene_json = json!({
            "main": "bin/index.js",
            "runtimeVersion": "7",
            "scene": { "parcels": parcels, "base": parcels[0] }
        });
        project_with(dir, name, scene_json, body)
    }

    pub(crate) fn project_with(dir: &Path, name: &str, scene_json: Value, body: &str) -> Project {
        let root = dir.join(name);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/index.js"), body).unwrap();
        std::fs::write(root.join("scene.json"), scene_json.to_string()).unwrap();
        Project {
            root: root.canonicalize().unwrap(),
            scene_json,
        }
    }

    pub(crate) async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Refuses a mutating request whose peer is not this machine: a loopback
/// socket that is a tunnel replay counts as remote (the agent stamps
/// [`crate::tunnel::FORWARDED_HEADER`]).
fn remote_peer(allow_remote: bool, peer: SocketAddr, headers: &HeaderMap) -> bool {
    !allow_remote
        && (headers.contains_key(crate::tunnel::FORWARDED_HEADER) || !peer.ip().is_loopback())
}

/// The same-origin half of the write gate, shared by every mutating route:
/// `Sec-Fetch-Site` when the browser sends it, then `Origin` parsed and
/// compared with `Host` as a whole — scheme, host and port — so
/// `http://evil127.0.0.1:8000` is not `127.0.0.1:8000`.
fn cross_origin_refusal(headers: &HeaderMap) -> Option<&'static str> {
    const WHY: &str = "this request did not come from the preview's own page";
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && site != "same-origin"
        && site != "none"
    {
        return Some(WHY);
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !same_origin(origin, forwarded_proto(headers), host) {
            return Some(WHY);
        }
    }
    None
}

/// The gate's answer to DNS rebinding, for a route that admits only this
/// machine: after a rebind `Origin` and `Host` both name the attacker's
/// domain and agree, and the peer is still loopback, so the `Origin` host
/// itself has to be a loopback one. Not applied when remote peers are let
/// in on purpose (`--allow-remote-deploy`): their origin is a LAN address.
fn rebound_origin_refusal(headers: &HeaderMap) -> Option<&'static str> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())?;
    let host = url::Url::parse(origin.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))?;
    (!loopback_host(&host)).then_some("this request did not come from the preview's own page")
}

/// Whether an `Origin` header names exactly the origin of a request that
/// arrived over `proto` for `host` (a `Host` header, port optional; the
/// scheme's default port when absent on either side).
fn same_origin(origin: &str, proto: &str, host: &str) -> bool {
    let Ok(origin) = url::Url::parse(origin.trim()) else {
        return false;
    };
    let Ok(local) = url::Url::parse(&format!("{proto}://{host}")) else {
        return false;
    };
    origin.scheme() == proto
        && origin.host().is_some()
        && origin.host() == local.host()
        && origin.port_or_known_default() == local.port_or_known_default()
}

/// 32 random bytes, base64url: a bearer that rides a URL fragment unencoded.
fn mint_token() -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// Whether `Authorization: Bearer <token>` carries this run's token.
fn bearer_ok(st: &AppState, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        })
        .is_some_and(|given| constant_time_eq(given, &st.token))
}

/// The token gate on a websocket upgrade, where a browser cannot set a
/// header: the `Authorization` header when there is one, else `?token=` on
/// the upgrade URL (loopback only, never the URL a person is handed).
fn bearer_or_query_ok(st: &AppState, headers: &HeaderMap, query: Option<&str>) -> bool {
    bearer_ok(st, headers)
        || query.is_some_and(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .any(|(k, v)| k == "token" && constant_time_eq(&v, &st.token))
        })
}

fn constant_time_eq(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

pub(super) const TOKEN_HINT: &str = "this write needs the preview's capability token: open the preview from a link dcl-scene-cli start printed (it carries #token=) or send Authorization: Bearer <token>";

fn unauthorized() -> Response {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        format!("{TOKEN_HINT}\n"),
    )
        .into_response()
}

/// The token, for a page that may carry it: a top-level navigation
/// (`Sec-Fetch-Mode: navigate` and `Sec-Fetch-Dest: document`, both
/// present) with no `Origin` to a loopback `Host`, straight from the socket
/// rather than through the tunnel or a proxy; or a request that already
/// proves it holds the token as the bearer (a page re-fetching itself to
/// redraw). Nothing else: `/` is CORS-readable, so a same-origin fetch
/// would hand it to a scene in the embedded player, a header-less request
/// to a scene in the native client, and a framed navigation
/// (`Sec-Fetch-Dest: iframe`) to whoever framed the page. A client without
/// Fetch Metadata gets the page without it.
fn page_token<'a>(st: &'a AppState, headers: &HeaderMap) -> Option<&'a str> {
    if bearer_ok(st, headers) {
        return Some(st.token.as_str());
    }
    if headers.contains_key(header::ORIGIN)
        || headers.contains_key(crate::tunnel::FORWARDED_HEADER)
        || headers.contains_key("x-forwarded-for")
        || headers.contains_key("x-forwarded-host")
    {
        return None;
    }
    let fetch_metadata = |name: &str, expected: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case(expected))
    };
    if !fetch_metadata("sec-fetch-mode", "navigate")
        || !fetch_metadata("sec-fetch-dest", "document")
    {
        return None;
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok())?;
    loopback_host(host).then_some(st.token.as_str())
}

/// Whether a browser sent this request: `Sec-Fetch-Site` rides every
/// request a current browser makes, and `Origin` every one that is not a
/// same-origin GET. A request with neither is a local process — the host
/// isolate, the `storage` CLI, curl — which the loopback gate already
/// trusts; one with either is a page, which must hold the token to write
/// even from this origin, since the embedded player makes a scene
/// same-origin with the editing routes.
fn browser_marked(headers: &HeaderMap) -> bool {
    headers.contains_key("sec-fetch-site") || headers.contains_key(header::ORIGIN)
}

/// Where a page, an editor or a script reads what is wrong with the running
/// preview: the warnings `start` printed that still hold.
pub(crate) const PROBLEMS_PATH: &str = "/api/problems";

/// `GET /api/problems`: the open build, type-check, scene and preview
/// problems, each with the steps the terminal printed. `revision` moves when
/// the list does, so a poller redraws only on a change.
async fn problems() -> Response {
    let (revision, problems) = ux::problems();
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(serde_json::json!({ "revision": revision, "problems": problems })),
    )
        .into_response()
}

/// The URL `--talk` hands the browser opener: `/talk` on this preview,
/// which redirects a top-level navigation to the player with the token in
/// the fragment. The opener's argv — readable by every local user in
/// `ps` for as long as xdg-open and its helpers live — never carries it.
fn talk_opener_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/talk")
}

/// `GET /talk`: the `--talk` player URL, with `#token=` for a request
/// [`page_token`] would serve the token to, bare otherwise.
async fn talk_redirect(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(url) = st.talk_url.get() else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            "this preview was not started with --talk\n",
        )
            .into_response();
    };
    let target = match page_token(&st, &headers) {
        Some(token) => with_token_fragment(url, token),
        None => url.clone(),
    };
    axum::response::Redirect::to(&target).into_response()
}

/// `localhost`, `127.0.0.0/8` or `[::1]`, with or without a port.
fn loopback_host(host: &str) -> bool {
    let Ok(url) = url::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// `#token=<token>` on a URL, after any fragment it already has: the HUD
/// and the Creator Hub read the fragment as query pairs, so `mcptoken` and
/// this coexist. A fragment, never a query: it stays out of request lines,
/// logs and referrers.
fn with_token_fragment(url: &str, token: &str) -> String {
    match url.split_once('#') {
        Some((_, fragment)) if !fragment.is_empty() => format!("{url}&token={token}"),
        Some((base, _)) => format!("{base}#token={token}"),
        None => format!("{url}#token={token}"),
    }
}

/// Bounds what is held; `RECENT_REQUESTS_SHOWN` bounds what is drawn. A few
/// hundred lines covers a whole scene load.
const RECENT_REQUESTS_CAP: usize = 200;

/// A path is attacker-chosen (any LAN or tunnel peer can log one just by
/// asking), retained `RECENT_REQUESTS_CAP` times over and re-rendered into
/// every page, so it is cut on the way in. A real path is a scene file.
const MAX_LOGGED_PATH: usize = 120;

const ENTITY_CACHE_TTL: Duration = Duration::from_millis(500);

fn lock_cache(st: &AppState) -> MutexGuard<'_, HashMap<PathBuf, (Instant, Value)>> {
    st.entity_cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The deep-link parameters a run turns on. With neither flag, both
/// parameters; with a flag, only the parameters of the given flags, as
/// upstream does.
fn link_defaults(multi_instance: bool, skip_auth_screen: bool) -> Vec<&'static str> {
    if !multi_instance && !skip_auth_screen {
        return landing::DEFAULT_ON.to_vec();
    }
    [multi_instance, skip_auth_screen]
        .into_iter()
        .zip(landing::DEFAULT_ON)
        .filter_map(|(given, key)| given.then_some(key))
        .collect()
}

pub async fn start(opts: StartOptions) -> Result<()> {
    joinblock::set_dclenv(&opts.dclenv);
    let trunk_url = opts
        .tunnel
        .as_deref()
        .map(crate::tunnel::normalize_trunk_url)
        .transpose()?;
    let public_url = opts
        .public_url
        .as_deref()
        .filter(|raw| !raw.trim().is_empty())
        .map(PublicUrl::parse)
        .transpose()?;
    if let Some(public) = &public_url {
        let _ = PUBLIC_URL.set(public.clone());
    }
    let workspace = Workspace::load(&opts.dir)?;
    let first = workspace.projects[0].clone();
    crate::deploy::forget_remembered_target(&first.root);
    let (port, listener) =
        bind_preview_port("start", Ipv4Addr::UNSPECIFIED.into(), opts.port).await?;

    // The embedded player's page names the site its HUD reads services from;
    // set once, before the first /play/ request splices it in.
    if let Err(refused) = play_embed::configure(play_embed::PlayConfig {
        site: play_site(
            opts.talk.as_ref().and_then(|t| t.player.as_deref()),
            std::env::var("DCL_SCENE_CLI_WEB_EXPLORER").ok().as_deref(),
        ),
    }) {
        eprintln!(
            "play: the embedded player was configured before this start; keeping its site, not {}",
            refused.site
        );
    }

    // --talk: the explorer's origin passes the /api/project gate for this run,
    // registered before the server takes its first request.
    let talk_link = match &opts.talk {
        Some(talk) => {
            let player = talk_player(
                talk.player.as_deref(),
                play_embed::local_player(port).as_deref(),
                std::env::var("DCL_SCENE_CLI_WEB_EXPLORER").ok().as_deref(),
            );
            let realm = format!("http://127.0.0.1:{port}");
            let position = opts
                .position
                .unwrap_or_else(|| joinblock::base_coords(&first.scene_json));
            let url = talk_url(&player, &realm, position)
                .context("--talk needs a complete http:// or https:// web explorer URL (--player or DCL_SCENE_CLI_WEB_EXPLORER)")?;
            let origin = origin_of_url(&player).expect("a URL that joined is an http(s) URL");
            let _ = TALK_ORIGIN.set(origin);
            assistant_provider::note_at_start();
            Some((url, talk.open_browser))
        }
        None => None,
    };

    // Voice by default: a livekit-server of this preview's own, unless comms
    // are off, on a server elsewhere, or the preview is reached through a
    // tunnel, which forwards the preview port alone and never media ports.
    // A scene with a server stays on the ws-room too: see `host_slot`.
    let hosts_scene = !opts.no_host && crate::entrypoint::authoritative_multiplayer(&first);
    let mut embed_livekit = false;
    if opts.embedded_livekit && opts.livekit.is_none() && !opts.offline_comms {
        crate::livekit::validate_room(&opts.livekit_room)?;
        if hosts_scene {
            host_slot::note_voice_off();
        } else if trunk_url.is_some() {
            ux::note(
                "voice off over a tunnel: the built-in livekit-server is reachable from this machine and its LAN only; \
                 pass --livekit-url with a LiveKit server the tunnel's peers can reach",
            );
        } else if public_url.as_ref().is_some_and(|public| public.https) {
            ux::note(
                "voice off behind an https --public-url: a page served over https cannot open the built-in livekit-server's ws:// port; \
                 pass --livekit-url with a wss:// LiveKit server to keep voice",
            );
        } else {
            embed_livekit = true;
        }
    }

    let data_layer = if opts.data_layer {
        let public_dir = data_layer::locate_inspector_public(&first.root)?;
        if public_dir.is_none() {
            tracing::info!(
                "no @dcl/inspector UI installed \u{2014} serving the data layer on /data-layer only \
                 (npm install --save-dev @dcl/inspector to get /inspector/ too)"
            );
        }
        let host_rx = data_layer::spawn(&first.root).await?;
        Some(DataLayerState {
            host_rx,
            public_dir,
        })
    } else {
        None
    };

    let mut state = AppState::new(workspace.projects.clone(), port, broadcast::channel(32).0);
    state.project_watch = !opts.no_watch;
    state.offline_comms = opts.offline_comms;
    if let Some(named) = opts.livekit.clone() {
        let _ = state.livekit.set(named);
    }
    let (voice_ready, voice_starting) = tokio::sync::watch::channel(embed_livekit);
    state.voice_starting = voice_starting;
    state.no_host = opts.no_host;
    state.data_layer = data_layer;
    state.local_ab = opts.local_ab;
    state.mcp = opts.mcp;
    state.mcp_port = opts.mcp_port;
    state.allow_remote_deploy = opts.allow_remote_deploy;
    state.position = opts.position;
    state.explorer_params = opts.explorer_params.clone();
    state.link_defaults = link_defaults(opts.multi_instance, opts.skip_auth_screen);
    let state = Arc::new(state);
    let talk_link = talk_link.map(|(url, open)| {
        let _ = state.talk_url.set(url.clone());
        (with_token_fragment(&url, &state.token), open)
    });
    match scene_log_port(opts.mcp, opts.mcp_port, port) {
        Some(mcp_port) => {
            scene_logs::spawn(mcp_port, workspace.projects.clone(), opts.source_context)
        }
        None if opts.mcp => ux::report_watch(&mcp_port_clash(port).into()),
        None => {}
    }

    let comms_state = Arc::new(crate::comms::CommsState::default());

    // The built-in livekit-server starts beside the build and the preview
    // does not wait for it: pages, content and reloads are served at once,
    // and only what names the comms transport holds its answer until the
    // server has answered or been given up on (`AppState::voice_settled`).
    // The task's handle keeps the server's process for the session.
    let voice_note = |st: &AppState, server: String| {
        let Some(lk) = st.voice() else { return };
        let rooms = st
            .projects()
            .iter()
            .map(|project| lk.scene_room(&scene_id_for(project, &st.machine)))
            .collect::<Vec<_>>()
            .join(", ");
        ux::note_arrow(format!("Voice: comms on {server}, {rooms}"));
    };
    let livekit_server = embed_livekit.then(|| {
        let state = state.clone();
        let room = opts.livekit_room.clone();
        tokio::spawn(async move {
            let running = crate::livekit_server::spawn(crate::livekit_server::DEFAULT_PORT).await;
            if let Some(running) = &running {
                match crate::livekit::Livekit::embedded(
                    running.port,
                    crate::livekit_server::API_KEY,
                    running.api_secret(),
                    &room,
                ) {
                    Ok(lk) => {
                        let _ = state.livekit.set(lk);
                        voice_note(&state, format!("0.0.0.0:{}", running.port));
                    }
                    Err(e) => ux::report_watch(&e.context("voice off")),
                }
            }
            let _ = voice_ready.send(false);
            running
        })
    });

    // Opened beside the build, which leaves the database alone: creating it
    // and its schema costs as much as bundling a small scene.
    let storage_target = {
        let root = first.root.clone();
        tokio::task::spawn_blocking(move || crate::storage::open(&root).and_then(|db| db.target()))
    };

    let mut steps = if workspace.is_multi() {
        prepare_members(&opts, &workspace, &state).await?
    } else {
        prepare_single(&opts, first.clone(), &state).await?
    };

    let app = build_router(state.clone(), comms_state);

    if !embed_livekit && let Some(lk) = state.voice() {
        voice_note(&state, lk.describe_url());
    }

    // where Storage/EnvVar reads and writes go, before the host that uses them
    match storage_target
        .await
        .map_err(anyhow::Error::from)
        .and_then(|target| target)
    {
        Ok(target) => ux::note_arrow(storage_page::start_line(
            &target,
            storage_page::signer(&state).as_ref(),
            &format!("http://127.0.0.1:{port}"),
        )),
        Err(e) => ux::report_watch(&e.context("reading the storage target")),
    }

    state.follow_host(None);

    let mut sidecar = if opts.ab_sidecar {
        crate::asset_bundles::spawn_sidecar(port, &first.root)
    } else {
        None
    };
    let banner_state = state.clone();
    let scene_count = workspace.projects.len();
    let is_multi = workspace.is_multi();
    let scene_json = first.scene_json.clone();
    let mobile = opts.mobile;
    let local_ab = opts.local_ab;
    let tunnel_token = opts.tunnel_token.clone();
    let position = opts
        .position
        .unwrap_or_else(|| joinblock::base_coords(&scene_json));
    let realm_override = opts.realm.clone();
    let (launch_desktop, launch_web) = (opts.launch_desktop, opts.launch_web);
    tokio::spawn(async move {
        let mut optimized_assets_url = None;
        if let Some(s) = sidecar.as_mut()
            && s.wait_ready().await
        {
            ux::note_arrow(format!(
                "Selected abgen backend: {} at {}",
                s.backend_label(),
                s.url
            ));
            let _ = banner_state.optimized_assets_url.set(s.url.clone());
            optimized_assets_url = Some(s.url.clone());
        }
        let ifaces = netinfo::enumerate();
        let unreachable = probe_unreachable(&ifaces, port).await;
        let mut params = banner_state.explorer_params.clone();
        params.extend(
            banner_state
                .link_defaults
                .iter()
                .map(|flag| format!("--{flag}=true")),
        );
        let block = JoinBlock {
            title: joinblock::scene_title(&scene_json),
            position,
            port,
            ifaces,
            web_explorer: joinblock::web_explorer_base(),
            qr: if mobile && !ux::ci() {
                QrMode::Print
            } else {
                QrMode::Hint
            },
            unreachable,
            tunnel_hint: trunk_url.is_none(),
            editor: banner_state.data_layer.is_some(),
            optimized_assets_url: banner_ab_url(local_ab, optimized_assets_url),
            deep_link_extra: joinblock::deep_link_extra(
                banner_state.local_ab,
                banner_state.mcp,
                banner_state.mcp.then_some(banner_state.mcp_port),
                &params,
            ),
            realm_override,
            native_hud: true,
            native_bin: joinblock::detect_native_bin(),
        };
        if is_multi {
            ux::note(format!(
                "workspace preview: {scene_count} scenes served in one realm"
            ));
        }
        steps.done(block.heading());
        if ux::verbose() {
            ux::say(block.body());
        } else {
            ux::say(block.compact_body());
        }
        for line in play_lines(port, netinfo::share_ip(&block.ifaces).is_some()) {
            ux::note_arrow(line);
        }
        let mut launches = Vec::new();
        if let Some((url, open_browser)) = talk_link {
            // Under --json this text is on stderr, which a parent program
            // can copy to a log: the opener URL has no token in it.
            let shown = match ux::events() {
                true => talk_opener_url(port),
                false => url,
            };
            ux::note_arrow(format!("Talk: {shown}"));
            if open_browser {
                launches.push(open_client("talk", "the Talk page", &talk_opener_url(port)));
            }
        }
        if launch_desktop {
            let link = block.local_desktop_link();
            ux::note_arrow(format!("Desktop client: {link}"));
            launches.push(open_client("desktop", "the desktop client", &link));
        }
        if launch_web {
            let url = block.local_web_url();
            ux::note_arrow(format!("Web explorer: {url}"));
            launches.push(open_client("web", "the web explorer", &url));
        }
        let started = |client| {
            let found = launches
                .iter()
                .any(|(c, opener)| *c == client && opener.is_ok());
            (found, client)
        };
        ux::event(&ready_event(
            &block,
            &banner_state,
            [started("talk"), started("desktop"), started("web")],
        ));
        report_launches(launches);
        if let Some(trunk_url) = trunk_url {
            let events = crate::tunnel::spawn(crate::tunnel::AgentConfig {
                trunk_url,
                token: tunnel_token,
                local_port: port,
            });
            spawn_tunnel_printer(events, block.clone());
        }
        if let Some(public) = &public_url {
            ux::say(block.public_section(
                "Join at the PUBLIC address \u{2014} --public-url",
                &public.origin(),
            ));
        }
        ux::set_session_note(session_note(port));
    });
    let result = tokio::select! {
        r = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        ) => r.context("serving"),
        _ = shutdown_signal() => Ok(()),
    };
    crate::asset_bundles::kill_sidecar_group();
    crate::livekit_server::kill_group();
    if let Some(task) = livekit_server {
        task.abort();
    }
    state.release_host();
    result
}

/// The line a `start --json` run prints once the preview serves and its
/// links are known. `token` is the run's bearer, which `/data-layer` asks
/// for: the process that reads this stdout can open the editor's socket.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadyEvent {
    event: &'static str,
    port: u16,
    url: String,
    /// `None` without `--data-layer`, or with no editor UI installed.
    editor_url: Option<String>,
    /// `None` when the web explorer address does not parse.
    web_url: Option<String>,
    deep_link: String,
    /// `None` without `--data-layer`.
    data_layer_url: Option<String>,
    token: String,
    /// The clients whose opener process started; empty when none did.
    clients_opened: Vec<&'static str>,
}

/// A client this run handed to the system opener, and the start of the
/// opener.
type Launch = (&'static str, std::io::Result<std::process::Child>);

/// Starts the system opener on `url` for `client`. A start that fails prints
/// the note of the text form, with `name` in it.
fn open_client(client: &'static str, name: &str, url: &str) -> Launch {
    let opener = crate::linker::spawn_opener(url);
    if opener.is_err() {
        ux::note(format!(
            "could not open {name} automatically \u{2014} open the URL above manually"
        ));
    }
    (client, opener)
}

/// `start --json`: the launch of `client` failed. `message` has no URL in it.
#[derive(serde::Serialize)]
struct ClientFailedEvent {
    event: &'static str,
    client: &'static str,
    message: String,
}

fn client_failed(client: &'static str, message: String) {
    ux::event(&ClientFailedEvent {
        event: "client-failed",
        client,
        message,
    });
}

/// After the `ready` line. Waits for each opener that started, in the text
/// form too, so no ended opener stays as a process entry. `start --json`
/// gets one `client-failed` line for each opener that did not start, and one
/// for each opener that ends with an exit status that is not 0, when it
/// ends. An opener that a signal ended gives no line: a signal is not a
/// result of the launch. The wait has no limit: an opener can stay until the
/// application it started exits, and its status is then the status of that
/// application. The status of the Windows opener is not read, because it is
/// not a result of the launch.
fn report_launches(launches: Vec<Launch>) {
    let opener = crate::linker::OPENER;
    for (client, launch) in launches {
        match launch {
            Err(e) => client_failed(client, format!("{opener} did not start: {e}")),
            Ok(_) if cfg!(windows) => {}
            Ok(mut child) => {
                std::thread::spawn(move || {
                    if let Ok(status) = child.wait()
                        && status.code().is_some_and(|code| code != 0)
                    {
                        client_failed(client, format!("{opener} ended with {status}"));
                    }
                });
            }
        }
    }
}

fn ready_event(block: &JoinBlock, st: &AppState, opened: [(bool, &'static str); 3]) -> ReadyEvent {
    let port = block.port;
    let url = format!("http://127.0.0.1:{port}");
    ReadyEvent {
        event: "ready",
        port,
        editor_url: st
            .data_layer
            .as_ref()
            .and_then(|dl| dl.public_dir.as_ref())
            .map(|_| format!("{url}/inspector/")),
        web_url: Some(block.local_web_url()).filter(|url| !url.is_empty()),
        deep_link: block.local_desktop_link(),
        data_layer_url: st
            .data_layer
            .as_ref()
            .map(|_| format!("ws://127.0.0.1:{port}/data-layer")),
        token: st.token.clone(),
        clients_opened: opened
            .into_iter()
            .filter_map(|(on, client)| on.then_some(client))
            .collect(),
        url,
    }
}

/// A CLI publish waiting on its wallet signature, served as the normal preview
/// server: the printed URL is /deploy, which carries the signing panel, and the
/// scene about to go up can be walked from the same origin meanwhile. Resolves
/// after the browser publish completes and the user stops the preview.
pub(crate) async fn serve_signing(
    dir: &Path,
    (signing, port): (crate::linker::SigningBind, Option<u16>),
    open_browser: bool,
    timeout: Duration,
    programmatic: bool,
    signer: Arc<crate::linker::LinkerState>,
    rx: tokio::sync::oneshot::Receiver<Result<Published>>,
) -> Result<Published> {
    let workspace = Workspace::load(dir)?;
    let (port, listener) = bind_preview_port("deploy", signing.addr, port).await?;
    serve_signing_on(
        workspace,
        SigningListener {
            port,
            listener,
            signing,
        },
        open_browser,
        timeout,
        programmatic,
        signer,
        rx,
    )
    .await
}

/// The one line a programmatic run reads to drive the deploy: where the
/// signing page and the API are, the token that cancels, the id to sign.
fn programmatic_ready(
    origin: &str,
    port: u16,
    url: &str,
    signer: &Arc<crate::linker::LinkerState>,
) -> String {
    let (entity_id, delete_payload) = match signer.minted() {
        Ok((id, delete)) => (Some(id), delete),
        Err(_) => (None, None),
    };
    serde_json::json!({
        "event": "ready",
        "port": port,
        "url": url,
        "api": format!("{origin}{}", deploy_api::RESOURCE_PATH),
        "deployToken": signer.deploy_token(),
        "entityId": entity_id,
        "deleteScenesFromWorldPayload": delete_payload,
    })
    .to_string()
}

/// The origin the signing URLs name. A listener on IPv4 loopback or on every
/// address answers `localhost`; one on another address is named by it.
pub(crate) fn signing_origin(bound: SocketAddr) -> String {
    if bound.ip() == Ipv4Addr::LOCALHOST || bound.ip().is_unspecified() {
        return format!("http://localhost:{}", bound.port());
    }
    format!("http://{bound}")
}

/// The listener of the signing server, bound to `port`, and the
/// [`crate::linker::SigningBind`] of the run: whether its signing routes
/// answer a peer that is not this machine, and the note on another device.
struct SigningListener {
    port: u16,
    listener: tokio::net::TcpListener,
    signing: crate::linker::SigningBind,
}

/// [`serve_signing`] on a listener already bound.
async fn serve_signing_on(
    workspace: Workspace,
    SigningListener {
        port,
        listener,
        signing,
    }: SigningListener,
    open_browser: bool,
    timeout: Duration,
    programmatic: bool,
    signer: Arc<crate::linker::LinkerState>,
    rx: tokio::sync::oneshot::Receiver<Result<Published>>,
) -> Result<Published> {
    let mut state = AppState::new(workspace.projects.clone(), port, broadcast::channel(32).0);
    state.allow_remote_deploy = signing.remote;
    let state = Arc::new(state);
    deploy_page::adopt_cli_signing(&state, signer.clone());
    let app = build_router(state.clone(), Arc::new(crate::comms::CommsState::default()));
    let origin = signing_origin(
        listener
            .local_addr()
            .context("reading the address of the signing server")?,
    );
    let url = format!(
        "{origin}/deploy{}",
        crate::linker::linker_fragment(signer.deploy_token())
    );
    if programmatic {
        println!("{}", programmatic_ready(&origin, port, &url, &signer));
    } else {
        // Under --json stdout is the result document alone: the prompt
        // goes to stderr, where the notes below already are not (quiet).
        let prompt = |line: &str| match ux::json() {
            true => eprintln!("{line}"),
            false => println!("{line}"),
        };
        prompt("");
        prompt("Sign the deployment with your wallet in a browser:");
        prompt(&format!("  {url}"));
        ux::note("the same server previews the scene at / while you decide");
        if let Some(note) = signing.other_device_note() {
            ux::note(note);
        }
        if open_browser {
            crate::linker::spawn_browser(&url);
        } else {
            ux::note("browser auto-open disabled \u{2014} open the URL manually");
        }
    }
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    let serve = std::future::IntoFuture::into_future(serve);
    tokio::pin!(serve);
    tokio::select! {
        r = &mut serve => {
            r.context("serving the signing page")?;
            Err(UserError::new(
                "the signing page stopped before a signature arrived",
                TrySteps::one("re-run dcl-scene-cli deploy"),
            )
            .into())
        }
        outcome = crate::linker::await_outcome(&signer, rx, timeout, &url) => {
            deploy_page::finish_cli_signing(&state, &outcome);
            // A program drives the run: it ends with the deployment, after
            // a beat for a follower of /api/deploy to read the final state.
            // So does a --json run, whose document is the result, and an
            // unanswered CLI request must finish on its deadline. Once a
            // wallet submits, keep the preview available to inspect the
            // deployment result (including an upload failure) and retry.
            if programmatic
                || ux::json()
                || (outcome.is_err() && signer.signer_address().is_none())
            {
                tokio::time::sleep(signer.settle_delay()).await;
                return outcome;
            }
            match &outcome {
                Ok(published) => println!("{}", published.message),
                Err(error) => eprintln!("{error}"),
            }
            ux::note(format!("preview remains available at {url} — press Ctrl+C to stop"));
            tokio::select! {
                result = &mut serve => { result.context("serving the preview")?; }
                _ = shutdown_signal() => {}
            }
            outcome
        },
        _ = shutdown_signal() => Err(anyhow::anyhow!("publishing cancelled")),
    }
}

/// The banner's lines for the embedded web explorer: where it is and which
/// engine build it carries, and when a LAN address was printed above, that it
/// works only from this machine — WebGPU and the microphone need a secure
/// context, which plain http has on 127.0.0.1 and not on a LAN address.
fn play_lines(port: u16, lan_printed: bool) -> Vec<String> {
    let Some(player) = play_embed::local_player(port) else {
        return Vec::new();
    };
    let engine = play_embed::engine_rev().unwrap_or_else(|| "unknown".to_string());
    let mut lines = vec![format!(
        "Play: {player} (embedded web explorer, engine {engine})"
    )];
    if lan_printed {
        lines.push(
            "the embedded player needs 127.0.0.1 or https (no WebGPU or microphone over plain http on a LAN address); \
             another device uses the public player or the tunnel"
                .to_string(),
        );
    }
    lines
}

/// The line the watch session re-floats: the address someone else on the
/// network can reach, not the loopback one.
fn session_note(port: u16) -> String {
    let host = netinfo::share_ip(&netinfo::enumerate())
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    format!("you are running the dcl-scene-cli at http://{host}:{port}")
}

/// Everything this server answers, reachable by a test without a scene build,
/// a watcher and a tunnel.
fn build_router(state: Arc<AppState>, comms_state: Arc<crate::comms::CommsState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/about", get(about))
        .route("/get-scene-adapter", post(get_scene_adapter))
        .route("/signed-login", post(signed_login))
        .route("/scenes", get(scenes))
        .route("/scene.json", get(scene_json))
        .route("/preview-wearables", get(preview_wearables))
        .route("/preview-wearables/{id}", get(preview_wearable))
        .route("/feature-flags/{file}", get(feature_flags))
        .route("/content/contents/{hash}", get(contents).head(contents))
        .route("/content/entities/active", post(entities_active))
        .route("/content/entities/scene", get(entities_scene))
        .route("/content/entities", post(catalyst_proxy))
        .route("/lambdas/explore/realms", get(lambdas_explore_realms))
        .route("/lambdas/contracts/servers", get(lambdas_contracts_servers))
        .route("/lambdas/{*path}", any(catalyst_proxy))
        .route("/explorer/{*path}", any(catalyst_proxy))
        .route("/world/{name}/about", get(world_about))
        .route("/optimized-assets/{*path}", any(proxy::optimized_assets))
        .route(
            "/world-content/{name}/contents/{hash}",
            get(world_content).head(world_content),
        )
        .route("/mobile-preview", get(mobile_preview))
        .route("/scene-inspector", get(external_debug::producer))
        .route("/data-layer", get(data_layer_ws))
        .route("/inspector", get(inspector_redirect))
        .route("/inspector/", get(inspector_index))
        .route("/inspector/{*path}", get(inspector_asset))
        .route("/talk", get(talk_redirect))
        .route(PROBLEMS_PATH, get(problems))
        .with_state(state.clone())
        .merge(crate::comms::routes(comms_state))
        .layer(tower_http::cors::CorsLayer::permissive())
        .merge(
            Router::new()
                .route("/deploy", get(deploy_page::route).post(deploy_page::start))
                .route("/target", get(deploy_page::target_route))
                .route("/target/address", post(deploy_page::target_address))
                .route("/target/connect", post(deploy_page::target_connect))
                .route("/target/point", post(deploy_page::target_point))
                .route("/target/entrance", post(deploy_page::target_entrance))
                .route(
                    "/target/scene/remove",
                    post(deploy_page::target_remove_scene),
                )
                .route("/target/base", post(deploy_page::target_base))
                .route("/deploy/preflight", post(deploy_page::preflight))
                .route("/scene", get(scene_route))
                .route("/deploy/sign", post(deploy_page::sign_submit))
                .route("/deploy/progress", get(deploy_page::sign_progress))
                .route(
                    deploy_api::RESOURCE_PATH,
                    get(deploy_api::get)
                        .post(deploy_api::post)
                        .delete(deploy_api::cancel),
                )
                .route("/api/info", get(deploy_api::info))
                .route("/api/files", get(deploy_api::files))
                .route("/api/catalyst-pointers", get(deploy_api::catalyst_pointers))
                .route("/scene-json", post(edit::scene_json))
                .route("/scene-thumbnail", post(edit::thumbnail))
                .merge(storage_page::routes())
                .with_state(state.clone()),
        )
        .merge(project_bridge::routes(state.clone()))
        .merge(play_routes(state.clone()))
        .layer(middleware::from_fn_with_state(state, access_log))
        .layer(compression_layer())
}

/// The embedded web explorer when this binary carries one; otherwise `/play`
/// is still an address that plays the scene: it sends the browser to the
/// hosted web explorer, pointed at this preview.
fn play_routes(state: Arc<AppState>) -> Router {
    if play_embed::present() {
        return play_embed::routes();
    }
    Router::new()
        .route("/play", get(play_hosted))
        .route("/play/", get(play_hosted))
        .with_state(state)
}

async fn play_hosted(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let realm = preview_origin(&headers);
    let position = st
        .position
        .or_else(|| {
            st.first_project()
                .map(|p| joinblock::base_coords(&p.scene_json))
        })
        .unwrap_or((0, 0));
    if let Some(blocked) = joinblock::web_blocked(&joinblock::web_explorer_base(), &realm) {
        return (axum::http::StatusCode::CONFLICT, format!("{blocked}\n")).into_response();
    }
    let url = joinblock::web_join_url(&joinblock::web_explorer_base(), &realm, position);
    if url.is_empty() {
        return (axum::http::StatusCode::NOT_FOUND, play_embed::NOT_EMBEDDED).into_response();
    }
    axum::response::Redirect::temporary(&url).into_response()
}

/// gzip, fastest level, on text-like bodies above 1 KB: the scene's chunks
/// (`prebuilt/core.js` is 475 KB raw, 122 KB gzipped), the inspector's
/// bundles, JSON. Never media (already compressed), never `text/event-stream`
/// (buffering would hold the events back), and never a body with no
/// content-type, which is what the websocket upgrades answer with. Only gzip
/// is compiled in: brotli would add a native crate to every build for a
/// preview served over loopback or a tunnel.
fn compression_layer() -> tower_http::compression::CompressionLayer<
    tower_http::compression::predicate::And<
        tower_http::compression::predicate::SizeAbove,
        TextLike,
    >,
> {
    use tower_http::compression::predicate::{Predicate as _, SizeAbove};
    tower_http::compression::CompressionLayer::new()
        .quality(tower_http::CompressionLevel::Fastest)
        .compress_when(SizeAbove::new(1024).and(TextLike))
}

#[derive(Clone, Copy, Debug)]
struct TextLike;

impl tower_http::compression::Predicate for TextLike {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: axum::body::HttpBody,
    {
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(compressible_mime)
    }
}

fn compressible_mime(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    if mime == "text/event-stream" {
        return false;
    }
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/javascript"
                | "application/x-javascript"
                | "application/json"
                | "application/manifest+json"
                | "application/wasm"
                | "application/xml"
                | "image/svg+xml"
        )
}

/// The `optimized-assets-url` the join block advertises. Never alongside
/// `local-ab=true`: the explorer treats it as an OVERRIDE of the realm-derived
/// base (`DecentralandUrlsSource::ResolveOptimizedAssetsUrl`), so the pair
/// would silently defeat the flag.
fn banner_ab_url(local_ab: bool, sidecar_url: Option<String>) -> Option<String> {
    if local_ab { None } else { sidecar_url }
}

/// The scene-log poller POSTs `127.0.0.1:{mcp_port}` every 700ms; when that
/// port IS this server (`start --port 8123`) it would 404 against itself and
/// fill the request log, so it is skipped and reported instead.
fn scene_log_port(mcp: bool, mcp_port: u16, server_port: u16) -> Option<u16> {
    (mcp && mcp_port != server_port).then_some(mcp_port)
}

fn mcp_port_clash(port: u16) -> UserError {
    let other = port.saturating_add(1).max(1024);
    UserError::new(
        format!(
            "scene errors will not print \u{2014} --mcp-port {port} is the port this preview bound"
        ),
        TrySteps::one(format!(
            "dcl-scene-cli start --port {port} --mcp-port {other}"
        ))
        .and(format!(
            "or move the preview instead \u{2014} dcl-scene-cli start --port {other}"
        ))
        .and("or turn the reader off \u{2014} dcl-scene-cli start --no-mcp"),
    )
    .why(format!(
        "the reader would poll http://127.0.0.1:{port}/unity-explorer-mcp, which is this server: \
         it would answer its own polls 404 several times a second and fill the landing page's \
         request log with them, and the client cannot open that port while this server holds it"
    ))
}

fn spawn_tunnel_printer(
    mut events: tokio::sync::mpsc::UnboundedReceiver<crate::tunnel::AgentEvent>,
    block: JoinBlock,
) {
    tokio::spawn(async move {
        let mut announced: Option<String> = None;
        let mut warned = false;
        while let Some(event) = events.recv().await {
            match event {
                crate::tunnel::AgentEvent::Connected { public_url } => {
                    warned = false;
                    if announced.as_deref() == Some(public_url.as_str()) {
                        ux::note("tunnel reconnected \u{2014} public URL unchanged");
                    } else {
                        ux::say(block.internet_section(&public_url));
                        announced = Some(public_url);
                    }
                }
                crate::tunnel::AgentEvent::ConnectFailed { error } => {
                    if !warned {
                        warned = true;
                        ux::report_watch(
                            &UserError::new(
                                "tunnel connection failed \u{2014} retrying in background; the local and LAN links above still work",
                                TrySteps::one(
                                    "check the tunnel URL/service \u{2014} dcl-scene-cli start --tunnel help",
                                )
                                .and(
                                    "re-run with --verbose to log every retry attempt with its full cause",
                                ),
                            )
                            .why(error)
                            .into(),
                        );
                    }
                }
                crate::tunnel::AgentEvent::Disconnected { error } => {
                    ux::note(format!(
                        "tunnel disconnected ({error}) \u{2014} reconnecting"
                    ));
                }
            }
        }
    });
}

/// Built-in preview builds use the generated entry point, shaped by the
/// `--ignoreComposite` and `--customEntryPoint` a toolchain `scripts.build`
/// carries (its `--production` is not a preview's); custom scripts own
/// their output and receive DCL_SCENE_CLI_PRODUCTION=0.
fn preview_build_opts(opts: &StartOptions, dir: PathBuf) -> BuildOptions {
    crate::build_script::preview_options(&BuildOptions {
        built_in: false,
        dir,
        production: false,
        ignore_composite: opts.ignore_composite,
        custom_entry_point: false,
        skip_type_check: opts.skip_type_check,
        out_root: None,
        quiet: false,
    })
}

/// How many `steps` an initial build reports.
fn build_chunk(opts: &StartOptions) -> usize {
    if opts.skip_build { 0 } else { 3 }
}

async fn build_or_watch(
    opts: &StartOptions,
    project: Project,
    build_dir: PathBuf,
    scene: String,
    steps: &mut ux::Steps,
    state: &Arc<AppState>,
) -> Result<()> {
    let build_opts = preview_build_opts(opts, build_dir);
    if opts.no_watch {
        if !opts.skip_build {
            build::build_as(&build_opts, build::BuildMode::Preview).await?;
        }
        return Ok(());
    }
    watch_or_retry(
        project,
        build_opts,
        !opts.skip_build,
        steps,
        scene,
        state.clone(),
    )
    .await
}

async fn prepare_single(
    opts: &StartOptions,
    project: Project,
    state: &Arc<AppState>,
) -> Result<ux::Steps> {
    let total = if opts.no_watch {
        1
    } else {
        build_chunk(opts) + 2
    };
    let mut steps = ux::Steps::new(total);
    let scene = b64_hash(&project.root.display().to_string(), &state.machine);
    build_or_watch(opts, project, opts.dir.clone(), scene, &mut steps, state).await?;
    if !opts.no_watch {
        steps.done("Watching for changes");
    }
    Ok(steps)
}

async fn prepare_members(
    opts: &StartOptions,
    workspace: &Workspace,
    state: &Arc<AppState>,
) -> Result<ux::Steps> {
    for (i, project) in workspace.projects.iter().enumerate() {
        if let Some(header) = workspace.member_header(i) {
            ux::note(header);
        }
        let mut steps = ux::Steps::new(build_chunk(opts));
        let scene = scene_id_for(project, &state.machine);
        build_or_watch(
            opts,
            project.clone(),
            project.root.clone(),
            scene,
            &mut steps,
            state,
        )
        .await?;
    }
    let mut steps = ux::Steps::new(if opts.no_watch { 1 } else { 2 });
    if !opts.no_watch {
        steps.done("Watching for changes");
    }
    Ok(steps)
}

/// A failed INITIAL build must not kill `start`: the server still serves and
/// the watcher picks up the fix, like every re-build. Config errors
/// (scene.json main, tsconfig) stay fatal, pre-checked here — upstream dies on
/// those before bundling too.
async fn watch_or_retry(
    project: Project,
    build_opts: BuildOptions,
    initial_build: bool,
    steps: &mut ux::Steps,
    scene: String,
    state: Arc<AppState>,
) -> Result<()> {
    project.main_output()?;
    if crate::build_script::selected(&project, &build_opts)?.is_none() {
        project.tsconfig()?;
    }
    let fs = FsWatcher::new(&project.root)?;
    let root = project.root.clone();
    match WatchSession::create(project.clone(), &build_opts, initial_build, steps).await {
        Ok(session) => {
            tokio::spawn(run_watch(session, fs, root, scene, state));
        }
        Err(e) => {
            report_initial_failure(&e);
            tokio::spawn(retry_initial_build(
                project,
                build_opts,
                initial_build,
                fs,
                root,
                scene,
                state,
            ));
        }
    }
    Ok(())
}

/// Reports the build error the way the re-build loop does (keeping the
/// compiler diagnostic in the inner `why`), then that the session survived it.
fn report_initial_failure(e: &anyhow::Error) {
    ux::report_build(e);
    ux::note(
        "the preview server and watcher are still running \u{2014} save any file to retry the initial build",
    );
}

/// Every watch batch retries the initial build (with the session's skip-build
/// choice) until one succeeds, then hands the watcher to the normal loop.
async fn retry_initial_build(
    project: Project,
    build_opts: BuildOptions,
    initial_build: bool,
    mut fs: FsWatcher,
    root: PathBuf,
    scene: String,
    state: Arc<AppState>,
) {
    loop {
        let Some(batch) = fs.next_batch().await else {
            return;
        };
        let mut steps = ux::Steps::new(if initial_build { 3 } else { 0 });
        let started = Instant::now();
        match WatchSession::create(project.clone(), &build_opts, initial_build, &mut steps).await {
            Ok(session) => {
                crate::watch::rebuild_event(session.project(), &batch, started.elapsed());
                notify_reload(&root, &scene, &state, ReloadEvent::Scene);
                run_watch(session, fs, root, scene, state).await;
                return;
            }
            Err(e) => report_initial_failure(&e),
        }
    }
}

/// Push the change and say what it achieved. The client routes UpdateModel and
/// UpdateScene alike into TryReloadSceneAsync, so an asset save reloads the
/// whole scene; `send`'s receiver count tells "reloaded" from "nobody
/// listening".
fn notify_reload(root: &Path, scene: &str, state: &AppState, event: ReloadEvent) {
    state.refresh_scene_json(root);
    lock_cache(state).remove(root);
    state.follow_reload(root, &event);
    let mut clients = 0;
    for frame in live_reload::reload_frames(root, scene, &state.machine, &event) {
        clients = state.reload_tx.send(frame).unwrap_or(0);
    }
    match clients {
        0 => ux::note_absent(reload_note(0)),
        n => ux::note_arrow(reload_note(n)),
    }
    tracing::info!("scene update pushed to {clients} client(s)");
}

fn reload_note(clients: usize) -> String {
    match clients {
        0 => "no client connected".to_string(),
        1 => "reload issued".to_string(),
        n => format!("reload issued to {n} clients"),
    }
}

async fn run_watch(
    session: WatchSession,
    fs: FsWatcher,
    root: PathBuf,
    scene: String,
    state: Arc<AppState>,
) {
    let notify = {
        let root = root.clone();
        move |event: ReloadEvent| notify_reload(&root, &scene, &state, event)
    };
    if let Err(e) = session.run(fs, notify).await {
        tracing::error!("watch loop stopped: {e:#}");
        ux::report_watch(
            &UserError::new(
                "live reload stopped",
                TrySteps::one(
                    "restart dcl-scene-cli start to resume hot reload (the server is still serving the last build)",
                ),
            )
            .why(format!("{e:#}"))
            .into(),
        );
    }
}

async fn probe_unreachable(ifaces: &[Iface], port: u16) -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    for i in ifaces {
        if matches!(i.class, IfaceClass::Loopback | IfaceClass::LinkLocal) {
            continue;
        }
        let reachable = tokio::time::timeout(
            Duration::from_millis(400),
            tokio::net::TcpStream::connect(SocketAddr::from((i.ip, port))),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
        if !reachable {
            out.push(i.ip);
        }
    }
    out
}

/// An explicit port must bind exactly, and 0 takes any free port (the one
/// bound is the preview's port); the default scans 8000 upward and falls back
/// to an ephemeral port, so `start` never dies because 8000 is taken.
/// `command` is the subcommand that listens (`start`, `deploy`): the steps of
/// a failure name its `--port`.
async fn bind_preview_port(
    command: &str,
    host: IpAddr,
    requested: Option<u16>,
) -> Result<(u16, tokio::net::TcpListener)> {
    const DEFAULT_PORT: u16 = 8000;
    const SCAN: u16 = 20;
    let wide = |port: u16| SocketAddr::new(host, port);
    if let Some(port) = requested {
        return match tokio::net::TcpListener::bind(wide(port)).await {
            Ok(l) => Ok((l.local_addr().map_or(port, |a| a.port()), l)),
            Err(e) => Err(bind_error(command, port, wide(port), e)),
        };
    }
    for port in DEFAULT_PORT..DEFAULT_PORT + SCAN {
        match tokio::net::TcpListener::bind(wide(port)).await {
            Ok(l) => {
                if port != DEFAULT_PORT {
                    ux::note(format!("port {DEFAULT_PORT} is busy — serving on {port}"));
                }
                return Ok((port, l));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(bind_error(command, port, wide(port), e)),
        }
    }
    let listener = tokio::net::TcpListener::bind(wide(0))
        .await
        .map_err(|e| bind_error(command, 0, wide(0), e))?;
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    ux::note(format!(
        "ports {DEFAULT_PORT}\u{2013}{} are busy — serving on {port}",
        DEFAULT_PORT + SCAN - 1
    ));
    Ok((port, listener))
}

fn bind_error(command: &str, port: u16, addr: SocketAddr, e: std::io::Error) -> anyhow::Error {
    let next = port.checked_add(1).unwrap_or(8001);
    match e.kind() {
        std::io::ErrorKind::AddrInUse => UserError::new(
            format!("port {port} is already in use"),
            TrySteps::one(format!("dcl-scene-cli {command} --port {next}"))
                .and(format!("or stop the other process (lsof -i :{port})")),
        )
        .why(format!("something else is listening on {addr}"))
        .code("port-in-use")
        .caused_by(e)
        .into(),
        std::io::ErrorKind::PermissionDenied => UserError::new(
            format!("port {port} cannot be opened"),
            TrySteps::one(
                "ports below 1024 need elevated rights \u{2014} pick a higher port with --port",
            )
            .and(format!("dcl-scene-cli {command} --port 8001")),
        )
        .why(format!("binding {addr} was denied"))
        .caused_by(e)
        .into(),
        _ => anyhow::Error::from(e).context(format!("binding {addr}")),
    }
}

async fn access_log(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    let len = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let line = log_line(&method, &path);
    tracing::info!(target: "access", "{line} {status} {len}");
    record_request(&st, line, status);
    resp
}

/// The request's log line, cut to [`MAX_LOGGED_PATH`] on a char boundary.
fn log_line(method: &axum::http::Method, path: &str) -> String {
    if path.len() <= MAX_LOGGED_PATH {
        return format!("{method} {path}");
    }
    let mut cut = MAX_LOGGED_PATH;
    while !path.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{method} {}\u{2026}", &path[..cut])
}

/// A poisoned lock loses the line rather than the request.
fn record_request(st: &AppState, line: String, status: u16) {
    if let Ok(mut recent) = st.recent_requests.lock() {
        recent.push_back((line, status, Instant::now()));
        while recent.len() > RECENT_REQUESTS_CAP {
            recent.pop_front();
        }
    }
}

fn forwarded_proto(headers: &HeaderMap) -> &'static str {
    match headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        Some(p) if p.trim().eq_ignore_ascii_case("https") => "https",
        Some(_) => "http",
        None => match public_url_of(headers) {
            Some(public) if public.https => "https",
            _ => "http",
        },
    }
}

fn forwarded_prefix(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.trim().trim_end_matches('/'))
        .filter(|p| {
            p.starts_with('/') && !p.starts_with("//") && !p.contains(':') && !p.contains('\\')
        })
        .map(str::to_string)
        .or_else(|| public_url_of(headers).map(|public| public.prefix.clone()))
        .unwrap_or_default()
}

fn forwarded_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

fn authority_of(origin: &str) -> Option<String> {
    let after = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    (!authority.is_empty()).then(|| authority.to_ascii_lowercase())
}

/// What the `/api/project/*` gate accepts: `DCL_SCENE_CLI_ALLOWED_ORIGINS`
/// plus the `--talk` origin.
fn allowed_editor_origins() -> Vec<String> {
    editor_origins(
        std::env::var("DCL_SCENE_CLI_ALLOWED_ORIGINS")
            .ok()
            .as_deref(),
        TALK_ORIGIN.get().map(String::as_str),
    )
}

/// What the data-layer upgrade accepts besides same-origin: only
/// `DCL_SCENE_CLI_ALLOWED_ORIGINS`. `--talk` opens the Scene assistant's
/// `/api/project/*` and nothing wider.
fn data_layer_origins() -> Vec<String> {
    editor_origins(
        std::env::var("DCL_SCENE_CLI_ALLOWED_ORIGINS")
            .ok()
            .as_deref(),
        None,
    )
}

fn data_layer_origin_allowed(headers: &HeaderMap) -> bool {
    data_layer_origin_allowed_by(headers, &data_layer_origins())
}

fn data_layer_origin_allowed_by(headers: &HeaderMap, allowed: &[String]) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let origin = origin.trim();
    if origin.is_empty() || origin.eq_ignore_ascii_case("null") {
        return true;
    }
    let Some(origin_authority) = authority_of(origin) else {
        return false;
    };
    let request_authority = forwarded_host(headers)
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string)
        })
        .map(|h| h.to_ascii_lowercase());
    if request_authority.as_deref() == Some(origin_authority.as_str()) {
        return true;
    }
    allowed
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&origin_authority) || a.eq_ignore_ascii_case(origin))
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "storage_page_tests.rs"]
mod storage_page_tests;
