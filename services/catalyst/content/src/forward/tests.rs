//! The content lane against a fake upstream on the loopback. The DB-backed
//! tests run on the process's scratch database (`db::scratch`, skipped
//! without `CATALYST_TEST_DATABASE_URL`) with uuid-prefixed pointers; each
//! test's upstream has its own port, so its forwarder claims only the rows
//! queued for it.

use std::sync::Arc;
use std::time::Duration;

use alloy::signers::{Signer, local::PrivateKeySigner};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures::TryStreamExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::test_support::{FakeUpstream, Received, forwarder, parse};
use super::*;
use crate::auth::verify::verify_auth_chain_async;
use crate::auth::{AuthChain, hash_bytes_v1};
use crate::create_entity::extract_auth_chain_from_fields;
use crate::write_deployer::{DeployFailure, WriteDeployer};

const HARDHAT_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

async fn temp_storage() -> (Arc<Storage>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "catalyst-forward-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    (Arc::new(Storage::from_config(&dir).await.unwrap()), dir)
}

fn links(chain: &Value) -> Vec<Link> {
    chain_links(chain).unwrap()
}

async fn body_of(form: Form, storage: Arc<Storage>) -> std::io::Result<Vec<u8>> {
    form.into_stream(storage)
        .try_fold(Vec::new(), |mut body, chunk| async move {
            body.extend_from_slice(&chunk);
            Ok(body)
        })
        .await
}

#[tokio::test]
async fn the_form_is_catalyst_clients_byte_for_byte() {
    let (storage, dir) = temp_storage().await;
    let model = Bytes::from_static(b"glb bytes");
    let model_hash = hash_bytes_v1(&model);
    storage.store(&model_hash, model.clone()).await.unwrap();
    let chain = json!([
        { "type": "SIGNER", "payload": "0xabc" },
        { "type": "ECDSA_SIGNED_ENTITY", "payload": "bafyentity", "signature": "0x1234" }
    ]);
    let form = Form::new(
        "B".into(),
        "bafyentity",
        &links(&chain),
        Bytes::from_static(br#"{"e":1}"#),
        &[(model_hash.clone(), model.len() as u64)],
    );
    assert_eq!(form.content_type(), "multipart/form-data; boundary=B");
    let length = form.content_length();
    let body = body_of(form, storage.clone()).await.unwrap();
    let text = |name: &str, value: &str| {
        format!("--B\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
    };
    let file = |hash: &str, bytes: &str| {
        format!(
            "--B\r\nContent-Disposition: form-data; name=\"{hash}\"; filename=\"{hash}\"\r\nContent-Type: application/octet-stream\r\n\r\n{bytes}\r\n"
        )
    };
    let expected = [
        text("entityId", "bafyentity"),
        text("authChain[0][type]", "SIGNER"),
        text("authChain[0][payload]", "0xabc"),
        text("authChain[0][signature]", ""),
        text("authChain[1][type]", "ECDSA_SIGNED_ENTITY"),
        text("authChain[1][payload]", "bafyentity"),
        text("authChain[1][signature]", "0x1234"),
        file("bafyentity", r#"{"e":1}"#),
        file(&model_hash, "glb bytes"),
        "--B--\r\n".to_string(),
    ]
    .concat();
    assert_eq!(String::from_utf8(body.clone()).unwrap(), expected);
    assert_eq!(length, body.len() as u64);

    // A stored file whose size moved fails the body instead of sending a
    // length the bytes do not match.
    let wrong = Form::new(
        "B".into(),
        "bafyentity",
        &links(&chain),
        Bytes::from_static(b"{}"),
        &[(model_hash.clone(), model.len() as u64 + 1)],
    );
    assert!(body_of(wrong, storage.clone()).await.is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_missing_signature_is_sent_empty_and_a_malformed_chain_is_refused() {
    let chain = json!([
        { "type": "SIGNER", "payload": "0xabc" },
        { "type": "ECDSA_EPHEMERAL", "payload": "p", "signature": null },
        { "type": "ECDSA_SIGNED_ENTITY", "payload": "e", "signature": "0x1" }
    ]);
    let got = links(&chain);
    assert_eq!(
        got.iter().map(|l| l.signature.as_str()).collect::<Vec<_>>(),
        ["", "", "0x1"]
    );
    assert_eq!(got[1].kind, "ECDSA_EPHEMERAL");
    assert!(chain_links(&json!([])).is_err());
    assert!(chain_links(&json!({ "type": "SIGNER" })).is_err());
    assert!(chain_links(&json!([{ "payload": "x" }])).is_err());
    assert!(chain_links(&json!([{ "type": "SIGNER", "payload": 1 }])).is_err());
}

#[test]
fn file_probes_stay_under_catalyst_clients_url_bound() {
    let hashes: Vec<String> = (0..300).map(|i| format!("bafkreih{i:052}")).collect();
    let refs: Vec<&str> = hashes.iter().map(String::as_str).collect();
    let base = "https://upstream.example/content/available-content";
    let urls = cid_queries(base, &refs);
    assert!(urls.len() > 1);
    let mut seen = Vec::new();
    for url in &urls {
        assert!(url.len() < MAX_URL_LENGTH, "{} chars", url.len());
        let query = url.strip_prefix(&format!("{base}?")).unwrap();
        seen.extend(
            query
                .split('&')
                .map(|p| p.strip_prefix("cid=").unwrap().to_string()),
        );
    }
    assert_eq!(seen, hashes);
    assert!(cid_queries(base, &[]).is_empty());
}

#[test]
fn retries_double_to_the_cap_and_statuses_sort_into_outcomes() {
    let retry = Retry::new(12, 5);
    let delays: Vec<u64> = (1..=8).map(|n| retry.delay(n).as_secs()).collect();
    assert_eq!(delays, [5, 10, 20, 40, 80, 160, 300, 300]);
    assert_eq!(Retry::new(3, 3600).delay(4).as_secs(), 3600);
    assert_eq!(Retry::new(0, 5).max_attempts, 1);

    assert!(matches!(
        verdict(200, String::new()),
        Outcome::Accepted { status: 200 }
    ));
    assert!(matches!(
        verdict(201, String::new()),
        Outcome::Accepted { .. }
    ));
    for status in [404, 405, 408, 425, 429, 500, 502, 503, 504] {
        assert!(
            matches!(verdict(status, String::new()), Outcome::Retry { status: Some(s), .. } if s == status),
            "{status}"
        );
    }
    for status in [301, 400, 401, 403, 409, 413, 422] {
        assert!(
            matches!(verdict(status, String::new()), Outcome::Rejected { .. }),
            "{status}"
        );
    }
    // Past 599 (reqwest takes up to 999): not an HTTP status to store, retried.
    for status in [600, 999] {
        assert!(
            matches!(
                verdict(status, String::new()),
                Outcome::Retry { status: None, .. }
            ),
            "{status}"
        );
    }
    // The file probe retries only a busy or down upstream.
    assert!(transient(503) && transient(429) && !transient(404) && !transient(400));
    assert_eq!(capped("é".repeat(ERROR_TEXT_CAP)).len(), ERROR_TEXT_CAP);
    assert_eq!(
        origin("https://up.example:8443/base/key?x=1"),
        "https://up.example:8443"
    );
}

#[test]
fn an_entity_is_sent_only_inside_the_upstreams_window() {
    // catalyst's REQUEST_TTL_BACKWARDS / _FORWARDS, this node's own deploy bounds.
    assert_eq!(
        (CONTENT_LANE.max_age_ms, CONTENT_LANE.max_ahead_ms),
        (20 * 60_000, 15 * 60_000)
    );
    let now = 1_800_000_000_000;
    let at = |ms: i64| json!({ "timestamp": ms });
    assert_eq!(CONTENT_LANE.too_old(&at(now - 20 * 60_000), now), None);
    assert_eq!(CONTENT_LANE.too_old(&at(now + 60_000), now), None);
    let refusal = CONTENT_LANE
        .too_old(&at(now - 20 * 60_000 - 1), now)
        .unwrap();
    assert!(refusal.contains("(20 min"), "{refusal}");
    // No timestamp is the upstream's to judge.
    assert_eq!(CONTENT_LANE.too_old(&json!({}), now), None);
}

#[test]
fn an_upstream_is_named_by_its_base_url() {
    for ok in [
        "https://peer.example",
        "https://peer.example/",
        "http://peer.example:8080",
        " https://worlds.example/ ",
    ] {
        assert_eq!(check_target("X", ok), Ok(()), "{ok}");
    }
    for bad in [
        "https://upstream.test/content",
        "https://upstream.test/content/",
        "https://upstream.test/secret",
        "https://upstream.test/?secret=1",
        "https://upstream.test/#secret",
        "not a url",
    ] {
        let err = check_target("X", bad).unwrap_err();
        assert!(err.starts_with("X "), "{err}");
        assert!(
            !err.contains("upstream.test") && !err.contains("secret"),
            "the value stays out of the error: {err}"
        );
    }
}

#[test]
fn the_list_filter_names_a_status() {
    let q = |status: &str| ListQuery {
        status: Some(status.into()),
        ..ListQuery::default()
    };
    assert_eq!(q("rejected").status(), Ok(Some("rejected")));
    assert_eq!(q("").status(), Ok(None));
    assert!(q("bogus").status().is_err());
}

// ── DB-backed: a signed local deploy through the write deployer ────────────

/// A signed scene by the Hardhat key: the entity bytes, id, chain and files.
struct Signed {
    entity_id: String,
    entity: Bytes,
    chain: Value,
    files: Vec<Bytes>,
}

impl Signed {
    async fn scene(pointer: &str, timestamp: i64, files: &[&[u8]]) -> Self {
        let files: Vec<Bytes> = files.iter().map(|f| Bytes::copy_from_slice(f)).collect();
        let content: Vec<Value> = files
            .iter()
            .enumerate()
            .map(|(i, f)| json!({ "file": format!("f{i}.bin"), "hash": hash_bytes_v1(f) }))
            .collect();
        let entity = Bytes::from(
            serde_json::to_vec(&json!({
                "version": "v3",
                "type": "scene",
                "pointers": [pointer],
                "timestamp": timestamp,
                "content": content,
                "metadata": { "name": "forwarded" }
            }))
            .unwrap(),
        );
        let entity_id = hash_bytes_v1(&entity);
        let key: PrivateKeySigner = HARDHAT_KEY.parse().unwrap();
        let signature = key
            .sign_message(entity_id.as_bytes())
            .await
            .unwrap()
            .to_string();
        let chain = json!([
            { "type": "SIGNER", "payload": format!("{:#x}", key.address()) },
            { "type": "ECDSA_SIGNED_ENTITY", "payload": entity_id, "signature": signature }
        ]);
        Self {
            entity_id,
            entity,
            chain,
            files,
        }
    }

    fn hashes(&self) -> Vec<String> {
        self.files.iter().map(|f| hash_bytes_v1(f)).collect()
    }

    /// Deploys through a fresh deployer (its rate limiter is per instance).
    async fn deploy(
        &self,
        pool: &PgPool,
        storage: &Arc<Storage>,
        forward: Option<&Arc<Forwarder>>,
    ) -> Result<i64, DeployFailure> {
        let mut uploads = vec![self.entity.clone()];
        uploads.extend(self.files.iter().cloned());
        WriteDeployer::with_partial_validation_for_tests(pool.clone(), storage.clone(), None)
            .with_forwarder(forward.cloned())
            .deploy_entity(uploads, &self.entity_id, self.chain.clone(), "LOCAL")
            .await
    }
}

struct Lab {
    pool: PgPool,
    storage: Arc<Storage>,
    dir: std::path::PathBuf,
    upstream: FakeUpstream,
    forwarder: Arc<Forwarder>,
    prefix: String,
    entities: parking_lot::Mutex<Vec<String>>,
}

impl Lab {
    async fn new(max_attempts: i32) -> Option<Self> {
        Self::with_lane(max_attempts, CONTENT_LANE).await
    }

    async fn with_lane(max_attempts: i32, lane: Lane) -> Option<Self> {
        let pool = crate::db::scratch::pool(4).await?;
        let (storage, dir) = temp_storage().await;
        let upstream = FakeUpstream::start("/content").await;
        let forwarder = forwarder(
            pool.clone(),
            storage.clone(),
            lane,
            &upstream.base,
            max_attempts,
        );
        Some(Self {
            pool,
            storage,
            dir,
            upstream,
            forwarder,
            prefix: uuid::Uuid::new_v4().simple().to_string(),
            entities: parking_lot::Mutex::new(Vec::new()),
        })
    }

    fn pointer(&self, name: &str) -> String {
        format!("{}:{name}", self.prefix)
    }

    async fn scene(&self, pointer: &str, timestamp: i64, files: &[&[u8]]) -> Signed {
        let signed = Signed::scene(&self.pointer(pointer), timestamp, files).await;
        self.entities.lock().push(signed.entity_id.clone());
        signed
    }

    async fn pass(&self) -> PassReport {
        self.forwarder
            .run_pass(&CancellationToken::new())
            .await
            .unwrap()
    }

    async fn row(
        &self,
        entity_id: &str,
    ) -> Option<(String, i32, Option<String>, Option<i16>, String)> {
        sqlx::query_as(
            "SELECT status, attempts, last_error, upstream_status, target
             FROM content.forward_outbox WHERE entity_id = $1",
        )
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
    }

    /// Whether the row is due now.
    async fn due(&self, entity_id: &str) -> bool {
        sqlx::query_scalar(
            "SELECT next_attempt_at <= timezone('UTC', now()) FROM content.forward_outbox WHERE entity_id = $1",
        )
        .bind(entity_id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Makes the row due now, as if its retry delay had passed.
    async fn make_due(&self, entity_id: &str) {
        sqlx::query(
            "UPDATE content.forward_outbox SET next_attempt_at = timezone('UTC', now()) - interval '1 second'
             WHERE entity_id = $1",
        )
        .bind(entity_id)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// The content routes with the admin bearer `forward-admin`, the write
    /// path attached with this lab's forwarder (or with none).
    fn admin_app(&self, forwarding: bool) -> axum::Router {
        self.admin_app_with(forwarding, true)
    }

    fn admin_app_with(&self, forwarding: bool, writes: bool) -> axum::Router {
        let settings = super::super::Settings {
            admin_token: super::super::AdminToken::new(Some("forward-admin".into())),
            ..super::super::Settings::for_tests()
        };
        let state =
            super::super::ContentState::new(self.pool.clone(), settings, self.storage.clone());
        state.attach_writes(
            writes,
            Arc::new(
                WriteDeployer::with_partial_validation_for_tests(
                    self.pool.clone(),
                    self.storage.clone(),
                    None,
                )
                .with_forwarder(forwarding.then(|| self.forwarder.clone())),
            ),
        );
        super::super::build_router(Arc::new(state))
    }

    async fn cleanup(self) {
        let ids = self.entities.lock().clone();
        for sql in [
            "DELETE FROM content.forward_outbox WHERE entity_id = ANY($1)",
            "DELETE FROM content.content_files WHERE deployment IN (SELECT id FROM content.deployments WHERE entity_id = ANY($1))",
            "DELETE FROM content.local_entities WHERE entity_id = ANY($1)",
            "DELETE FROM content.active_pointers WHERE entity_id = ANY($1)",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&ids)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        // Overwritten deployments name their successor; clear the links first.
        sqlx::query(
            "UPDATE content.deployments SET deleter_deployment = NULL WHERE entity_id = ANY($1)",
        )
        .bind(&ids)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM content.deployments WHERE entity_id = ANY($1)")
            .bind(&ids)
            .execute(&self.pool)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&self.dir);
        self.pool.close().await;
    }
}

/// One request to `app`: the status and the JSON body (`Null` when none).
async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// The received form's entity, chain and files, the chain verified the way
/// an upstream verifies it.
async fn received_deployment(received: &Received) -> (String, AuthChain, Vec<(String, Bytes)>) {
    assert!(
        received
            .content_type
            .starts_with("multipart/form-data; boundary=")
    );
    assert_eq!(
        received.content_length,
        Some(received.body.len() as u64),
        "the announced length is the body's"
    );
    let form = parse(received).await;
    let entity_id = form.fields["entityId"].clone();
    let chain: AuthChain = serde_json::from_value(
        extract_auth_chain_from_fields(&form.fields)
            .unwrap()
            .expect("the form carries the indexed chain"),
    )
    .unwrap();
    verify_auth_chain_async(&chain, &entity_id, None, None)
        .await
        .expect("the forwarded chain signs the forwarded entity");
    (entity_id, chain, form.files)
}

#[tokio::test]
async fn a_local_deploy_is_forwarded_unchanged_and_accepted() {
    let Some(lab) = Lab::new(5).await else { return };
    let now = chrono::Utc::now().timestamp_millis();
    let signed = lab
        .scene("1,1", now, &[b"held upstream", b"only here"])
        .await;
    let [held, missing] = <[String; 2]>::try_from(signed.hashes()).unwrap();
    lab.upstream.hold(&held);

    signed
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    let (status, attempts, _, _, target) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!((status.as_str(), attempts), ("queued", 0));
    assert_eq!(target, lab.upstream.base);

    let report = lab.pass().await;
    assert_eq!(
        report,
        PassReport {
            accepted: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, error, upstream, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, error, upstream),
        ("accepted", 1, None, Some(200))
    );

    assert_eq!(lab.upstream.probed(), [held.clone(), missing.clone()]);
    let posts = lab.upstream.posts();
    assert_eq!(posts.len(), 1);
    let (entity_id, chain, files) = received_deployment(&posts[0]).await;
    assert_eq!(entity_id, signed.entity_id);
    let original: AuthChain = serde_json::from_value(signed.chain.clone()).unwrap();
    assert_eq!(chain.len(), original.len());
    for (sent, signed) in chain.iter().zip(original.iter()) {
        assert_eq!(sent.link_type, signed.link_type);
        assert_eq!(sent.payload, signed.payload);
        assert_eq!(
            sent.signature.as_deref().unwrap_or_default(),
            signed.signature.as_deref().unwrap_or_default()
        );
    }
    assert_eq!(
        files,
        vec![
            (signed.entity_id.clone(), signed.entity.clone()),
            (missing.clone(), signed.files[1].clone()),
        ],
        "the entity file byte for byte, then only the file the upstream lacks"
    );

    // Nothing is due any more.
    assert_eq!(lab.pass().await.total(), 0);
    assert_eq!(lab.upstream.posts().len(), 1);

    // The status routes, behind the content admin bearer.
    let app = lab.admin_app(true);
    let path = format!("/content/forwarding/{}", signed.entity_id);
    assert_eq!(
        call(&app, "GET", &path, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "GET", &path, Some("wrong")).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, record) = call(&app, "GET", &path, Some("forward-admin")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record["entityId"], json!(signed.entity_id));
    assert_eq!(record["status"], json!("accepted"));
    assert_eq!(record["upstreamStatus"], json!(200));
    assert_eq!(record["target"], json!(lab.upstream.base));
    assert_eq!(record["nextAttemptAt"], Value::Null);
    assert!(record.get("worldName").is_none());
    let unknown = "/content/forwarding/bafynothing";
    assert_eq!(
        call(&app, "GET", unknown, Some("forward-admin")).await.0,
        StatusCode::NOT_FOUND
    );
    let bad = "/content/forwarding?status=bogus";
    assert_eq!(
        call(&app, "GET", bad, Some("forward-admin")).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, page) = call(
        &app,
        "GET",
        "/content/forwarding?status=accepted&limit=1000",
        Some("forward-admin"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page["forwards"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["entityId"] == json!(signed.entity_id))
    );
    // An accepted record is not requeued.
    let (status, refusal) = call(
        &app,
        "POST",
        &format!("{path}/retry"),
        Some("forward-admin"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(refusal["error"].as_str().unwrap().contains("accepted"));

    lab.cleanup().await;
}

/// A requeue is a content write: it answers the content 503 while the
/// switch is off and while the restore stamps hold it, before it looks the
/// record up.
#[tokio::test]
async fn a_requeue_waits_for_the_content_switch_and_the_restore_stamps() {
    use crate::shared::writes::restore::{
        lab::{HOUR, StampLab},
        scoped,
    };
    let Some(lab) = Lab::new(5).await else { return };
    let retry = "/content/forwarding/bafynothing/retry";
    let (status, body) = call(
        &lab.admin_app_with(true, false),
        "POST",
        retry,
        Some("forward-admin"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], super::super::writes::DISABLED_MESSAGE);
    let app = lab.admin_app(true);
    let stamps = StampLab::fresh();
    scoped(stamps.stamps.clone(), async {
        assert_eq!(
            call(&app, "POST", retry, Some("forward-admin")).await.0,
            StatusCode::NOT_FOUND,
            "fresh stamps let the requeue look the record up"
        );
        stamps.advance(30 * HOUR);
        let (status, body) = call(&app, "POST", retry, Some("forward-admin")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["error"],
            "content writes are held on this node: the last complete backup (backup.json) is 31 h old, past WRITES_BACKUP_MAX_AGE_HOURS=26"
        );
    })
    .await;
    lab.cleanup().await;
}

#[tokio::test]
async fn an_upstream_refusal_is_terminal_and_keeps_its_reason() {
    let Some(lab) = Lab::new(5).await else { return };
    let signed = lab
        .scene("2,2", chrono::Utc::now().timestamp_millis(), &[b"x"])
        .await;
    lab.upstream
        .answer(400, r#"{"errors":["The entity is not allowed upstream"]}"#);
    signed
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();

    assert_eq!(
        lab.pass().await,
        PassReport {
            rejected: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, error, upstream, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, upstream),
        ("rejected", 1, Some(400))
    );
    assert_eq!(
        error.as_deref(),
        Some(r#"{"errors":["The entity is not allowed upstream"]}"#)
    );
    // Terminal: even due, it is never claimed again.
    lab.make_due(&signed.entity_id).await;
    assert_eq!(lab.pass().await.total(), 0);
    assert_eq!(lab.upstream.posts().len(), 1);

    // The operator requeues it, for the target configured now (the row's
    // was since replaced), behind the content admin bearer.
    sqlx::query(
        "UPDATE content.forward_outbox SET target = 'https://old.example' WHERE entity_id = $1",
    )
    .bind(&signed.entity_id)
    .execute(&lab.pool)
    .await
    .unwrap();
    let retry = format!("/content/forwarding/{}/retry", signed.entity_id);
    let app = lab.admin_app(true);
    assert_eq!(
        call(&app, "POST", &retry, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&lab.admin_app(false), "POST", &retry, Some("forward-admin"))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE,
        "no requeue while forwarding is off"
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/content/forwarding/bafynothing/retry",
            Some("forward-admin")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, record) = call(&app, "POST", &retry, Some("forward-admin")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (&record["status"], &record["attempts"], &record["target"]),
        (&json!("queued"), &json!(0), &json!(lab.upstream.base))
    );
    assert_eq!(
        lab.pass().await,
        PassReport {
            accepted: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, error, upstream, target) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, error, upstream, target),
        ("accepted", 1, None, Some(200), lab.upstream.base.clone())
    );
    assert_eq!(lab.upstream.posts().len(), 2);
    lab.cleanup().await;
}

#[tokio::test]
async fn an_upstream_outage_retries_with_backoff_until_accepted() {
    let Some(lab) = Lab::new(5).await else { return };
    let signed = lab
        .scene("3,3", chrono::Utc::now().timestamp_millis(), &[b"y"])
        .await;
    lab.upstream.answer(503, "upstream is restarting");
    signed
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();

    assert_eq!(
        lab.pass().await,
        PassReport {
            retried: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, error, upstream, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, error.as_deref(), upstream),
        ("queued", 1, Some("upstream is restarting"), Some(503))
    );
    let wait: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at - timezone('UTC', now()))::float8
         FROM content.forward_outbox WHERE entity_id = $1",
    )
    .bind(&signed.entity_id)
    .fetch_one(&lab.pool)
    .await
    .unwrap();
    assert!(wait > 3000.0, "the retry waits its delay ({wait} s)");
    assert_eq!(lab.pass().await.total(), 0, "not due yet");

    // A missing route (a read-only catalyst) and a status past 599 retry
    // too; the latter stores no upstream status.
    lab.upstream.answer(404, "Not Found");
    lab.upstream.answer(600, "odd");
    for (attempts, error, upstream) in [(2, "Not Found", Some(404)), (3, "status 600: odd", None)] {
        lab.make_due(&signed.entity_id).await;
        assert_eq!(lab.pass().await.retried, 1);
        let (status, got, got_error, got_upstream, _) = lab.row(&signed.entity_id).await.unwrap();
        assert_eq!(
            (status.as_str(), got, got_error.as_deref(), got_upstream),
            ("queued", attempts, Some(error), upstream)
        );
    }

    lab.make_due(&signed.entity_id).await;
    assert_eq!(
        lab.pass().await,
        PassReport {
            accepted: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, error, upstream, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, error, upstream),
        ("accepted", 4, None, Some(200))
    );
    let posts = lab.upstream.posts();
    assert_eq!(posts.len(), 4);
    assert!(
        posts.iter().all(|p| p.body.len() == posts[0].body.len()),
        "the same deployment every time"
    );
    lab.cleanup().await;
}

#[tokio::test]
async fn a_refused_file_probe_sends_every_file_and_a_failing_one_retries() {
    let Some(lab) = Lab::new(5).await else { return };
    let now = chrono::Utc::now().timestamp_millis();
    let signed = lab.scene("11,11", now, &[b"one", b"two"]).await;
    for hash in signed.hashes() {
        lab.upstream.hold(&hash);
    }
    lab.upstream.refuse_probes(400);
    signed
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    assert_eq!(lab.pass().await.accepted, 1);
    let posts = lab.upstream.posts();
    let (_, _, files) = received_deployment(&posts[0]).await;
    let sent: Vec<&String> = files.iter().map(|(hash, _)| hash).collect();
    let mut expected = vec![signed.entity_id.clone()];
    expected.extend(signed.hashes());
    assert_eq!(sent, expected.iter().collect::<Vec<_>>());

    let later = lab.scene("12,12", now, &[b"three"]).await;
    lab.upstream.refuse_probes(503);
    later
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    assert_eq!(lab.pass().await.retried, 1);
    let (status, _, error, upstream, _) = lab.row(&later.entity_id).await.unwrap();
    assert_eq!((status.as_str(), upstream), ("queued", Some(503)));
    assert!(error.unwrap().starts_with("GET /content/available-content"));
    assert_eq!(
        lab.upstream.posts().len(),
        1,
        "no deploy while the upstream is down"
    );
    lab.cleanup().await;
}

#[tokio::test]
async fn an_unreachable_upstream_fails_after_its_attempts() {
    let Some(lab) = Lab::new(2).await else { return };
    // A port nothing listens on any more.
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let forwarder = forwarder(
        lab.pool.clone(),
        lab.storage.clone(),
        CONTENT_LANE,
        &closed,
        2,
    );
    let signed = lab
        .scene("4,4", chrono::Utc::now().timestamp_millis(), &[b"z"])
        .await;
    signed
        .deploy(&lab.pool, &lab.storage, Some(&forwarder))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(forwarder.run_pass(&cancel).await.unwrap().retried, 1);
    lab.make_due(&signed.entity_id).await;
    assert_eq!(forwarder.run_pass(&cancel).await.unwrap().failed, 1);
    let (status, attempts, error, upstream, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!((status.as_str(), attempts, upstream), ("failed", 2, None));
    assert!(error.unwrap().contains("/content/available-content"));
    lab.cleanup().await;
}

#[tokio::test]
async fn a_crash_between_commit_and_send_still_sends() {
    let Some(lab) = Lab::new(5).await else { return };
    let now = chrono::Utc::now().timestamp_millis();

    // The process that accepted the deploy dies before its worker ran: the
    // row committed with the deployment, and the next process's job sends it.
    let orphan = lab.scene("5,5", now, &[b"orphan"]).await;
    {
        let dead = forwarder(
            lab.pool.clone(),
            lab.storage.clone(),
            CONTENT_LANE,
            &lab.upstream.base,
            5,
        );
        orphan
            .deploy(&lab.pool, &lab.storage, Some(&dead))
            .await
            .unwrap();
    }
    // A worker died mid-send: its claim's lease ran out.
    let stranded = lab.scene("6,6", now, &[b"stranded"]).await;
    stranded
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    // A worker died mid-send on the row's last allowed attempt: it ends
    // `failed` when its claim expires, never sent past the bound.
    let exhausted = lab.scene("14,14", now, &[b"exhausted"]).await;
    exhausted
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    for (entity_id, attempts) in [(&stranded.entity_id, 1), (&exhausted.entity_id, 5)] {
        sqlx::query(
            "UPDATE content.forward_outbox SET status = 'sending', attempts = $2,
                    next_attempt_at = timezone('UTC', now()) - interval '1 second'
             WHERE entity_id = $1",
        )
        .bind(entity_id)
        .bind(attempts)
        .execute(&lab.pool)
        .await
        .unwrap();
    }

    let jobs = crate::jobs::Jobs::new();
    let restarted = forwarder(
        lab.pool.clone(),
        lab.storage.clone(),
        CONTENT_LANE,
        &lab.upstream.base,
        5,
    );
    restarted.spawn(&jobs);
    let mut rows = None;
    let settled = crate::shared::test_wait::until(async || {
        let orphan_row = lab.row(&orphan.entity_id).await.unwrap();
        let stranded_row = lab.row(&stranded.entity_id).await.unwrap();
        let exhausted_row = lab.row(&exhausted.entity_id).await.unwrap();
        let done = orphan_row.0 == "accepted"
            && stranded_row.0 == "accepted"
            && exhausted_row.0 == "failed";
        rows = Some((orphan_row, stranded_row, exhausted_row));
        done
    })
    .await;
    let (orphan_row, stranded_row, exhausted_row) = rows.unwrap();
    assert!(settled, "{orphan_row:?} {stranded_row:?} {exhausted_row:?}");
    assert_eq!(orphan_row.1, 1);
    assert_eq!(stranded_row.1, 2, "the expired claim counts as an attempt");
    assert_eq!(exhausted_row.1, 6);
    assert!(exhausted_row.2.unwrap().contains("FORWARD_MAX_ATTEMPTS"));
    jobs.shutdown(Duration::from_secs(5)).await;
    let mut sent = Vec::new();
    for post in lab.upstream.posts() {
        sent.push(received_deployment(&post).await.0);
    }
    sent.sort();
    let mut expected = vec![orphan.entity_id.clone(), stranded.entity_id.clone()];
    expected.sort();
    assert_eq!(sent, expected);
    lab.cleanup().await;
}

#[tokio::test]
async fn a_locally_refused_deploy_is_never_queued() {
    let Some(lab) = Lab::new(5).await else { return };
    let now = chrono::Utc::now().timestamp_millis();

    // A chain whose signature does not recover to its signer.
    let forged = lab.scene("7,7", now, &[b"forged"]).await;
    let mut chain = forged.chain.clone();
    chain[0]["payload"] = json!("0x0000000000000000000000000000000000000001");
    let refused = Signed { chain, ..forged };
    let err = refused
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap_err();
    assert!(matches!(err, DeployFailure::Rejected(_, _)), "{err:?}");
    assert!(lab.row(&refused.entity_id).await.is_none());

    // An entity older than the pointer's current one.
    let newer = lab.scene("8,8", now, &[b"newer"]).await;
    let older = lab.scene("8,8", now - 60_000, &[b"older"]).await;
    newer.deploy(&lab.pool, &lab.storage, None).await.unwrap();
    let err = older
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DeployFailure::Rejected(crate::write_deployer::RejectionKind::Superseded, _)
        ),
        "{err:?}"
    );
    assert!(lab.row(&older.entity_id).await.is_none());
    assert!(
        lab.row(&newer.entity_id).await.is_none(),
        "forwarding was off for it"
    );

    // A deploy without a forwarder queues nothing; one of the same entity
    // with it is "already deployed" and queues nothing either.
    let plain = lab.scene("9,9", now, &[b"plain"]).await;
    plain.deploy(&lab.pool, &lab.storage, None).await.unwrap();
    plain
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    assert!(lab.row(&plain.entity_id).await.is_none());

    assert_eq!(lab.pass().await.total(), 0);
    assert!(lab.upstream.posts().is_empty());
    lab.cleanup().await;
}

#[tokio::test]
async fn an_overwritten_deploy_is_superseded_and_never_sent() {
    let Some(lab) = Lab::new(5).await else { return };
    let now = chrono::Utc::now().timestamp_millis();
    let first = lab.scene("10,10", now - 60_000, &[b"first"]).await;
    let second = lab.scene("10,10", now, &[b"second"]).await;
    first
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    second
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();

    assert_eq!(
        lab.pass().await,
        PassReport {
            accepted: 1,
            superseded: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, error, upstream, _) = lab.row(&first.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, upstream),
        ("superseded", 1, None)
    );
    assert!(error.unwrap().contains("not sent"));
    assert_eq!(lab.row(&second.entity_id).await.unwrap().0, "accepted");
    let posts = lab.upstream.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(received_deployment(&posts[0]).await.0, second.entity_id);
    lab.cleanup().await;
}

#[tokio::test]
async fn an_unpublished_scene_is_superseded_and_never_sent() {
    let Some(lab) = Lab::new(5).await else { return };
    let signed = lab
        .scene(
            "13,13",
            chrono::Utc::now().timestamp_millis(),
            &[b"taken down"],
        )
        .await;
    lab.upstream.answer(503, "upstream is restarting");
    signed
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    assert_eq!(lab.pass().await.retried, 1);

    // The LAND holder takes the scene down while its send waits to retry:
    // the tombstone leaves `deleter_deployment` alone, and the scene stays
    // down upstream too.
    crate::land_publish::unpublish_local_scene(&lab.pool, &lab.pointer("13,13"), &signed.entity_id)
        .await
        .unwrap();
    lab.make_due(&signed.entity_id).await;
    assert_eq!(
        lab.pass().await,
        PassReport {
            superseded: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, _, _, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!((status.as_str(), attempts), ("superseded", 2));
    assert_eq!(lab.upstream.posts().len(), 1, "only the first, failed send");
    lab.cleanup().await;
}

#[tokio::test]
async fn the_list_pages_newest_first_and_the_sweep_drops_old_terminal_rows() {
    let Some(lab) = Lab::new(5).await else { return };
    let ids: Vec<String> = (0..3).map(|i| format!("{}-list-{i}", lab.prefix)).collect();
    lab.entities.lock().extend(ids.iter().cloned());
    let mut row_ids = Vec::new();
    // Recent rows: another test's job sweeps on its start, and must not
    // take these before they are listed.
    for (id, status) in ids.iter().zip(["accepted", "rejected", "queued"]) {
        let row: i64 = sqlx::query_scalar(
            "INSERT INTO content.forward_outbox (entity_id, target, status)
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(id)
        .bind(format!("{}/never", lab.upstream.base))
        .bind(status)
        .fetch_one(&lab.pool)
        .await
        .unwrap();
        row_ids.push(row);
    }
    let top = *row_ids.last().unwrap();
    let page = list(&lab.pool, &CONTENT_LANE, None, Some(top + 1), Some(1))
        .await
        .unwrap();
    assert_eq!(page["forwards"][0]["entityId"], json!(ids[2]));
    assert_eq!(page["forwards"][0]["status"], json!("queued"));
    assert!(page["forwards"][0]["nextAttemptAt"].is_string());
    assert_eq!(page["next"], json!(top));
    let rest = list(&lab.pool, &CONTENT_LANE, None, Some(top), Some(1000))
        .await
        .unwrap();
    let ours: Vec<&Value> = rest["forwards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["entityId"])
        .filter(|id| ids.iter().any(|ours| *id == &json!(ours)))
        .collect();
    assert_eq!(ours, [&json!(ids[1]), &json!(ids[0])]);
    let rejected = list(
        &lab.pool,
        &CONTENT_LANE,
        Some("rejected"),
        Some(top + 1),
        Some(1),
    )
    .await
    .unwrap();
    assert_eq!(rejected["forwards"][0]["entityId"], json!(ids[1]));

    // The sweep deletes terminal rows past the retention, never open ones,
    // a backlog larger than one statement's batch in one call.
    sqlx::query(
        "UPDATE content.forward_outbox SET updated_at = timezone('UTC', now()) - interval '40 days'
         WHERE entity_id = ANY($1)",
    )
    .bind(&ids)
    .execute(&lab.pool)
    .await
    .unwrap();
    let backlog: Vec<String> = (0..2 * SWEEP_BATCH + 500)
        .map(|i| format!("{}-old-{i}", lab.prefix))
        .collect();
    lab.entities.lock().extend(backlog.iter().cloned());
    sqlx::query(
        "INSERT INTO content.forward_outbox (entity_id, target, status, updated_at)
         SELECT id, 'https://old.example', 'accepted', timezone('UTC', now()) - interval '40 days'
         FROM unnest($1::text[]) id",
    )
    .bind(&backlog)
    .execute(&lab.pool)
    .await
    .unwrap();
    let cutoff = now() - chrono::Duration::days(30);
    lab.forwarder
        .sweep(cutoff, &CancellationToken::new())
        .await
        .unwrap();
    let left: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM content.forward_outbox WHERE entity_id = ANY($1) OR entity_id = ANY($2) ORDER BY id",
    )
    .bind(&ids)
    .bind(&backlog)
    .fetch_all(&lab.pool)
    .await
    .unwrap();
    assert_eq!(left, [ids[2].clone()]);
    lab.cleanup().await;
}

#[tokio::test]
async fn an_entity_past_the_upstreams_window_is_never_sent_nor_requeued() {
    // The content lane with a one-minute window: an entity signed two
    // minutes ago deploys here (its window is 20) and is past the upstream's.
    let lane = Lane {
        max_age_ms: 60_000,
        ..CONTENT_LANE
    };
    let Some(lab) = Lab::with_lane(5, lane).await else {
        return;
    };
    let now = chrono::Utc::now().timestamp_millis();
    let stale = lab.scene("15,15", now - 120_000, &[b"stale"]).await;
    let fresh = lab.scene("16,16", now, &[b"fresh"]).await;
    for signed in [&stale, &fresh] {
        signed
            .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
            .await
            .unwrap();
    }
    assert_eq!(
        lab.pass().await,
        PassReport {
            accepted: 1,
            failed: 1,
            ..PassReport::default()
        }
    );
    let (status, attempts, error, upstream, _) = lab.row(&stale.entity_id).await.unwrap();
    assert_eq!((status.as_str(), attempts, upstream), ("failed", 1, None));
    assert!(
        error
            .unwrap()
            .contains("older than the upstream accepts (1 min"),
        "the reason names the window"
    );
    let posts = lab.upstream.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(received_deployment(&posts[0]).await.0, fresh.entity_id);
    assert_eq!(
        lab.upstream.probed(),
        fresh.hashes(),
        "nothing asked about the stale one"
    );

    // No requeue can make it sendable.
    let app = lab.admin_app(true);
    let retry = format!("/content/forwarding/{}/retry", stale.entity_id);
    let (status, refusal) = call(&app, "POST", &retry, Some("forward-admin")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("older than the upstream accepts"),
        "{refusal}"
    );
    let (status, attempts, ..) = lab.row(&stale.entity_id).await.unwrap();
    assert_eq!((status.as_str(), attempts), ("failed", 1));
    lab.cleanup().await;
}

#[tokio::test]
async fn a_shutdown_mid_send_gives_the_claim_back_due_now() {
    let Some(lab) = Lab::new(5).await else { return };
    let signed = lab
        .scene("17,17", chrono::Utc::now().timestamp_millis(), &[b"slow"])
        .await;
    signed
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    lab.upstream.stall(true);
    let cancel = CancellationToken::new();
    let pass = tokio::spawn({
        let forwarder = lab.forwarder.clone();
        let cancel = cancel.clone();
        async move { forwarder.run_pass(&cancel).await }
    });
    tokio::time::timeout(crate::shared::test_wait::HANG_GUARD, lab.upstream.posted(1))
        .await
        .expect("the send never arrived");
    assert_eq!(lab.row(&signed.entity_id).await.unwrap().0, "sending");

    // The shutdown does not wait for the upstream's answer, and the row is
    // the next worker's at once, its cut-short attempt uncounted.
    cancel.cancel();
    let report = tokio::time::timeout(Duration::from_secs(5), pass)
        .await
        .expect("the pass ends on cancel")
        .unwrap()
        .unwrap();
    assert_eq!(report.total(), 0);
    let (status, attempts, error, ..) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!((status.as_str(), attempts, error), ("queued", 0, None));
    assert!(lab.due(&signed.entity_id).await);

    lab.upstream.stall(false);
    assert_eq!(lab.pass().await.accepted, 1);
    let (status, attempts, ..) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!((status.as_str(), attempts), ("accepted", 1));
    assert_eq!(lab.upstream.posts().len(), 2);
    lab.cleanup().await;
}

#[tokio::test]
async fn a_late_finish_never_overwrites_a_newer_claim() {
    let Some(lab) = Lab::new(5).await else { return };
    let signed = lab
        .scene("18,18", chrono::Utc::now().timestamp_millis(), &[b"fenced"])
        .await;
    signed
        .deploy(&lab.pool, &lab.storage, Some(&lab.forwarder))
        .await
        .unwrap();
    let first = lab.forwarder.claim().await.unwrap().unwrap();
    assert_eq!(first.attempts, 1);
    // Its lease runs out while it still sends, and another worker claims it.
    lab.make_due(&signed.entity_id).await;
    let second = lab.forwarder.claim().await.unwrap().unwrap();
    assert_eq!((second.id, second.attempts), (first.id, 2));

    // The first worker's late outcome, or its release, is dropped.
    let late = Outcome::Rejected {
        status: 400,
        body: "late".into(),
    };
    lab.forwarder.finish(&first, &late).await.unwrap();
    lab.forwarder.release(&first).await.unwrap();
    let (status, attempts, error, upstream, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, error, upstream),
        ("sending", 2, None, None)
    );

    lab.forwarder
        .finish(&second, &Outcome::Accepted { status: 200 })
        .await
        .unwrap();
    let (status, attempts, _, upstream, _) = lab.row(&signed.entity_id).await.unwrap();
    assert_eq!(
        (status.as_str(), attempts, upstream),
        ("accepted", 2, Some(200))
    );
    lab.cleanup().await;
}

#[tokio::test]
async fn rows_of_another_upstream_close_once_no_upstream_takes_them() {
    let Some(lab) = Lab::new(5).await else { return };
    let ids: Vec<String> = ["old", "recent", "leased"]
        .iter()
        .map(|name| format!("{}-foreign-{name}", lab.prefix))
        .collect();
    lab.entities.lock().extend(ids.iter().cloned());
    // Queued for an upstream this node no longer names: created past the
    // whole window (20 + 15 minutes), within it, and past it but held by a
    // live claim of a worker that still names it.
    for (id, status, created, due) in [
        (&ids[0], "queued", "40 minutes", "- interval '1 second'"),
        (&ids[1], "queued", "1 minute", "- interval '1 second'"),
        (&ids[2], "sending", "40 minutes", "+ interval '10 minutes'"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO content.forward_outbox (entity_id, target, status, created_at, next_attempt_at)
             VALUES ($1, 'https://old.example', $2,
                     timezone('UTC', now()) - interval '{created}', timezone('UTC', now()) {due})"
        )))
        .bind(id)
        .bind(status)
        .execute(&lab.pool)
        .await
        .unwrap();
    }
    // (Another test's job may close the first row before this call does.)
    lab.forwarder
        .close_foreign(&CancellationToken::new())
        .await
        .unwrap();
    let mut states = Vec::new();
    for id in &ids {
        let (status, _, error, _, target) = lab.row(id).await.unwrap();
        assert_eq!(target, "https://old.example");
        states.push((status, error.is_some()));
    }
    assert_eq!(
        states,
        [
            ("failed".to_string(), true),
            ("queued".to_string(), false),
            ("sending".to_string(), false),
        ]
    );
    let (_, _, error, _, _) = lab.row(&ids[0]).await.unwrap();
    assert!(error.unwrap().contains("another upstream"));
    lab.cleanup().await;
}
