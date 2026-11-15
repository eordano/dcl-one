use crate::deploy::{self, Prepared};
use crate::ux::{self, TrySteps, UserError};
use anyhow::Result;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub struct LinkerDeploy {
    /// The scene root: the signing page is the preview server's landing page.
    pub dir: PathBuf,
    pub prepared: Prepared,
    pub target_content: String,
    pub world: Option<String>,
    pub needs_delete: bool,
    pub timestamp_override: Option<i64>,
    pub entity_out: Option<PathBuf>,
    pub scene_title: String,
    pub base_parcel: String,
    pub multi_scene: bool,
    pub gate: deploy::PermissionGate,
    /// What `X-Deploy-Token` must carry to cancel through the deploy API.
    pub deploy_token: String,
    /// Told to a program through `/api/info`.
    pub skip_validations: bool,
}

pub struct LinkerOptions {
    pub port: Option<u16>,
    pub open_browser: bool,
    pub timeout: Duration,
    /// A program drives the run: stdout carries JSON lines, the CLI ends
    /// with the deployment instead of keeping the preview up.
    pub programmatic: bool,
    /// The caller hosts the signing routes on a server it already runs (the
    /// preview server's /deploy/sign/): no listener bound, no browser opened.
    pub host: Option<HostSigner>,
}

#[derive(Clone)]
pub struct HostSigner {
    pub register: Arc<dyn Fn(Arc<LinkerState>) + Send + Sync>,
    pub url: String,
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

pub const MIN_DEPLOY_TOKEN_LENGTH: usize = 16;

/// The `--deploy-token` given, or a random one for this run.
pub fn resolve_deploy_token(given: Option<&str>) -> Result<String> {
    match given {
        None => Ok(hex::encode(rand::random::<[u8; 16]>())),
        Some(t) if t.len() < MIN_DEPLOY_TOKEN_LENGTH => Err(UserError::new(
            format!("--deploy-token must be at least {MIN_DEPLOY_TOKEN_LENGTH} characters long"),
            TrySteps::one("pass a longer token, or drop the flag for a random one per run"),
        )
        .into()),
        Some(t) => Ok(t.to_string()),
    }
}

/// Constant time: a guess learns nothing from how long the comparison took.
pub fn token_matches(provided: &str, expected: &str) -> bool {
    let (a, b) = (provided.as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Where the signing page carries the token: a fragment never travels in a
/// request, and the page's cancel button reads it from there.
pub fn linker_fragment(token: &str) -> String {
    format!("#token={}", deploy::encode_segment(token))
}

/// The deployment of this run as `/api/deploy` tells it, in the shape
/// `@dcl/sdk-commands` serves.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeploymentState {
    pub phase: &'static str,
    pub upload: Option<DeploymentUpload>,
    pub result: Option<DeploymentResult>,
    pub error: Option<DeploymentError>,
    #[serde(rename = "cancelRequested")]
    pub cancel_requested: bool,
}

impl DeploymentState {
    pub fn is_final(&self) -> bool {
        matches!(self.phase, "deployed" | "failed" | "cancelled")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeploymentUpload {
    pub stage: &'static str,
    #[serde(rename = "sentBytes")]
    pub sent_bytes: u64,
    #[serde(rename = "totalBytes")]
    pub total_bytes: u64,
    /// Always null: this CLI uploads in one request.
    #[serde(rename = "completedBatches")]
    pub completed_batches: Option<u32>,
    #[serde(rename = "totalBatches")]
    pub total_batches: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeploymentResult {
    #[serde(rename = "entityId")]
    pub entity_id: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeploymentError {
    pub code: String,
    pub message: String,
}

/// A publish that went through: the id the server holds and the outcome line.
#[derive(Clone, Debug)]
pub struct Published {
    pub entity_id: String,
    pub message: String,
}

pub const DEPLOY_CANCELLED: &str = "DEPLOY_CANCELLED";
pub const DEPLOY_CANCELLED_AFTER_UNDEPLOY: &str = "DEPLOY_CANCELLED_AFTER_UNDEPLOY";
pub const DEPLOY_FAILED: &str = "DEPLOY_FAILED";

/// A run ended through `DELETE /api/deploy`; the code says what happened to
/// the world meanwhile.
#[derive(Clone, Debug)]
pub struct Cancelled {
    pub code: &'static str,
}

impl Cancelled {
    pub fn message(&self) -> &'static str {
        match self.code {
            DEPLOY_CANCELLED_AFTER_UNDEPLOY => {
                "Deployment cancelled after the world's previous scenes were removed. Deploy again to publish the scene."
            }
            _ => "Deployment cancelled",
        }
    }
}

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Cancelled {}

fn cancel_error(c: &Cancelled) -> Option<DeploymentError> {
    (c.code != DEPLOY_CANCELLED).then(|| DeploymentError {
        code: c.code.to_string(),
        message: c.message().to_string(),
    })
}

/// The error as the API tells it: rendered for a reader, minus the prompt.
fn rendered(e: &anyhow::Error) -> String {
    let text = crate::ux::render(e, false, false);
    let text = text.trim();
    text.strip_prefix("Error:")
        .map(str::trim)
        .unwrap_or(text)
        .to_string()
}

/// The final state a programmatic run prints from its outcome, when the run
/// never reached the API (headless signing, a failure before the server):
/// the same shape the API serves.
pub fn final_state(outcome: &Result<Published>) -> DeploymentState {
    let (phase, result, error, cancel_requested) = match outcome {
        Ok(p) => (
            "deployed",
            Some(DeploymentResult {
                entity_id: p.entity_id.clone(),
                message: p.message.clone(),
            }),
            None,
            false,
        ),
        Err(e) => match e.downcast_ref::<Cancelled>() {
            Some(c) => ("cancelled", None, cancel_error(c), true),
            None => (
                "failed",
                None,
                Some(DeploymentError {
                    code: DEPLOY_FAILED.to_string(),
                    message: rendered(e),
                }),
                false,
            ),
        },
    };
    DeploymentState {
        phase,
        upload: None,
        result,
        error,
        cancel_requested,
    }
}

/// The deploy API's view of the run, beside the signing state.
#[derive(Default)]
struct Resource {
    /// A signed payload was accepted: nothing else may start.
    started: bool,
    /// The world's other scenes are being removed: a cancel is refused.
    undeploying: bool,
    cancel_requested: bool,
    /// Someone follows the run through the API: the CLI lingers after the
    /// end so the final state stays readable.
    followed: bool,
    outcome: Option<Final>,
}

#[derive(Clone)]
enum Final {
    Deployed(Published),
    Failed(DeploymentError),
    Cancelled(Option<DeploymentError>),
}

#[derive(Debug, PartialEq)]
pub enum CancelOutcome {
    /// Before any signature: ended right away.
    Cancelled,
    /// The upload in flight is being aborted; the run settles the outcome.
    Cancelling,
    /// The world's scenes are being removed: too late to stop cleanly.
    Refused,
    Final,
}

/// How long the CLI stays after the end for a follower to read the final
/// state, and when nobody followed.
pub const FOLLOWED_SETTLE: Duration = Duration::from_secs(2);
pub const SETTLE: Duration = Duration::from_millis(100);

pub fn linker_timeout() -> Duration {
    let secs = std::env::var("DCL_SCENE_CLI_LINKER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

struct PendingEntity {
    bytes: Vec<u8>,
    delete_payload: Option<String>,
    /// Mint order, oldest lowest: what falls out past [`PENDING_KEEP`]. A
    /// count, not a time, so two mints inside one clock tick still order.
    seq: u64,
    /// The entity timestamp, so a re-mint never repeats a signable id.
    ts: i64,
}

/// The entity the panel currently shows. The deploy page re-renders the
/// panel every couple of seconds while it waits, and the browser keeps the
/// first render's id for as long as the wallet takes; minting per render
/// would leave that id behind within a minute (which is exactly what used
/// to happen: 32 renders, then "unknown or stale entity id" for anyone who
/// took over a minute to sign).
struct Minted {
    entity_id: String,
    delete_payload: Option<String>,
    at: Instant,
}

/// How long one minted entity is re-rendered as-is. A fresh timestamp after
/// this keeps the entity from losing to anything published on the parcels in
/// the meantime; the ids minted before stay signable (see [`PENDING_KEEP`]).
const MINT_FRESH: Duration = Duration::from_secs(600);

/// How many minted ids stay signable at once: with one mint per
/// [`MINT_FRESH`], the panel a browser opened stays valid for over an hour.
const PENDING_KEEP: usize = 8;

pub(crate) type DoneSender = tokio::sync::oneshot::Sender<Result<Published>>;

pub struct LinkerState {
    dep: LinkerDeploy,
    pending: Mutex<HashMap<String, PendingEntity>>,
    minted: Mutex<Option<Minted>>,
    progress: deploy::UploadProgress,
    done: Mutex<Option<DoneSender>>,
    /// The address that signed, kept past the upload: the preview pages
    /// personalize on it, and a signature is the one moment a wallet names
    /// itself.
    signer: Mutex<Option<String>>,
    resource: Mutex<Resource>,
    /// Flipped once by a cancel; the upload in flight selects on it.
    cancel: tokio::sync::watch::Sender<bool>,
}

impl LinkerState {
    pub(crate) fn target_content(&self) -> &str {
        &self.dep.target_content
    }

    pub(crate) fn dep(&self) -> &LinkerDeploy {
        &self.dep
    }

    pub(crate) fn deploy_token(&self) -> &str {
        &self.dep.deploy_token
    }

    /// The id a program signs (minted now if none is fresh) and the removal
    /// payload a world overwrite also needs signed.
    pub(crate) fn minted(self: &Arc<Self>) -> Result<(String, Option<String>)> {
        mint(self)
    }

    /// Resolves on every change of the run after it is taken.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.progress.changes()
    }

    fn resource(&self) -> std::sync::MutexGuard<'_, Resource> {
        self.resource.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a signed payload can still start the run.
    pub(crate) fn can_start(&self) -> bool {
        let r = self.resource();
        !r.started && !r.cancel_requested && r.outcome.is_none()
    }

    pub(crate) fn follow(&self) {
        self.resource().followed = true;
    }

    pub(crate) fn settle_delay(&self) -> Duration {
        match self.resource().followed {
            true => FOLLOWED_SETTLE,
            false => SETTLE,
        }
    }

    pub(crate) fn deployment_state(&self) -> DeploymentState {
        let progress = self.progress.snapshot();
        let upload = match progress.phase {
            "idle" => None,
            phase => Some(DeploymentUpload {
                stage: match phase {
                    "checking" | "staging" => "checking",
                    "uploading" => "uploading",
                    _ => "processing",
                },
                sent_bytes: progress.sent,
                total_bytes: progress.total,
                completed_batches: None,
                total_batches: None,
            }),
        };
        let r = self.resource();
        let (phase, result, error) = match &r.outcome {
            Some(Final::Deployed(p)) => (
                "deployed",
                Some(DeploymentResult {
                    entity_id: p.entity_id.clone(),
                    message: p.message.clone(),
                }),
                None,
            ),
            Some(Final::Failed(e)) => ("failed", None, Some(e.clone())),
            Some(Final::Cancelled(e)) => ("cancelled", None, e.clone()),
            None if r.undeploying => ("undeploying", None, None),
            None if r.started => ("uploading", None, None),
            None => ("awaiting-signature", None, None),
        };
        DeploymentState {
            phase,
            upload,
            result,
            error,
            cancel_requested: r.cancel_requested,
        }
    }

    /// `DELETE /api/deploy`: before the signature the run ends here; during
    /// the upload the request in flight is dropped and the run settles.
    pub(crate) fn cancel(&self) -> CancelOutcome {
        let before_signature = {
            let mut r = self.resource();
            if r.outcome.is_some() {
                return CancelOutcome::Final;
            }
            if r.undeploying {
                return CancelOutcome::Refused;
            }
            r.followed = true;
            if r.cancel_requested {
                return CancelOutcome::Cancelling;
            }
            r.cancel_requested = true;
            !r.started
        };
        let _ = self.cancel.send(true);
        self.progress.touch();
        if before_signature {
            self.conclude(Err(Cancelled {
                code: DEPLOY_CANCELLED,
            }
            .into()));
            return CancelOutcome::Cancelled;
        }
        CancelOutcome::Cancelling
    }

    /// The run's one ending: recorded for the API, handed to the CLI.
    pub(crate) fn conclude(&self, outcome: Result<Published>) {
        self.end(outcome, false);
    }

    /// The signature deadline: ends a run that no signature started, and
    /// says whether it did. A started run goes on to its own end; the check
    /// and the ending share one lock with [`claim`], so a signature that
    /// lands as the deadline fires is either refused or runs to the end.
    pub(crate) fn expire(&self, e: anyhow::Error) -> bool {
        self.end(Err(e), true)
    }

    fn end(&self, outcome: Result<Published>, unsigned_only: bool) -> bool {
        let ended = match &outcome {
            Ok(p) => Final::Deployed(p.clone()),
            Err(e) => match e.downcast_ref::<Cancelled>() {
                Some(c) => Final::Cancelled(cancel_error(c)),
                None => Final::Failed(DeploymentError {
                    code: DEPLOY_FAILED.to_string(),
                    message: rendered(e),
                }),
            },
        };
        {
            let mut r = self.resource();
            if r.outcome.is_some() || (unsigned_only && r.started) {
                return false;
            }
            r.outcome = Some(ended);
        }
        self.progress.touch();
        finish(&self.done, outcome);
        true
    }

    pub(crate) fn signer_address(&self) -> Option<String> {
        self.signer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The upload so far: idle until a signature arrives.
    pub(crate) fn progress(&self) -> deploy::ProgressState {
        self.progress.snapshot()
    }

    /// What [`sign`] does the moment a signature arrives, for tests that need
    /// a "the wallet already answered" state without a wallet.
    #[cfg(test)]
    pub(crate) fn note_signer_for_tests(&self, address: &str) {
        *self.signer.lock().unwrap_or_else(PoisonError::into_inner) = Some(address.to_string());
    }
}

pub fn new_state(
    dep: LinkerDeploy,
) -> (
    Arc<LinkerState>,
    tokio::sync::oneshot::Receiver<Result<Published>>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (
        Arc::new(LinkerState {
            dep,
            pending: Mutex::new(HashMap::new()),
            minted: Mutex::new(None),
            progress: deploy::UploadProgress::default(),
            done: Mutex::new(Some(tx)),
            signer: Mutex::new(None),
            resource: Mutex::new(Resource::default()),
            cancel: tokio::sync::watch::Sender::new(false),
        }),
        rx,
    )
}

/// The id the panel draws: the one already minted while it is fresh, else a
/// new entity registered pending. Only the oldest ids past [`PENDING_KEEP`]
/// stop being signable.
fn mint(st: &Arc<LinkerState>) -> Result<(String, Option<String>)> {
    let d = &st.dep;
    let mut minted = st.minted.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(m) = minted.as_ref().filter(|m| m.at.elapsed() < MINT_FRESH) {
        return Ok((m.entity_id.clone(), m.delete_payload.clone()));
    }
    let ts = match d.timestamp_override {
        Some(t) => t,
        None => {
            let floor = st
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .map(|p| p.ts)
                .max()
                .map_or(0, |t| t + 1);
            deploy::now_ms().max(floor)
        }
    };
    let (entity_id, entity_bytes) = deploy::build_entity(&d.prepared, ts)?;
    let delete_payload = match d.needs_delete {
        true => d.world.as_deref().map(deploy::build_delete_payload),
        false => None,
    };
    let now = Instant::now();
    {
        let mut pending = st.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = pending.values().map(|p| p.seq + 1).max().unwrap_or(0);
        pending.insert(
            entity_id.clone(),
            PendingEntity {
                bytes: entity_bytes,
                delete_payload: delete_payload.clone(),
                seq,
                ts,
            },
        );
        while pending.len() > PENDING_KEEP {
            let oldest = pending
                .iter()
                .min_by_key(|(_, p)| p.seq)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => pending.remove(&id),
                None => break,
            };
        }
    }
    *minted = Some(Minted {
        entity_id: entity_id.clone(),
        delete_payload: delete_payload.clone(),
        at: now,
    });
    Ok((entity_id, delete_payload))
}

/// The signing panel. The entity is minted once and registered pending, so
/// the id printed is the id the wallet signs, and every re-render within
/// [`MINT_FRESH`] draws the same id; the browser is left exactly one job,
/// the wallet hand-off. `api` is the absolute path the script POSTs the
/// signature to.
pub(crate) fn sign_section(st: &Arc<LinkerState>, api: &str) -> String {
    use crate::start::chrome::{esc, kv};
    let d = &st.dep;
    let (entity_id, delete_payload) = match mint(st) {
        Ok(x) => x,
        Err(e) => {
            return format!(
                r#"<div class="panel panel--warn" id="sign-panel"><h2>Cannot sign</h2><p class="note">could not build the entity: {}</p></div>"#,
                esc(&format!("{e:#}"))
            );
        }
    };
    let realm_url = match &d.world {
        Some(w) => crate::deep_link::world_realm_url(&d.target_content, w),
        None => d
            .target_content
            .trim_end_matches('/')
            .trim_end_matches("/content")
            .to_string(),
    };
    let deep_link = crate::deep_link::realm_deep_link(
        &realm_url,
        crate::deep_link::parse_position(Some(&d.base_parcel)),
    );
    let where_to = match &d.world {
        Some(w) if d.multi_scene => format!("world {w} (multi-scene, additive)"),
        Some(w) => format!("world {w}"),
        None => "Genesis City LAND".to_string(),
    };
    let total: usize = d.prepared.files.iter().map(|(_, _, b)| b.len()).sum();
    let delete_warn = match &delete_payload {
        Some(_) => {
            r#"<p class="note sign-warn">This deploy also REMOVES the scenes currently
    published on other parcels of the world; the wallet asks for a second signature authorizing
    the removal.</p>"#
        }
        None => "",
    };
    let parcels = if d.prepared.pointers.len() > 6 {
        format!(
            "{} parcels · base {}",
            d.prepared.pointers.len(),
            d.base_parcel
        )
    } else {
        format!(
            "{}  (base {})",
            d.prepared.pointers.join("  "),
            d.base_parcel
        )
    };
    format!(
        r#"<div class="panel" id="sign-panel" data-api="{api}" data-entity-id="{id}"{delete_attr} data-deep-link="{deep}" data-target="{target}">
  <h2>Sign the deployment</h2>
  <span class="note">Connect the wallet that may publish this scene; nothing uploads until it answers.</span>
  <div class="kvs">
    {scene}{where_kv}{parcels}{entity}{payload}
  </div>
  {delete_warn}
  <button class="btn btn--primary btn--lg jn__cta" id="sign-go" type="button">Connect wallet and sign</button>
  <button class="btn btn--ghost btn--lg" id="sign-cancel" type="button" hidden>Cancel this deployment</button>
  <div class="sign-progress" id="sign-progress" hidden>
    <div class="sign-progress__head"><span class="sign-progress__big" id="sign-progress-big"></span><span class="sign-progress__pct" id="sign-progress-pct"></span></div>
    <div class="u-progress sign-progress__bar"><span class="sign-progress__fill" id="sign-progress-fill"></span></div>
    <div class="sign-progress__meta" id="sign-progress-meta"></div>
  </div>
  <p class="note sign-status" id="sign-status" hidden></p>
  <noscript><p class="note">The wallet hand-off needs JavaScript; everything above is exact without it.</p></noscript>
</div>"#,
        api = esc(api),
        id = esc(&entity_id),
        delete_attr = match &delete_payload {
            Some(p) => format!(r#" data-delete-payload="{}""#, esc(p)),
            None => String::new(),
        },
        deep = esc(&deep_link),
        target = esc(&d.target_content),
        scene = kv("Scene", esc(&d.scene_title)),
        where_kv = kv("Deploying to", esc(&where_to)),
        parcels = kv("Parcels", esc(&parcels)),
        entity = kv("Entity", format!("<code>{}</code>", esc(&entity_id))),
        payload = kv(
            "Payload",
            esc(&format!(
                "{} files · {} · to {}",
                d.prepared.files.len(),
                deploy::human_size(total as u64),
                d.target_content
            )),
        ),
    )
}

#[derive(Deserialize)]
pub(crate) struct SignReq {
    address: String,
    signature: String,
    #[serde(rename = "entityId")]
    entity_id: String,
    #[serde(rename = "deleteSignature")]
    delete_signature: Option<String>,
}

fn retry(error: &str) -> Json<Value> {
    Json(json!({ "ok": false, "fatal": false, "error": error }))
}

/// A signed payload: the chain the content server verifies and the id it
/// covers. The panel sends a bare signature; the API sends the chain.
pub(crate) struct Signed {
    pub address: String,
    pub entity_id: String,
    pub auth_chain: Value,
    pub delete_signature: Option<String>,
}

pub(crate) enum SignOutcome {
    Published(Published),
    /// The id is not pending here any more: the page re-renders a fresh one.
    Stale,
    /// Nothing ran; the message says what the payload lacks.
    Retry(&'static str),
    /// The run ended in this error, already concluded.
    Fatal(String),
    /// A run already started, or the deployment was cancelled.
    Refused,
}

const NEEDS_DELETE_SIGNATURE: &str = "this deploy also removes the existing world scenes and needs the second signature \u{2014} sign both prompts";

/// What stops a payload before anything runs.
fn precheck(st: &LinkerState, signed: &Signed) -> Option<SignOutcome> {
    if !st.can_start() {
        return Some(SignOutcome::Refused);
    }
    let pending = st.pending.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(entry) = pending.get(&signed.entity_id) else {
        drop(pending);
        *st.minted.lock().unwrap_or_else(PoisonError::into_inner) = None;
        return Some(SignOutcome::Stale);
    };
    if entry.delete_payload.is_some() && signed.delete_signature.is_none() {
        return Some(SignOutcome::Retry(NEEDS_DELETE_SIGNATURE));
    }
    None
}

fn fatal(st: &LinkerState, e: anyhow::Error) -> SignOutcome {
    let msg = crate::ux::render(&e, false, false);
    st.conclude(Err(e));
    SignOutcome::Fatal(msg)
}

/// Takes the pending entity and marks the run started. Of two payloads that
/// arrive together one wins; the other is refused here, before any answer.
fn claim(st: &LinkerState, signed: &Signed) -> Result<PendingEntity, SignOutcome> {
    if let Some(stop) = precheck(st, signed) {
        return Err(stop);
    }
    let mut r = st.resource();
    if r.started || r.cancel_requested || r.outcome.is_some() {
        return Err(SignOutcome::Refused);
    }
    let mut pending = st.pending.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(entry) = pending.remove(&signed.entity_id) else {
        return Err(SignOutcome::Stale);
    };
    r.started = true;
    Ok(entry)
}

pub(crate) type Run = tokio::task::JoinHandle<SignOutcome>;

/// Starts the run as its own task, or says why the payload cannot run. The
/// run belongs to no request: a caller that goes away (a client that timed
/// out or disconnected) drops the handle and the run still reaches its end.
pub(crate) fn start_signed(st: &Arc<LinkerState>, signed: Signed) -> Result<Run, SignOutcome> {
    let pending = claim(st, &signed)?;
    let st = st.clone();
    Ok(tokio::spawn(async move {
        run_claimed(&st, signed, pending).await
    }))
}

/// The end of a started run. A run that panicked is concluded as failed, so
/// its state never stays `uploading`.
pub(crate) async fn joined(st: &LinkerState, run: Run) -> SignOutcome {
    match run.await {
        Ok(outcome) => outcome,
        Err(e) => fatal(st, anyhow::anyhow!("the deployment stopped: {e}")),
    }
}

pub(crate) async fn run_signed(st: &Arc<LinkerState>, signed: Signed) -> SignOutcome {
    match start_signed(st, signed) {
        Ok(run) => joined(st, run).await,
        Err(stop) => stop,
    }
}

/// The run itself: check the wallet, remove the world's other scenes when
/// asked, upload. A cancel through the API drops the upload in flight unless
/// every byte is out, when the server's answer is the truth.
async fn run_claimed(st: &LinkerState, signed: Signed, pending: PendingEntity) -> SignOutcome {
    *st.signer.lock().unwrap_or_else(PoisonError::into_inner) = Some(signed.address.clone());
    st.progress.touch();
    let world = st.dep.world.as_deref();
    if let Err(e) = st.dep.gate.verify(&signed.address).await {
        return fatal(st, e);
    }
    let mut undeployed = false;
    if let Some(payload) = &pending.delete_payload {
        let Some(dsig) = &signed.delete_signature else {
            return fatal(
                st,
                UserError::new(NEEDS_DELETE_SIGNATURE, TrySteps::one("sign both prompts")).into(),
            );
        };
        {
            let mut r = st.resource();
            if r.cancel_requested {
                drop(r);
                return fatal(
                    st,
                    Cancelled {
                        code: DEPLOY_CANCELLED,
                    }
                    .into(),
                );
            }
            r.undeploying = true;
        }
        st.progress.touch();
        let chain = deploy::simple_auth_chain(&signed.address, payload, dsig);
        let removed =
            deploy::send_world_delete(&st.dep.target_content, world.unwrap_or_default(), &chain)
                .await;
        st.resource().undeploying = false;
        st.progress.touch();
        if let Err(e) = removed {
            return fatal(st, e);
        }
        undeployed = true;
    }
    let destination = match st.dep.world.as_deref() {
        Some(name) => deploy::UploadDestination::World {
            name,
            multi_scene: st.dep.multi_scene,
        },
        None => deploy::UploadDestination::Land {
            base: &st.dep.base_parcel,
            parcels: st.dep.prepared.pointers.len(),
        },
    };
    let entity = deploy::UploadEntity {
        id: &signed.entity_id,
        bytes: pending.bytes.clone(),
        files: &st.dep.prepared.files,
    };
    let upload = deploy::upload_entity_with_chain_to(
        &st.dep.target_content,
        entity,
        &signed.address,
        signed.auth_chain.clone(),
        destination,
        &st.progress,
    );
    tokio::pin!(upload);
    let mut cancel = st.cancel.subscribe();
    let answered = tokio::select! {
        r = &mut upload => Some(r),
        _ = cancel.wait_for(|c| *c) => None,
    };
    let uploaded = match answered {
        Some(r) => r,
        // Every byte is out: the server may publish regardless, so its
        // answer is the truth.
        None if st.progress.snapshot().phase == "validating" => upload.await,
        None => Err(Cancelled {
            code: match undeployed {
                true => DEPLOY_CANCELLED_AFTER_UNDEPLOY,
                false => DEPLOY_CANCELLED,
            },
        }
        .into()),
    };
    match uploaded {
        Ok(message) => {
            if let Some(path) = &st.dep.entity_out
                && let Err(e) = std::fs::write(path, &pending.bytes)
            {
                tracing::warn!("could not write --entity-out {}: {e}", path.display());
            }
            let published = Published {
                entity_id: signed.entity_id.clone(),
                message,
            };
            st.conclude(Ok(published.clone()));
            SignOutcome::Published(published)
        }
        Err(e) => fatal(st, e),
    }
}

/// `POST /deploy/sign`, the panel's route: a bare signature over the entity
/// id, wrapped as a simple auth chain.
pub(crate) async fn sign(
    State(st): State<Arc<LinkerState>>,
    Json(req): Json<SignReq>,
) -> Json<Value> {
    let auth_chain = deploy::simple_auth_chain(&req.address, &req.entity_id, &req.signature);
    let signed = Signed {
        address: req.address,
        entity_id: req.entity_id,
        auth_chain,
        delete_signature: req.delete_signature,
    };
    match run_signed(&st, signed).await {
        SignOutcome::Published(p) => Json(json!({ "ok": true, "message": p.message })),
        SignOutcome::Stale => Json(json!({
            "ok": false,
            "fatal": false,
            "stale": true,
            "error": "that signing request is no longer on the server \u{2014} the preview was rebuilt or restarted since this page loaded",
        })),
        SignOutcome::Retry(m) => retry(m),
        SignOutcome::Fatal(m) => Json(json!({ "ok": false, "fatal": true, "error": m })),
        SignOutcome::Refused => Json(json!({
            "ok": false,
            "fatal": true,
            "error": "this deployment already started or was cancelled \u{2014} nothing more can be signed",
        })),
    }
}

/// Hand the outcome to the CLI once, after a beat so the page's response
/// lands first.
pub(crate) fn finish<T: Send + 'static>(
    done: &Mutex<Option<tokio::sync::oneshot::Sender<Result<T>>>>,
    result: Result<T>,
) {
    let tx = done.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(tx) = tx {
        tokio::spawn(async move {
            let _ = tx.send(result);
        });
    }
}

/// The program of the system that opens a URL with the application
/// registered for it.
pub(crate) const OPENER: &str = if cfg!(target_os = "macos") {
    "open"
} else if cfg!(target_os = "windows") {
    "explorer"
} else {
    "xdg-open"
};

/// Starts [`OPENER`] on `url`. `Ok` says that the opener process started,
/// not that an application opened the URL.
pub(crate) fn spawn_opener(url: &str) -> std::io::Result<std::process::Child> {
    spawn_apart(OPENER, url)
}

/// Starts `program` on `url` with no stream of this process and, on Unix, in
/// a process group of its own: a Ctrl-C in the terminal goes to the group of
/// the CLI, and not to the opener or to an application that the opener runs.
fn spawn_apart(program: &str, url: &str) -> std::io::Result<std::process::Child> {
    let mut command = std::process::Command::new(program);
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn()
}

pub(crate) fn spawn_browser(url: &str) {
    if spawn_opener(url).is_err() {
        ux::note("could not open a browser automatically \u{2014} open the URL above manually");
    }
}

pub(crate) fn fmt_wait(timeout: Duration) -> String {
    let secs = timeout.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute".to_string()
        } else {
            format!("{mins} minutes")
        }
    } else if secs == 1 {
        "1 second".to_string()
    } else {
        format!("{secs} seconds")
    }
}

fn timeout_error(timeout: Duration, url: &str) -> anyhow::Error {
    UserError::new(
        format!(
            "no signature arrived within {} \u{2014} deployment abandoned",
            fmt_wait(timeout)
        ),
        TrySteps::one("re-run dcl-scene-cli deploy and sign on the printed URL")
            .and("raise the wait with DCL_SCENE_CLI_LINKER_TIMEOUT_SECS=<seconds>")
            .and("for headless deploys set DCL_PRIVATE_KEY or pass --sign-key <file>"),
    )
    .why(format!("the signing page was served at {url}"))
    .into()
}

/// The run's outcome. `timeout` is the wait for a signature only: once a
/// signature starts the run, the wait lasts until the run ends.
pub(crate) async fn await_outcome(
    st: &LinkerState,
    mut rx: tokio::sync::oneshot::Receiver<Result<Published>>,
    timeout: Duration,
    url: &str,
) -> Result<Published> {
    let res = tokio::select! {
        res = &mut rx => res,
        _ = tokio::time::sleep(timeout) => {
            st.expire(timeout_error(timeout, url));
            rx.await
        }
    };
    res.unwrap_or_else(|_| {
        Err(UserError::new(
            "the signing flow ended without a result",
            TrySteps::one("re-run dcl-scene-cli deploy"),
        )
        .into())
    })
}

/// Where the signing server of `deploy` listens and whom its signing routes
/// answer. Resolved once for a run: the listener, the printed origin and the
/// gates of the routes all read this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigningBind {
    /// `--host`, then `DCL_SCENE_CLI_LINKER_HOST`, then IPv4 loopback.
    pub addr: IpAddr,
    /// Remote signing: the signing routes answer a peer that is not this
    /// machine. `DCL_SCENE_CLI_LINKER_HOST` is the switch, an address that
    /// is not loopback; `--host` alone never opens them, and a listener on
    /// loopback keeps them closed.
    pub remote: bool,
    /// The variable set to `0.0.0.0` on this command line would turn remote
    /// signing on. False for `--host` with a loopback address: the flag wins
    /// over the variable, and no other machine reaches that listener.
    pub openable: bool,
}

impl SigningBind {
    /// The default, and a run whose signing routes another server hosts.
    pub const LOCAL: Self = SigningBind {
        addr: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        remote: false,
        openable: true,
    };

    /// `flag` is `--host`, `variable` the address of
    /// `DCL_SCENE_CLI_LINKER_HOST`. With remote signing off the routes admit
    /// a loopback peer and a loopback `Host` only, so a listener on one
    /// other address would refuse each caller: that `--host` is an error.
    fn resolve(flag: Option<IpAddr>, variable: Option<IpAddr>) -> Result<Self> {
        let addr = flag.or(variable).unwrap_or(Self::LOCAL.addr);
        let remote = variable.is_some_and(|v| !v.is_loopback()) && !addr.is_loopback();
        if !remote && !addr.is_loopback() && !addr.is_unspecified() {
            return Err(UserError::new(
                format!("--host {addr} needs remote signing, and remote signing is off"),
                TrySteps::one(
                    "pass --host 0.0.0.0 to open the preview to other devices and sign on this machine",
                )
                .and(format!(
                    "or set DCL_SCENE_CLI_LINKER_HOST={addr} to sign from other devices too"
                )),
            )
            .why(format!(
                "the signing routes answer this machine only, through a loopback address; a server on {addr} alone would refuse each request"
            ))
            .usage()
            .into());
        }
        Ok(SigningBind {
            addr,
            remote,
            openable: !flag.is_some_and(|flag| flag.is_loopback()),
        })
    }

    /// The note under the signing URL on a signature from another device.
    /// `None` when the command line keeps the listener on a loopback
    /// address: no value of the variable opens that run. `None` too with
    /// remote signing on one address: the URL names that address.
    pub(crate) fn other_device_note(self) -> Option<&'static str> {
        match (self.remote, self.openable) {
            (true, _) if self.addr.is_unspecified() => {
                Some("from another device, replace localhost with this machine's address")
            }
            (true, _) => None,
            (false, true) => {
                Some("to sign from another device, re-run with DCL_SCENE_CLI_LINKER_HOST=0.0.0.0")
            }
            (false, false) => None,
        }
    }
}

/// [`SigningBind`] of this run, from `--host` and the environment.
pub fn signing_bind(flag: Option<IpAddr>) -> Result<SigningBind> {
    let variable = match std::env::var("DCL_SCENE_CLI_LINKER_HOST") {
        // The one name that is not an address.
        Ok(host) if host.trim().eq_ignore_ascii_case("localhost") => Some(SigningBind::LOCAL.addr),
        Ok(host) if !host.trim().is_empty() => Some(host.trim().parse().map_err(|_| {
            UserError::new(
                format!(
                    "DCL_SCENE_CLI_LINKER_HOST is not an IP address: {}",
                    host.trim()
                ),
                TrySteps::one("set it to an address, for example 0.0.0.0")
                    .and("or remove the variable to sign on this machine"),
            )
        })?),
        _ => None,
    };
    SigningBind::resolve(flag, variable)
}

/// `signing` is where the signing server listens: see [`signing_bind`].
pub async fn run(
    dep: LinkerDeploy,
    opts: LinkerOptions,
    signing: SigningBind,
) -> Result<Published> {
    let dir = dep.dir.clone();
    let (state, rx) = new_state(dep);
    if let Some(host) = opts.host {
        (host.register)(state.clone());
        return await_outcome(&state, rx, opts.timeout, &host.url).await;
    }
    crate::start::serve_signing(
        &dir,
        (signing, opts.port),
        opts.open_browser,
        opts.timeout,
        opts.programmatic,
        state,
        rx,
    )
    .await
}

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn finish_reports_the_outcome_without_advancing_time() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let started = tokio::time::Instant::now();
        super::finish(
            &std::sync::Mutex::new(Some(tx)),
            Ok(super::Published {
                entity_id: "id".into(),
                message: "published".into(),
            }),
        );
        assert_eq!(rx.await.unwrap().unwrap().message, "published");
        assert_eq!(tokio::time::Instant::now(), started);
    }

    use super::*;
    use crate::deploy::TempTree;
    use crate::scene::Project;
    use std::path::Path;

    const WAIT: Duration = Duration::from_secs(600);

    #[tokio::test(start_paused = true)]
    async fn the_deadline_ends_a_run_that_no_signature_started() {
        let (_tree, dep) = fixture("deadline-unsigned", None);
        let (st, rx) = new_state(dep);
        let error = await_outcome(&st, rx, WAIT, "http://localhost/deploy")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no signature arrived"),
            "{error}"
        );
        let state = st.deployment_state();
        assert_eq!(state.phase, "failed", "a follower reads the end");
        assert_eq!(state.error.unwrap().code, DEPLOY_FAILED);
        assert!(!st.can_start(), "a late signature starts nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_leaves_a_started_run_to_its_end() {
        let (_tree, dep) = fixture("deadline-signed", None);
        let (st, rx) = new_state(dep);
        st.resource().started = true;
        let waiting = {
            let st = st.clone();
            tokio::spawn(
                async move { await_outcome(&st, rx, WAIT, "http://localhost/deploy").await },
            )
        };
        tokio::time::sleep(WAIT * 2).await;
        assert!(!waiting.is_finished(), "the upload outlasts the deadline");
        assert_eq!(st.deployment_state().phase, "uploading");
        st.conclude(Ok(Published {
            entity_id: "id".into(),
            message: "published".into(),
        }));
        assert_eq!(waiting.await.unwrap().unwrap().message, "published");
        assert_eq!(st.deployment_state().phase, "deployed");
    }

    fn fixture(tag: &str, world: Option<&str>) -> (TempTree, LinkerDeploy) {
        let t = TempTree::new(tag);
        let world_cfg = match world {
            Some(w) => format!(",\"worldConfiguration\":{{\"name\":\"{w}\"}}"),
            None => String::new(),
        };
        t.write(
            "scene.json",
            &format!(
                "{{\"runtimeVersion\":\"7\",\"main\":\"bin/index.js\",\"display\":{{\"title\":\"Linker Smoke\"}},\"scene\":{{\"parcels\":[\"0,0\"],\"base\":\"0,0\"}}{world_cfg}}}"
            ),
        );
        t.write("bin/index.js", "console.log(\"linker\");\n");
        let project = Project::load(&t.0).unwrap();
        let prepared = deploy::prepare(&project).unwrap();
        let dep = LinkerDeploy {
            dir: t.0.clone(),
            prepared,
            target_content: "http://127.0.0.1:9".to_string(),
            world: world.map(str::to_string),
            needs_delete: false,
            timestamp_override: None,
            entity_out: None,
            scene_title: "Linker Smoke".to_string(),
            base_parcel: "0,0".to_string(),
            multi_scene: false,
            gate: deploy::PermissionGate::off(),
            deploy_token: "t".repeat(MIN_DEPLOY_TOKEN_LENGTH),
            skip_validations: false,
        };
        (t, dep)
    }

    /// The id the rendered panel carries, which is also the pending key.
    fn minted_entity_id(section: &str) -> String {
        let marker = r#"data-entity-id=""#;
        let at = section.find(marker).expect("panel carries the entity id") + marker.len();
        section[at..].split('"').next().unwrap().to_string()
    }

    async fn sign_as(
        state: &Arc<LinkerState>,
        address: String,
        signature: String,
        entity_id: String,
    ) -> Value {
        sign(
            State(state.clone()),
            Json(SignReq {
                address,
                signature,
                entity_id,
                delete_signature: None,
            }),
        )
        .await
        .0
    }

    async fn sign_minted(state: &Arc<LinkerState>, signer: &dcl_auth::Wallet) -> Value {
        let entity_id = minted_entity_id(&sign_section(state, "/deploy/sign"));
        let sig = signer.sign_message(entity_id.as_bytes()).unwrap();
        sign_as(state, signer.address(), sig, entity_id).await
    }

    /// The opener is not in the process group of the CLI, so the Ctrl-C that
    /// a terminal sends to that group does not reach it.
    #[cfg(unix)]
    #[test]
    fn the_opener_runs_in_a_process_group_of_its_own() {
        let mut child = spawn_apart("sleep", "600").unwrap();
        let pid = child.id() as libc::pid_t;
        let (group, own) = unsafe { (libc::getpgid(pid), libc::getpgid(0)) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(group, pid, "the opener leads a group of its own");
        assert_ne!(group, own);
    }

    #[test]
    fn the_deploy_server_listens_on_the_flag_then_the_variable_then_loopback() {
        let _env = crate::deploy::ENV_LOCK.blocking_lock();
        let lan: IpAddr = "192.0.2.7".parse().unwrap();
        let every = IpAddr::from([0, 0, 0, 0]);
        let local = IpAddr::from([127, 0, 0, 1]);
        let bind = |flag| signing_bind(flag).unwrap();
        let on = |addr, remote| SigningBind {
            addr,
            remote,
            openable: true,
        };
        crate::remove_test_env("DCL_SCENE_CLI_LINKER_HOST");
        assert_eq!(bind(None), SigningBind::LOCAL);
        assert_eq!(
            bind(Some(every)),
            on(every, false),
            "the flag moves the listener and opens no signing route"
        );
        crate::set_test_env("DCL_SCENE_CLI_LINKER_HOST", "0.0.0.0");
        assert_eq!(bind(None), on(every, true));
        assert_eq!(bind(Some(lan)), on(lan, true), "the flag wins");
        assert_eq!(
            bind(Some(local)),
            SigningBind {
                openable: false,
                ..on(local, false)
            },
            "no other machine reaches a loopback listener"
        );
        crate::set_test_env("DCL_SCENE_CLI_LINKER_HOST", "192.0.2.7");
        assert_eq!(bind(None), on(lan, true));
        crate::set_test_env("DCL_SCENE_CLI_LINKER_HOST", "::1");
        assert_eq!(
            bind(None),
            on(std::net::Ipv6Addr::LOCALHOST.into(), false),
            "a loopback address is not the switch"
        );
        for name in ["localhost", " LocalHost "] {
            crate::set_test_env("DCL_SCENE_CLI_LINKER_HOST", name);
            assert_eq!(bind(None), SigningBind::LOCAL, "{name:?}");
        }
        crate::set_test_env("DCL_SCENE_CLI_LINKER_HOST", "not-an-address");
        for flag in [None, Some(local)] {
            let err = signing_bind(flag).unwrap_err();
            let text = crate::ux::render(&err, false, false);
            assert!(text.contains("is not an IP address"), "{text}");
            assert!(!text.contains("--host"), "the flag does not help: {text}");
            assert!(!crate::ux::usage_error(&err));
        }
        crate::remove_test_env("DCL_SCENE_CLI_LINKER_HOST");
    }

    /// `--host` with one address that is not loopback, while remote signing
    /// is off: the gates would refuse each caller, so the run does not start.
    #[test]
    fn a_host_the_gates_would_refuse_is_a_usage_error() {
        let lan: IpAddr = "192.0.2.7".parse().unwrap();
        for variable in [None, Some(IpAddr::from([127, 0, 0, 1]))] {
            let err = SigningBind::resolve(Some(lan), variable).unwrap_err();
            assert!(crate::ux::usage_error(&err));
            let text = crate::ux::render(&err, false, false);
            assert!(
                text.contains("--host 192.0.2.7 needs remote signing"),
                "{text}"
            );
            assert!(text.contains("\u{2192} try: pass --host 0.0.0.0"), "{text}");
            assert!(
                text.contains("DCL_SCENE_CLI_LINKER_HOST=192.0.2.7"),
                "{text}"
            );
        }
    }

    /// The note on a signature from another device is true for the run. The
    /// note that names the variable is there only when the variable, on the
    /// same command line, turns remote signing on: `--host` with a loopback
    /// address wins over the variable, so that run has no note. The note on
    /// `localhost` is there only when the listener is on every address.
    #[test]
    fn the_note_on_another_device_is_true_for_the_run() {
        const RERUN: &str =
            "to sign from another device, re-run with DCL_SCENE_CLI_LINKER_HOST=0.0.0.0";
        let local = IpAddr::from([127, 0, 0, 1]);
        let every = IpAddr::from([0, 0, 0, 0]);
        let lan: IpAddr = "192.0.2.7".parse().unwrap();
        let bind = |flag, variable| SigningBind::resolve(flag, variable).unwrap();
        for (flag, variable) in [
            (None, None),
            (None, Some(local)),
            (Some(every), None),
            (Some(every), Some(local)),
        ] {
            let note = bind(flag, variable).other_device_note();
            assert_eq!(note, Some(RERUN), "{flag:?} {variable:?}");
            assert!(bind(flag, Some(every)).remote, "{flag:?}: the note is true");
        }
        for variable in [None, Some(local), Some(every), Some(lan)] {
            let run = bind(Some(local), variable);
            assert_eq!(run.other_device_note(), None, "{variable:?}");
            assert!(!bind(Some(local), Some(every)).remote);
        }
        for flag in [None, Some(every)] {
            let note = bind(flag, Some(every)).other_device_note();
            assert_eq!(
                note,
                Some("from another device, replace localhost with this machine's address"),
                "{flag:?}"
            );
        }
        // Remote signing on one address: the URL names it, so there is no
        // `localhost` to replace.
        for (flag, variable) in [(Some(lan), every), (None, lan), (Some(lan), lan)] {
            let run = bind(flag, Some(variable));
            assert!(run.remote, "{flag:?} {variable}");
            assert_eq!(run.other_device_note(), None, "{flag:?} {variable}");
        }
    }

    #[tokio::test]
    async fn section_mints_pending_and_stale_sign_answers_nonfatal() {
        let (_t, dep) = fixture("smoke", None);
        let (state, _rx) = new_state(dep);

        let section = sign_section(&state, "/deploy/sign");
        assert!(section.contains(r#"data-api="/deploy/sign""#));
        assert!(section.contains("Linker Smoke"));
        assert!(
            section.contains("2 files"),
            "payload row is server-rendered"
        );
        assert!(!section.contains("data-delete-payload"));
        let id = minted_entity_id(&section);
        assert!(id.starts_with("bafkrei"));
        assert!(
            state.pending.lock().unwrap().contains_key(&id),
            "the rendered id is registered pending"
        );
        assert!(
            section.contains(r#"id="sign-progress" hidden"#),
            "{section}"
        );

        for _ in 0..40 {
            assert_eq!(minted_entity_id(&sign_section(&state, "/deploy/sign")), id);
        }
        assert_eq!(state.pending.lock().unwrap().len(), 1);
        assert!(
            state.pending.lock().unwrap().contains_key(&id),
            "forty renders later the first id is still pending"
        );

        let stale = sign_as(&state, "0x0".into(), "0x0".into(), "bogus".into()).await;
        assert_eq!(stale["ok"], false);
        assert_eq!(stale["fatal"], false);
        assert_eq!(stale["stale"], true, "{stale}");
        assert!(
            stale["error"]
                .as_str()
                .unwrap()
                .contains("no longer on the server"),
            "{stale}"
        );
        let fresh = minted_entity_id(&sign_section(&state, "/deploy/sign"));
        assert_ne!(fresh, id);
        assert_eq!(state.pending.lock().unwrap().len(), 2);
        assert_eq!(state.progress().phase, "idle");
    }

    /// Past the keep count only the oldest ids stop being signable.
    #[test]
    fn only_the_oldest_minted_ids_fall_out() {
        let (_t, dep) = fixture("keep", None);
        let (state, _rx) = new_state(dep);
        let mut ids = Vec::new();
        for _ in 0..(PENDING_KEEP + 3) {
            ids.push(minted_entity_id(&sign_section(&state, "/deploy/sign")));
            *state.minted.lock().unwrap() = None;
        }
        let pending = state.pending.lock().unwrap();
        assert_eq!(pending.len(), PENDING_KEEP);
        for id in &ids[..3] {
            assert!(!pending.contains_key(id), "the oldest fell out");
        }
        for id in &ids[3..] {
            assert!(pending.contains_key(id), "the newest stay");
        }
    }

    #[tokio::test]
    async fn unreachable_target_fails_fatal_and_resolves_cli() {
        let (_t, dep) = fixture("fatal", None);
        let (state, rx) = new_state(dep);
        let resp = sign_minted(&state, &crate::random_test_wallet()).await;
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["fatal"], true);
        let outcome = rx.await.unwrap();
        assert!(outcome.is_err());
        let s = state.deployment_state();
        assert_eq!(s.phase, "failed");
        assert_eq!(s.error.as_ref().unwrap().code, DEPLOY_FAILED);
        assert!(!s.error.as_ref().unwrap().message.starts_with("Error:"));
        assert_eq!(state.cancel(), CancelOutcome::Final);
        assert_eq!(s, state.deployment_state(), "a final state never moves");
    }

    #[test]
    fn deploy_tokens_are_random_by_default_and_at_least_sixteen_characters() {
        let a = resolve_deploy_token(None).unwrap();
        let b = resolve_deploy_token(None).unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(
            resolve_deploy_token(Some("0123456789abcdef")).unwrap(),
            "0123456789abcdef"
        );
        let short = resolve_deploy_token(Some("short")).unwrap_err().to_string();
        assert!(short.contains("16 characters"), "{short}");
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("ab", "abc"));
        assert!(!token_matches("", "abc"));
        assert_eq!(linker_fragment("a b"), "#token=a%20b");
    }

    #[tokio::test]
    async fn a_cancel_before_the_signature_ends_the_run_cancelled() {
        let (_t, dep) = fixture("cancel-early", None);
        let (state, rx) = new_state(dep);
        let s = state.deployment_state();
        assert_eq!(s.phase, "awaiting-signature");
        assert!(s.upload.is_none());
        assert!(!s.cancel_requested);
        assert!(!s.is_final());
        assert_eq!(state.settle_delay(), SETTLE, "nobody follows yet");

        assert_eq!(state.cancel(), CancelOutcome::Cancelled);
        let s = state.deployment_state();
        assert_eq!(s.phase, "cancelled");
        assert!(s.cancel_requested);
        assert!(s.error.is_none(), "a plain cancel carries no error");
        assert!(s.is_final());
        assert_eq!(state.cancel(), CancelOutcome::Final);
        let err = rx.await.unwrap().unwrap_err();
        assert_eq!(
            err.downcast_ref::<Cancelled>().unwrap().code,
            DEPLOY_CANCELLED
        );
        assert!(!state.can_start());
        assert_eq!(
            state.settle_delay(),
            FOLLOWED_SETTLE,
            "a cancel follows the run"
        );

        let id = minted_entity_id(&sign_section(&state, "/deploy/sign"));
        let resp = sign_as(&state, "0x0".into(), "0x0".into(), id).await;
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["fatal"], true);
        assert!(
            resp["error"].as_str().unwrap().contains("cancelled"),
            "{resp}"
        );
        assert_eq!(state.deployment_state().phase, "cancelled");
    }

    #[tokio::test]
    async fn a_cancel_mid_upload_drops_the_request_and_ends_cancelled() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let mut kept = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                kept.push(socket);
            }
        });
        let (_t, mut dep) = fixture("cancel-mid", None);
        dep.target_content = format!("http://{addr}");
        let (state, rx) = new_state(dep);
        let signer = crate::random_test_wallet();
        let run = {
            let state = state.clone();
            tokio::spawn(async move { sign_minted(&state, &signer).await })
        };
        let mut changes = state.changes();
        let started = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if state.deployment_state().phase == "uploading" && state.progress().phase != "idle"
                {
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await;
        assert!(started.is_ok(), "the upload never got under way");
        assert_eq!(state.cancel(), CancelOutcome::Cancelling);
        assert!(state.deployment_state().cancel_requested);
        assert_eq!(
            state.cancel(),
            CancelOutcome::Cancelling,
            "asking twice changes nothing"
        );
        let resp = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the cancelled upload settles")
            .unwrap();
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["fatal"], true);
        let s = state.deployment_state();
        assert_eq!(s.phase, "cancelled", "{s:?}");
        assert!(s.upload.is_some(), "the upload so far stays readable");
        let err = rx.await.unwrap().unwrap_err();
        assert_eq!(
            err.downcast_ref::<Cancelled>().unwrap().code,
            DEPLOY_CANCELLED
        );
        hold.abort();
    }

    #[test]
    fn the_final_state_of_an_outcome_matches_the_api_shape() {
        let ok = final_state(&Ok(Published {
            entity_id: "bafy".into(),
            message: "Deployed bafy (HTTP 200)".into(),
        }));
        assert_eq!(ok.phase, "deployed");
        assert_eq!(ok.result.as_ref().unwrap().entity_id, "bafy");
        let plain = final_state(&Err(Cancelled {
            code: DEPLOY_CANCELLED,
        }
        .into()));
        assert_eq!(plain.phase, "cancelled");
        assert!(plain.cancel_requested);
        assert!(plain.error.is_none());
        let late = final_state(&Err(Cancelled {
            code: DEPLOY_CANCELLED_AFTER_UNDEPLOY,
        }
        .into()));
        assert_eq!(
            late.error.as_ref().unwrap().code,
            DEPLOY_CANCELLED_AFTER_UNDEPLOY
        );
        let failed = final_state(&Err(UserError::new(
            "the content server refused",
            TrySteps::one("retry"),
        )
        .into()));
        assert_eq!(failed.phase, "failed");
        assert_eq!(failed.error.as_ref().unwrap().code, DEPLOY_FAILED);
        assert!(
            failed
                .error
                .as_ref()
                .unwrap()
                .message
                .starts_with("the content server refused"),
            "{failed:?}"
        );
        let json = serde_json::to_value(&failed).unwrap();
        assert!(json["cancelRequested"].is_boolean());
        assert!(
            json.get("upload").is_some(),
            "null fields are present, as upstream"
        );
    }

    #[tokio::test]
    async fn sign_flow_completes_against_local_worlds() {
        let Ok(key_path) = std::env::var("DCL_SCENE_CLI_LINKER_SMOKE_KEY") else {
            eprintln!("skipping: DCL_SCENE_CLI_LINKER_SMOKE_KEY not set");
            return;
        };
        let target = std::env::var("DCL_SCENE_CLI_LINKER_SMOKE_TARGET")
            .unwrap_or_else(|_| "http://127.0.0.1:5142".to_string());
        let world = std::env::var("DCL_SCENE_CLI_LINKER_SMOKE_WORLD")
            .unwrap_or_else(|_| "dcl1test.dcl.eth".to_string());
        let raw = std::fs::read_to_string(Path::new(&key_path)).unwrap();
        let signer = dcl_auth::Wallet::from_hex(&raw).unwrap();

        let (_t, mut dep) = fixture("worlds", Some(&world));
        dep.target_content = target;
        let (state, rx) = new_state(dep);
        let resp = sign_minted(&state, &signer).await;
        assert_eq!(resp["ok"], true, "sign flow failed: {}", resp);
        let outcome = rx.await.unwrap();
        let message = outcome.unwrap().message;
        assert!(
            message.contains("Deployed"),
            "unexpected message: {message}"
        );
    }
}
