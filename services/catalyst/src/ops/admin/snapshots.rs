//! `POST /admin/api/content/snapshots/regenerate`: the console's request to
//! rebuild the snapshots now (`content::snapshot_gen`). The job runs it at
//! its next wake (within its 15 s tick) once the generator's gate is open,
//! writing every window from the current rows and reusing none (a window
//! whose lines did not change keeps its CID); the answer is the generator's
//! report as it stands.
//!
//! Authorization is the console's (`AdminSession`, 403 without it). Open
//! while `OPS_WRITES_ENABLED` is false, like the sync controls: the request
//! moves the next pass of a job that runs regardless of that switch, and
//! what the pass writes is derived from `content.deployments` (snapshot
//! files and `content.snapshots` rows), never local authority. Audited after
//! the effect (`content` / `snapshots.regenerate`). A node with generation
//! off (`SNAPSHOT_GENERATION_INTERVAL_HOURS=0`) answers the old backend's
//! 501, which the old live node answered always (its generator had no
//! trigger).

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use serde_json::json;

use super::api::AdminEnvelope;
use super::auth::AdminSession;
use super::{SharedState, audit};
use crate::content::snapshot_gen::SnapshotGenerator;

pub const REGENERATE_PATH: &str = "/admin/api/content/snapshots/regenerate";

const UNSUPPORTED: &str = "snapshot regeneration not supported by this backend";

#[derive(Clone)]
pub struct SnapshotsState {
    pub admin: SharedState,
    /// `None` when generation is off.
    pub generator: Option<Arc<SnapshotGenerator>>,
}

impl FromRef<SnapshotsState> for SharedState {
    fn from_ref(state: &SnapshotsState) -> Self {
        state.admin.clone()
    }
}

/// The route at its old path, for the integrator to merge beside
/// [`super::router`] (both at the root).
pub fn router(admin: SharedState, generator: Option<Arc<SnapshotGenerator>>) -> Router {
    Router::new()
        .route(REGENERATE_PATH, post(regenerate))
        .with_state(SnapshotsState { admin, generator })
}

pub async fn regenerate(session: AdminSession, State(state): State<SnapshotsState>) -> Response {
    let entry = audit::Entry::new(session.address(), "content", "snapshots.regenerate");
    let Some(generator) = &state.generator else {
        audit::record_after(
            &state.admin.pool,
            &entry
                .detail(json!({ "error": UNSUPPORTED }))
                .result("error"),
        )
        .await;
        return AdminEnvelope::error(StatusCode::NOT_IMPLEMENTED, json!({ "error": UNSUPPORTED }));
    };
    let report = generator.request_regeneration();
    audit::record_after(
        &state.admin.pool,
        &entry.detail(json!({ "ready": report.ready, "running": report.running, "syncState": report.sync_state })),
    )
    .await;
    AdminEnvelope::success(
        json!({ "regeneration": "queued", "mode": "rebuild", "generator": report }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::SyncState;
    use crate::content::snapshot_gen::Settings as GenSettings;
    use crate::ops::admin::tests::{HOST, admin_state, lazy_pool, minted_cookie as cookie};
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use serde_json::Value;
    use tower::ServiceExt;

    const ADMIN: &str = "0x00000000000000000000000000000000000000aa";

    async fn harness(
        writes_enabled: bool,
        generation: bool,
    ) -> (Router, SharedState, Option<Arc<SnapshotGenerator>>) {
        let admin = Arc::new(admin_state(lazy_pool(), writes_enabled, ADMIN, None).await);
        let generator = generation.then(|| {
            SnapshotGenerator::new(
                admin.content.clone(),
                true,
                GenSettings::from_hours(6).unwrap(),
            )
        });
        (router(admin.clone(), generator.clone()), admin, generator)
    }

    async fn post(app: &Router, cookie: Option<&str>) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(REGENERATE_PATH)
            .header(header::HOST, HOST);
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
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
    async fn regenerate_is_403_without_an_allowlisted_session() {
        let (app, admin, generator) = harness(true, true).await;
        assert_eq!(post(&app, None).await.0, StatusCode::FORBIDDEN);
        let stranger = cookie(&admin, "0x00000000000000000000000000000000000000bb");
        assert_eq!(post(&app, Some(&stranger)).await.0, StatusCode::FORBIDDEN);
        assert!(
            !generator.unwrap().report().requested,
            "a refused call queues nothing"
        );
    }

    /// Queued for the job, open while operator writes are disabled, and the
    /// gate is reported: a node still bootstrapping keeps the request until
    /// it reaches `Syncing`.
    #[tokio::test]
    async fn regenerate_queues_a_pass_and_reports_the_gate() {
        let (app, admin, generator) = harness(false, true).await;
        let generator = generator.unwrap();
        let (status, body) = post(&app, Some(&cookie(&admin, ADMIN))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["body"]["regeneration"], "queued");
        assert_eq!(body["body"]["mode"], "rebuild");
        assert_eq!(body["body"]["generator"]["ready"], false);
        assert_eq!(body["body"]["generator"]["syncState"], "Bootstrapping");
        assert_eq!(body["body"]["generator"]["intervalHours"], 6);
        assert!(generator.report().requested);

        admin.content.sync.set_state(SyncState::Syncing);
        let (_, body) = post(&app, Some(&cookie(&admin, ADMIN))).await;
        assert_eq!(body["body"]["generator"]["ready"], true);
    }

    #[tokio::test]
    async fn generation_off_answers_the_old_501() {
        let (app, admin, _) = harness(true, false).await;
        let (status, body) = post(&app, Some(&cookie(&admin, ADMIN))).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            body,
            json!({ "ok": false, "status": 501, "body": { "error": UNSUPPORTED } })
        );
    }
}
