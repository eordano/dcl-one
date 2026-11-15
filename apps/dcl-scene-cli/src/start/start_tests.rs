use super::http::{build_scene_entity, entities_for, scene_id_for};
use super::testkit::{self, Tmp, body_text, project_with, state};
use super::*;
use axum::Json;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use serde_json::json;

fn member(tmp: &Tmp, name: &str, parcels: &[&str]) -> Project {
    testkit::scene(&tmp.0, name, parcels, "module.exports={}")
}

/// One `scene-a` at `0,0`, and a state serving it.
fn one_scene(tag: &str) -> (Tmp, Project, Arc<AppState>) {
    let tmp = Tmp::new(tag);
    let a = member(&tmp, "scene-a", &["0,0"]);
    let st = Arc::new(state(vec![a.clone()]));
    (tmp, a, st)
}

/// `one_scene` with comms on, so `/about` advertises a ws-room adapter.
fn online_scene(tag: &str) -> (Tmp, AppState) {
    let tmp = Tmp::new(tag);
    let mut st = state(vec![member(&tmp, "scene-a", &["0,0"])]);
    st.offline_comms = false;
    (tmp, st)
}

fn request(uri: &str, headers: &[(&str, &str)]) -> Request {
    let mut req = Request::builder().uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(axum::body::Body::empty()).unwrap()
}

const LOCAL: &[(&str, &str)] = &[("host", "127.0.0.1:8000")];

#[test]
fn a_deep_link_never_carries_both_ab_forms() {
    let sidecar = || Some("http://127.0.0.1:5147".to_string());
    assert_eq!(banner_ab_url(true, sidecar()), None);
    assert_eq!(banner_ab_url(true, None), None);
    assert_eq!(banner_ab_url(false, None), None);
    assert_eq!(banner_ab_url(false, sidecar()), sidecar());

    assert_eq!(
        joinblock::deep_link_extra(true, false, None, &[]),
        "&local-ab=true"
    );
    assert_eq!(joinblock::deep_link_extra(false, false, None, &[]), "");
}

#[test]
fn entities_union_serves_every_member() {
    let tmp = Tmp::new("union");
    let a = member(&tmp, "scene-a", &["0,0"]);
    let b = member(&tmp, "scene-b", &["1,0", "1,1"]);
    let st = state(vec![a.clone(), b.clone()]);
    let all = entities_for(&st, &[]);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["id"], json!(scene_id_for(&a, "test-machine")));
    assert_eq!(all[1]["id"], json!(scene_id_for(&b, "test-machine")));
    assert_eq!(all[1]["pointers"], json!(["1,0", "1,1"]));
}

#[test]
fn entities_filter_by_pointer_returns_only_matches() {
    let tmp = Tmp::new("filter");
    let a = member(&tmp, "scene-a", &["0,0"]);
    let b = member(&tmp, "scene-b", &["1,0"]);
    let st = state(vec![a, b.clone()]);
    let hit = entities_for(&st, &["1,0".to_string()]);
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0]["id"], json!(scene_id_for(&b, "test-machine")));
    let both = entities_for(&st, &["0,0".to_string(), "1,0".to_string()]);
    assert_eq!(both.len(), 2);
    let miss = entities_for(&st, &["9,9".to_string()]);
    assert!(miss.is_empty());
}

#[tokio::test]
async fn about_honors_x_forwarded_proto_host_prefix() {
    let (_tmp, st) = online_scene("fwd");
    let req = request(
        "/about",
        &[
            ("host", "127.0.0.1:8000"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "tunnel.example"),
            ("x-forwarded-prefix", "/t/abc123defg/"),
        ],
    );
    let Json(v) = about(State(Arc::new(st)), req).await;
    assert_eq!(
        v["comms"]["fixedAdapter"],
        json!("ws-room:wss://tunnel.example/t/abc123defg/mini-comms/room-1")
    );
    assert_eq!(
        v["content"]["publicUrl"],
        json!("https://tunnel.example/t/abc123defg/content")
    );
    assert_eq!(
        v["lambdas"]["publicUrl"],
        json!("https://tunnel.example/t/abc123defg/lambdas")
    );
    assert!(
        v["configurations"]["scenesUrn"][0]
            .as_str()
            .unwrap()
            .contains("baseUrl=https://tunnel.example/t/abc123defg/content/contents/")
    );
}

#[tokio::test]
async fn about_without_forwarding_headers_stays_plain_http() {
    let (_tmp, st) = online_scene("nofwd");
    let req = request("/about", &[("host", "198.51.100.20:8000")]);
    let Json(v) = about(State(Arc::new(st)), req).await;
    assert_eq!(
        v["comms"]["fixedAdapter"],
        json!("ws-room:ws://198.51.100.20:8000/mini-comms/room-1")
    );
    assert_eq!(
        v["content"]["publicUrl"],
        json!("http://198.51.100.20:8000/content")
    );
}

#[tokio::test]
async fn root_redirect_honors_forwarded_prefix() {
    let (_tmp, _, st) = one_scene("redir");
    let req = request("/", &[("x-forwarded-prefix", "/t/abc123defg")]);
    let resp = root(State(st.clone()), req).await;
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap(),
        "/t/abc123defg/about"
    );
    let resp = root(State(st), request("/", &[])).await;
    assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/about");
}

async fn scene_body(st: &Arc<AppState>, headers: &[(&str, &str)]) -> String {
    let mut h = HeaderMap::new();
    for (k, v) in headers {
        h.insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    let resp = super::landing::scene_page(st, &h, true);
    assert_eq!(resp.status(), StatusCode::OK);
    body_text(resp).await
}

/// The landing page exactly as a browser receives it: through `root`, with
/// the headers and the query string a visitor would send.
async fn landing_body(st: &Arc<AppState>, uri: &str, headers: &[(&str, &str)]) -> String {
    let mut req = Request::builder()
        .uri(uri)
        .header("accept", "text/html,application/xhtml+xml");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = root(
        State(st.clone()),
        req.body(axum::body::Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    body_text(resp).await
}

/// Nothing loaded from anywhere, no `javascript:` url, no inline handler.
fn assert_no_foreign_scripts(lower: &str) {
    assert!(
        !lower.contains("<script src"),
        "no script loads from anywhere"
    );
    assert!(!lower.contains("javascript:"), "no javascript: url");
    assert_no_inline_handlers(lower);
}

/// `/deploy` holds the landing page's script posture: at most its own one
/// inline script, and every POST carries the token and the payload
/// fingerprint.
fn assert_no_javascript(body: &str) {
    let body = body.to_lowercase();
    assert!(
        body.matches("<script").count() <= 1,
        "at most the page's own inline script"
    );
    let posts = body.matches(r#"method="post""#).count();
    if posts > 0 {
        assert!(
            posts <= 2,
            "the publish button and the bar's connect, nothing more"
        );
        assert!(
            body.matches(r#"name="token""#).count() >= posts,
            "every POST carries the token, or it would be forgeable cross-origin"
        );
        assert!(
            body.contains(r#"name="fingerprint""#),
            "a POST without the payload fingerprint could publish something \
             other than what the page showed"
        );
    }
    assert_no_foreign_scripts(&body);
}

/// The landing page ships exactly its own two scripts — the `#edit-data`
/// JSON blob and the inline editor — and exactly one POSTing form (the
/// bar's connect button; the scene's mutations stay fetches).
fn assert_only_the_landing_scripts(body: &str) {
    let lower = body.to_lowercase();
    assert_eq!(
        lower.matches("<script").count(),
        2,
        "the data blob and the editor script, nothing more"
    );
    assert!(body.contains(r#"<script type="application/json" id="edit-data">"#));
    assert_eq!(
        lower.matches(r#"method="post""#).count(),
        1,
        "the bar's connect button is this page's one form POST; the scene's \
         mutations stay fetches to the gated routes"
    );
    assert_no_foreign_scripts(&lower);
}

fn assert_no_inline_handlers(lower: &str) {
    for (attr, _) in lower.match_indices(" on") {
        let rest = &lower[attr + 3..];
        let name_len = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        assert!(
            !(name_len > 0 && rest[name_len..].starts_with('=')),
            "inline event handler: on{}",
            &rest[..name_len]
        );
    }
}

/// A form on any page here must be a GET aimed back at the page that drew
/// it: a GET carries no side effect, so a page you happen to have open
/// still cannot make this server do anything. `action` is the whole
/// expected attribute value, so behind a proxy it has to carry the
/// forwarded prefix. The one standing exception is the bar's connect
/// button — a POST, but aimed at a route gated like the publish button and
/// carrying the page token.
fn assert_forms_are_gets(body: &str, action: &str, count: usize) {
    let mut connects = 0;
    for form in body.match_indices("<form").map(|(i, _)| &body[i..]) {
        let tag = &form[..form.find('>').expect("unterminated <form")];
        if tag.contains("/target/connect") || tag.contains("/target/address") {
            connects += 1;
            assert!(
                tag.contains(r#"method="post""#),
                "the bar's form is the connect (or disconnect) POST and nothing else: {tag}"
            );
            let scope = &form[..form.find("</form>").unwrap_or(form.len())];
            assert!(
                scope.contains(r#"name="token""#),
                "the connect POST carries the token: {tag}"
            );
            continue;
        }
        assert!(
            tag.contains(r#"method="get""#),
            "every form must be a GET: {tag}"
        );
        assert!(
            tag.contains(&format!(r#"action="{action}""#)),
            "a form may only target this page (expected {action}): {tag}"
        );
    }
    assert!(connects <= 1, "one connect form at most");
    assert_eq!(
        body.matches("<form").count() - connects,
        count,
        "the page draws exactly {count} form(s) beside the bar's connect"
    );
}

#[tokio::test]
async fn root_serves_a_landing_page_to_browsers() {
    let (_tmp, _, st) = one_scene("landing");
    let body = landing_body(&st, "/", LOCAL).await;
    assert!(body.contains("dcl-scene-cli"));
    assert!(body.contains("decentraland://realm=http%3A%2F%2F127.0.0.1%3A8000"));
    assert_eq!(
        body.matches(r#"id="launch""#).count(),
        1,
        "one launch button, for the selected target"
    );
    assert!(
        !body.contains("https://decentraland.org/bevy-web/"),
        "the web target is not the selected one"
    );
    let web = landing_body(&st, "/?where=web", LOCAL).await;
    assert!(web.contains(
        "https://decentraland.org/bevy-web/?preview=true&amp;realm=http://127.0.0.1:8000"
    ));
    assert!(
        !web.contains("decentraland://realm="),
        "and then the desktop card is the one that is gone"
    );
    assert_eq!(web.matches(r#"id="launch""#).count(), 1);

    let must = |needle: &str| {
        body.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}"))
    };
    assert!(
        must(r#"class="pgnav""#) < must(r#"id="join""#),
        "the section nav leads the page"
    );
    assert_eq!(
        body.matches(r#"class="pgnav__lnk""#).count(),
        5,
        "Preview, Scene, Storage, Target, Deploy: {body}"
    );
    assert!(
        body.contains(r#"href="/" aria-current="page">Preview"#),
        "the current section is marked: {body}"
    );
    assert!(
        !body.contains(r#"id="scene-layout""#),
        "the layout card lives on /scene now"
    );
    assert!(
        must(r#"id="requests""#) > must(r#"id="join""#),
        "the request log is the last section"
    );

    let sc = scene_body(&st, LOCAL).await;
    let smust = |needle: &str| {
        sc.find(needle)
            .unwrap_or_else(|| panic!("missing on /scene: {needle}"))
    };
    let layout = smust(r#"<div class="ui-card jn lay lay--info" id="scene-layout">"#);
    assert!(
        layout < smust(r#"data-laytab="info""#)
            && smust(r#"data-laytab="info""#) < smust(r#"data-laytab="parcels""#)
            && smust(r#"data-laytab="parcels""#) < smust(r#"data-laytab="spawns""#)
            && smust(r#"data-laytab="spawns""#) < smust(r#"data-laytab="perms""#),
        "one layout card, four tabs in order"
    );
    assert!(
        smust(r#"id="lay-grid""#) < smust(r#"data-pane="info""#),
        "the shared grid pane sits left of the rail panes"
    );
    assert!(
        sc.contains(r#"<h1 class="scene__title" id="edit-title""#),
        "the visible title is the h1, and it is the title editor: {sc}"
    );
    assert!(
        sc.contains(r#"href="/scene" aria-current="page">Scene"#),
        "the scene page marks its own nav entry: {sc}"
    );
    assert!(
        !body.contains(r#"id="deploy""#),
        "and the landing page no longer carries a deploy section"
    );
    assert!(
        !body.contains("--dir"),
        "the page never names the scene path"
    );
    for page in [
        "/about",
        "/scenes",
        "/scene.json",
        "/preview-wearables",
        "/deploy",
    ] {
        assert!(body.contains(&format!(r#"href="{page}""#)), "{page} linked");
    }
    assert_forms_are_gets(&body, "/", 1);
    let form = body
        .find(r#"<form class="side""#)
        .expect("the knob form (the bar's connect form is not it)");
    let end = body[form..].find("</form>").expect("unterminated form") + form;
    for knob in [r#"name="where""#, r#"name="mcp""#, r#"name="opt""#] {
        let at = body.find(knob).unwrap_or_else(|| panic!("missing {knob}"));
        assert!(at > form && at < end, "{knob} must ride inside the form");
    }
    for gone in [
        "comms ws-room",
        "abgen ready",
        "bar__realm",
        ">setup<",
        ">server<",
        "snapshot",
        "/inspector/",
        "class=\"foot\"",
    ] {
        assert!(!body.contains(gone), "{gone} should be gone");
    }
    assert!(!body.contains(&format!("dcl-scene-cli {}", env!("CARGO_PKG_VERSION"))));
}

#[tokio::test]
async fn landing_page_honors_forwarded_headers_and_escapes_titles() {
    let tmp = Tmp::new("landing-fwd");
    let scene_json = json!({
        "main": "bin/index.js",
        "display": { "title": "a <script> title" },
        "scene": { "parcels": ["0,0"], "base": "0,0" }
    });
    let project = project_with(&tmp.0, "scene-x", scene_json, "module.exports={}");
    let st = Arc::new(state(vec![project]));
    let fwd = [
        ("host", "127.0.0.1:8000"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-host", "tunnel.example"),
        ("x-forwarded-prefix", "/t/abc123defg/"),
    ];
    let body = landing_body(&st, "/", &fwd).await;
    assert!(body.contains("https%3A%2F%2Ftunnel.example%2Ft%2Fabc123defg"));
    assert!(!body.contains("a <script> title"));
    assert!(body.contains("a &lt;script&gt; title"));
    let web = landing_body(&st, "/?where=web", &fwd).await;
    assert!(web.contains("https://tunnel.example/t/abc123defg"));
    assert!(!web.contains("a <script> title"));

    assert!(
        body.contains(r#"<a class="pgnav__lnk" href="/t/abc123defg/deploy""#),
        "the section nav keeps the forwarded prefix"
    );
    assert_forms_are_gets(&body, "/t/abc123defg/", 1);
    for page in ["/about", "/scenes", "/scene.json", "/deploy"] {
        assert!(
            body.contains(&format!(r#"href="/t/abc123defg{page}""#)),
            "{page} keeps the forwarded prefix"
        );
    }
    assert!(
        !body.contains(r#"href="/deploy""#),
        "and no link is left pointing at the unprefixed root"
    );
}

/// The landing page ships its editor script, and that is the whole
/// JavaScript budget: however hostile the query string, nothing else
/// executable may appear, because everything reflected into the page is
/// either allowlisted or escaped.
#[tokio::test]
async fn the_landing_page_ships_only_its_own_script() {
    let tmp = Tmp::new("landing-nojs");
    let scene_json = json!({
        "main": "bin/index.js",
        "display": { "title": "Plain" },
        "scene": { "parcels": ["0,0"], "base": "0,0" },
        "requiredPermissions": ["USE_FETCH"],
        "spawnPoints": [{ "name": "spawn", "default": true,
                          "position": { "x": 8, "y": 0, "z": 8 } }]
    });
    let project = project_with(&tmp.0, "scene-x", scene_json, "module.exports={}");
    let st = Arc::new(state(vec![project]));
    let body = landing_body(&st, "/", LOCAL).await;
    assert_only_the_landing_scripts(&body);
    assert_forms_are_gets(&body, "/", 1);

    let hostile = concat!(
        "/?spawn=%3Cscript%3Ealert%281%29%3C%2Fscript%3E",
        "&opt=%22+onload%3Dalert%281%29",
        "&where=%22%3E%3Cimg+src%3Dx+onerror%3Dalert%281%29%3E",
        "&mcp=javascript%3Aalert%281%29",
        "&args=--gatekeeper-url%3Dhttps%3A%2F%2Fevil.example",
        "&%3Cscript%3E=%3Cscript%3E",
    );
    let poisoned = landing_body(&st, hostile, LOCAL).await;
    assert_only_the_landing_scripts(&poisoned);
    assert_forms_are_gets(&poisoned, "/", 1);
    for smuggled in ["alert(1)", "gatekeeper-url", "evil.example", "onerror"] {
        assert!(
            !poisoned.contains(smuggled),
            "attacker-chosen {smuggled:?} reached the page"
        );
    }
    assert!(poisoned.contains(r#"id="launch""#));
    assert!(poisoned.contains(r#"name="spawn""#), "this scene names one");
}

/// The request log is the one part of this page built out of strings a
/// stranger chose; the buffer starts empty, so only a seeded one exercises
/// the escaping.
#[tokio::test]
async fn the_request_log_replays_a_hostile_path_escaped_and_newest_first() {
    let (_tmp, _, st) = one_scene("reqlog");
    let method = axum::http::Method::GET;
    record_request(&st, log_line(&method, "/<script>alert(1)</script>"), 404);
    record_request(&st, log_line(&method, "/newest.glb"), 200);

    let body = landing_body(&st, "/", LOCAL).await;
    assert!(
        body.contains("GET /&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the path is replayed escaped"
    );
    assert_only_the_landing_scripts(&body);
    assert!(
        body.find("/newest.glb").unwrap() < body.find("&lt;script&gt;").unwrap(),
        "newest first, so the log reads as a tail of what just happened"
    );
    assert!(
        body.contains("<summary>Recent requests \u{b7} 2</summary>"),
        "the count is the buffer's, plain text on the drawer that holds it: {body}"
    );
    assert!(
        body.contains(r#"<b class="reqs__st reqs__st--warn">404</b>"#)
            && body.contains(r#"<b class="reqs__st reqs__st--ok">200</b>"#),
        "each row carries its own status tone"
    );
}

/// The buffer bounds the section, so the eviction is the only thing
/// keeping a long-running preview's page from growing without limit.
#[test]
fn the_request_log_forgets_the_oldest_past_its_cap() {
    let (_tmp, _, st) = one_scene("reqcap");
    let method = axum::http::Method::GET;
    for i in 0..RECENT_REQUESTS_CAP + 5 {
        record_request(&st, log_line(&method, &format!("/{i}.glb")), 200);
    }
    let recent = st.recent_requests.lock().unwrap();
    assert_eq!(recent.len(), RECENT_REQUESTS_CAP);
    assert_eq!(
        recent.front().unwrap().0,
        "GET /5.glb",
        "the oldest five go"
    );
    assert_eq!(
        recent.back().unwrap().0,
        format!("GET /{}.glb", RECENT_REQUESTS_CAP + 4)
    );
}

#[test]
fn a_path_too_long_to_be_a_path_is_cut_before_it_is_kept() {
    let method = axum::http::Method::GET;
    assert_eq!(
        log_line(&method, "/about"),
        "GET /about",
        "short paths whole"
    );

    let long = format!("/{}", "a".repeat(7000));
    let line = log_line(&method, &long);
    assert!(
        line.len() < MAX_LOGGED_PATH + 8,
        "a 7000-character path is not retained: kept {} bytes",
        line.len()
    );
    assert!(line.starts_with("GET /aaa") && line.ends_with('\u{2026}'));

    let wide = format!("/{}", "\u{2764}".repeat(2000));
    let cut = log_line(&method, &wide);
    assert!(cut.ends_with('\u{2026}'));
    assert!(
        cut.trim_start_matches("GET /")
            .trim_end_matches('\u{2026}')
            .chars()
            .all(|c| c == '\u{2764}'),
        "no replacement or split character: {cut}"
    );
}

/// The whole app on a real socket, so a test can ask it the way a browser
/// does — through the routing table and the access-log layer, instead of
/// reaching past both to call one handler.
async fn serve(st: &Arc<AppState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = build_router(st.clone(), Arc::new(crate::comms::CommsState::default()));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    (addr, handle)
}

async fn fetch(addr: SocketAddr, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://{addr}{path}"))
        .send()
        .await
        .unwrap()
}

/// `fetch` as the address bar does it: a top-level navigation, which is
/// what the token and the form tokens are rendered for.
async fn navigate(addr: SocketAddr, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://{addr}{path}"))
        .header("accept", "text/html")
        .header("sec-fetch-site", "none")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-dest", "document")
        .send()
        .await
        .unwrap()
}

/// A binary without the embedded explorer still answers `/play`: the browser
/// is sent to the hosted web explorer with this preview as its realm.
#[tokio::test]
async fn play_without_an_embedded_explorer_goes_to_the_hosted_one() {
    if crate::play_embed::present() {
        return;
    }
    let (_tmp, _project, st) = one_scene("playhosted");
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for path in ["/play", "/play/"] {
        let resp = client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::TEMPORARY_REDIRECT,
            "{path}"
        );
        let location = resp.headers()["location"].to_str().unwrap();
        assert!(
            location.starts_with(&joinblock::web_explorer_base()),
            "{path} -> {location}"
        );
        let url = url::Url::parse(location).unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs.get("preview").map(String::as_str),
            Some("true"),
            "{location}"
        );
        assert_eq!(
            pairs.get("realm"),
            Some(&format!("http://{addr}")),
            "{location}"
        );
        assert_eq!(
            pairs.get("position").map(String::as_str),
            Some("0,0"),
            "{location}"
        );
    }
    server.abort();
}

/// `--position` moves the hosted `/play` fallback the way it moves the
/// printed join links: the base parcel is only the default.
#[tokio::test]
async fn play_without_an_embedded_explorer_lands_on_the_position_flag() {
    if crate::play_embed::present() {
        return;
    }
    let tmp = Tmp::new("playposition");
    let mut st = state(vec![member(&tmp, "scene-a", &["0,0"])]);
    st.position = Some((3, -4));
    let st = Arc::new(st);
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{addr}/play"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    let location = resp.headers()["location"].to_str().unwrap();
    let url = url::Url::parse(location).unwrap();
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        pairs.get("position").map(String::as_str),
        Some("3,-4"),
        "{location}"
    );
    server.abort();
}

/// With no client attached, `broadcast::send` reports zero receivers, and a
/// developer staring at "reload issued" while nothing moves is the exact
/// confusion this line exists to prevent.
#[test]
fn the_reload_note_says_whether_anything_received_it() {
    assert_eq!(reload_note(1), "reload issued");
    assert_eq!(reload_note(3), "reload issued to 3 clients");
    let none = reload_note(0);
    assert!(none.contains("no client connected"), "{none}");
    assert!(!none.contains("reload issued"), "{none}");
}

/// The client routes UpdateModel and UpdateScene alike into
/// TryReloadSceneAsync, so both events must report a reload.
#[tokio::test]
async fn both_a_scene_and_a_model_change_report_a_reload() {
    let (_tmp, project, st) = one_scene("reloadnote");
    let root = project.root.clone();
    let mut rx = st.reload_tx.subscribe();

    for event in [
        ReloadEvent::Scene,
        ReloadEvent::Model {
            path: root.join("assets/spiral.glb"),
            removed: false,
        },
    ] {
        notify_reload(&root, "scene-a", &st, event);
        assert!(rx.try_recv().is_ok(), "text frame");
        assert!(rx.try_recv().is_ok(), "binary frame");
    }
}

/// Every page carries a header button pointing at `/deploy`; fetched through
/// the real router because the registration is the untested half.
#[tokio::test]
async fn deploy_is_a_page_this_server_actually_serves() {
    let (_tmp, a, st) = one_scene("deployroute");
    let scene_dir = a.root.display().to_string();
    let (addr, server) = serve(&st).await;
    let resp = fetch(addr, "/deploy").await;
    assert_eq!(resp.status().as_u16(), 200, "the header button leads here");
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let body = resp.text().await.unwrap();
    server.abort();
    assert!(
        body.contains("dcl-scene-cli deploy"),
        "the page still prints the command, for anyone who wants to run it"
    );
    assert!(
        body.contains(r#"method="post""#) && body.contains(r#"name="token""#),
        "the publish button is live for a loopback caller"
    );
    assert_no_javascript(&body);
    assert!(
        !body.contains(&scene_dir) && !body.contains("deployroute"),
        "the page never names the scene directory"
    );
}

/// The publish gates, each asserted by breaking it and watching the POST be
/// refused — a deploy that ran here would build and sign a scene.
#[tokio::test]
async fn publishing_refuses_a_forged_or_stale_post() {
    let (_tmp, _, st) = one_scene("deploypost");
    let (addr, server) = serve(&st).await;

    let page = navigate(addr, "/deploy").await.text().await.unwrap();
    let field = |name: &str| {
        let at = page
            .find(&format!(r#"name="{name}" value=""#))
            .unwrap_or_else(|| panic!("no {name} field on the page"));
        let from = page[at..].find("value=\"").unwrap() + at + 7;
        page[from..][..page[from..].find('"').unwrap()].to_string()
    };
    let token = field("token");
    let print = field("fingerprint");
    assert!(!token.is_empty() && !print.is_empty());

    let poster = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let post = |form: Vec<(&'static str, String)>| {
        let c = poster.clone();
        async move {
            c.post(format!("http://{addr}/deploy"))
                .form(&form)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };

    assert_eq!(
        post(vec![
            ("token", "not-the-token".to_string()),
            ("fingerprint", print.clone()),
        ])
        .await,
        403,
        "a forged token must not start a deploy"
    );

    assert_eq!(
        post(vec![
            ("token", token.clone()),
            ("fingerprint", "0000stale0000".to_string()),
        ])
        .await,
        303,
        "a stale fingerprint redirects rather than publishing"
    );
    let after = fetch(addr, "/deploy").await.text().await.unwrap();
    assert!(
        after.contains("Nothing was published"),
        "and it says so: {after:.400}"
    );
    server.abort();
}

/// A same-origin POST from loopback rewrites scene.json on disk, and every
/// read surface serves the edit at once: the state was updated, not just
/// the file.
#[tokio::test]
async fn a_scene_edit_reaches_disk_state_and_every_page() {
    let (_tmp, a, st) = one_scene("editflow");
    let root = a.root;
    let (addr, server) = serve(&st).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/scene-json"))
        .header("authorization", format!("Bearer {}", st.token))
        .json(&json!({
            "title": "Renamed Stage",
            "description": "Now with a description.",
            "tags": ["events", "theatre"],
            "parcels": ["0,0", "1,0"],
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let echoed: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{echoed}");
    assert_eq!(echoed["display"]["title"], json!("Renamed Stage"));

    let disk: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("scene.json")).unwrap()).unwrap();
    assert_eq!(disk["display"]["title"], json!("Renamed Stage"));
    assert_eq!(disk["scene"]["parcels"], json!(["0,0", "1,0"]));
    assert_eq!(
        disk["main"],
        json!("bin/index.js"),
        "untouched keys survive"
    );

    let page = fetch(addr, "/scene").await.text().await.unwrap();
    assert!(page.contains("Renamed Stage"));
    assert!(page.contains("2 parcels"));
    assert!(page.contains("events"));

    let served: Value = fetch(addr, "/scene.json").await.json().await.unwrap();
    assert_eq!(served["display"]["title"], json!("Renamed Stage"));

    let about: Value = fetch(addr, "/about").await.json().await.unwrap();
    assert_eq!(
        about["configurations"]["localSceneParcels"],
        json!(["0,0", "1,0"]),
        "the realm advertises the new layout"
    );
    server.abort();
}

/// Every way a stranger's page could aim a POST at the editor, refused
/// before it touches disk: a cross-origin `Origin`, a cross-site
/// `Sec-Fetch-Site`, a tunnel replay, a missing or wrong token, and a body
/// field the page never drew an editor for.
#[tokio::test]
async fn a_forged_scene_edit_never_touches_disk() {
    let (_tmp, a, st) = one_scene("editforge");
    let root = a.root;
    let before = std::fs::read_to_string(root.join("scene.json")).unwrap();
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::new();
    let bearer: &'static str = format!("Bearer {}", st.token).leak();
    let post = |headers: Vec<(&'static str, &'static str)>, body: Value| {
        let client = client.clone();
        let url = format!("http://{addr}/scene-json");
        async move {
            let mut req = client.post(url).json(&body);
            let mut with_token = false;
            for (k, v) in headers {
                with_token |= k == "authorization";
                req = req.header(k, v);
            }
            if !with_token {
                req = req.header("authorization", bearer);
            }
            req.send().await.unwrap().status().as_u16()
        }
    };

    let title = json!({ "title": "Forged" });
    assert_eq!(
        post(
            vec![("authorization", "Bearer not-the-token")],
            title.clone()
        )
        .await,
        401,
        "a wrong token is refused"
    );
    assert_eq!(
        post(vec![("authorization", "")], title.clone()).await,
        401,
        "and so is none: the page's own origin alone does not write"
    );
    assert_eq!(
        post(vec![("origin", "https://evil.example")], title.clone()).await,
        403,
        "a cross-origin POST is refused"
    );
    assert_eq!(
        post(vec![("sec-fetch-site", "cross-site")], title.clone()).await,
        403
    );
    assert_eq!(
        post(vec![(crate::tunnel::FORWARDED_HEADER, "1")], title.clone()).await,
        403,
        "a tunnel replay arrives on a loopback socket and is still refused"
    );
    assert_eq!(
        post(vec![], json!({ "realm": "https://evil.example" })).await,
        422,
        "a field the page has no editor for is refused, not ignored"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("scene.json")).unwrap(),
        before,
        "nothing above touched the file"
    );

    let own: &'static str = format!("http://{addr}").leak();
    assert_eq!(
        post(vec![("origin", own)], title).await,
        200,
        "the page's own origin is the one that works"
    );
    server.abort();
}

/// The thumbnail route writes the image where scene.json will find it,
/// repoints `display.navmapThumbnail`, and refuses bytes that are not the
/// image type they claim.
#[tokio::test]
async fn a_thumbnail_upload_lands_in_the_scene_and_scene_json() {
    let (_tmp, a, st) = one_scene("editthumb");
    let root = a.root;
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::new();
    let bearer = format!("Bearer {}", st.token);
    let upload = |bytes: Vec<u8>| {
        client
            .post(format!("http://{addr}/scene-thumbnail"))
            .header("content-type", "image/png")
            .header("authorization", &bearer)
            .body(bytes)
            .send()
    };

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0u8; 24]);
    let resp = upload(png.clone()).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["navmapThumbnail"], json!("scene-thumbnail.png"));
    assert_eq!(
        std::fs::read(root.join("scene-thumbnail.png")).unwrap(),
        png
    );
    let disk: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("scene.json")).unwrap()).unwrap();
    assert_eq!(
        disk["display"]["navmapThumbnail"],
        json!("scene-thumbnail.png")
    );

    let forged = upload(b"GIF89a not a png".to_vec()).await.unwrap();
    assert_eq!(
        forged.status().as_u16(),
        415,
        "the magic bytes decide, not the header"
    );
    server.abort();
}

/// `access_log` is a layer, so a test that calls a handler directly walks
/// straight past it.
#[tokio::test]
async fn the_access_log_keeps_one_bounded_line_per_request() {
    let (_tmp, _, st) = one_scene("accesslog");
    let (addr, server) = serve(&st).await;
    let long = format!("/{}", "a".repeat(4000));
    fetch(addr, &long).await;
    fetch(addr, "/about").await;
    server.abort();

    let recent: Vec<(String, u16)> = st
        .recent_requests
        .lock()
        .unwrap()
        .iter()
        .map(|(line, status, _)| (line.clone(), *status))
        .collect();
    assert_eq!(recent.len(), 2, "one line per request: {recent:?}");
    assert!(
        recent[0].0.len() < MAX_LOGGED_PATH + 8,
        "a 4000-character path must not be retained whole, kept {} bytes",
        recent[0].0.len()
    );
    assert!(recent[0].0.ends_with('\u{2026}'), "{}", recent[0].0);
    assert_eq!(recent[0].1, 404, "and the status it really got");
    assert_eq!(recent[1], ("GET /about".to_string(), 200));
}

/// `--port 0` serves on whichever free port the bind took, and that port,
/// not 0, is the one the preview goes by: tests start previews this way and
/// read the port back from the banner.
#[tokio::test]
async fn port_zero_is_the_port_the_bind_took() {
    let (port, listener) = bind_preview_port("start", Ipv4Addr::UNSPECIFIED.into(), Some(0))
        .await
        .unwrap();
    assert_ne!(port, 0);
    assert_eq!(port, listener.local_addr().unwrap().port());
}

/// The deploy server listens on the address it is given, and its URLs name
/// `localhost` for IPv4 loopback and for every address.
#[tokio::test]
async fn the_signing_server_binds_the_given_address() {
    let (port, listener) = bind_preview_port("deploy", Ipv4Addr::LOCALHOST.into(), Some(0))
        .await
        .unwrap();
    let bound = listener.local_addr().unwrap();
    assert_eq!(bound, SocketAddr::from(([127, 0, 0, 1], port)));
    let localhost = format!("http://localhost:{port}");
    assert_eq!(signing_origin(bound), localhost);
    assert_eq!(
        signing_origin(SocketAddr::from(([0, 0, 0, 0], port))),
        localhost
    );
    assert_eq!(
        signing_origin(SocketAddr::from(([192, 0, 2, 7], port))),
        format!("http://192.0.2.7:{port}")
    );
    assert_eq!(
        signing_origin(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))),
        format!("http://[::1]:{port}")
    );
}

/// A taken port: the step names `--port` of the command that listens, and
/// the cause has the same code for each command.
#[tokio::test]
async fn a_taken_port_names_the_flag_of_the_command_that_ran() {
    let held = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let port = held.local_addr().unwrap().port();
    let next = port.checked_add(1).unwrap_or(8001);
    for (command, other) in [("deploy", "start"), ("start", "deploy")] {
        let err = bind_preview_port(command, Ipv4Addr::LOCALHOST.into(), Some(port))
            .await
            .unwrap_err();
        let text = ux::render(&err, false, false);
        assert!(
            text.starts_with(&format!("Error: port {port} is already in use\n")),
            "{text}"
        );
        assert!(
            text.contains(&format!("dcl-scene-cli {command} --port {next}\n")),
            "{text}"
        );
        assert!(!text.contains(&format!("{other} --port")), "{text}");
    }
}

/// `start --port 8123` (the default MCP port) once aimed the scene-log
/// poller at this very server.
#[test]
fn the_scene_log_reader_never_polls_this_server() {
    let mcp = crate::joinblock::DEFAULT_EXPLORER_MCP_PORT;
    assert_eq!(
        scene_log_port(true, mcp, 8000),
        Some(mcp),
        "the normal case"
    );
    assert_eq!(
        scene_log_port(true, mcp, mcp),
        None,
        "start --port {mcp} must not start a poller aimed at itself"
    );
    assert_eq!(scene_log_port(false, mcp, 8000), None, "--no-mcp");
    assert_eq!(scene_log_port(false, mcp, mcp), None);

    let said = ux::render(&mcp_port_clash(mcp).into(), false, false);
    assert!(said.contains(&format!("--mcp-port {mcp}")), "{said}");
    assert!(said.contains("--mcp-port 8124"), "a way out: {said}");
    assert!(said.contains("--no-mcp"), "and a way to silence it: {said}");
}

fn state_with_data_layer(public_dir: PathBuf) -> Arc<AppState> {
    let (_tx, host_rx) = tokio::sync::watch::channel(Default::default());
    std::mem::forget(_tx);
    let mut st = state(vec![]);
    st.data_layer = Some(DataLayerState {
        host_rx,
        public_dir: Some(public_dir),
    });
    Arc::new(st)
}

fn ready_block(port: u16) -> JoinBlock {
    JoinBlock {
        title: "ready".to_string(),
        position: (4, -2),
        port,
        ifaces: Vec::new(),
        web_explorer: "https://decentraland.org/bevy-web".to_string(),
        qr: QrMode::Hint,
        unreachable: Vec::new(),
        tunnel_hint: false,
        editor: false,
        optimized_assets_url: None,
        deep_link_extra: "&hub=true".to_string(),
        realm_override: None,
        native_hud: true,
        native_bin: None,
    }
}

/// `start --json`, the `ready` line with no editor and no client: the links
/// are the ones of the text form, and the token is the run's bearer.
#[test]
fn the_ready_event_names_the_links_and_the_run_token() {
    let block = ready_block(8123);
    let st = state(vec![]);
    let line = serde_json::to_string(&ready_event(
        &block,
        &st,
        [(false, "talk"), (false, "desktop"), (false, "web")],
    ))
    .unwrap();
    assert!(
        line.starts_with(r#"{"event":"ready","port":8123,"#),
        "{line}"
    );
    let event: serde_json::Value = serde_json::from_str(&line).unwrap();
    let deep_link = block.local_desktop_link();
    assert!(deep_link.starts_with("decentraland://"), "{deep_link}");
    assert!(deep_link.ends_with("&hub=true"), "{deep_link}");
    assert!(
        block
            .compact_body()
            .lines()
            .any(|row| row.trim_start().starts_with("desktop:") && row.ends_with(&deep_link)),
        "the deep link is the one of the desktop: row"
    );
    assert_eq!(
        event,
        json!({
            "event": "ready",
            "port": 8123,
            "url": "http://127.0.0.1:8123",
            "editorUrl": null,
            "webUrl": block.local_web_url(),
            "deepLink": deep_link,
            "dataLayerUrl": null,
            "token": st.token,
            "clientsOpened": [],
        })
    );
    assert!(!st.token.is_empty());
}

/// With `--data-layer` the line names the socket, and the editor page only
/// when an editor UI is installed; `clientsOpened` lists what the run opened.
#[test]
fn the_ready_event_names_the_data_layer_and_the_opened_clients() {
    let block = ready_block(8123);
    let opened = [(true, "talk"), (false, "desktop"), (true, "web")];
    let with_ui = state_with_data_layer(PathBuf::from("inspector-public"));
    let event = serde_json::to_value(ready_event(&block, &with_ui, opened)).unwrap();
    assert_eq!(event["dataLayerUrl"], "ws://127.0.0.1:8123/data-layer");
    assert_eq!(event["editorUrl"], "http://127.0.0.1:8123/inspector/");
    assert_eq!(event["clientsOpened"], json!(["talk", "web"]));

    let (tx, host_rx) = tokio::sync::watch::channel(Default::default());
    let mut no_ui = state(vec![]);
    no_ui.data_layer = Some(DataLayerState {
        host_rx,
        public_dir: None,
    });
    let event = serde_json::to_value(ready_event(&block, &no_ui, opened)).unwrap();
    drop(tx);
    assert_eq!(event["dataLayerUrl"], "ws://127.0.0.1:8123/data-layer");
    assert_eq!(event["editorUrl"], serde_json::Value::Null);
}

async fn get_asset(st: &Arc<AppState>, path: &str, headers: HeaderMap) -> Response {
    inspector_asset(State(st.clone()), AxPath(path.to_string()), headers).await
}

#[tokio::test]
async fn contents_refuses_dclignored_files() {
    let (_tmp, a, st) = one_scene("dclignore");
    std::fs::write(a.root.join("package.json"), "{\"secret\":\"key\"}").unwrap();
    let get = |rel: &str| {
        let hash = b64_hash(&a.root.join(rel).display().to_string(), "test-machine");
        contents(
            axum::http::Method::GET,
            State(st.clone()),
            AxPath(hash),
            HeaderMap::new(),
        )
    };
    assert_eq!(get("bin/index.js").await.status(), StatusCode::OK);
    assert_eq!(
        get("package.json").await.status(),
        StatusCode::NOT_FOUND,
        "a .dclignored file must not be byte-served via /content/contents"
    );
}

#[tokio::test]
async fn inspector_asset_refuses_absolute_and_dotdot_paths() {
    let tmp = Tmp::new("inspector");
    let public = tmp.0.join("public");
    std::fs::create_dir_all(&public).unwrap();
    std::fs::write(public.join("app.js"), "console.log(1)").unwrap();
    let secret = tmp.0.join("secret.txt");
    std::fs::write(&secret, "top secret").unwrap();
    let st = state_with_data_layer(public.clone());

    let ok = get_asset(&st, "app.js", HeaderMap::new()).await;
    assert_eq!(ok.status(), StatusCode::OK);

    let abs = secret.canonicalize().unwrap().display().to_string();
    let escaped = get_asset(&st, &abs, HeaderMap::new()).await;
    assert_eq!(
        escaped.status(),
        StatusCode::NOT_FOUND,
        "an absolute path outside public_dir must be refused"
    );

    let dotdot = get_asset(&st, "../secret.txt", HeaderMap::new()).await;
    assert_eq!(dotdot.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn inspector_serves_the_gzipped_bundle_both_ways() {
    use std::io::Write;
    let tmp = Tmp::new("inspector-gz");
    let public = tmp.0.join("public");
    std::fs::create_dir_all(&public).unwrap();
    let plain = b"globalThis.InspectorConfig\n".repeat(40);
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&plain).unwrap();
    std::fs::write(public.join("bundle.js.gz"), enc.finish().unwrap()).unwrap();
    let st = state_with_data_layer(public.clone());

    let mut gz_headers = HeaderMap::new();
    gz_headers.insert(header::ACCEPT_ENCODING, "gzip, deflate".parse().unwrap());
    let compressed = get_asset(&st, "bundle.js", gz_headers).await;
    assert_eq!(compressed.status(), StatusCode::OK);
    assert_eq!(
        compressed.headers().get(header::CONTENT_ENCODING).unwrap(),
        "gzip"
    );
    assert_eq!(
        compressed.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/javascript",
        "the mime must come from the request path, not the .gz on disk"
    );
    let body = axum::body::to_bytes(compressed.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.len() < plain.len());
    assert_eq!(crate::data_layer::gunzip(&body).unwrap(), plain);

    let expanded = get_asset(&st, "bundle.js", HeaderMap::new()).await;
    assert_eq!(expanded.status(), StatusCode::OK);
    assert!(expanded.headers().get(header::CONTENT_ENCODING).is_none());
    let body = axum::body::to_bytes(expanded.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), plain.as_slice());
}

#[tokio::test]
async fn inspector_revalidates_by_etag_and_serves_the_index_from_memory() {
    let tmp = Tmp::new("inspector-etag");
    let public = tmp.0.join("public");
    std::fs::create_dir_all(&public).unwrap();
    std::fs::write(public.join("app.js"), "console.log(1)").unwrap();
    std::fs::write(
        public.join("index.html"),
        "<html><head><script>const config = '$CONFIG'</script></head><body></body></html>",
    )
    .unwrap();
    let st = state_with_data_layer(public.clone());

    let first = get_asset(&st, "app.js", HeaderMap::new()).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first.headers()["cross-origin-embedder-policy"],
        "require-corp"
    );
    assert_eq!(
        first.headers()["cross-origin-resource-policy"],
        "cross-origin"
    );
    let etag = first.headers().get(header::ETAG).expect("etag").clone();
    assert_eq!(
        first.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );
    let mut again = HeaderMap::new();
    again.insert(header::IF_NONE_MATCH, etag.clone());
    let revalidated = get_asset(&st, "app.js", again.clone()).await;
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        revalidated.headers()["cross-origin-embedder-policy"],
        "require-corp"
    );
    assert_eq!(revalidated.headers().get(header::ETAG).unwrap(), &etag);
    let body = axum::body::to_bytes(revalidated.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty());

    std::fs::write(public.join("app.js"), "console.log(1);/*edited*/").unwrap();
    let edited = get_asset(&st, "app.js", again).await;
    assert_eq!(
        edited.status(),
        StatusCode::OK,
        "a changed length is a new tag"
    );
    assert_ne!(edited.headers().get(header::ETAG).unwrap(), &etag);

    let mut host = HeaderMap::new();
    host.insert(header::HOST, "127.0.0.1:8000".parse().unwrap());
    let index = inspector_index(State(st.clone()), host.clone()).await;
    assert_eq!(index.status(), StatusCode::OK);
    let tag = index
        .headers()
        .get(header::ETAG)
        .expect("index etag")
        .clone();
    let html = body_text(index).await;
    assert!(html.contains("127.0.0.1:8000/data-layer"), "{html}");
    host.insert(header::IF_NONE_MATCH, tag.clone());
    let same = inspector_index(State(st.clone()), host.clone()).await;
    assert_eq!(same.status(), StatusCode::NOT_MODIFIED);
    let mut other = HeaderMap::new();
    other.insert(header::HOST, "198.51.100.5:8000".parse().unwrap());
    other.insert(header::IF_NONE_MATCH, tag.clone());
    let elsewhere = inspector_index(State(st.clone()), other).await;
    assert_eq!(
        elsewhere.status(),
        StatusCode::OK,
        "the injected origin is part of the page, so its tag differs"
    );
    assert_ne!(elsewhere.headers().get(header::ETAG).unwrap(), &tag);
}

#[test]
fn data_layer_origin_gate_allows_same_origin_and_native_rejects_cross() {
    let headers = |pairs: &[(header::HeaderName, &str)]| {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), v.parse().unwrap());
        }
        h
    };
    assert!(
        data_layer_origin_allowed(&HeaderMap::new()),
        "native clients send no Origin"
    );
    assert!(
        data_layer_origin_allowed(&headers(&[(header::ORIGIN, "null")])),
        "null Origin is native"
    );
    assert!(data_layer_origin_allowed(&headers(&[
        (header::HOST, "127.0.0.1:8000"),
        (header::ORIGIN, "http://127.0.0.1:8000"),
    ])));
    assert!(
        data_layer_origin_allowed(&headers(&[
            (
                header::HeaderName::from_static("x-forwarded-host"),
                "tunnel.example"
            ),
            (header::HOST, "127.0.0.1:8000"),
            (header::ORIGIN, "https://tunnel.example"),
        ])),
        "same-origin behind nginx"
    );
    assert!(!data_layer_origin_allowed(&headers(&[
        (header::HOST, "127.0.0.1:8000"),
        (header::ORIGIN, "https://evil.example"),
    ])));
}

#[test]
fn scene_entity_content_hashes_are_member_scoped() {
    let tmp = Tmp::new("entity");
    let b = member(&tmp, "scene-b", &["1,0"]);
    let entity = build_scene_entity(&b, "test-machine");
    let content = entity["content"].as_array().unwrap();
    assert!(content.iter().any(|c| {
        c["file"] == json!("bin/index.js")
            && c["hash"]
                == json!(crate::scene::b64_content_hash(
                    &b.root.join("bin/index.js").display().to_string(),
                    "test-machine"
                ))
    }));
}

/// The compression predicate: text-like bodies only. Media is already
/// compressed, an event stream must not be buffered, and a body with no
/// content-type is what the websocket upgrades answer with.
#[test]
fn only_text_like_responses_are_compressible() {
    for yes in [
        "text/html; charset=utf-8",
        "text/javascript",
        "application/javascript",
        "application/json",
        "application/wasm",
        "image/svg+xml",
    ] {
        assert!(compressible_mime(yes), "{yes}");
    }
    for no in [
        "text/event-stream",
        "image/png",
        "video/mp4",
        "application/octet-stream",
        "model/gltf-binary",
        "",
    ] {
        assert!(!compressible_mime(no), "{no}");
    }
}

/// The layer sits outside every route, the merged deploy pages included:
/// a browser asking for gzip gets the scene page gzipped, a client that does
/// not ask gets it plain, and a body under the 1 KB floor (the one-scene
/// landing JSON) stays plain for everyone.
#[tokio::test]
async fn the_preview_gzips_text_for_clients_that_accept_it() {
    let (_tmp, _project, st) = one_scene("gzip");
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::builder().no_gzip().build().unwrap();
    let encoding = |r: &reqwest::Response| {
        r.headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap().to_string())
    };
    let gz = client
        .get(format!("http://{addr}/scene"))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(gz.status(), StatusCode::OK);
    assert_eq!(encoding(&gz).as_deref(), Some("gzip"), "{:?}", gz.headers());
    assert!(
        gz.headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().starts_with("text/html"))
            .unwrap_or(false)
    );
    let plain = client
        .get(format!("http://{addr}/scene"))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    assert_eq!(encoding(&plain), None);
    assert!(
        plain.text().await.unwrap().len() > 1024,
        "the scene page is the >1 KB sample"
    );
    let small = client
        .get(format!("http://{addr}/"))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(small.status(), StatusCode::OK);
    assert_eq!(encoding(&small), None, "{:?}", small.headers());
    assert!(
        small.text().await.unwrap().len() <= 1024,
        "the landing JSON is the small sample"
    );
    server.abort();
}

/// A streamed content file survives gzip end to end. The first gzip layer
/// truncated every `/content/contents/{hash}` body: tower-http polls the body
/// once more after it ends, and the unfused `unfold` stream behind the file
/// panicked on that poll, closing the connection mid-response.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_content_files_survive_gzip() {
    let tmp = Tmp::new("gzip-stream");
    let body: String = (0..2000)
        .map(|i| format!("export const line{i} = 'the scene chunk, line {i}';\n"))
        .collect();
    let project = testkit::scene(&tmp.0, "scene-a", &["0,0"], &body);
    let st = Arc::new(state(vec![project.clone()]));
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::builder().no_gzip().build().unwrap();
    let hash = crate::scene::b64_content_hash_in_root(
        &crate::scene::root_tag(&project.root, &st.machine),
        "bin/index.js",
        &project.root.join("bin/index.js"),
    );
    let gz = client
        .get(format!("http://{addr}/content/contents/{hash}"))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(gz.status(), StatusCode::OK);
    assert_eq!(
        gz.headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap()),
        Some("gzip"),
        "{:?}",
        gz.headers()
    );
    let packed = gz.bytes().await.expect("the whole gzip body arrives");
    assert_eq!(&packed[..2], &[0x1f, 0x8b], "gzip magic");
    assert!(packed.len() < body.len() / 4, "{} B packed", packed.len());
    let mut unpacked = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(&packed[..]),
        &mut unpacked,
    )
    .expect("a complete gzip member");
    assert_eq!(unpacked, body);
    let plain = client
        .get(format!("http://{addr}/content/contents/{hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    assert_eq!(
        plain
            .headers()
            .get("content-length")
            .map(|v| v.to_str().unwrap()),
        Some(body.len().to_string().as_str()),
        "uncompressed bodies keep their length"
    );
    assert_eq!(plain.text().await.unwrap(), body);
    server.abort();
}

#[test]
fn the_talk_player_prefers_the_flag_then_the_embedded_player_then_the_env_then_the_site() {
    assert_eq!(talk_player(None, None, None), talk_web_explorer());
    assert_eq!(talk_player(Some("  "), None, Some("")), talk_web_explorer());
    assert_eq!(
        talk_player(None, None, Some("https://decentraland.org/bevy-web/")),
        "https://decentraland.org/bevy-web"
    );
    assert_eq!(
        talk_player(
            Some(" http://localhost:5173/play/ "),
            None,
            Some("https://decentraland.org/bevy-web")
        ),
        "http://localhost:5173/play"
    );
    let embedded = Some("http://127.0.0.1:8000/play/");
    assert_eq!(
        talk_player(None, embedded, Some("https://decentraland.org/bevy-web")),
        "http://127.0.0.1:8000/play",
        "the embedded player beats the environment"
    );
    assert_eq!(
        talk_player(Some("https://example.org/play/"), embedded, None),
        "https://example.org/play",
        "and an explicit --player beats the embedded player"
    );
    let url = talk_url(
        &talk_player(None, embedded, None),
        "http://127.0.0.1:8000",
        (3, -4),
    )
    .unwrap();
    assert!(
        url.starts_with(
            "http://127.0.0.1:8000/play/?preview=true&realm=http://127.0.0.1:8000&position=3,-4"
        ),
        "the talk URL lands in the embedded player: {url}"
    );
    assert!(url.contains("&assistant=1"), "{url}");
}

/// The `Play:` line names the player and the engine build only when the
/// binary carries one, and warns about the secure context only when a LAN
/// address was printed for other devices to use.
#[test]
fn the_play_lines_follow_the_embedded_player_and_the_lan_address() {
    let alone = play_lines(8000, false);
    let with_lan = play_lines(8000, true);
    if play_embed::present() {
        assert_eq!(alone.len(), 1, "{alone:?}");
        assert!(
            alone[0].starts_with("Play: http://127.0.0.1:8000/play/ "),
            "{alone:?}"
        );
        assert!(
            alone[0].contains(&play_embed::engine_rev().unwrap()),
            "{alone:?}"
        );
        assert_eq!(with_lan.len(), 2, "{with_lan:?}");
        assert!(with_lan[1].contains("127.0.0.1 or https"), "{with_lan:?}");
        assert!(
            with_lan[1].contains("public player or the tunnel"),
            "{with_lan:?}"
        );
    } else {
        assert!(
            alone.is_empty() && with_lan.is_empty(),
            "{alone:?} {with_lan:?}"
        );
    }
}

/// The query is checked as parsed pairs: `web_join_url` adds a `portables`
/// pair whenever the shell has `DCL_SCENE_CLI_WORLD_BASE` set, and that pair is
/// not what this test is about.
#[test]
fn the_talk_url_opens_the_assistant_ahead_of_any_fragment() {
    let parts = |url: &str| {
        let parsed = url::Url::parse(url).unwrap();
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .filter(|(k, _)| k != "portables")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        (
            parsed.path().to_string(),
            pairs,
            parsed.fragment().map(str::to_string),
        )
    };
    let pair = |k: &str, v: &str| (k.to_string(), v.to_string());

    let url = talk_url("https://example.org/play", "http://127.0.0.1:8000", (5, -3)).unwrap();
    assert!(url.starts_with(
        "https://example.org/play/?preview=true&realm=http://127.0.0.1:8000&position=5,-3"
    ));
    assert!(url.ends_with("&assistant=1"), "{url}");
    assert_eq!(
        parts(&url),
        (
            "/play/".to_string(),
            vec![
                pair("preview", "true"),
                pair("realm", "http://127.0.0.1:8000"),
                pair("position", "5,-3"),
                pair("assistant", "1"),
            ],
            None
        )
    );

    let url = talk_url(
        "https://example.org/play/?quality=low#view",
        "http://127.0.0.1:8000",
        (0, 0),
    )
    .unwrap();
    assert!(url.ends_with("&quality=low&assistant=1#view"), "{url}");
    assert_eq!(
        parts(&url),
        (
            "/play/".to_string(),
            vec![
                pair("preview", "true"),
                pair("realm", "http://127.0.0.1:8000"),
                pair("position", "0,0"),
                pair("quality", "low"),
                pair("assistant", "1"),
            ],
            Some("view".to_string())
        )
    );

    assert_eq!(talk_url("not a url", "http://127.0.0.1:8000", (0, 0)), None);
    assert_eq!(
        talk_url("ftp://example.org/play", "http://127.0.0.1:8000", (0, 0)),
        None
    );
}

/// The HUD reads the first `assistant` pair, so one the player URL carried
/// (`--player 'https://example.org/play?assistant=0'`) must not stay ahead of
/// ours.
#[test]
fn the_talk_url_replaces_an_assistant_pair_the_player_url_carried() {
    for player in [
        "https://example.org/play?assistant=0",
        "https://example.org/play/?assistant=0&quality=low&assistant=1#view",
        "https://example.org/play?assistant=",
    ] {
        let url = talk_url(player, "http://127.0.0.1:8000", (0, 0)).unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let assistant: Vec<_> = parsed
            .query_pairs()
            .filter(|(k, _)| k == "assistant")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(assistant, ["1"], "{url}");
        assert_eq!(url.matches("assistant=").count(), 1, "{url}");
        let (last_key, _) = parsed.query_pairs().last().unwrap();
        assert_eq!(last_key, "assistant", "{url}");
        assert_eq!(
            parsed
                .query_pairs()
                .any(|(k, v)| k == "quality" && v == "low"),
            player.contains("quality=low"),
            "other pairs stay: {url}"
        );
    }
}

#[test]
fn the_talk_origin_is_the_gate_form_of_the_player_url() {
    assert_eq!(
        origin_of_url("https://example.org/play/?x=1#f").as_deref(),
        Some("https://example.org")
    );
    assert_eq!(
        origin_of_url("https://EXAMPLE.org:443/play").as_deref(),
        Some("https://example.org")
    );
    assert_eq!(
        origin_of_url("http://localhost:5173/play/").as_deref(),
        Some("http://localhost:5173")
    );
    assert_eq!(origin_of_url("ftp://example.org/"), None);
    assert_eq!(origin_of_url("example.org/play"), None);
}

/// `--talk` widens the `/api/project/*` gate and nothing else: the data-layer
/// upgrade (the Creator Hub's scene-editing websocket) keeps to the
/// `DCL_SCENE_CLI_ALLOWED_ORIGINS` list, as the README and `--talk` help say.
#[test]
fn the_talk_origin_reaches_the_project_bridge_but_not_the_data_layer() {
    let talk = "https://talk-origin.test";
    let _ = TALK_ORIGIN.set(talk.to_string());
    assert_eq!(
        TALK_ORIGIN.get().map(String::as_str),
        Some(talk),
        "another test set it first"
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "127.0.0.1:8000".parse().unwrap());
    headers.insert(header::ORIGIN, talk.parse().unwrap());

    let bridge = allowed_editor_origins();
    assert!(bridge.iter().any(|o| o == talk), "{bridge:?}");
    assert!(project_bridge::allowed(
        &headers,
        "127.0.0.1:8000".parse().unwrap(),
        &bridge
    ));

    let data_layer = data_layer_origins();
    assert!(!data_layer.iter().any(|o| o == talk), "{data_layer:?}");
    assert!(
        !data_layer_origin_allowed(&headers),
        "the talk origin must not open data-layer sessions"
    );
    assert!(
        data_layer_origin_allowed_by(&headers, &bridge),
        "the origin list is the only difference"
    );
}

#[test]
fn editor_origins_merge_the_env_list_with_the_talk_origin_once() {
    assert!(editor_origins(None, None).is_empty());
    assert_eq!(
        editor_origins(Some(" https://a.test, ,https://b.test "), None),
        ["https://a.test", "https://b.test"]
    );
    assert_eq!(
        editor_origins(None, Some("https://example.org")),
        ["https://example.org"]
    );
    assert_eq!(
        editor_origins(
            Some("https://example.org,https://a.test"),
            Some("https://example.org")
        ),
        ["https://example.org", "https://a.test"]
    );
}

/// Every route that changes the project wants the bearer; every read goes
/// without it, so an older Creator Hub keeps reading. The token gate is
/// checked before the route's own preconditions (a 428 or 503 with the
/// token is the route answering; a 401 without it is the gate).
#[tokio::test]
async fn every_mutating_route_needs_the_token_and_reads_do_not() {
    let (_tmp, a, st) = one_scene("tokengate");
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::new();
    let bearer = format!("Bearer {}", st.token);
    let own: &'static str = format!("http://{addr}").leak();
    let writes: &[(&str, &str, Option<Value>)] = &[
        (
            "PUT",
            "/api/project/file?path=src/index.ts",
            Some(json!({"content":"x","revision":null})),
        ),
        ("DELETE", "/api/project/file?path=src/index.ts", None),
        ("PUT", "/api/project/asset?path=assets/a.glb", None),
        ("DELETE", "/api/project/asset?path=assets/a.glb", None),
        (
            "POST",
            "/api/project/assistant/turn",
            Some(json!({"provider":"codex","prompt":"hi"})),
        ),
        ("DELETE", "/api/project/assistant/turn/abc", None),
        ("DELETE", "/api/project/assistant/conversations/abc", None),
        (
            "POST",
            "/api/project/assistant/mcp",
            Some(json!({"jsonrpc":"2.0","id":1,"method":"ping"})),
        ),
        (
            "POST",
            "/api/project/debug/command",
            Some(json!({"sessionId":1,"cmd":"pause"})),
        ),
        ("POST", "/scene-json", Some(json!({"title":"T"}))),
        ("POST", "/scene-thumbnail", None),
    ];
    for (method, path, body) in writes {
        let send = |token: Option<&str>| {
            let mut req = client
                .request(method.parse().unwrap(), format!("{own}{path}"))
                .header("origin", own);
            if let Some(token) = token {
                req = req.header("authorization", token);
            }
            if let Some(body) = body {
                req = req.json(body);
            } else if *path == "/scene-thumbnail" {
                req = req
                    .header("content-type", "image/png")
                    .body(b"\x89PNG\r\n\x1a\n".to_vec());
            }
            req.send()
        };
        let bare = send(None).await.unwrap();
        assert_eq!(
            bare.status().as_u16(),
            401,
            "{method} {path} without the token"
        );
        assert!(
            bare.text().await.unwrap().contains("Authorization: Bearer"),
            "{method} {path} hints at the token"
        );
        let wrong = send(Some("Bearer nope")).await.unwrap();
        assert_eq!(
            wrong.status().as_u16(),
            401,
            "{method} {path} with a wrong token"
        );
        let with = send(Some(&bearer)).await.unwrap().status().as_u16();
        assert!(
            with != 401 && with != 403,
            "{method} {path} with the token answered {with}"
        );
    }
    let data_layer = client
        .get(format!("{own}/data-layer"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(
        data_layer.status().as_u16(),
        401,
        "the data-layer upgrade wants the token too"
    );
    let data_layer = client
        .get(format!("{own}/data-layer?token={}", st.token))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(
        data_layer.status().as_u16(),
        404,
        "with it, the upgrade reaches the route (no data layer in this preview)"
    );
    for path in [
        "/api/project",
        "/api/project/files",
        "/api/project/file?path=scene.json",
        "/api/project/assets",
        "/api/project/assistant/conversations",
        "/api/project/assistant/providers",
        "/api/project/debug",
        "/scene.json",
        "/about",
    ] {
        let status = client
            .get(format!("{own}{path}"))
            .header("origin", own)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_ne!(status, 401, "GET {path} needs no token");
        assert_ne!(status, 403, "GET {path} needs no token");
    }
    assert!(a.root.join("scene.json").is_file());
    server.abort();
}

/// The token is server-rendered into the page for a top-level navigation
/// (`Sec-Fetch-Mode: navigate` and `Sec-Fetch-Dest: document`) on a
/// loopback host, and into the Creator Hub link's fragment there. A
/// same-origin fetch of `/` (what a scene in the embedded player can do), a
/// header-less request (what a scene in the native client sends), a framed
/// navigation, a cross-origin fetch, a LAN host, a tunnel replay and a
/// proxied request all get the page without it; a request that already
/// holds the token as the bearer gets it back (a page redrawing itself).
#[tokio::test]
async fn the_token_rides_only_top_level_navigations_on_a_loopback_host() {
    let (_tmp, _a, st) = one_scene("tokenmeta");
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::new();
    let meta = format!(
        "<meta name=\"{}\" content=\"{}\">",
        landing::TOKEN_META,
        st.token
    );
    let page = |path: &'static str, headers: Vec<(&'static str, String)>| {
        let client = client.clone();
        async move {
            let mut req = client
                .get(format!("http://{addr}{path}"))
                .header("accept", "text/html");
            for (k, v) in headers {
                req = req.header(k, v);
            }
            req.send().await.unwrap().text().await.unwrap()
        }
    };
    let top_level = || {
        vec![
            ("sec-fetch-site", "none".to_string()),
            ("sec-fetch-mode", "navigate".to_string()),
            ("sec-fetch-dest", "document".to_string()),
        ]
    };
    let typed = page("/", top_level()).await;
    assert!(typed.contains(&meta), "a typed navigation: {typed:.300}");
    assert!(
        super::project_bridge::creator_url("x").is_none()
            || typed.contains(&format!(
                "projectUrl=http%3A%2F%2F{addr}#token={}",
                st.token
            ))
            || typed.contains(&format!("#token={}\"", st.token)),
        "the Creator Hub link, when the build names one, carries the fragment: {typed:.400}"
    );
    assert!(
        page("/scene", top_level()).await.contains(&meta),
        "/scene edits too"
    );
    let linked = page(
        "/",
        vec![
            ("sec-fetch-site", "cross-site".into()),
            ("sec-fetch-mode", "navigate".into()),
            ("sec-fetch-dest", "document".into()),
        ],
    )
    .await;
    assert!(
        linked.contains(&meta),
        "a link from elsewhere is a top-level navigation the page's own scripts read"
    );
    let mut localhost = top_level();
    localhost.push(("host", format!("localhost:{}", addr.port())));
    assert!(page("/", localhost).await.contains(&meta));
    let redrawn = page(
        "/",
        vec![
            ("sec-fetch-site", "same-origin".into()),
            ("sec-fetch-mode", "cors".into()),
            ("authorization", format!("Bearer {}", st.token)),
        ],
    )
    .await;
    assert!(
        redrawn.contains(&meta),
        "a page redrawing itself with the bearer gets it back"
    );

    let plain = page("/", vec![]).await;
    assert!(
        !plain.contains(&st.token),
        "no Fetch Metadata (a scene in the native client, curl) gets none: {plain:.300}"
    );
    let scene_fetch = page(
        "/",
        vec![
            ("sec-fetch-site", "same-origin".into()),
            ("sec-fetch-mode", "cors".into()),
            ("sec-fetch-dest", "empty".into()),
        ],
    )
    .await;
    assert!(
        !scene_fetch.contains(&st.token),
        "a same-origin fetch (a scene in the embedded player) gets none"
    );
    let untyped_navigate = page(
        "/",
        vec![
            ("sec-fetch-site", "none".into()),
            ("sec-fetch-mode", "navigate".into()),
        ],
    )
    .await;
    assert!(
        !untyped_navigate.contains(&st.token),
        "navigate without a document destination is not a top-level page"
    );
    let framed = page(
        "/",
        vec![
            ("sec-fetch-site", "cross-site".into()),
            ("sec-fetch-mode", "navigate".into()),
            ("sec-fetch-dest", "iframe".into()),
        ],
    )
    .await;
    assert!(
        !framed.contains(&st.token),
        "a cross-site iframe navigates but gets none"
    );
    let fetched = page("/", vec![("origin", "https://evil.example".into())]).await;
    assert!(
        !fetched.contains(&st.token),
        "a cross-origin fetch carries Origin and gets none"
    );
    let fetched = page(
        "/",
        vec![
            ("sec-fetch-site", "cross-site".into()),
            ("sec-fetch-mode", "cors".into()),
        ],
    )
    .await;
    assert!(!fetched.contains(&st.token));
    let mut lan = top_level();
    lan.push(("host", format!("192.168.1.20:{}", addr.port())));
    assert!(
        !page("/", lan).await.contains(&st.token),
        "a LAN host gets the page without it"
    );
    let mut rebound = top_level();
    rebound.push(("host", "evil.example".into()));
    assert!(!page("/", rebound).await.contains(&st.token));
    let mut tunnel = top_level();
    tunnel.push((crate::tunnel::FORWARDED_HEADER, "1".into()));
    assert!(!page("/", tunnel).await.contains(&st.token));
    let mut proxied = top_level();
    proxied.push(("x-forwarded-for", "203.0.113.2".into()));
    let proxied = page("/", proxied).await;
    assert!(!proxied.contains(&st.token));
    assert!(
        !proxied.contains("#token="),
        "and the Creator Hub link stays bare: {proxied:.200}"
    );
    server.abort();
}

/// The pages that carry the token or a form token refuse to be framed by
/// another origin: `/` in a cross-site iframe would otherwise be an editor
/// whose every click posts with the token the page's own scripts attach.
#[tokio::test]
async fn token_bearing_pages_refuse_a_foreign_frame() {
    let (_tmp, _a, st) = one_scene("frames");
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::new();
    for path in ["/", "/scene", "/deploy", "/target", "/storage"] {
        let resp = client
            .get(format!("http://{addr}{path}"))
            .header("accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{path}");
        assert_eq!(resp.headers()["x-frame-options"], "SAMEORIGIN", "{path}");
        assert_eq!(
            resp.headers()["content-security-policy"],
            "frame-ancestors 'self'",
            "{path}"
        );
    }
    server.abort();
}

/// The deploy pages' form token (what `/target/*`, `POST /deploy` and
/// `/storage/target` check) is rendered on the run token's terms only: a
/// scene in the embedded player fetching `/deploy` same-origin used to read
/// it out of the HTML and could then move the deploy target.
#[tokio::test]
async fn the_form_token_is_rendered_only_where_the_run_token_is() {
    let (_tmp, _a, st) = one_scene("formtoken");
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::new();
    let form_token = deploy_page::token(&st).to_string();
    let filled = format!("name=\"token\" value=\"{form_token}\"");
    let empty = "name=\"token\" value=\"\"";
    for path in ["/", "/scene", "/deploy", "/target", "/storage"] {
        let scene_fetch = client
            .get(format!("http://{addr}{path}"))
            .header("accept", "text/html")
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-dest", "empty")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !scene_fetch.contains(&form_token),
            "{path} fetched same-origin carries no form token"
        );
        assert!(scene_fetch.contains(empty), "{path}: {scene_fetch:.400}");
        let navigated = client
            .get(format!("http://{addr}{path}"))
            .header("accept", "text/html")
            .header("sec-fetch-site", "none")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-dest", "document")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            navigated.contains(&filled),
            "{path} navigated to: {navigated:.400}"
        );
        let redrawn = client
            .get(format!("http://{addr}{path}"))
            .header("accept", "text/html")
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "cors")
            .header("authorization", format!("Bearer {}", st.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(redrawn.contains(&filled), "{path} redrawn with the bearer");
        if path != "/" && path != "/scene" {
            assert!(
                redrawn.contains(&format!("name=\"{}\"", landing::TOKEN_META)),
                "{path} carries the run token's meta for its scripts"
            );
        }
    }
    // A cross-site page cannot post the form token it never saw; the
    // navigation's own form can.
    let posted = |tok: &str| {
        client
            .post(format!("http://{addr}/target/address"))
            .header("origin", format!("http://{addr}"))
            .form(&[
                ("token", tok),
                ("address", "0x1111111111111111111111111111111111111111"),
            ])
            .send()
    };
    assert_eq!(posted("").await.unwrap().status().as_u16(), 403);
    assert_ne!(posted(&form_token).await.unwrap().status().as_u16(), 403);
    server.abort();
}

/// `--talk --open` hands the opener `/talk`, never the player URL with the
/// token: the argv of xdg-open and its helpers is readable by every local
/// user for as long as they live. `/talk` then redirects a top-level
/// navigation to the player with `#token=`, and anything else to the bare
/// player URL.
#[tokio::test]
async fn the_browser_opener_never_sees_the_token() {
    let (_tmp, _a, st) = one_scene("talk");
    let (addr, server) = serve(&st).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let opener = talk_opener_url(addr.port());
    assert_eq!(opener, format!("http://127.0.0.1:{}/talk", addr.port()));
    assert!(!opener.contains('#'));
    let not_talking = client.get(&opener).send().await.unwrap();
    assert_eq!(not_talking.status().as_u16(), 404);
    assert!(not_talking.text().await.unwrap().contains("--talk"));

    let player =
        format!("http://{addr}/play/?preview=true&realm=http://{addr}&position=0,0&assistant=1");
    st.talk_url.set(player.clone()).unwrap();
    let opened = client
        .get(&opener)
        .header("sec-fetch-site", "none")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-dest", "document")
        .send()
        .await
        .unwrap();
    assert_eq!(opened.status().as_u16(), 303);
    assert_eq!(
        opened.headers()["location"],
        format!("{player}#token={}", st.token)
    );
    let fetched = client
        .get(&opener)
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "cors")
        .send()
        .await
        .unwrap();
    assert_eq!(fetched.status().as_u16(), 303);
    assert_eq!(fetched.headers()["location"], player);
    let bare = client.get(&opener).send().await.unwrap();
    assert_eq!(
        bare.headers()["location"],
        player,
        "no Fetch Metadata, no token"
    );
    server.abort();
}

/// The Origin gate compares a parsed origin — scheme, host, port — with the
/// Host header, not a suffix: `http://evil127.0.0.1:8000` used to pass for
/// `127.0.0.1:8000`.
#[test]
fn cross_origin_refusal_compares_the_parsed_origin_exactly() {
    let refused = |host: &str, origin: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        headers.insert(header::ORIGIN, origin.parse().unwrap());
        cross_origin_refusal(&headers).is_some()
    };
    assert!(!refused("127.0.0.1:8000", "http://127.0.0.1:8000"));
    assert!(!refused("localhost:8000", "http://LOCALHOST:8000"));
    assert!(!refused("localhost", "http://localhost"));
    assert!(!refused("localhost:80", "http://localhost"));
    assert!(refused("127.0.0.1:8000", "http://evil127.0.0.1:8000"));
    assert!(refused(
        "127.0.0.1:8000",
        "http://evil.example/127.0.0.1:8000"
    ));
    assert!(refused("127.0.0.1:8000", "http://127.0.0.1:8001"));
    assert!(refused("127.0.0.1:8000", "http://127.0.0.1"));
    assert!(refused("127.0.0.1:8000", "https://127.0.0.1:8000"));
    assert!(refused("127.0.0.1:8000", "null"));
    assert!(refused("127.0.0.1:8000", "127.0.0.1:8000"));
    assert!(refused("", "http://127.0.0.1:8000"));
    let mut https = HeaderMap::new();
    https.insert(header::HOST, "preview.example".parse().unwrap());
    https.insert(header::ORIGIN, "https://preview.example".parse().unwrap());
    https.insert("x-forwarded-proto", "https".parse().unwrap());
    assert!(
        cross_origin_refusal(&https).is_none(),
        "behind a TLS tunnel the scheme is the forwarded one"
    );
    let mut none = HeaderMap::new();
    none.insert(header::HOST, "127.0.0.1:8000".parse().unwrap());
    assert!(
        cross_origin_refusal(&none).is_none(),
        "no Origin is a same-origin or non-browser request"
    );
}

/// The printed URLs carry the token as a fragment, never a query: after the
/// fragment a player URL already had, so `mcptoken` and this coexist.
#[test]
fn printed_urls_carry_the_token_as_a_fragment() {
    assert_eq!(
        with_token_fragment(
            "http://127.0.0.1:8000/play/?preview=true&assistant=1",
            "abc"
        ),
        "http://127.0.0.1:8000/play/?preview=true&assistant=1#token=abc"
    );
    assert_eq!(
        with_token_fragment("https://example.org/play/?assistant=1#mcptoken=m", "abc"),
        "https://example.org/play/?assistant=1#mcptoken=m&token=abc"
    );
    assert_eq!(
        with_token_fragment("https://example.org/play/#", "abc"),
        "https://example.org/play/#token=abc"
    );
    assert_eq!(
        with_token_fragment(
            "https://example.org/create?projectUrl=http%3A%2F%2F127.0.0.1%3A8000",
            "abc"
        ),
        "https://example.org/create?projectUrl=http%3A%2F%2F127.0.0.1%3A8000#token=abc"
    );
    let token = mint_token();
    assert_eq!(token.len(), 43, "32 bytes, base64url, unpadded: {token}");
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert_ne!(token, mint_token());
    let st = state(vec![]);
    let mut headers = HeaderMap::new();
    assert!(!bearer_ok(&st, &headers));
    headers.insert(
        header::AUTHORIZATION,
        format!("bearer  {} ", st.token).parse().unwrap(),
    );
    assert!(
        bearer_ok(&st, &headers),
        "scheme case and padding do not matter"
    );
    headers.insert(
        header::AUTHORIZATION,
        format!("Basic {}", st.token).parse().unwrap(),
    );
    assert!(!bearer_ok(&st, &headers));
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {}x", st.token).parse().unwrap(),
    );
    assert!(!bearer_ok(&st, &headers));
    assert!(bearer_or_query_ok(
        &st,
        &HeaderMap::new(),
        Some(&format!("a=1&token={}", st.token))
    ));
    assert!(!bearer_or_query_ok(
        &st,
        &HeaderMap::new(),
        Some("token=nope")
    ));
    assert!(!bearer_or_query_ok(&st, &HeaderMap::new(), None));
}

/// The site the embedded HUD reads its services from: the first player
/// origin that is not this machine, else the build's site. A loopback,
/// unspecified or `.localhost` host is this machine whatever its spelling.
#[test]
fn play_site_is_the_first_player_origin_off_this_machine() {
    assert_eq!(
        play_site(None, None),
        play_embed::default_site().unwrap_or_default()
    );
    assert_eq!(
        play_site(Some("https://example.org/play/?x=1"), None),
        "https://example.org"
    );
    assert_eq!(
        play_site(
            Some("http://127.0.0.1:8160/play/"),
            Some("https://self.host:8443/play")
        ),
        "https://self.host:8443"
    );
    for local in [
        "http://127.0.0.1:8000/play/",
        "http://127.0.0.2:8000/play/",
        "http://0.0.0.0:8000/play/",
        "http://localhost:8000/play/",
        "http://app.localhost:8000/play/",
        "http://[::1]:8000/play/",
        "http://[0:0:0:0:0:0:0:1]:8000/play/",
    ] {
        assert_eq!(
            play_site(Some(local), None),
            play_embed::default_site().unwrap_or_default(),
            "{local}"
        );
    }
    assert_eq!(
        play_site(Some("  "), Some("not a url")),
        play_embed::default_site().unwrap_or_default()
    );
    assert_eq!(
        play_site(Some("ftp://x/play"), None),
        play_embed::default_site().unwrap_or_default()
    );
    assert!(!is_local_origin("https://example.org"));
    assert!(is_local_origin("http://[::]:1"));
}

#[test]
fn public_url_is_an_http_origin_with_an_optional_prefix() {
    let public = PublicUrl::parse("https://Preview.Example.com/p/").unwrap();
    assert_eq!(public.origin(), "https://preview.example.com/p");
    assert_eq!(
        PublicUrl::parse("http://example.com:8080")
            .unwrap()
            .origin(),
        "http://example.com:8080"
    );
    for bad in [
        "preview.example.com",
        "ftp://example.com",
        "https://example.com/?x=1",
        "https://example.com/#f",
        "https://user:pw@example.com",
    ] {
        let err = PublicUrl::parse(bad).unwrap_err();
        assert!(
            crate::ux::render(&err, false, false)
                .contains("--public-url https://preview.example.com"),
            "{bad} must be refused with the form to pass"
        );
    }
}

#[test]
fn public_url_names_a_request_by_forwarded_host_then_host() {
    let public = PublicUrl::parse("https://preview.example.com").unwrap();
    let with = |name: &'static str, value: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert(name, axum::http::HeaderValue::from_static(value));
        headers
    };
    assert!(public.names(&with("host", "preview.example.com")));
    assert!(public.names(&with("host", "PREVIEW.example.com:443")));
    assert!(public.names(&with("x-forwarded-host", "preview.example.com")));
    assert!(!public.names(&with("host", "127.0.0.1:8000")));
    assert!(!public.names(&with("host", "preview.example.com:8443")));
    assert!(!public.names(&with("host", "evil-preview.example.com")));
    let mut proxied = with("host", "preview.example.com");
    proxied.insert(
        "x-forwarded-host",
        axum::http::HeaderValue::from_static("other.example"),
    );
    assert!(
        !public.names(&proxied),
        "a proxy's forwarded host wins over Host, as it does for every advertised URL"
    );
}

/// A warning `start` printed is what `/api/problems` serves and what every
/// page's banner polls, steps included, until it stops being true.
#[tokio::test]
async fn open_problems_are_served_and_drawn_by_the_pages() {
    let tmp = Tmp::new("problems-route");
    let st = Arc::new(state(vec![testkit::scene(
        &tmp.0,
        "s",
        &["0,0"],
        "export {}",
    )]));
    let (addr, server) = serve(&st).await;
    let failed: anyhow::Error = UserError::new(
        "problems-route build failed",
        TrySteps::one("fix the errors above"),
    )
    .why("src/index.ts:1:1 unexpected token")
    .into();
    ux::report_build(&failed);

    let response = fetch(addr, PROBLEMS_PATH).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body: Value = response.json().await.unwrap();
    let mine = body["problems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["what"] == "problems-route build failed")
        .expect("the reported problem is open");
    assert_eq!(mine["topic"], "build");
    assert_eq!(mine["why"], "src/index.ts:1:1 unexpected token");
    assert_eq!(mine["steps"], json!(["fix the errors above"]));
    assert!(body["revision"].as_u64().unwrap() > 0);

    let page = navigate(addr, "/").await.text().await.unwrap();
    assert!(
        page.contains(
            r#"id="preview-problems" class="problems" data-problems-url="/api/problems""#
        ),
        "the page names the route its banner polls"
    );
    ux::resolve(ux::Topic::Build);
    server.abort();
}

/// `--multi-instance` and `--skip-auth-screen`: with neither, the link has
/// both parameters; with a flag, only the parameters of the given flags.
#[test]
fn the_link_flags_select_the_parameters_the_deep_link_turns_on() {
    assert_eq!(
        link_defaults(false, false),
        ["multi-instance", "skip-auth-screen"]
    );
    assert_eq!(link_defaults(true, false), ["multi-instance"]);
    assert_eq!(link_defaults(false, true), ["skip-auth-screen"]);
    assert_eq!(
        link_defaults(true, true),
        ["multi-instance", "skip-auth-screen"]
    );
}
