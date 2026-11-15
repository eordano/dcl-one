//! The console API over the process's scratch database (`db::scratch`;
//! without `CATALYST_TEST_DATABASE_URL` the DB-backed tests skip). Rows are
//! scoped by a random wallet or a uuid entity id and deleted at the end.

use std::sync::Arc;

use alloy::signers::{Signer as _, local::PrivateKeySigner};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use futures::FutureExt as _;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::{
    ADMIN_API_ROUTES, OPEN_WHILE_WRITES_DISABLED, RawSettings, Settings, SharedState, State, audit,
    router, session,
};
use crate::content::sync::orchestrator::SyncControl;
use crate::content::{ContentState, Settings as ContentSettings};
use crate::jobs::Jobs;
use crate::storage::Storage;

pub(super) const HOST: &str = "realm.test";
pub(super) const SECRET: &str = "test-secret-key-0123456789";

pub(super) fn lazy_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
        .unwrap()
}

pub(super) fn wallet(signer: &PrivateKeySigner) -> String {
    format!("{:#x}", signer.address())
}

/// The console state every suite builds on: content over a private temp
/// storage, the shared session secret, `admins` as the allowlist and an idle
/// job runner. Callers add their domains and routers.
pub(super) async fn admin_state(
    pool: PgPool,
    writes_enabled: bool,
    admins: &str,
    control: Option<SyncControl>,
) -> State {
    let tmp = std::env::temp_dir().join(format!(
        "catalyst-ops-admin-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&tmp).await.unwrap());
    let content = Arc::new(ContentState::new(
        pool.clone(),
        ContentSettings::for_tests(),
        storage,
    ));
    let settings = Settings::parse(RawSettings {
        session_secret: Some(SECRET.to_string()),
        admin_addresses: admins.to_string(),
        session_ttl_secs: 0,
        cookie_insecure: true,
        eth_network: "mainnet".to_string(),
        writes_enabled,
    });
    State::new(pool, settings, content, Arc::new(Jobs::new()), control)
}

/// A session cookie minted directly for `wallet`, skipping the SIWE round
/// trip.
pub(super) fn minted_cookie(state: &State, wallet: &str) -> String {
    format!(
        "cat_admin={}",
        session::mint(&state.settings, wallet).unwrap()
    )
}

async fn harness(
    pool: PgPool,
    writes_enabled: bool,
    control: Option<SyncControl>,
    admins: &str,
) -> (Router, SharedState) {
    let state = Arc::new(admin_state(pool, writes_enabled, admins, control).await);
    (router(state.clone()), state)
}

pub(super) async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = match body {
        Some(v) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, headers, body)
}

fn cookie_of(headers: &HeaderMap) -> String {
    let set = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    let value = set
        .strip_prefix("cat_admin=")
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    format!("cat_admin={value}")
}

/// The SIWE round trip as the console does it: nonce, sign, verify.
async fn sign_in(app: &Router, signer: &PrivateKeySigner) -> (StatusCode, HeaderMap, Value) {
    let (status, _, body) = send(
        app,
        "GET",
        &format!("/admin/auth/nonce?address={}", wallet(signer)),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let message = body["message"].as_str().unwrap().to_string();
    let signature = signer
        .sign_message(message.as_bytes())
        .await
        .unwrap()
        .to_string();
    send(
        app,
        "POST",
        "/admin/auth/verify",
        &[],
        Some(json!({ "message": message, "signature": signature })),
    )
    .await
}

pub(super) async fn session_cookie(app: &Router, signer: &PrivateKeySigner) -> String {
    let (status, headers, body) = sign_in(app, signer).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    cookie_of(&headers)
}

#[tokio::test]
async fn every_admin_api_route_is_403_without_a_valid_same_origin_cookie() {
    let signer = PrivateKeySigner::random();
    let (app, state) = harness(
        lazy_pool(),
        true,
        Some(SyncControl::default()),
        &wallet(&signer),
    )
    .await;
    for (method, path) in ADMIN_API_ROUTES {
        let (status, _, _) = send(&app, method, path, &[], Some(json!({}))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} without a cookie"
        );
    }
    let stranger = session::mint(&state.settings, &wallet(&PrivateKeySigner::random())).unwrap();
    let cookie = format!("cat_admin={stranger}");
    let (status, _, _) = send(&app, "GET", "/admin/api/jobs", &[("cookie", &cookie)], None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a signed cookie for a wallet outside ADMIN_ADDRESSES"
    );

    let cookie = session_cookie(&app, &signer).await;
    let (status, _, _) = send(&app, "GET", "/admin/api/jobs", &[("cookie", &cookie)], None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = send(
        &app,
        "GET",
        "/admin/api/jobs",
        &[("cookie", &cookie), ("origin", "https://evil.test")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _, _) = send(
        &app,
        "GET",
        "/admin/api/jobs",
        &[("cookie", &cookie), ("origin", "https://realm.test")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn siwe_sign_in_admits_an_allowlisted_wallet_and_nobody_else() {
    let admin = PrivateKeySigner::random();
    let (app, state) = harness(lazy_pool(), false, None, &wallet(&admin)).await;

    let (status, headers, body) = sign_in(&app, &admin).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "authenticated": true, "address": wallet(&admin) })
    );
    let set = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Strict") && !set.contains("Secure"),
        "{set}"
    );
    let cookie = cookie_of(&headers);
    let (status, _, body) = send(&app, "GET", "/admin/auth/me", &[("cookie", &cookie)], None).await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({ "authenticated": true, "address": wallet(&admin) })
        )
    );
    let (status, headers, _) = send(&app, "POST", "/admin/auth/logout", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let (status, _, _) = send(&app, "GET", "/admin/auth/me", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Not in the allowlist: the message and signature are fine, the mint is refused.
    let (status, _, _) = sign_in(&app, &PrivateKeySigner::random()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&app, "GET", "/admin/auth/nonce?address=0x123", &[], None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A message minted for one host is not accepted on another, nor with a
    // forged nonce, nor signed by a different key than it names.
    let (_, _, body) = send(
        &app,
        "GET",
        &format!("/admin/auth/nonce?address={}", wallet(&admin)),
        &[],
        None,
    )
    .await;
    let message = body["message"].as_str().unwrap().to_string();
    let signature = admin
        .sign_message(message.as_bytes())
        .await
        .unwrap()
        .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/admin/auth/verify")
        .header(header::HOST, "other.test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "message": message, "signature": signature }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let forged = message.replace("Nonce: ", "Nonce: x");
    let signature = admin
        .sign_message(forged.as_bytes())
        .await
        .unwrap()
        .to_string();
    let (status, _, _) = send(
        &app,
        "POST",
        "/admin/auth/verify",
        &[],
        Some(json!({ "message": forged, "signature": signature })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let other = PrivateKeySigner::random()
        .sign_message(message.as_bytes())
        .await
        .unwrap()
        .to_string();
    let (status, _, _) = send(
        &app,
        "POST",
        "/admin/auth/verify",
        &[],
        Some(json!({ "message": message, "signature": other })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // No secret: the console is off.
    let mut off = Settings::clone(&state.settings);
    off.session_secret = None;
    assert!(!off.admin_enabled());
    let off_state = Arc::new(State::new(
        lazy_pool(),
        off,
        state.content.clone(),
        state.jobs.clone(),
        None,
    ));
    let (status, _, _) = send(
        &router(off_state),
        "GET",
        &format!("/admin/auth/nonce?address={}", wallet(&admin)),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_sync_control_holds_new_passes_while_paused_and_stop_wins() {
    let control = SyncControl::default();
    let cancel = CancellationToken::new();
    assert!(
        !control.wait_while_paused(&cancel).await,
        "not paused: a pass starts at once"
    );

    // Each waiter is polled once, so it is parked on the control before the test acts.
    control.pause();
    assert!(control.is_paused());
    let mut waiter = std::pin::pin!(control.wait_while_paused(&cancel));
    assert!(
        waiter.as_mut().now_or_never().is_none(),
        "paused: the pass is held"
    );
    control.resume();
    assert!(!waiter.await, "resume releases the waiter into the pass");

    control.pause();
    let mut changed = std::pin::pin!(control.changed());
    assert!(changed.as_mut().now_or_never().is_none());
    control.force();
    assert!(!control.is_paused(), "force is resume");
    changed
        .now_or_never()
        .expect("force wakes the steady-state sleep");

    control.pause();
    let mut waiter = std::pin::pin!(control.wait_while_paused(&cancel));
    assert!(
        waiter.as_mut().now_or_never().is_none(),
        "paused: the pass is held"
    );
    cancel.cancel();
    assert!(
        waiter.await,
        "a stop while paused ends the pass instead of holding shutdown"
    );
}

pub(super) async fn audit_rows(pool: &PgPool, actor: &str) -> Vec<audit::Row> {
    audit::list(
        pool,
        &audit::Filter {
            actor: Some(actor.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

pub(super) async fn cleanup(pool: &PgPool, actor: &str) {
    sqlx::query("DELETE FROM ops.admin_audit WHERE actor = $1")
        .bind(actor)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_sync_controls_stay_open_while_writes_are_disabled_and_are_audited() {
    let Some(pool) = crate::db::scratch::pool(4).await else {
        return;
    };
    let admin = PrivateKeySigner::random();
    let actor = wallet(&admin);
    let control = SyncControl::default();
    let (app, _) = harness(pool.clone(), false, Some(control.clone()), &actor).await;
    let cookie = session_cookie(&app, &admin).await;
    let h = [("cookie", cookie.as_str())];

    for path in OPEN_WHILE_WRITES_DISABLED {
        assert!(path.starts_with("/admin/api/content/sync/"), "{path}");
    }
    let (status, _, body) = send(&app, "POST", "/admin/api/content/sync/pause", &h, None).await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({ "ok": true, "status": 200, "body": { "control": "paused" } })
        )
    );
    assert!(control.is_paused());
    let (status, _, body) = send(&app, "GET", "/admin/api/sync", &h, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["body"]["control"], "paused");
    assert_eq!(body["body"]["state"], "Bootstrapping");
    assert_eq!(body["body"]["writesEnabled"], false);
    let (status, _, body) = send(&app, "POST", "/admin/api/content/sync/resume", &h, None).await;
    assert_eq!(
        (status, body["body"]["control"].as_str()),
        (StatusCode::OK, Some("run"))
    );
    assert!(!control.is_paused());
    control.pause();
    let (status, _, body) = send(&app, "POST", "/admin/api/content/sync/force", &h, None).await;
    assert_eq!(
        (status, body["body"]["forced"].as_bool()),
        (StatusCode::OK, Some(true))
    );
    assert!(!control.is_paused());

    let rows = audit_rows(&pool, &actor).await;
    let actions: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.domain.as_str(), r.action.as_str(), r.result.as_str()))
        .collect();
    assert_eq!(
        actions,
        vec![
            ("content", "sync.force", "ok"),
            ("content", "sync.resume", "ok"),
            ("content", "sync.pause", "ok")
        ],
        "newest first"
    );
    assert!(
        rows.iter()
            .all(|r| r.target.is_none() && r.detail == json!({}))
    );

    // No sync worker attached: the old backend's 501, audited as an error.
    let (app, _) = harness(pool.clone(), false, None, &actor).await;
    let cookie = session_cookie(&app, &admin).await;
    let (status, _, body) = send(
        &app,
        "POST",
        "/admin/api/content/sync/pause",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        body["body"]["error"],
        "sync pause not supported by this backend"
    );
    let newest = &audit_rows(&pool, &actor).await[0];
    assert_eq!(
        (newest.action.as_str(), newest.result.as_str()),
        ("sync.pause", "error")
    );
    cleanup(&pool, &actor).await;
}

/// `OPS_WRITES_ENABLED` on, the console's own writes still wait for the
/// restore stamps: a stale drill stamp answers the 503 naming it and its age.
#[tokio::test]
async fn a_stale_restore_stamp_holds_the_console_writes() {
    use crate::shared::writes::restore::{
        Stamp,
        lab::{DAY, StampLab},
        scoped,
    };
    let admin = PrivateKeySigner::random();
    let (app, _) = harness(lazy_pool(), true, None, &wallet(&admin)).await;
    let cookie = session_cookie(&app, &admin).await;
    let h = [("cookie", cookie.as_str())];
    let lab = StampLab::fresh();
    lab.write_aged(Stamp::Drill, 12 * DAY);
    let (status, headers, body) = scoped(
        lab.stamps.clone(),
        send(&app, "POST", "/admin/api/content/flush-cache", &h, None),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(headers.get(header::RETRY_AFTER).unwrap(), "3600");
    assert_eq!(
        body["body"]["message"],
        "operator-plane writes are held on this node: the last passing restore drill (drill.json) is 12 d old, past WRITES_DRILL_MAX_AGE_DAYS=8"
    );
}

#[tokio::test]
async fn gated_writes_answer_503_until_enabled_then_land_with_their_audit_row() {
    let Some(pool) = crate::db::scratch::pool(4).await else {
        return;
    };
    let admin = PrivateKeySigner::random();
    let actor = wallet(&admin);
    let entity = format!("bafy-ops-admin-{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO content.failed_deployments
            (entity_id, entity_type, failure_time, reason, auth_chain, error_description, snapshot_hash, retry_count, next_retry_at)
         VALUES ($1, 'scene', timezone('UTC', now()), 'DEPLOYMENT_ERROR', '[]'::json, 'boom', '', 7, timezone('UTC', now()) + interval '1 day')",
    )
    .bind(&entity)
    .execute(&pool)
    .await
    .unwrap();

    let (app, _) = harness(pool.clone(), false, None, &actor).await;
    let cookie = session_cookie(&app, &admin).await;
    let h = [("cookie", cookie.as_str())];
    for path in [
        "/admin/api/content/flush-cache",
        "/admin/api/content/failed-deployments/retry",
        "/admin/api/content/failed-deployments/clear",
    ] {
        let (status, headers, body) =
            send(&app, "POST", path, &h, Some(json!({ "id": entity }))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
        assert_eq!(headers.get(header::RETRY_AFTER).unwrap(), "3600");
        assert_eq!(body["ok"], false);
        assert!(
            body["body"]["message"]
                .as_str()
                .unwrap()
                .contains("OPS_WRITES_ENABLED=false"),
            "{body}"
        );
    }
    assert!(
        audit_rows(&pool, &actor).await.is_empty(),
        "a refused write is not an operator action"
    );
    let (retry_count,): (i32,) =
        sqlx::query_as("SELECT retry_count FROM content.failed_deployments WHERE entity_id = $1")
            .bind(&entity)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retry_count, 7);

    let (app, state) = harness(pool.clone(), true, None, &actor).await;
    let cookie = session_cookie(&app, &admin).await;
    let h = [("cookie", cookie.as_str())];
    let (status, _, body) = send(
        &app,
        "POST",
        "/admin/api/content/failed-deployments/retry",
        &h,
        Some(json!({ "entityId": entity })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["body"],
        json!({ "entityId": entity, "rearmed": true, "worker": "retry-failed" })
    );
    let (retry_count, due): (i32, chrono::NaiveDateTime) = sqlx::query_as(
        "SELECT retry_count, next_retry_at FROM content.failed_deployments WHERE entity_id = $1",
    )
    .bind(&entity)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (retry_count, due.and_utc().timestamp()),
        (0, 0),
        "re-armed: due at the epoch, attempts reset"
    );
    let (status, _, body) = send(
        &app,
        "POST",
        "/admin/api/content/failed-deployments/retry",
        &h,
        Some(json!({ "id": "bafy-nobody" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _, _) = send(
        &app,
        "POST",
        "/admin/api/content/failed-deployments/retry",
        &h,
        Some(json!({ "id": "has space" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    state
        .content
        .deployments_cache
        .insert("k".to_string(), bytes::Bytes::from_static(b"v"));
    let (status, _, body) = send(&app, "POST", "/admin/api/content/flush-cache", &h, None).await;
    assert_eq!(
        (status, body["body"]["cleared"].as_bool()),
        (StatusCode::OK, Some(true))
    );
    assert_eq!(state.content.deployments_cache.len(), 0);

    let (status, _, body) = send(
        &app,
        "POST",
        "/admin/api/content/failed-deployments/clear",
        &h,
        Some(json!({ "id": entity })),
    )
    .await;
    assert_eq!(
        (status, body["body"].clone()),
        (StatusCode::OK, json!({ "removed": 1, "scope": "one" }))
    );
    let (left,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM content.failed_deployments WHERE entity_id = $1")
            .bind(&entity)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);

    let rows = audit_rows(&pool, &actor).await;
    let summary: Vec<(String, Option<String>, String)> = rows
        .iter()
        .map(|r| (r.action.clone(), r.target.clone(), r.result.clone()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "failed-deployments.clear".into(),
                Some(entity.clone()),
                "ok".into()
            ),
            ("flush-cache".into(), None, "ok".into()),
            (
                "failed-deployments.retry".into(),
                Some("bafy-nobody".into()),
                "not-found".into()
            ),
            (
                "failed-deployments.retry".into(),
                Some(entity.clone()),
                "ok".into()
            ),
        ]
    );
    assert_eq!(
        rows[0].detail,
        json!({ "scope": "one", "entityId": entity, "removed": 1 })
    );
    assert_eq!(rows[1].detail, json!({ "entries": 1 }));
    cleanup(&pool, &actor).await;
}

#[tokio::test]
async fn the_reports_read_the_registry_the_caches_the_cursors_and_the_mirror_rows() {
    let Some(pool) = crate::db::scratch::pool(4).await else {
        return;
    };
    let admin = PrivateKeySigner::random();
    let actor = wallet(&admin);
    let (app, state) = harness(pool.clone(), false, Some(SyncControl::default()), &actor).await;
    let cookie = session_cookie(&app, &admin).await;
    let h = [("cookie", cookie.as_str())];

    state.jobs.spawn("ops-admin-test-job", |ctx| async move {
        ctx.progress("ticking");
        ctx.pass_done();
        ctx.cancel.cancelled().await;
        Ok(())
    });
    tokio::task::yield_now().await;
    let (status, _, body) = send(&app, "GET", "/admin/api/jobs", &h, None).await;
    assert_eq!(status, StatusCode::OK);
    let job = body["body"]["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["name"] == "ops-admin-test-job")
        .unwrap();
    assert_eq!(
        (
            job["state"].as_str(),
            job["progress"].as_str(),
            job["lastError"].is_null(),
            job["passes"].as_u64()
        ),
        (Some("running"), Some("ticking"), true, Some(1))
    );

    state.content.denylist.add("bafy-denied");
    let (status, _, body) = send(&app, "GET", "/admin/api/cache", &h, None).await;
    assert_eq!(
        (status, body["body"].clone()),
        (
            StatusCode::OK,
            json!({ "deployments": { "entries": 0 }, "denylist": { "ids": 1 } })
        )
    );

    let peer = format!(
        "https://peer-{}.test/content",
        uuid::Uuid::new_v4().simple()
    );
    sqlx::query(
        "INSERT INTO content.server_sync_cursors (server_url, cursor_ms) VALUES ($1, 1234)",
    )
    .bind(&peer)
    .execute(&pool)
    .await
    .unwrap();
    let (status, _, body) = send(&app, "GET", "/admin/api/sync", &h, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mine = body["body"]["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["server"] == peer)
        .expect("the seeded cursor");
    assert_eq!(mine["cursorMs"], 1234);
    let failed = &body["body"]["failedDeployments"];
    for key in ["total", "dueNow", "waiting", "withoutAuthChain"] {
        assert!(failed[key].is_number(), "{key}: {failed}");
    }
    assert_eq!(body["body"]["control"], "run");
    sqlx::query("DELETE FROM content.server_sync_cursors WHERE server_url = $1")
        .bind(&peer)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _, body) = send(&app, "GET", "/admin/api/mirrors", &h, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for key in [
        "explore",
        "explorerApi",
        "worlds",
        "marketplace",
        "landAuthz",
    ] {
        assert!(body["body"][key].is_array(), "{key}: {body}");
    }

    // The audit log: three rows for this actor, newest first, keyset paged, filtered by the indexed columns.
    for (i, action) in ["one", "two", "three"].iter().enumerate() {
        let id = audit::record(
            &pool,
            &audit::Entry::new(&actor, "ops", action)
                .target(Some("t"))
                .detail(json!({ "i": i })),
        )
        .await
        .unwrap();
        assert!(id > 0);
    }
    let (status, _, body) = send(
        &app,
        "GET",
        &format!("/admin/api/audit?actor={actor}&limit=2"),
        &h,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["body"]["rows"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["action"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["three", "two"]
    );
    let before = body["body"]["nextBefore"].as_i64().unwrap();
    assert_eq!(before, rows[1]["id"].as_i64().unwrap());
    let (_, _, body) = send(
        &app,
        "GET",
        &format!("/admin/api/audit?actor={actor}&limit=2&before={before}"),
        &h,
        None,
    )
    .await;
    let rows = body["body"]["rows"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["action"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["one"]
    );
    assert!(body["body"]["nextBefore"].is_null(), "the last page");
    let (_, _, body) = send(
        &app,
        "GET",
        &format!("/admin/api/audit?actor={actor}&domain=ops&action=two&target=t"),
        &h,
        None,
    )
    .await;
    assert_eq!(body["body"]["rows"].as_array().unwrap().len(), 1);
    let (_, _, body) = send(
        &app,
        "GET",
        &format!("/admin/api/audit?actor={actor}&domain=content"),
        &h,
        None,
    )
    .await;
    assert_eq!(body["body"]["rows"].as_array().unwrap().len(), 0);

    let err = audit::record(&pool, &audit::Entry::new(&actor, "nope", "x"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown domain"), "{err}");
    cleanup(&pool, &actor).await;
}
