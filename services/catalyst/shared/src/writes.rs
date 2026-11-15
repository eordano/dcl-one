//! The write switches every domain applies: the route-local [`Gate`]
//! middleware (the creator plane's, reused by camera-reel), the data plane's [`DataWrites`] (`DATA_WRITES_ENABLED`) and the
//! operator plane's [`OPS_WRITES_ENABLED`]. Off, a gated mutating request
//! answers 503 with a `Retry-After`; the reads keep answering. On, a write
//! still waits for the restore stamps ([`restore`]) when the node has them:
//! every gate asks [`refusal`] (the operator plane [`ops_refusal`]), so the
//! six planes hold and release together.

pub mod restore;

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

pub use restore::{Hold, RestoreStamps};

/// Every authoritative write of the operator slice answers 503 until this is
/// set; the twin of `SOCIAL_WRITES_ENABLED`.
pub const OPS_WRITES_ENABLED: &str = "OPS_WRITES_ENABLED";

pub const WRITES_DISABLED_MESSAGE: &str = "operator-plane writes are disabled on this node (OPS_WRITES_ENABLED=false): a backup restore has to be demonstrated before this node accepts credits or registry writes";

/// Why a gated write is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The plane's switch is off.
    Off,
    /// The switch is on, but the restore stamps do not vouch for this node.
    Held(Hold),
}

impl Refusal {
    /// The 503's message: the plane's own `off` text while its switch is
    /// off, else "`plane` writes are held on this node: <the stamp, its age>".
    pub fn message(&self, plane: &str, off: &'static str) -> Cow<'static, str> {
        match self {
            Refusal::Off => Cow::Borrowed(off),
            Refusal::Held(hold) => {
                Cow::Owned(format!("{plane} writes are held on this node: {hold}"))
            }
        }
    }
}

/// The one decision every write gate makes for a write it covers: `None`
/// lets it through. A switch that is on is held while the process's restore
/// stamps ([`restore::install`]) are missing or stale, or left the blobs
/// out; without stamps the switch alone decides.
pub fn refusal(switch_on: bool) -> Option<Refusal> {
    if !switch_on {
        return Some(Refusal::Off);
    }
    restore::blob_hold().map(Refusal::Held)
}

/// The planes whose writes store into the blob Storage (the writers are
/// listed by plane in this module's tests): a backup or drill that left
/// the blobs out does not vouch for them. Every plane but `ops`.
pub const BLOB_PLANES: [&str; 5] = ["content", "create", "data", "social", "worlds"];

/// [`refusal`] for the operator plane, which stores no blobs: a backup that
/// left them out still vouches for its writes.
pub fn ops_refusal(switch_on: bool) -> Option<Refusal> {
    if !switch_on {
        return Some(Refusal::Off);
    }
    restore::hold().map(Refusal::Held)
}

/// A background writer's refusal ([`refusal`] or [`ops_refusal`], as its
/// plane needs), asked at the start of each pass: the hold and its release
/// are logged once per transition, not once per tick.
pub struct PassGate {
    job: &'static str,
    refusal: fn(bool) -> Option<Refusal>,
    held: AtomicBool,
}

impl PassGate {
    pub const fn new(job: &'static str, refusal: fn(bool) -> Option<Refusal>) -> Self {
        Self {
            job,
            refusal,
            held: AtomicBool::new(false),
        }
    }

    /// `None` lets this pass write.
    pub fn check(&self, switch_on: bool) -> Option<Refusal> {
        self.decide(switch_on).0
    }

    /// The refusal and whether this pass logged a transition.
    fn decide(&self, switch_on: bool) -> (Option<Refusal>, bool) {
        let refusal = (self.refusal)(switch_on);
        let held = refusal.is_some();
        let turned = self.held.swap(held, Ordering::SeqCst) != held;
        if turned {
            match &refusal {
                Some(r) => tracing::warn!(
                    job = self.job,
                    "{}; its writes are skipped until released",
                    r.message("background", "the job's write switch is off")
                ),
                None => tracing::info!(job = self.job, "the hold is released; writes resume"),
            }
        }
        (refusal, turned)
    }
}

/// Whether `method` is a write this gate refuses while disabled.
pub fn is_gated(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// One router's view of the switch: the paths (as the router sees them,
/// after any nest prefix is stripped) whose mutating methods stay open, the
/// 503 it answers, and whether its plane stores blobs (else [`ops_refusal`]).
#[derive(Clone, Copy)]
pub struct Gate {
    pub enabled: bool,
    pub open_while_disabled: &'static [&'static str],
    pub refuse: fn(&Refusal) -> Response,
    pub blobs: bool,
}

impl Gate {
    /// Whether the switch alone refuses `method` on `path`.
    pub fn refuses(&self, method: &Method, path: &str) -> bool {
        !self.enabled && self.covers(method, path)
    }

    fn covers(&self, method: &Method, path: &str) -> bool {
        is_gated(method) && !self.open_while_disabled.contains(&path)
    }

    /// [`refusal`] or [`ops_refusal`], as the plane needs.
    pub fn refusal(&self) -> Option<Refusal> {
        if self.blobs {
            refusal(self.enabled)
        } else {
            ops_refusal(self.enabled)
        }
    }
}

/// The gate as a middleware function, for a router that layers it on its
/// write routes alone (`MethodRouter::route_layer`) rather than through
/// [`guard_with`].
pub async fn gate(State(gate): State<Gate>, req: Request, next: Next) -> Response {
    if gate.covers(req.method(), req.uri().path())
        && let Some(refusal) = gate.refusal()
    {
        return (gate.refuse)(&refusal);
    }
    next.run(req).await
}

/// Wraps a router in `gate`. Applied per router, before nesting, so the path
/// the gate sees is the route-local one and the same rule holds at the root
/// and behind the prefix.
pub fn guard_with(router: Router, gate: Gate) -> Router {
    router.layer(middleware::from_fn_with_state(gate, self::gate))
}

/// `DATA_WRITES_ENABLED`: the one switch in front of every data-plane write
/// (the social switch's twin, `social::writes`). Off (the default), a
/// mutating request to a gated router answers 503 with the lambdas envelope
/// and `Retry-After`; the reads keep answering. `POST /v1/payments/verify`
/// reads a receipt and changes nothing, so it stays open, as do the
/// economy's two relayer controls (`/v1/admin/relayer/{toggle,signer}`):
/// they flip process-local atomics behind `CATALYRST_ECONOMY_ADMIN_TOKEN`
/// and write no durable row, the social gate's `disconnect` rule, so an
/// operator can pause broadcasting or pick the signer before turning
/// writes on.
pub struct DataWrites;

impl DataWrites {
    pub const DISABLED_MESSAGE: &'static str = "data-plane writes are disabled on this node (DATA_WRITES_ENABLED=false): a backup restore has to be demonstrated before this node accepts user writes";

    const OPEN_WHILE_DISABLED: &'static [&'static str] = &[
        "/v1/payments/verify",
        "/v1/admin/relayer/toggle",
        "/v1/admin/relayer/signer",
    ];

    pub fn is_gated(method: &Method, path: &str) -> bool {
        if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return false;
        }
        !Self::OPEN_WHILE_DISABLED.contains(&path)
    }

    pub fn disabled_response() -> Response {
        Self::refused_response(&Refusal::Off)
    }

    pub fn refused_response(refusal: &Refusal) -> Response {
        let message = refusal.message("data-plane", Self::DISABLED_MESSAGE);
        let body = json!({ "ok": false, "error": "Service Unavailable", "message": message });
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::RETRY_AFTER, "3600"),
            ],
            body.to_string(),
        )
            .into_response()
    }

    /// Wraps a router in the gate; applied per router, before nesting, so the
    /// path the gate sees is the route-local one.
    pub fn guard(router: Router, writes_enabled: bool) -> Router {
        Self::guard_open(router, writes_enabled, &[])
    }

    /// `guard`, plus the router's own route-local paths that change nothing
    /// and stay open while writes are off (a quote, say).
    pub fn guard_open(
        router: Router,
        writes_enabled: bool,
        open: &'static [&'static str],
    ) -> Router {
        async fn gate(
            State((enabled, open)): State<(bool, &'static [&'static str])>,
            req: Request,
            next: Next,
        ) -> Response {
            let path = req.uri().path();
            if DataWrites::is_gated(req.method(), path)
                && !open.contains(&path)
                && let Some(refusal) = refusal(enabled)
            {
                return DataWrites::refused_response(&refusal);
            }
            next.run(req).await
        }
        router.layer(middleware::from_fn_with_state((writes_enabled, open), gate))
    }
}

/// The six write switches as `WRITES=` names them, with their variables.
#[derive(Clone, Copy, Debug, Default)]
pub struct Switches {
    pub content: bool,
    pub create: bool,
    pub data: bool,
    pub social: bool,
    pub worlds: bool,
    pub ops: bool,
}

impl Switches {
    pub fn planes(&self) -> [(&'static str, &'static str, bool); 6] {
        [
            ("content", "CONTENT_WRITES_ENABLED", self.content),
            ("create", "CREATE_WRITES_ENABLED", self.create),
            ("data", "DATA_WRITES_ENABLED", self.data),
            ("social", "SOCIAL_WRITES_ENABLED", self.social),
            ("worlds", "WORLDS_WRITES_ENABLED", self.worlds),
            ("ops", OPS_WRITES_ENABLED, self.ops),
        ]
    }
}

/// `GET /writes`: each plane's switch and whether its writes go through
/// right now, with the reason they do not and the restore stamps' ages, for
/// a front that keeps state of its own (the sites' `/create` drafts) to
/// apply the same gate.
pub fn status_document(switches: &Switches) -> serde_json::Value {
    let stamps = restore::installed();
    let hold = stamps.as_ref().and_then(|s| s.check().err());
    let blob_hold = stamps.as_ref().and_then(|s| s.check_blobs().err());
    let planes: serde_json::Map<String, serde_json::Value> = switches
        .planes()
        .into_iter()
        .map(|(plane, var, on)| {
            let held = if BLOB_PLANES.contains(&plane) {
                &blob_hold
            } else {
                &hold
            };
            let refused = if on {
                held.clone().map(Refusal::Held)
            } else {
                Some(Refusal::Off)
            };
            let reason = refused.as_ref().map(|r| match r {
                Refusal::Off => format!("{var}=false"),
                Refusal::Held(h) => h.to_string(),
            });
            (
                plane.to_string(),
                json!({ "switch": var, "on": on, "open": refused.is_none(), "reason": reason }),
            )
        })
        .collect();
    let stamps = stamps.map(|s| {
        let states: serde_json::Map<String, serde_json::Value> = s
            .states()
            .into_iter()
            .map(|st| {
                (
                    st.stamp.kind().to_string(),
                    json!({
                        "completedAt": st.completed_at,
                        "snapshot": st.snapshot,
                        "blobs": st.blobs,
                        "ageSecs": st.age.map(|a| a.as_secs()),
                        "maxAgeSecs": st.budget.as_secs(),
                        "hold": st.hold.map(|h| h.to_string()),
                    }),
                )
            })
            .collect();
        serde_json::Value::Object(states)
    });
    json!({ "planes": planes, "restoreStamps": stamps })
}

pub fn status_router(switches: Switches) -> Router {
    Router::new().route(
        "/writes",
        get(move || async move {
            (
                [(header::CACHE_CONTROL, "no-store")],
                axum::Json(status_document(&switches)),
            )
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::restore::lab::{DAY, HOUR, StampLab};
    use super::restore::{Stamp, scoped};
    use super::*;
    use crate::test_http::call;
    use axum::body::Body;
    use axum::routing::post;
    use tower::ServiceExt;

    fn refuse(refusal: &Refusal) -> Response {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            refusal
                .message("lab", "lab writes are disabled (LAB=false)")
                .into_owned(),
        )
            .into_response()
    }

    fn app(enabled: bool) -> Router {
        let inner = Router::new()
            .route("/w", post(|| async { "written" }).get(|| async { "read" }))
            .route("/open", post(|| async { "open" }));
        let gate = Gate {
            enabled,
            open_while_disabled: &["/open"],
            refuse,
            blobs: false,
        };
        Router::new()
            .nest("/p", guard_with(inner.clone(), gate))
            .merge(DataWrites::guard(
                Router::new().route("/v1/lists", post(|| async { "listed" })),
                enabled,
            ))
    }

    #[tokio::test]
    async fn guard_open_keeps_the_routers_own_open_paths_answering_while_writes_are_off() {
        let inner = || {
            Router::new()
                .route("/quote", post(|| async { "quoted" }))
                .route("/buy", post(|| async { "bought" }))
        };
        let gated = DataWrites::guard_open(inner(), false, &["/quote"]);
        assert_eq!(call(&gated, "POST", "/quote").await.0, StatusCode::OK);
        assert_eq!(
            call(&gated, "POST", "/buy").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let plain = DataWrites::guard(inner(), false);
        assert_eq!(
            call(&plain, "POST", "/quote").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn without_stamps_the_switch_alone_decides() {
        assert_eq!(refusal(false), Some(Refusal::Off));
        assert_eq!(refusal(true), None);
        assert_eq!(
            call(&app(true), "POST", "/p/w").await,
            (StatusCode::OK, "written".into())
        );
        assert_eq!(
            call(&app(false), "POST", "/p/w").await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "lab writes are disabled (LAB=false)".into()
            )
        );
    }

    #[tokio::test]
    async fn a_switch_that_is_on_is_held_by_a_stale_stamp_and_released_by_a_fresh_one() {
        let lab = StampLab::fresh();
        let app = app(true);
        scoped(lab.stamps.clone(), async {
            assert_eq!(refusal(true), None);
            assert_eq!(
                call(&app, "POST", "/p/w").await,
                (StatusCode::OK, "written".into())
            );
            lab.advance(26 * HOUR);
            let (status, body) = call(&app, "POST", "/p/w").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                body,
                "lab writes are held on this node: the last complete backup (backup.json) is 27 h old, past WRITES_BACKUP_MAX_AGE_HOURS=26"
            );
            let (status, body) = call(&app, "POST", "/v1/lists").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                v["message"],
                "data-plane writes are held on this node: the last complete backup (backup.json) is 27 h old, past WRITES_BACKUP_MAX_AGE_HOURS=26"
            );
            assert_eq!(
                call(&app, "GET", "/p/w").await,
                (StatusCode::OK, "read".into()),
                "reads keep answering"
            );
            assert_eq!(
                call(&app, "POST", "/p/open").await,
                (StatusCode::OK, "open".into()),
                "an open path stays open"
            );
            lab.write_aged(Stamp::Backup, HOUR);
            assert_eq!(
                call(&app, "POST", "/p/w").await,
                (StatusCode::OK, "written".into()),
                "the next backup releases writes without a restart"
            );
            lab.advance(8 * DAY);
            let (_, body) = call(&app, "POST", "/p/w").await;
            assert!(body.contains("the last complete backup"), "{body}");
            lab.write_aged(Stamp::Backup, HOUR);
            let (status, body) = call(&app, "POST", "/p/w").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                body.ends_with("the last passing restore drill (drill.json) is 9 d 3 h old, past WRITES_DRILL_MAX_AGE_DAYS=8"),
                "{body}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_switch_that_is_off_says_off_whatever_the_stamps() {
        let lab = StampLab::new();
        scoped(lab.stamps.clone(), async {
            assert_eq!(refusal(false), Some(Refusal::Off));
            assert!(matches!(refusal(true), Some(Refusal::Held(_))));
        })
        .await;
    }

    #[tokio::test]
    async fn a_background_pass_skips_while_held_and_logs_each_transition_once() {
        let lab = StampLab::fresh();
        let gate = PassGate::new("lab-job", refusal);
        scoped(lab.stamps.clone(), async {
            assert_eq!(gate.decide(true), (None, false));
            lab.advance(27 * HOUR);
            let (held, logged) = gate.decide(true);
            assert!(matches!(held, Some(Refusal::Held(_))) && logged);
            let (held, logged) = gate.decide(true);
            assert!(held.is_some() && !logged, "still held: no second log");
            lab.write_aged(Stamp::Backup, HOUR);
            assert_eq!(gate.decide(true), (None, true), "the release logs once");
            assert_eq!(gate.decide(true), (None, false));
        })
        .await;
    }

    #[tokio::test]
    async fn a_backup_without_blobs_holds_only_the_planes_that_store_them() {
        let lab = StampLab::fresh();
        let mut backup = serde_json::json!({
            "kind": "backup",
            "completed_at": (lab.now() - chrono::Duration::hours(1)).to_rfc3339(),
            "snapshot": "20261002T020000Z",
            "manifest_sha256": "ab",
            "blobs": { "included": false },
        });
        lab.write_json(Stamp::Backup, backup.clone());
        let blob_gate = Gate {
            enabled: true,
            open_while_disabled: &[],
            refuse,
            blobs: true,
        };
        let app = app(true).nest(
            "/b",
            guard_with(
                Router::new().route("/w", post(|| async { "stored" })),
                blob_gate,
            ),
        );
        let switches = Switches {
            content: true,
            social: true,
            data: true,
            ops: true,
            ..Default::default()
        };
        scoped(lab.stamps.clone(), async {
            assert_eq!(ops_refusal(true), None);
            assert!(matches!(refusal(true), Some(Refusal::Held(Hold::NoBlobs(_)))));
            assert_eq!(
                call(&app, "POST", "/p/w").await,
                (StatusCode::OK, "written".into()),
                "the operator plane stores no blobs: it goes through"
            );
            let (status, body) = call(&app, "POST", "/v1/lists").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "the data plane stores blobs");
            assert!(body.contains("data-plane writes are held on this node: the last complete backup (backup.json) left the blobs out"), "{body}");
            assert_eq!(
                call(&app, "POST", "/b/w").await,
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "lab writes are held on this node: the last complete backup (backup.json) left the blobs out (backup.sh --no-blobs)".into()
                )
            );
            let doc = status_document(&switches);
            assert_eq!(doc["planes"]["ops"]["open"], true);
            for plane in BLOB_PLANES {
                assert_eq!(doc["planes"][plane]["open"], false, "{plane}");
            }
            assert_eq!(doc["restoreStamps"]["backup"]["blobs"], false);
            backup["blobs"]["included"] = serde_json::json!(true);
            lab.write_json(Stamp::Backup, backup);
            assert_eq!(
                call(&app, "POST", "/b/w").await,
                (StatusCode::OK, "stored".into())
            );
            assert_eq!(status_document(&switches)["planes"]["content"]["open"], true);
        })
        .await;
    }

    /// Every non-test source file that stores into the blob Storage, with
    /// the plane whose switch gates the write; `mirror` is a read-through
    /// copy of upstream bytes, fetched again on a miss.
    #[cfg(unix)]
    const STORAGE_WRITERS: &[(&str, &str)] = &[
        ("builder/src/catalog.rs", "create"),
        ("builder/src/drafts.rs", "create"),
        ("builder/src/projects.rs", "create"),
        ("camera-reel/src/handlers/images.rs", "create"),
        ("content/src/deploy_remote_entity.rs", "content"),
        ("content/src/partial_deployments.rs", "content"),
        ("content/src/read_through.rs", "mirror"),
        ("content/src/snapshot_gen.rs", "content"),
        ("content/src/write_deployer.rs", "content"),
        ("explore/src/api/posters.rs", "mirror"),
        ("explore/src/api/satellite.rs", "mirror"),
        ("explore/src/api/world_manifest.rs", "mirror"),
        ("explore/src/events/posters.rs", "data"),
        ("shop/src/outfits/mod.rs", "data"),
        ("social/src/communities/fed_content.rs", "social"),
        ("social/src/communities/thumbnail.rs", "social"),
        ("social/src/messaging.rs", "social"),
        ("social/src/profile_images/cache.rs", "mirror"),
        ("worlds/src/core/deploy.rs", "worlds"),
        ("worlds/src/core/partial.rs", "worlds"),
        ("worlds/src/core/settings.rs", "worlds"),
        ("worlds/src/mirror_reconcile.rs", "worlds"),
        ("worlds/src/mirror_upstream.rs", "worlds"),
    ];

    /// The workspace files that call a Storage writer outside their inline
    /// test module, test files aside.
    #[cfg(unix)]
    fn storage_writers(root: &std::path::Path) -> std::collections::BTreeSet<String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if name != "tests" {
                        walk(&path, out);
                    }
                } else if name.ends_with(".rs")
                    && name != "tests.rs"
                    && !name.ends_with("_tests.rs")
                {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files);
        for krate in std::fs::read_dir(root).unwrap().flatten() {
            if krate.path().join("Cargo.toml").is_file() && krate.file_name() != "storage" {
                walk(&krate.path().join("src"), &mut files);
            }
        }
        // Every call of a Storage writer: `store`, `store_file`, `rekey` and
        // `create_staging_file`, its arguments read to the closing parenthesis
        // however rustfmt wrapped them. Not a store: an accessor's (no
        // arguments) and an atomic's (its last argument an ordering).
        let writes_blobs = |text: &str| {
            ["store(", "store_file(", "rekey(", "create_staging_file("]
                .iter()
                .any(|method| {
                    text.match_indices(&format!(".{method}")).any(|(at, call)| {
                        let mut depth = 1;
                        let args: String = text[at + call.len()..]
                            .chars()
                            .take_while(|c| {
                                depth += match c {
                                    '(' => 1,
                                    ')' => -1,
                                    _ => 0,
                                };
                                depth > 0
                            })
                            .collect();
                        let last = args
                            .split(',')
                            .map(str::trim)
                            .rfind(|a| !a.is_empty())
                            .unwrap_or("");
                        let ordering = [
                            "Ordering::",
                            "SeqCst",
                            "Relaxed",
                            "Release",
                            "Acquire",
                            "AcqRel",
                        ]
                        .iter()
                        .any(|o| last.starts_with(o))
                            || last.contains("atomic::Ordering::");
                        !args.trim().is_empty() && !ordering
                    })
                })
        };
        files
            .into_iter()
            .filter(|file| {
                let text = std::fs::read_to_string(file).unwrap();
                let tests_at = text
                    .match_indices("#[cfg(test)]\nmod ")
                    .find(|(at, _)| text[*at..].lines().nth(1).is_some_and(|l| l.ends_with('{')))
                    .map_or(text.len(), |(at, _)| at);
                writes_blobs(&text[..tests_at])
            })
            .map(|file| {
                file.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    // unix only: the source scan expects `/` paths and LF line ends, not Windows' `\` and CRLF.
    #[cfg(unix)]
    #[test]
    fn every_blob_writer_is_on_a_plane_a_backup_without_blobs_holds() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let listed: std::collections::BTreeSet<String> = STORAGE_WRITERS
            .iter()
            .map(|(file, _)| file.to_string())
            .collect();
        assert_eq!(
            storage_writers(&root),
            listed,
            "a file that stores into Storage names its plane in STORAGE_WRITERS"
        );
        for (file, plane) in STORAGE_WRITERS {
            assert!(
                *plane == "mirror" || BLOB_PLANES.contains(plane),
                "{file} stores blobs on the {plane} plane, which a backup without blobs does not hold"
            );
        }
        let blobless: Vec<&str> = Switches::default()
            .planes()
            .into_iter()
            .map(|(plane, _, _)| plane)
            .filter(|plane| !BLOB_PLANES.contains(plane))
            .collect();
        assert_eq!(blobless, ["ops"]);
    }

    #[tokio::test]
    async fn the_status_document_names_each_plane_and_the_stamps() {
        let switches = Switches {
            create: true,
            social: true,
            ..Default::default()
        };
        let doc = status_document(&switches);
        assert_eq!(doc["restoreStamps"], serde_json::Value::Null);
        assert_eq!(doc["planes"]["create"]["open"], true);
        assert_eq!(doc["planes"]["content"]["open"], false);
        assert_eq!(
            doc["planes"]["content"]["reason"],
            "CONTENT_WRITES_ENABLED=false"
        );
        let lab = StampLab::new();
        lab.write_aged(Stamp::Backup, 2 * HOUR);
        let doc = scoped(lab.stamps.clone(), async { status_document(&switches) }).await;
        assert_eq!(doc["planes"]["create"]["open"], false);
        assert_eq!(
            doc["planes"]["create"]["reason"],
            "there is no restore-drill stamp (drill.json in WRITES_RESTORE_STAMP_DIR)"
        );
        assert_eq!(doc["restoreStamps"]["backup"]["ageSecs"], 7200);
        assert_eq!(doc["restoreStamps"]["backup"]["maxAgeSecs"], 26 * 3600);
        assert_eq!(
            doc["restoreStamps"]["drill"]["completedAt"],
            serde_json::Value::Null
        );
        let res = status_router(switches)
            .oneshot(Request::get("/writes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    }
}
