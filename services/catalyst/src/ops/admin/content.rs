//! The content server's own console operations, at their old paths:
//!
//! - `POST /admin/api/content/denylist/{add,remove}` `{id}` (aliases
//!   `entity_id`, `entityId`): the durable operator denylist
//!   (`content.denylist`, [`crate::content::denylist`]). The row and its
//!   `ops.admin_audit` row land in one transaction; the in-process projection
//!   the reads consult follows at once. A durable operator write, so behind
//!   `OPS_WRITES_ENABLED` (the 503 envelope) like the failed-deployments
//!   purge. `POST /admin/api/content/denylist/list`: the table's ids, open.
//! - `POST /admin/api/content/challenge/refresh`: replaces this process's
//!   `/challenge` text (otherwise one UUID per process, as upstream).
//! - `POST /admin/api/content/read-only` `{enabled}`: the old `READ_ONLY`
//!   toggle; while on, `POST /entities` and `DELETE /scenes/{coord}` answer
//!   the old 403 (`CONTENT_WRITES_ENABLED` is the static gate in front).
//! - `POST /admin/api/content/accepting-users` `{enabled}`: whether `/about`
//!   reports `acceptingUsers` on a healthy node.
//!
//! The last three change process state only (a restart restores the
//! defaults: writable, accepting), so they stay open while operator writes
//! are disabled, like the sync controls, and are audited after the effect.
//! Authorization is the console's: every route takes [`AdminSession`] (403
//! without an allowlisted same-origin cookie), checked before the switch.
//! Envelopes are the old `{ok, status, body}`.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;
use serde_json::json;

use super::api::{AdminEnvelope, ops_refused};
use super::auth::AdminSession;
use super::{SharedState, audit};
use crate::content::denylist;

const DOMAIN: &str = "content";

pub const DENYLIST_ADD: &str = "/admin/api/content/denylist/add";
pub const DENYLIST_REMOVE: &str = "/admin/api/content/denylist/remove";
pub const DENYLIST_LIST: &str = "/admin/api/content/denylist/list";
pub const CHALLENGE_REFRESH: &str = "/admin/api/content/challenge/refresh";
pub const READ_ONLY: &str = "/admin/api/content/read-only";
pub const ACCEPTING_USERS: &str = "/admin/api/content/accepting-users";

/// Every route of this router, for the gating tests.
pub const ROUTES: &[(&str, &str)] = &[
    ("POST", DENYLIST_ADD),
    ("POST", DENYLIST_REMOVE),
    ("POST", DENYLIST_LIST),
    ("POST", CHALLENGE_REFRESH),
    ("POST", READ_ONLY),
    ("POST", ACCEPTING_USERS),
];

/// The routes at their old paths, for the integrator to merge beside
/// [`super::router`] (both at the root).
pub fn router(admin: SharedState) -> Router {
    Router::new()
        .route(DENYLIST_ADD, post(denylist_add))
        .route(DENYLIST_REMOVE, post(denylist_remove))
        .route(DENYLIST_LIST, post(denylist_list))
        .route(CHALLENGE_REFRESH, post(challenge_refresh))
        .route(READ_ONLY, post(read_only))
        .route(ACCEPTING_USERS, post(accepting_users))
        .with_state(admin)
}

#[derive(Deserialize)]
pub struct DenylistReq {
    #[serde(alias = "entity_id", alias = "entityId")]
    pub id: String,
}

#[derive(Deserialize)]
pub struct ToggleReq {
    pub enabled: bool,
}

fn internal(e: impl std::fmt::Display) -> Response {
    AdminEnvelope::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": e.to_string() }),
    )
}

/// One denylist edit: the row and its audit row in one transaction, then
/// the projection; answers whether the table changed.
async fn denylist_edit(
    state: &SharedState,
    session: &AdminSession,
    id: &str,
    add: bool,
) -> Result<bool, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let changed = if add {
        denylist::add(&mut *tx, id, session.address()).await?
    } else {
        denylist::remove(&mut *tx, id).await?
    };
    let (action, detail) = if add {
        ("denylist.add", json!({ "id": id, "added": changed }))
    } else {
        ("denylist.remove", json!({ "id": id, "removed": changed }))
    };
    let entry = audit::Entry::new(session.address(), DOMAIN, action)
        .target(Some(id))
        .detail(detail);
    audit::record(&mut *tx, &entry).await?;
    tx.commit().await?;
    if add {
        state.content.denylist.operator_insert(id);
    } else {
        state.content.denylist.operator_remove(id);
    }
    Ok(changed)
}

pub async fn denylist_add(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(req): Json<DenylistReq>,
) -> Response {
    if let Some(refused) = ops_refused(state.settings.writes_enabled) {
        return refused;
    }
    if !denylist::valid_id(&req.id) {
        return AdminEnvelope::error(StatusCode::BAD_REQUEST, json!({ "error": "invalid-id" }));
    }
    match denylist_edit(&state, &session, &req.id, true).await {
        Ok(added) => AdminEnvelope::success(json!({ "added": added })),
        Err(e) => internal(e),
    }
}

pub async fn denylist_remove(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(req): Json<DenylistReq>,
) -> Response {
    if let Some(refused) = ops_refused(state.settings.writes_enabled) {
        return refused;
    }
    if !denylist::valid_id(&req.id) {
        return AdminEnvelope::error(StatusCode::BAD_REQUEST, json!({ "error": "invalid-id" }));
    }
    match denylist_edit(&state, &session, &req.id, false).await {
        Ok(removed) => AdminEnvelope::success(json!({ "removed": removed })),
        Err(e) => internal(e),
    }
}

/// The table's ids (the authority, not the projection), audited as the old
/// console audited its list read.
pub async fn denylist_list(session: AdminSession, State(state): State<SharedState>) -> Response {
    match denylist::load(&state.pool).await {
        Ok(ids) => {
            audit::record_after(
                &state.pool,
                &audit::Entry::new(session.address(), DOMAIN, "denylist.list")
                    .detail(json!({ "count": ids.len() })),
            )
            .await;
            AdminEnvelope::success(json!({ "ids": ids }))
        }
        Err(e) => internal(e),
    }
}

pub async fn challenge_refresh(
    session: AdminSession,
    State(state): State<SharedState>,
) -> Response {
    let text = state.content.refresh_challenge();
    audit::record_after(
        &state.pool,
        &audit::Entry::new(session.address(), DOMAIN, "challenge.refresh"),
    )
    .await;
    AdminEnvelope::success(json!({ "challenge": text }))
}

pub async fn read_only(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(req): Json<ToggleReq>,
) -> Response {
    let now = state.content.set_read_only(req.enabled);
    audit::record_after(
        &state.pool,
        &audit::Entry::new(session.address(), DOMAIN, "read-only")
            .detail(json!({ "enabled": req.enabled })),
    )
    .await;
    AdminEnvelope::success(json!({ "readOnly": now }))
}

pub async fn accepting_users(
    session: AdminSession,
    State(state): State<SharedState>,
    Json(req): Json<ToggleReq>,
) -> Response {
    let now = state.content.set_accepting_users(req.enabled);
    audit::record_after(
        &state.pool,
        &audit::Entry::new(session.address(), DOMAIN, "accepting-users")
            .detail(json!({ "enabled": req.enabled })),
    )
    .await;
    AdminEnvelope::success(json!({ "acceptingUsers": now }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use serde_json::Value;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use super::*;
    use crate::content::{ContentState, Settings as ContentSettings, SyncState};
    use crate::ops::admin::tests::{HOST, admin_state, lazy_pool, minted_cookie as cookie};

    const ADMIN: &str = "0x00000000000000000000000000000000000000aa";

    async fn harness(pool: PgPool, writes_enabled: bool) -> (Router, SharedState) {
        let admin = Arc::new(admin_state(pool, writes_enabled, ADMIN, None).await);
        (router(admin.clone()), admin)
    }

    async fn post(
        app: &Router,
        path: &str,
        cookie: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn every_route_is_403_without_an_allowlisted_session() {
        let (app, admin) = harness(lazy_pool(), true).await;
        let stranger = cookie(&admin, "0x00000000000000000000000000000000000000bb");
        for (_, path) in ROUTES {
            assert_eq!(
                post(&app, path, None, json!({"id": "bafy", "enabled": true}))
                    .await
                    .0,
                StatusCode::FORBIDDEN,
                "{path}"
            );
            assert_eq!(
                post(
                    &app,
                    path,
                    Some(&stranger),
                    json!({"id": "bafy", "enabled": true})
                )
                .await
                .0,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        assert!(
            !admin.content.is_read_only(),
            "a refused call changed nothing"
        );
    }

    /// The process toggles work while operator writes are off; the durable
    /// edits answer the 503 envelope with `Retry-After`.
    #[tokio::test]
    async fn toggles_stay_open_and_denylist_edits_follow_ops_writes() {
        let (app, admin) = harness(lazy_pool(), false).await;
        let admin_cookie = cookie(&admin, ADMIN);

        let (status, body) = post(
            &app,
            READ_ONLY,
            Some(&admin_cookie),
            json!({"enabled": true}),
        )
        .await;
        assert_eq!(
            (status, body),
            (
                StatusCode::OK,
                json!({"ok": true, "status": 200, "body": {"readOnly": true}})
            )
        );
        assert!(admin.content.is_read_only());
        post(
            &app,
            READ_ONLY,
            Some(&admin_cookie),
            json!({"enabled": false}),
        )
        .await;
        assert!(!admin.content.is_read_only());

        admin.content.sync.set_state(SyncState::Syncing);
        let (_, body) = post(
            &app,
            ACCEPTING_USERS,
            Some(&admin_cookie),
            json!({"enabled": false}),
        )
        .await;
        assert_eq!(body["body"], json!({"acceptingUsers": false}));
        assert!(!admin.content.is_accepting_users());

        let before = admin.content.challenge();
        let (status, body) = post(&app, CHALLENGE_REFRESH, Some(&admin_cookie), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let refreshed = body["body"]["challenge"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(refreshed).is_ok(), "{refreshed}");
        assert_ne!(refreshed, before);
        assert_eq!(admin.content.challenge(), refreshed);

        for path in [DENYLIST_ADD, DENYLIST_REMOVE] {
            let req = Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, HOST)
                .header(header::COOKIE, &admin_cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"id": "bafy"}).to_string()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(resp.headers()[header::RETRY_AFTER], "3600");
        }
        assert!(admin.content.denylist.operator_ids().is_empty());
    }

    /// A durable edit: the row, its audit row in the same transaction, the
    /// projection the reads use, and the list from the table; a malformed
    /// id is the old 400.
    #[tokio::test]
    async fn denylist_edits_are_durable_audited_and_projected() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
            return;
        };
        let (app, admin) = harness(pool.clone(), true).await;
        let admin_cookie = cookie(&admin, ADMIN);
        let id = format!("bafy-console-{}", uuid::Uuid::new_v4().simple());

        let (status, body) = post(
            &app,
            DENYLIST_ADD,
            Some(&admin_cookie),
            json!({"entityId": id}),
        )
        .await;
        assert_eq!(
            (status, &body["body"]),
            (StatusCode::OK, &json!({"added": true})),
            "{body}"
        );
        assert!(
            admin.content.denylist.contains(&id),
            "the reads see it at once"
        );
        let (_, body) = post(&app, DENYLIST_ADD, Some(&admin_cookie), json!({"id": id})).await;
        assert_eq!(body["body"], json!({"added": false}));

        let (_, body) = post(&app, DENYLIST_LIST, Some(&admin_cookie), json!({})).await;
        assert!(
            body["body"]["ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == &json!(id))
        );

        let restarted = ContentState::new(
            pool.clone(),
            ContentSettings::for_tests(),
            admin.content.storage.clone(),
        );
        restarted.load_denylist().await.unwrap();
        assert!(restarted.denylist.contains(&id), "a restart loads it back");

        let audited: Vec<(String, String, Value)> = sqlx::query_as(
            "SELECT action, actor, detail FROM ops.admin_audit WHERE domain = 'content' AND target = $1 ORDER BY id",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(audited.len(), 2);
        assert_eq!(
            (audited[0].0.as_str(), audited[0].1.as_str()),
            ("denylist.add", ADMIN)
        );
        assert_eq!(audited[0].2["added"], true);

        let (_, body) = post(
            &app,
            DENYLIST_REMOVE,
            Some(&admin_cookie),
            json!({"entity_id": id}),
        )
        .await;
        assert_eq!(body["body"], json!({"removed": true}));
        assert!(!admin.content.denylist.contains(&id));
        let (_, body) = post(
            &app,
            DENYLIST_REMOVE,
            Some(&admin_cookie),
            json!({"id": id}),
        )
        .await;
        assert_eq!(body["body"], json!({"removed": false}));

        let (status, body) = post(
            &app,
            DENYLIST_ADD,
            Some(&admin_cookie),
            json!({"id": "has space"}),
        )
        .await;
        assert_eq!(
            (status, body["body"].clone()),
            (StatusCode::BAD_REQUEST, json!({"error": "invalid-id"}))
        );

        sqlx::query("DELETE FROM ops.admin_audit WHERE target = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }
}
