//! The player report intake (upstream's `report-user` service, at
//! `/api/reports/players`), the node area `serve` nests at `/report-user`:
//! a presign mints the evidence slots, each file is `PUT` to the path the
//! plan named, then the report is filed. Every request is the reporter's
//! own signed fetch over the route path (the nest-stripped path the client
//! signs, or the request path). The moderator token, or a listed moderator
//! wallet, reads the queue and the evidence. Rows: `social.player_reports`
//! and `social.player_report_evidence` (`schemas/40_social.sql`; the slots
//! come first, the report row at filing, so no FK between them); bytes: the
//! shared hash-addressed store. A fresh key is a new wallet, so beside the
//! per-wallet quotas each client address has a slot budget and a byte budget
//! that no key escapes. A sender with many addresses escapes those too, so
//! a presign also counts, under one advisory lock, the declared size of
//! every slot of the last 24 hours that holds a file or is still in its
//! upload time (`RESERVATION_MINUTES`), and refuses a new slot past the
//! operator's bound (`Settings::report_evidence_daily_bytes`) or past the
//! share of it that one client address may take. A slot takes one body, in
//! its upload time, so the store never holds more than the bound counts. A
//! presign deletes the unfiled slots past the pending window so the store's
//! collector frees their bytes. Limits: `DATABASE.md`, "Player report
//! intake workload".

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use catalyst_storage::Storage;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use super::auth::{ModeratorMode, authorize_moderator, verify_wallet_signed_fetch};
use super::errors::{ApiError, forbidden};
use super::moderation::handlers::moderator_creds;
use super::{CommsState, SharedState};
use crate::limits::{RateLimitDecision, RateLimiter};
use crate::shared::identity::is_valid_eth_address;
use crate::shared::signed_fetch::SignedPath;
use crate::shared::{cors, nul_guard};

/// Evidence files of one report: the report form's count (`REPORT_LIMITS`
/// in `apps/sites/src/data/lib/catalyst/landings/report.ts`).
const MAX_FILES: usize = 5;
/// The node's cap on one evidence file, enough for a screenshot or a short
/// clip. It is half the form's 10 MB: the bytes stay in the shared store for
/// as long as the report does.
const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;
/// Reports one wallet may file in a rolling hour (`player_reports_reporter_idx`):
/// a person writes each report by hand, and five an hour is above what the
/// whole node receives in a day.
const REPORTS_PER_HOUR: i64 = 5;
/// Unfiled slots one wallet may hold, counted over the last
/// `PENDING_SLOT_WINDOW_HOURS` (`player_report_evidence_reporter_idx`): the
/// evidence of one hour's reports.
const PENDING_SLOTS_PER_REPORTER: i64 = REPORTS_PER_HOUR * MAX_FILES as i64;
/// A form left open is filed within a day or not at all; past it the slots
/// stop counting and the sweep deletes them.
const PENDING_SLOT_WINDOW_HOURS: i32 = 24;
/// Evidence slots one client address may presign, whatever wallet signs: a
/// bucket of this size that refills over a day. Four wallets' pending
/// quotas, for the people one address can stand for (a household, an
/// office), and far above the form's use (`MAX_FILES` per report, a few
/// reports a day on the whole node).
const SOURCE_SLOTS_PER_DAY: u32 = 4 * PENDING_SLOTS_PER_REPORTER as u32;
/// Evidence one client address may send to the store, in KiB: a bucket of
/// this size that refills over a day. It is the slot budget with every slot
/// filled once at the file cap; each `PUT` of the declared size and type
/// spends its size, also one the slot then refuses.
const SOURCE_KIB_PER_DAY: u32 = SOURCE_SLOTS_PER_DAY * (MAX_FILE_BYTES / 1024) as u32;
const SOURCE_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// What one slot counts for at least in the node's bound, whatever size it
/// declares. A slot may declare one byte, so the declared sizes alone do
/// not bound the slots of a window; with this floor the bound divided by it
/// does (8,192 slots at the default 2,048 MiB), and that is also the most
/// rows the bound's sum reads. A quarter MiB is below most screenshots, so
/// the form's files count at their own size.
const NODE_SLOT_FLOOR_BYTES: i64 = 256 * 1024;
/// How long a slot without a file keeps its place in the node's bound, and
/// how long it takes its `PUT`. The form sends its files right after the
/// presign; five files at the cap in a quarter hour need less than
/// 230 kbit/s. A policy, not a measurement: a longer time lets presigns
/// alone hold the bound for longer, a shorter one refuses slower senders.
const RESERVATION_MINUTES: i32 = 15;
/// The part of the node's bound one client address may take in 24 hours: a
/// sixteenth, 128 MiB at the default 2,048 MiB, which is the evidence of one
/// wallet's hour (`REPORTS_PER_HOUR` reports of `MAX_FILES` files at the
/// cap, 125 MiB). Fewer than sixteen addresses then never fill the bound.
/// A policy, not a measurement.
const NODE_SHARE_DIVISOR: i64 = 16;
/// The least an address's share is, whatever the bound: one report's files
/// at the cap, so a small bound still takes a whole report from an address.
const NODE_SHARE_FLOOR_BYTES: i64 = (MAX_FILES * MAX_FILE_BYTES) as i64;
/// Expired slots one presign deletes at most. A presign mints at most
/// `MAX_FILES` slots, so twenty presigns' worth a pass keeps the sweep ahead
/// of the slots that expire and bounds the rows one request locks.
const SWEEP_BATCH: i64 = 20 * MAX_FILES as i64;
/// The form's limit for the description and for the comments.
const TEXT_MAX: usize = 500;
/// The largest valid body is five 255-character filenames or two
/// `TEXT_MAX` texts, under 16 KiB with every character written as a JSON
/// `\u` escape pair; four times that refuses nothing valid.
const JSON_BODY_LIMIT: usize = 64 * 1024;
/// The moderator queue's page: one screen by default, and at most as many
/// reports as keep one answer to a thousand evidence rows.
const LIST_DEFAULT: i64 = 50;
const LIST_MAX: i64 = 200;
const REASONS: [&str; 5] = [
    "scam_phishing",
    "illegal_content",
    "harassment",
    "cheating",
    "impersonation",
];
/// The form's accepted evidence types.
const ACCEPTED_TYPES: [&str; 7] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "video/mp4",
    "video/webm",
    "application/pdf",
];

struct ReportsState {
    comms: SharedState,
    storage: Arc<Storage>,
    /// The per-address budgets, in this process's memory: every `serve`
    /// process counts its own requests.
    source_slots: RateLimiter,
    source_kib: RateLimiter,
}

type Shared = Arc<ReportsState>;

/// The area's router, for `serve` to nest at `/report-user`.
pub fn router(comms: SharedState, storage: Arc<Storage>) -> Router {
    let state = Arc::new(ReportsState {
        comms,
        storage,
        source_slots: RateLimiter::new(SOURCE_SLOTS_PER_DAY, SOURCE_WINDOW),
        source_kib: RateLimiter::new(SOURCE_KIB_PER_DAY, SOURCE_WINDOW),
    });
    Router::new()
        .route(
            "/api/reports/players/presign",
            post(presign).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route(
            "/api/reports/players",
            post(create).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route(
            "/api/reports/players/{id}/evidence/{key}",
            put(upload).layer(DefaultBodyLimit::max(MAX_FILE_BYTES + 1)),
        )
        .route("/api/reports", get(list))
        .route("/api/reports/{id}/evidence/{key}", get(evidence))
        .layer(axum::middleware::from_fn(nul_guard::nul_guard_middleware))
        .layer(axum::middleware::from_fn(cors::cors_middleware))
        .with_state(state)
}

fn upload_path(id: Uuid, key: &str) -> String {
    format!("/api/reports/players/{id}/evidence/{key}")
}

/// The key's filename part: ASCII letters, digits, `.`, `_` and `-`, at most 100 chars.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| ApiError::bad_request(format!("Invalid body: {e}")))
}

fn parse_report_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::bad_request("Invalid reportId"))
}

/// Whether `bytes` begin as a file of the declared type does. The declared
/// type is what the moderator read answers with, so the bytes must not be
/// another kind of file.
fn starts_as(content_type: &str, bytes: &[u8]) -> bool {
    match content_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]),
        "video/mp4" => bytes.get(4..8) == Some(&b"ftyp"[..]),
        "video/webm" => bytes.starts_with(b"\x1a\x45\xdf\xa3"),
        "application/pdf" => bytes.starts_with(b"%PDF-"),
        _ => false,
    }
}

fn already_filed() -> ApiError {
    ApiError::http(409, "The report is already submitted")
}

/// Refuses a filed report. Its callers hold the report's slot locks, and it
/// is a statement of its own, so it reads the state after those locks, not
/// the snapshot of the statement that waited for them.
async fn refuse_filed(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    let filed: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM social.player_reports WHERE id = $1)")
            .bind(id)
            .fetch_one(conn)
            .await?;
    if filed {
        return Err(already_filed());
    }
    Ok(())
}

async fn reporter_of(
    state: &CommsState,
    headers: &HeaderMap,
    method: &str,
    original: &Uri,
    local: &Uri,
) -> Result<String, ApiError> {
    let verified = verify_wallet_signed_fetch(
        headers,
        method,
        &SignedPath::from_uris(original, local),
        state.eip1654(),
    )
    .await?;
    Ok(verified.signer.as_str().to_lowercase())
}

/// The client address the per-address budgets count: the one `serve` pins
/// to the socket peer or takes from a trusted proxy
/// (`shared::client_ip::pin_client_headers`), never a header the client
/// chose; IPv6 by its /64. Requests that carry no address (an in-process
/// caller) share one key.
fn source_of(headers: &HeaderMap) -> String {
    crate::shared::client_ip::source_key(&crate::shared::client_ip::client_ip(headers))
}

/// Spends `cost` of the address's budget, or refuses with `429`.
fn spend(budget: &RateLimiter, source: &str, cost: u32, what: &str) -> Result<(), ApiError> {
    match budget.check_cost(source, cost) {
        RateLimitDecision::Allow => Ok(()),
        RateLimitDecision::Deny => Err(ApiError::http(
            429,
            format!("Too many {what} from this address, try again later"),
        )),
    }
}

/// A file's cost in the byte budget: its size in whole KiB, rounded up.
fn kib(bytes: usize) -> u32 {
    bytes.div_ceil(1024) as u32
}

/// Deletes up to `SWEEP_BATCH` unfiled slots older than the pending window,
/// oldest first, so the store's collector frees their bytes. A filing locks
/// its slots before it inserts the report, so the sweep takes only slots no
/// filing holds (`SKIP LOCKED`) and checks again, in a statement that sees
/// every report committed before the locks were taken, that the report is
/// not filed: a filed report keeps its evidence.
async fn sweep_expired_slots(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let expired = sqlx::query(
        "SELECT e.report_id, e.evidence_key FROM social.player_report_evidence e \
         WHERE e.created_at <= timezone('UTC', now()) - $1::int * interval '1 hour' \
           AND NOT EXISTS (SELECT 1 FROM social.player_reports r WHERE r.id = e.report_id) \
         ORDER BY e.created_at LIMIT $2 FOR UPDATE OF e SKIP LOCKED",
    )
    .bind(PENDING_SLOT_WINDOW_HOURS)
    .bind(SWEEP_BATCH)
    .fetch_all(&mut *tx)
    .await?;
    if expired.is_empty() {
        return Ok(0);
    }
    let ids: Vec<Uuid> = expired.iter().map(|r| r.get("report_id")).collect();
    let keys: Vec<String> = expired.iter().map(|r| r.get("evidence_key")).collect();
    let deleted = sqlx::query(
        "DELETE FROM social.player_report_evidence e \
         USING unnest($1::uuid[], $2::text[]) AS x(report_id, evidence_key) \
         WHERE e.report_id = x.report_id AND e.evidence_key = x.evidence_key \
           AND NOT EXISTS (SELECT 1 FROM social.player_reports r WHERE r.id = e.report_id)",
    )
    .bind(&ids)
    .bind(&keys)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(deleted)
}

/// Clears the address key of up to `SWEEP_BATCH` slots that hold a file and
/// are older than the pending window, oldest first: the share counts no
/// slot past that window, and a filed report keeps its slots, so without
/// this the key would stay as long as the report. `SKIP LOCKED` as the
/// sweep, for the slots a filing holds.
async fn forget_sources(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let cleared = sqlx::query(
        "UPDATE social.player_report_evidence e SET source_key = NULL \
         FROM (SELECT report_id, evidence_key FROM social.player_report_evidence \
               WHERE content_hash IS NOT NULL AND source_key IS NOT NULL \
                 AND created_at <= timezone('UTC', now()) - $1::int * interval '1 hour' \
               ORDER BY created_at LIMIT $2 FOR UPDATE SKIP LOCKED) x \
         WHERE e.report_id = x.report_id AND e.evidence_key = x.evidence_key",
    )
    .bind(PENDING_SLOT_WINDOW_HOURS)
    .bind(SWEEP_BATCH)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(cleared)
}

fn rfc3339(ts: NaiveDateTime) -> String {
    DateTime::<Utc>::from_naive_utc_and_offset(ts, Utc).to_rfc3339()
}

/// Serializes one reporter's quota checks against its own concurrent requests.
async fn lock_reporter(conn: &mut PgConnection, reporter: &str) -> Result<(), ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("player-report:{reporter}"))
        .execute(conn)
        .await?;
    Ok(())
}

/// What a slot of `declared_size` bytes counts for in the node's bound.
fn node_cost(declared_size: i64) -> i64 {
    declared_size.max(NODE_SLOT_FLOOR_BYTES)
}

/// The lock of the node-wide bound, held to the end of the transaction. It
/// serializes, over every `serve` process, each presign's count and inserts
/// and each first `PUT` of a slot. A caller takes it after its reporter lock
/// (a presign) or its slot's row lock (a `PUT`) and takes no other lock
/// while it holds it.
async fn lock_node(conn: &mut PgConnection) -> Result<(), ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('player-report-evidence:node', 0))")
        .execute(conn)
        .await?;
    Ok(())
}

/// What one client address may take of a node bound of `cap` bytes.
fn source_share(cap: i64) -> i64 {
    (cap / NODE_SHARE_DIVISOR).max(NODE_SHARE_FLOOR_BYTES)
}

/// The node-wide bound: refuses when the counted slots of the last
/// `PENDING_SLOT_WINDOW_HOURS`, plus `cost` for the new ones, pass `cap`
/// bytes, or when those `source` presigned pass its share. A slot counts
/// while it holds a file, or for `RESERVATION_MINUTES` from its presign: a
/// slot nobody filled takes no `PUT` after that time (`claim_slot`), so a
/// declaration alone holds the bound only that long. The count is a
/// statement of its own under `lock_node`, so it sees each presign and each
/// first `PUT` that held the lock before; a `PUT` reads the clock after it
/// holds the lock, so no slot this count left out takes a file later. The
/// sweep deletes no slot younger than the window.
async fn check_node_bound(
    conn: &mut PgConnection,
    cap: i64,
    cost: i64,
    source: &str,
) -> Result<(), ApiError> {
    lock_node(&mut *conn).await?;
    let taken = sqlx::query(
        "SELECT coalesce(sum(cost), 0)::bigint AS node, \
                coalesce(sum(cost) FILTER (WHERE source_key = $4), 0)::bigint AS source \
         FROM (SELECT greatest(declared_size, $2) AS cost, source_key \
               FROM social.player_report_evidence \
               WHERE content_hash IS NOT NULL \
                 AND created_at > timezone('UTC', now()) - $1::int * interval '1 hour' \
               UNION ALL \
               SELECT greatest(declared_size, $2), source_key \
               FROM social.player_report_evidence \
               WHERE content_hash IS NULL \
                 AND created_at > timezone('UTC', statement_timestamp()) \
                                  - $3::int * interval '1 minute') s",
    )
    .bind(PENDING_SLOT_WINDOW_HOURS)
    .bind(NODE_SLOT_FLOOR_BYTES)
    .bind(RESERVATION_MINUTES)
    .bind(source)
    .fetch_one(conn)
    .await?;
    if taken.get::<i64, _>("node") + cost > cap {
        return Err(ApiError::http(
            429,
            "The node accepts no more report evidence today, try again later",
        ));
    }
    if taken.get::<i64, _>("source") + cost > source_share(cap) {
        return Err(ApiError::http(
            429,
            "This address sent its share of the node's report evidence today, try again later",
        ));
    }
    Ok(())
}

/// The hourly limit: reports this wallet filed in the last hour.
async fn check_hourly_limit(conn: &mut PgConnection, reporter: &str) -> Result<(), ApiError> {
    let filed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social.player_reports \
         WHERE reporter_address = $1 \
           AND created_at > timezone('UTC', now()) - interval '1 hour'",
    )
    .bind(reporter)
    .fetch_one(conn)
    .await?;
    if filed >= REPORTS_PER_HOUR {
        return Err(ApiError::http(
            429,
            format!("At most {REPORTS_PER_HOUR} reports per hour"),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresignFile {
    filename: String,
    content_type: String,
    file_size: i64,
}

#[derive(Deserialize)]
struct PresignBody {
    files: Vec<PresignFile>,
}

/// `POST /api/reports/players/presign` `{files:[{filename, contentType,
/// fileSize}]}`: one upload slot per file under a fresh report id, for the
/// signer, `{reportId, files:[{key, uploadPath}]}` (`uploadPath` relative to
/// the mount). The report row itself comes at filing. The slots spend the
/// address's slot budget after the wallet's quotas pass, and the node's
/// bound, with the address's share of it, is the last gate: a presign it
/// refuses keeps the address's spend, so no address makes the node sum its
/// window without limit. Each slot keeps the address's key for that share.
async fn presign(
    State(state): State<Shared>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let reporter = reporter_of(&state.comms, &headers, "post", &original, &uri).await?;
    let body: PresignBody = parse_body(&body)?;
    if body.files.is_empty() || body.files.len() > MAX_FILES {
        return Err(ApiError::bad_request(format!(
            "files must hold between 1 and {MAX_FILES} entries"
        )));
    }
    let mut slots = Vec::with_capacity(body.files.len());
    for (position, file) in body.files.iter().enumerate() {
        if file.filename.trim().is_empty() || file.filename.chars().count() > 255 {
            return Err(ApiError::bad_request(
                "filename must be 1 to 255 characters",
            ));
        }
        if !ACCEPTED_TYPES.contains(&file.content_type.as_str()) {
            return Err(ApiError::bad_request(format!(
                "contentType {} is not accepted",
                file.content_type
            )));
        }
        if file.file_size < 1 || file.file_size > MAX_FILE_BYTES as i64 {
            return Err(ApiError::bad_request(format!(
                "fileSize must be between 1 and {MAX_FILE_BYTES} bytes"
            )));
        }
        let key = format!("{position}-{}", sanitize_filename(&file.filename));
        slots.push((key, file));
    }

    let id = Uuid::new_v4();
    let mut tx = state.comms.pool.begin().await?;
    lock_reporter(&mut tx, &reporter).await?;
    check_hourly_limit(&mut tx, &reporter).await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social.player_report_evidence e \
         WHERE e.reporter_address = $1 \
           AND e.created_at > timezone('UTC', now()) - $2::int * interval '1 hour' \
           AND NOT EXISTS (SELECT 1 FROM social.player_reports r WHERE r.id = e.report_id)",
    )
    .bind(&reporter)
    .bind(PENDING_SLOT_WINDOW_HOURS)
    .fetch_one(&mut *tx)
    .await?;
    if pending + slots.len() as i64 > PENDING_SLOTS_PER_REPORTER {
        return Err(ApiError::http(
            429,
            format!("At most {PENDING_SLOTS_PER_REPORTER} unsubmitted evidence files at a time"),
        ));
    }
    let source = source_of(&headers);
    spend(
        &state.source_slots,
        &source,
        slots.len() as u32,
        "evidence files",
    )?;
    check_node_bound(
        &mut tx,
        state.comms.settings.report_evidence_daily_bytes,
        slots
            .iter()
            .map(|(_, file)| node_cost(file.file_size))
            .sum(),
        &source,
    )
    .await?;
    for (key, file) in &slots {
        sqlx::query(
            "INSERT INTO social.player_report_evidence \
             (report_id, evidence_key, reporter_address, filename, content_type, declared_size, \
              source_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(key)
        .bind(&reporter)
        .bind(file.filename.trim())
        .bind(&file.content_type)
        .bind(file.file_size)
        .bind(&source)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if let Err(error) = sweep_expired_slots(&state.comms.pool).await {
        tracing::warn!(%error, "sweeping expired report evidence slots");
    }
    if let Err(error) = forget_sources(&state.comms.pool).await {
        tracing::warn!(%error, "clearing the address keys of report evidence slots");
    }

    let files: Vec<Value> = slots
        .iter()
        .map(|(key, _)| json!({ "key": key, "uploadPath": upload_path(id, key) }))
        .collect();
    Ok(Json(json!({ "reportId": id.to_string(), "files": files })))
}

/// Claims the slot for the body `hash` names, under the slot's row lock
/// (`create` takes it before it files, so the claim comes before or after a
/// filing). `true`: this call made the claim and stores the body. `false`:
/// the slot holds that same body. A slot that holds other bytes answers
/// `409`: one slot, one body. A slot nobody filled within
/// `RESERVATION_MINUTES` of its presign left the node's count and answers
/// `410`; the clock is read under `lock_node`, after every count that left
/// the slot out. The sweep deletes an expired slot, so it can be gone.
async fn claim_slot(pool: &PgPool, id: Uuid, key: &str, hash: &str) -> Result<bool, ApiError> {
    let mut tx = pool.begin().await?;
    let held: Option<Option<String>> = sqlx::query_scalar(
        "SELECT content_hash FROM social.player_report_evidence \
         WHERE report_id = $1 AND evidence_key = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(held) = held else {
        return Err(ApiError::not_found("Unknown report or evidence key"));
    };
    refuse_filed(&mut tx, id).await?;
    match held {
        Some(held) if held == hash => return Ok(false),
        Some(_) => {
            return Err(ApiError::http(409, "The evidence file is already uploaded"));
        }
        None => {}
    }
    lock_node(&mut tx).await?;
    let claimed = sqlx::query(
        "UPDATE social.player_report_evidence \
         SET content_hash = $3, uploaded_at = timezone('UTC', now()) \
         WHERE report_id = $1 AND evidence_key = $2 AND content_hash IS NULL \
           AND created_at > timezone('UTC', statement_timestamp()) - $4::int * interval '1 minute'",
    )
    .bind(id)
    .bind(key)
    .bind(hash)
    .bind(RESERVATION_MINUTES)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Err(ApiError::http(
            410,
            "The upload time of this evidence file is over, submit the report again",
        ));
    }
    tx.commit().await?;
    Ok(true)
}

/// Stores `body` in the slot once. Only the call that claimed the slot
/// writes to the store, in a task of its own, so a client that goes away
/// does not leave a claim without its bytes; when the store fails the claim
/// is cleared and the slot takes a `PUT` again. So the store holds at most
/// one body for a slot, of the size the node's bound counted.
async fn store_once(
    state: Shared,
    id: Uuid,
    key: String,
    hash: String,
    body: Bytes,
) -> Result<(), ApiError> {
    let internal = || ApiError::internal("Internal Server Error");
    if !claim_slot(&state.comms.pool, id, &key, &hash).await? {
        return match state.storage.exist(&hash).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(ApiError::http(
                409,
                "The evidence file is still being uploaded",
            )),
            Err(error) => {
                tracing::error!(%error, "reading report evidence");
                Err(internal())
            }
        };
    }
    tokio::spawn(async move {
        let Err(error) = state.storage.store(&hash, body).await else {
            return Ok(());
        };
        tracing::error!(%error, "storing report evidence");
        let cleared = sqlx::query(
            "UPDATE social.player_report_evidence \
             SET content_hash = NULL, uploaded_at = NULL \
             WHERE report_id = $1 AND evidence_key = $2 AND content_hash = $3",
        )
        .bind(id)
        .bind(&key)
        .bind(&hash)
        .execute(&state.comms.pool)
        .await;
        if let Err(error) = cleared {
            tracing::error!(%error, "clearing the claim of a report evidence slot");
        }
        Err(internal())
    })
    .await
    .unwrap_or_else(|_| Err(internal()))
}

/// `PUT /api/reports/players/{id}/evidence/{key}`: the file of one presigned
/// slot, by the slot's signer while the report is not filed and within
/// `RESERVATION_MINUTES` of the presign. The slot takes one body: the bytes
/// go to the shared store under their content hash, the same bytes again
/// answer `200` and other bytes `409`. Each `PUT` that comes this far spends
/// the address's byte budget, also one the slot then refuses.
async fn upload(
    State(state): State<Shared>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    Path((id, key)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let reporter = reporter_of(&state.comms, &headers, "put", &original, &uri).await?;
    let id = parse_report_id(&id)?;
    let row = sqlx::query(
        "SELECT e.reporter_address, e.content_type, e.declared_size, \
                EXISTS (SELECT 1 FROM social.player_reports r WHERE r.id = e.report_id) AS filed \
         FROM social.player_report_evidence e \
         WHERE e.report_id = $1 AND e.evidence_key = $2",
    )
    .bind(id)
    .bind(&key)
    .fetch_optional(&state.comms.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("Unknown report or evidence key"))?;
    if row.get::<String, _>("reporter_address") != reporter {
        return Err(forbidden("The report belongs to another wallet"));
    }
    if row.get::<bool, _>("filed") {
        return Err(already_filed());
    }
    let declared: String = row.get("content_type");
    let sent = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if sent != declared {
        return Err(ApiError::bad_request(format!(
            "content-type must be {declared}"
        )));
    }
    let declared_size: i64 = row.get("declared_size");
    if body.len() as i64 != declared_size {
        return Err(ApiError::bad_request(format!(
            "the file must hold the {declared_size} bytes the presign declared"
        )));
    }
    if !starts_as(&declared, &body) {
        return Err(ApiError::bad_request(format!(
            "the file is not a {declared} file"
        )));
    }

    let source = source_of(&headers);
    let cost = kib(body.len());
    spend(&state.source_kib, &source, cost, "evidence bytes")?;
    let hash = crate::auth::hash_bytes_v1(&body);
    if let Err(refused) = store_once(state.clone(), id, key.clone(), hash, body).await {
        // A failure of the node gives the bytes back; a refusal keeps the spend.
        if refused.code >= 500 {
            state.source_kib.refund(&source, cost);
        }
        return Err(refused);
    }
    Ok(Json(json!({ "ok": true, "key": key })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    report_id: String,
    reported_address: String,
    reason: String,
    description: String,
    #[serde(default)]
    additional_comments: Option<String>,
    #[serde(default)]
    confirm_accuracy: bool,
    evidence_keys: Vec<String>,
}

/// `POST /api/reports/players`: files the presigned report once, by the
/// slots' signer, naming every presigned (and uploaded) evidence key;
/// `{data:{id, evidenceKeys}}`.
async fn create(
    State(state): State<Shared>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let reporter = reporter_of(&state.comms, &headers, "post", &original, &uri).await?;
    let body: CreateBody = parse_body(&body)?;
    let id = parse_report_id(&body.report_id)?;
    let reported = body.reported_address.trim().to_lowercase();
    if !is_valid_eth_address(&reported) {
        return Err(ApiError::bad_request(
            "reportedAddress must be a wallet address",
        ));
    }
    if !REASONS.contains(&body.reason.as_str()) {
        return Err(ApiError::bad_request(
            "reason is not one of the report reasons",
        ));
    }
    let description = body.description.trim();
    if description.is_empty() || description.chars().count() > TEXT_MAX {
        return Err(ApiError::bad_request(format!(
            "description must be 1 to {TEXT_MAX} characters"
        )));
    }
    let comments = body
        .additional_comments
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    if comments.is_some_and(|c| c.chars().count() > TEXT_MAX) {
        return Err(ApiError::bad_request(format!(
            "additionalComments must be at most {TEXT_MAX} characters"
        )));
    }
    if !body.confirm_accuracy {
        return Err(ApiError::bad_request("confirmAccuracy must be true"));
    }

    let mut tx = state.comms.pool.begin().await?;
    let rows = sqlx::query(
        "SELECT evidence_key, reporter_address, content_hash \
         FROM social.player_report_evidence WHERE report_id = $1 \
         ORDER BY evidence_key FOR UPDATE",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        return Err(ApiError::not_found("Unknown reportId"));
    }
    if rows
        .iter()
        .any(|r| r.get::<String, _>("reporter_address") != reporter)
    {
        return Err(forbidden("The report belongs to another wallet"));
    }
    refuse_filed(&mut tx, id).await?;
    let keys: Vec<String> = rows.iter().map(|r| r.get("evidence_key")).collect();
    let mut named = body.evidence_keys.clone();
    named.sort();
    named.dedup();
    if named != keys {
        return Err(ApiError::bad_request(
            "evidenceKeys must name every presigned file and nothing else",
        ));
    }
    for row in &rows {
        // A claimed slot is uploaded when its bytes are in the store.
        let uploaded = match row.get::<Option<String>, _>("content_hash") {
            Some(hash) => state.storage.exist(&hash).await.map_err(|error| {
                tracing::error!(%error, "reading report evidence");
                ApiError::internal("Internal Server Error")
            })?,
            None => false,
        };
        if !uploaded {
            return Err(ApiError::bad_request(format!(
                "evidence {} was not uploaded",
                row.get::<String, _>("evidence_key")
            )));
        }
    }
    lock_reporter(&mut tx, &reporter).await?;
    check_hourly_limit(&mut tx, &reporter).await?;
    sqlx::query(
        "INSERT INTO social.player_reports \
         (id, reporter_address, reported_address, reason, description, additional_comments, \
          evidence_keys, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'open')",
    )
    .bind(id)
    .bind(&reporter)
    .bind(&reported)
    .bind(&body.reason)
    .bind(description)
    .bind(comments)
    .bind(&keys)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({ "data": { "id": id.to_string(), "evidenceKeys": keys } }),
    ))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    reported: Option<String>,
}

/// `GET /api/reports?limit=&offset=&reported=`: the moderator queue, filed
/// reports newest first with their evidence, `{data:[…], total}`.
async fn list(
    State(state): State<Shared>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize_moderator(
        &moderator_creds(&state.comms),
        &headers,
        "get",
        &SignedPath::from_uris(&original, &uri),
        ModeratorMode::Read,
        None,
        state.comms.eip1654(),
    )
    .await?;
    let limit = q.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
    let offset = q.offset.unwrap_or(0).max(0);
    let reported = q
        .reported
        .as_deref()
        .map(|r| r.trim().to_lowercase())
        .filter(|r| !r.is_empty());
    if reported
        .as_deref()
        .is_some_and(|r| !is_valid_eth_address(r))
    {
        return Err(ApiError::bad_request("reported must be a wallet address"));
    }
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social.player_reports \
         WHERE ($1::text IS NULL OR reported_address = $1)",
    )
    .bind(&reported)
    .fetch_one(&state.comms.pool)
    .await?;
    let reports = sqlx::query(
        "SELECT id, reporter_address, reported_address, reason, description, \
                additional_comments, evidence_keys, status, created_at \
         FROM social.player_reports \
         WHERE ($1::text IS NULL OR reported_address = $1) \
         ORDER BY created_at DESC, id LIMIT $2 OFFSET $3",
    )
    .bind(&reported)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.comms.pool)
    .await?;
    let ids: Vec<Uuid> = reports.iter().map(|r| r.get("id")).collect();
    let evidence = sqlx::query(
        "SELECT report_id, evidence_key, filename, content_type, declared_size, content_hash, \
                uploaded_at \
         FROM social.player_report_evidence WHERE report_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.comms.pool)
    .await?;
    let data: Vec<Value> = reports
        .iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            let keys: Vec<String> = r.get("evidence_keys");
            let files: Vec<Value> = keys
                .iter()
                .filter_map(|key| {
                    evidence.iter().find(|e| {
                        e.get::<Uuid, _>("report_id") == id
                            && e.get::<&str, _>("evidence_key") == key.as_str()
                    })
                })
                .map(|e| {
                    json!({
                        "key": e.get::<String, _>("evidence_key"),
                        "filename": e.get::<String, _>("filename"),
                        "contentType": e.get::<String, _>("content_type"),
                        "declaredSize": e.get::<i64, _>("declared_size"),
                        "contentHash": e.get::<Option<String>, _>("content_hash"),
                        "uploadedAt": e.get::<Option<NaiveDateTime>, _>("uploaded_at").map(rfc3339),
                    })
                })
                .collect();
            json!({
                "id": id.to_string(),
                "reporter": r.get::<String, _>("reporter_address"),
                "reportedAddress": r.get::<String, _>("reported_address"),
                "reason": r.get::<String, _>("reason"),
                "description": r.get::<String, _>("description"),
                "additionalComments": r.get::<Option<String>, _>("additional_comments"),
                "status": r.get::<String, _>("status"),
                "createdAt": rfc3339(r.get("created_at")),
                "evidence": files,
            })
        })
        .collect();
    Ok(Json(json!({ "data": data, "total": total })))
}

/// `GET /api/reports/{id}/evidence/{key}`: one uploaded file, for a moderator.
/// The bytes are a stranger's upload on the node's own origin, so the answer
/// is a download that the browser neither sniffs nor runs.
async fn evidence(
    State(state): State<Shared>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    Path((id, key)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    authorize_moderator(
        &moderator_creds(&state.comms),
        &headers,
        "get",
        &SignedPath::from_uris(&original, &uri),
        ModeratorMode::Read,
        None,
        state.comms.eip1654(),
    )
    .await?;
    let id = parse_report_id(&id)?;
    let row = sqlx::query(
        "SELECT content_type, content_hash FROM social.player_report_evidence \
         WHERE report_id = $1 AND evidence_key = $2 AND content_hash IS NOT NULL",
    )
    .bind(id)
    .bind(&key)
    .fetch_optional(&state.comms.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("Unknown or unuploaded evidence"))?;
    let hash: String = row.get("content_hash");
    let bytes = state
        .storage
        .retrieve(&hash)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "reading report evidence");
            ApiError::internal("Internal Server Error")
        })?
        .ok_or_else(|| ApiError::not_found("Evidence bytes are gone"))?;
    let content_type: String = row.get("content_type");
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, "private, no-store".to_string()),
            (X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (CONTENT_SECURITY_POLICY, "sandbox".to_string()),
            // The key is `sanitize_filename`'s output: no quote, no control character.
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{key}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::super::auth::test_support::{OUTSIDER_KEY, address_of, random_key, signed};
    use super::super::{CommsState, Settings};
    use super::*;
    use crate::http::Outbound;
    use crate::interfaces::test_support::{StubDirectory, StubPolicy, StubScenes};

    const MOUNT: &str = "/report-user";
    const PRESIGN: &str = "/api/reports/players/presign";
    const CREATE: &str = "/api/reports/players";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-not-really-a-png-";

    async fn app() -> Option<(Router, std::path::PathBuf)> {
        let (app, dir, _) = app_and_pool().await?;
        Some((app, dir))
    }

    async fn app_and_pool() -> Option<(Router, std::path::PathBuf, PgPool)> {
        let pool = crate::db::scratch::pool(4).await?;
        let daily_bytes = Settings::for_tests().report_evidence_daily_bytes;
        Some(app_over(pool, daily_bytes).await)
    }

    /// The area over a database of its own, with `daily_bytes` as the node's
    /// bound: that bound sums every slot of the database, so its tests do
    /// not share one with the others.
    async fn app_alone(daily_bytes: i64) -> Option<(Router, std::path::PathBuf, PgPool)> {
        let pool = crate::db::scratch::isolated_pool(4).await?;
        Some(app_over(pool, daily_bytes).await)
    }

    async fn app_over(pool: PgPool, daily_bytes: i64) -> (Router, std::path::PathBuf, PgPool) {
        let dir = std::env::temp_dir().join(format!(
            "catalyst-player-reports-{}",
            Uuid::new_v4().simple()
        ));
        let storage = Arc::new(
            Storage::from_config(&dir)
                .await
                .expect("a scratch blob store"),
        );
        let comms = Arc::new(CommsState::new(
            pool.clone(),
            Outbound::for_tests(),
            Settings {
                moderator_token: Some("moderator-token".into()),
                report_evidence_daily_bytes: daily_bytes,
                ..Settings::for_tests()
            },
            None,
            None,
            Arc::new(StubPolicy::default()),
            Arc::new(StubScenes::default()),
            Arc::new(crate::presence::Presence::new()),
            Arc::new(StubDirectory::default()),
        ));
        (Router::new().nest(MOUNT, router(comms, storage)), dir, pool)
    }

    /// One presigned file of `size` bytes, by a fresh wallet at `address`.
    async fn presign_sized(app: &Router, address: &str, size: i64) -> (StatusCode, Value) {
        let body = json!({ "files": [{ "filename": "shot.png", "contentType": "image/png", "fileSize": size }] });
        let headers = from_address(
            signed_at_mount(&random_key(), "post", PRESIGN).await,
            address,
        );
        let (status, value, _) = call(
            app,
            "post",
            PRESIGN,
            headers,
            "application/json",
            body.to_string().into_bytes(),
        )
        .await;
        (status, value)
    }

    /// `headers` as `serve` hands them over for a client at `address`.
    fn from_address(mut headers: HeaderMap, address: &str) -> HeaderMap {
        headers.insert("x-real-ip", address.parse().unwrap());
        headers
    }

    async fn presign_full(app: &Router, key: &str, address: &str) -> (StatusCode, Value) {
        let file =
            json!({ "filename": "shot.png", "contentType": "image/png", "fileSize": PNG.len() });
        let body = json!({ "files": vec![file; MAX_FILES] });
        let headers = from_address(signed_at_mount(key, "post", PRESIGN).await, address);
        let (status, value, _) = call(
            app,
            "post",
            PRESIGN,
            headers,
            "application/json",
            body.to_string().into_bytes(),
        )
        .await;
        (status, value)
    }

    /// A request at the mount; `signed_at_mount` signs it, as the client
    /// does, over the path it is sent to.
    async fn call(
        app: &Router,
        method: &str,
        local: &str,
        headers: HeaderMap,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Value, Vec<u8>) {
        let mut request = Request::builder()
            .method(method.to_uppercase().as_str())
            .uri(format!("{MOUNT}{local}"))
            .header("content-type", content_type);
        request.headers_mut().unwrap().extend(headers);
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), MAX_FILE_BYTES + 1024)
            .await
            .unwrap()
            .to_vec();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, bytes)
    }

    async fn signed_at_mount(key: &str, method: &str, local: &str) -> HeaderMap {
        signed(key, method, &format!("{MOUNT}{local}"), "{}").await
    }

    async fn presign_one(app: &Router, key: &str) -> (String, String, String) {
        let body = json!({ "files": [{ "filename": "shot one.png", "contentType": "image/png", "fileSize": PNG.len() }] });
        let headers = signed_at_mount(key, "post", PRESIGN).await;
        let (status, value, _) = call(
            app,
            "post",
            PRESIGN,
            headers,
            "application/json",
            body.to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let report_id = value["reportId"].as_str().unwrap().to_string();
        let slot = &value["files"][0];
        (
            report_id,
            slot["key"].as_str().unwrap().to_string(),
            slot["uploadPath"].as_str().unwrap().to_string(),
        )
    }

    /// Presigns, uploads and files one report; the filing's status and body.
    async fn file_one(app: &Router, key: &str, reported: &str) -> (StatusCode, Value) {
        let (report_id, slot, upload_path) = presign_one(app, key).await;
        let headers = signed_at_mount(key, "put", &upload_path).await;
        let (status, value, _) =
            call(app, "put", &upload_path, headers, "image/png", PNG.to_vec()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let headers = signed_at_mount(key, "post", CREATE).await;
        let (status, value, _) = call(
            app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&report_id, reported, &[&slot]),
        )
        .await;
        (status, value)
    }

    fn create_body(report_id: &str, reported: &str, keys: &[&str]) -> Vec<u8> {
        json!({
            "reportId": report_id,
            "reportedAddress": reported,
            "reason": "harassment",
            "description": "  Followed me around shouting.  ",
            "additionalComments": "",
            "confirmAccuracy": true,
            "evidenceKeys": keys,
        })
        .to_string()
        .into_bytes()
    }

    /// The signer's `PUT` of `body` to its slot, from a client at `address`.
    async fn put_from(
        app: &Router,
        key: &str,
        upload_path: &str,
        address: &str,
        body: &[u8],
    ) -> StatusCode {
        let headers = from_address(signed_at_mount(key, "put", upload_path).await, address);
        call(app, "put", upload_path, headers, "image/png", body.to_vec())
            .await
            .0
    }

    /// `PNG`'s size and first bytes, other content.
    fn other_png() -> Vec<u8> {
        let mut other = PNG.to_vec();
        *other.last_mut().unwrap() ^= 1;
        other
    }

    async fn content_hash_of(pool: &PgPool, report_id: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT content_hash FROM social.player_report_evidence WHERE report_id = $1",
        )
        .bind(Uuid::parse_str(report_id).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Makes the slots of `report_id` `minutes` older.
    async fn age(pool: &PgPool, report_id: &str, minutes: i32) {
        sqlx::query(
            "UPDATE social.player_report_evidence \
             SET created_at = created_at - $2::int * interval '1 minute' WHERE report_id = $1",
        )
        .bind(Uuid::parse_str(report_id).unwrap())
        .bind(minutes)
        .execute(pool)
        .await
        .unwrap();
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    fn fresh_wallet() -> String {
        format!("0x{}", &Uuid::new_v4().simple().to_string().repeat(2)[..40])
    }

    #[tokio::test]
    async fn presign_upload_create_then_moderator_reads() {
        let Some((app, dir)) = app().await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let reporter_key = random_key();
        let (report_id, key, upload_path) = presign_one(&app, &reporter_key).await;
        assert_eq!(key, "0-shot_one.png");
        assert_eq!(
            upload_path,
            super::upload_path(report_id.parse().unwrap(), &key)
        );

        // The bytes must be what the presign declared.
        let headers = signed_at_mount(&reporter_key, "put", &upload_path).await;
        let (status, _, _) = call(
            &app,
            "put",
            &upload_path,
            headers,
            "image/png",
            PNG[1..].to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let headers = signed_at_mount(&reporter_key, "put", &upload_path).await;
        let (status, value, _) = call(
            &app,
            "put",
            &upload_path,
            headers,
            "image/png",
            PNG.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");

        let reported = fresh_wallet();
        let headers = signed_at_mount(&reporter_key, "post", CREATE).await;
        let (status, value, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(
                &report_id,
                &reported.to_uppercase().replace("0X", "0x"),
                &[&key],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["data"]["id"], report_id);
        assert_eq!(value["data"]["evidenceKeys"], json!([key]));

        // A second filing, or another upload, of a filed report is refused.
        let headers = signed_at_mount(&reporter_key, "post", CREATE).await;
        let (status, _, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&report_id, &reported, &[&key]),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let headers = signed_at_mount(&reporter_key, "put", &upload_path).await;
        let (status, _, _) = call(
            &app,
            "put",
            &upload_path,
            headers,
            "image/png",
            PNG.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // The moderator token reads the queue; nobody else does.
        let list_path = format!("/api/reports?reported={reported}");
        let (status, _, _) = call(&app, "get", &list_path, HeaderMap::new(), "", vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, value, _) = call(
            &app,
            "get",
            &list_path,
            bearer("moderator-token"),
            "",
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["total"], 1);
        let row = &value["data"][0];
        assert_eq!(row["id"], report_id);
        assert_eq!(row["reporter"], address_of(&reporter_key).to_lowercase());
        assert_eq!(row["reportedAddress"], reported);
        assert_eq!(row["reason"], "harassment");
        assert_eq!(row["description"], "Followed me around shouting.");
        assert_eq!(row["additionalComments"], Value::Null);
        assert_eq!(row["status"], "open");
        assert_eq!(row["evidence"][0]["key"], key);
        assert_eq!(row["evidence"][0]["contentType"], "image/png");
        assert_eq!(row["evidence"][0]["declaredSize"], PNG.len());
        assert!(row["evidence"][0]["contentHash"].is_string());

        let evidence_path = format!("/api/reports/{report_id}/evidence/{key}");
        let (status, _, bytes) = call(
            &app,
            "get",
            &evidence_path,
            bearer("moderator-token"),
            "",
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PNG);
        let (status, _, _) = call(&app, "get", &evidence_path, HeaderMap::new(), "", vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn another_signer_cannot_upload_or_file_a_presign() {
        let Some((app, dir)) = app().await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let reporter_key = random_key();
        let (report_id, key, upload_path) = presign_one(&app, &reporter_key).await;

        let headers = signed_at_mount(OUTSIDER_KEY, "put", &upload_path).await;
        let (status, _, _) = call(
            &app,
            "put",
            &upload_path,
            headers,
            "image/png",
            PNG.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let reported = "0x1111111111111111111111111111111111111111";
        let headers = signed_at_mount(OUTSIDER_KEY, "post", CREATE).await;
        let (status, _, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&report_id, reported, &[&key]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // The signer itself cannot file before every upload landed, nor with
        // a key the presign never named, nor under an unknown report id.
        let headers = signed_at_mount(&reporter_key, "post", CREATE).await;
        let (status, _, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&report_id, reported, &[&key]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let headers = signed_at_mount(&reporter_key, "post", CREATE).await;
        let (status, _, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&report_id, reported, &[&key, "9-other.png"]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let headers = signed_at_mount(&reporter_key, "post", CREATE).await;
        let (status, _, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&Uuid::new_v4().to_string(), reported, &[&key]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Unsigned requests (no auth chain) never reach the rows.
        let (status, _, _) = call(
            &app,
            "post",
            PRESIGN,
            HeaderMap::new(),
            "application/json",
            b"{\"files\":[]}".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_wallet_files_at_most_five_reports_an_hour() {
        let Some((app, dir)) = app().await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let reporter_key = random_key();
        let reported = fresh_wallet();
        // Slots presigned ahead of the limit are refused at filing time.
        let (spare_id, spare_key, spare_upload) = presign_one(&app, &reporter_key).await;
        let headers = signed_at_mount(&reporter_key, "put", &spare_upload).await;
        let (status, value, _) = call(
            &app,
            "put",
            &spare_upload,
            headers,
            "image/png",
            PNG.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        for _ in 0..REPORTS_PER_HOUR {
            let (status, value) = file_one(&app, &reporter_key, &reported).await;
            assert_eq!(status, StatusCode::OK, "{value}");
        }
        let headers = signed_at_mount(&reporter_key, "post", CREATE).await;
        let (status, _, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&spare_id, &reported, &[&spare_key]),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        // A new presign is refused early, with the same status.
        let body = json!({ "files": [{ "filename": "late.png", "contentType": "image/png", "fileSize": PNG.len() }] });
        let headers = signed_at_mount(&reporter_key, "post", PRESIGN).await;
        let (status, _, _) = call(
            &app,
            "post",
            PRESIGN,
            headers,
            "application/json",
            body.to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        // The limit is per wallet.
        let (status, value) = file_one(&app, &random_key(), &reported).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let list_path = format!("/api/reports?reported={reported}");
        let (status, value, _) = call(
            &app,
            "get",
            &list_path,
            bearer("moderator-token"),
            "",
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["total"], REPORTS_PER_HOUR + 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fresh_wallets_share_the_budget_of_their_address() {
        let Some((app, dir)) = app().await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        // Each presign is a fresh key, so no wallet quota is near.
        for _ in 0..SOURCE_SLOTS_PER_DAY as usize / MAX_FILES {
            let (status, value) = presign_full(&app, &random_key(), "203.0.113.7").await;
            assert_eq!(status, StatusCode::OK, "{value}");
        }
        let (status, _) = presign_full(&app, &random_key(), "203.0.113.7").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        // An IPv6 client is its /64, and another address has its own budget.
        for _ in 0..SOURCE_SLOTS_PER_DAY as usize / MAX_FILES {
            let (status, value) = presign_full(&app, &random_key(), "2001:db8:0:1::7").await;
            assert_eq!(status, StatusCode::OK, "{value}");
        }
        let (status, _) = presign_full(&app, &random_key(), "2001:db8:0:1::8").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let (status, value) = presign_full(&app, &random_key(), "203.0.113.8").await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_node_bounds_a_day_of_evidence_from_all_wallets_and_addresses() {
        let cap = 2 * MAX_FILE_BYTES as i64 + NODE_SLOT_FLOOR_BYTES;
        let Some((app, dir, pool)) = app_alone(cap).await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        // Each presign is a fresh wallet at a fresh address, so no wallet
        // quota and no address budget is near. A small file counts for the floor.
        let (status, value) = presign_sized(&app, "203.0.113.21", MAX_FILE_BYTES as i64).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let first = Uuid::parse_str(value["reportId"].as_str().unwrap()).unwrap();
        let (status, value) = presign_sized(&app, "203.0.113.22", MAX_FILE_BYTES as i64).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let (status, value) = presign_sized(&app, "203.0.113.23", 1).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        // The bound is full: one more byte is refused and mints no slot.
        let (status, value) = presign_sized(&app, "203.0.113.24", 1).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
        assert!(
            value.to_string().contains("no more report evidence"),
            "{value}"
        );
        let slots: i64 = sqlx::query_scalar("SELECT count(*) FROM social.player_report_evidence")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(slots, 3);
        // A slot past the window no longer counts.
        sqlx::query(
            "UPDATE social.player_report_evidence \
             SET created_at = created_at - ($2::int + 1) * interval '1 hour' \
             WHERE report_id = $1",
        )
        .bind(first)
        .bind(PENDING_SLOT_WINDOW_HOURS)
        .execute(&pool)
        .await
        .unwrap();
        let (status, value) = presign_sized(&app, "203.0.113.25", MAX_FILE_BYTES as i64).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let (status, _) = presign_sized(&app, "203.0.113.26", 1).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn concurrent_presigns_do_not_pass_the_node_bound_together() {
        let room = 3;
        let Some((app, dir, pool)) = app_alone(room * NODE_SLOT_FLOOR_BYTES).await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let addresses: Vec<String> = (1..=12).map(|n| format!("198.51.100.{n}")).collect();
        let answers = futures::future::join_all(
            addresses
                .iter()
                .map(|address| presign_sized(&app, address, 1)),
        )
        .await;
        let accepted = answers
            .iter()
            .filter(|(status, _)| *status == StatusCode::OK)
            .count();
        let refused = answers
            .iter()
            .filter(|(status, _)| *status == StatusCode::TOO_MANY_REQUESTS)
            .count();
        assert_eq!((accepted, refused), (room as usize, 9), "{answers:?}");
        let slots: i64 = sqlx::query_scalar("SELECT count(*) FROM social.player_report_evidence")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(slots, room);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn addresses_at_their_share_leave_the_node_open_to_another_address() {
        let share = 6 * MAX_FILE_BYTES as i64;
        let Some((app, dir, _)) = app_alone(NODE_SHARE_DIVISOR * share).await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        assert_eq!(source_share(NODE_SHARE_DIVISOR * share), share);
        assert_eq!(source_share(1), NODE_SHARE_FLOOR_BYTES);
        // Five addresses take all they may, each presign a fresh wallet.
        for n in 1..=5 {
            let address = format!("203.0.113.{n}");
            for _ in 0..6 {
                let (status, value) = presign_sized(&app, &address, MAX_FILE_BYTES as i64).await;
                assert_eq!(status, StatusCode::OK, "{value}");
            }
            let (status, value) = presign_sized(&app, &address, 1).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
            assert!(value.to_string().contains("This address"), "{value}");
        }
        // The node still takes the files of a sixth.
        let (status, value) = presign_sized(&app, "203.0.113.6", MAX_FILE_BYTES as i64).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_presign_nobody_filled_stops_counting_after_its_upload_time() {
        let Some((app, dir, pool)) = app_alone(2 * NODE_SLOT_FLOOR_BYTES).await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let (empty_key, filled_key) = (random_key(), random_key());
        let (empty, _, empty_path) = presign_one(&app, &empty_key).await;
        let (filled, _, filled_path) = presign_one(&app, &filled_key).await;
        let status = put_from(&app, &filled_key, &filled_path, "203.0.113.30", PNG).await;
        assert_eq!(status, StatusCode::OK);
        // Both slots count in their upload time.
        let (status, value) = presign_sized(&app, "203.0.113.31", 1).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
        // Past it the slot nobody filled leaves the count; the filled one stays.
        for id in [&empty, &filled] {
            age(&pool, id, RESERVATION_MINUTES + 1).await;
        }
        let (status, value) = presign_sized(&app, "203.0.113.32", 1).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let (status, value) = presign_sized(&app, "203.0.113.33", 1).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
        // The slot that left the count takes no file.
        let status = put_from(&app, &empty_key, &empty_path, "203.0.113.30", PNG).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(content_hash_of(&pool, &empty).await, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_slot_takes_one_body_from_any_address() {
        let Some((app, dir, pool)) = app_and_pool().await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let key = random_key();
        let (report_id, _, upload_path) = presign_one(&app, &key).await;
        let status = put_from(&app, &key, &upload_path, "203.0.113.41", PNG).await;
        assert_eq!(status, StatusCode::OK);
        // Other bytes are refused, whatever address sends them.
        let other = other_png();
        for address in ["203.0.113.42", "203.0.113.43", "2001:db8:0:42::1"] {
            let status = put_from(&app, &key, &upload_path, address, &other).await;
            assert_eq!(status, StatusCode::CONFLICT);
        }
        // The same bytes again answer as the first time.
        let status = put_from(&app, &key, &upload_path, "203.0.113.44", PNG).await;
        assert_eq!(status, StatusCode::OK);
        let (first, second) = (
            crate::auth::hash_bytes_v1(PNG),
            crate::auth::hash_bytes_v1(&other),
        );
        assert_eq!(
            content_hash_of(&pool, &report_id).await,
            Some(first.clone())
        );
        let store = Storage::from_config(&dir).await.unwrap();
        assert!(store.exist(&first).await.unwrap());
        assert!(!store.exist(&second).await.unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failed_store_clears_the_claim() {
        let Some((app, dir, pool)) = app_and_pool().await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let key = random_key();
        let (report_id, _, upload_path) = presign_one(&app, &key).await;
        // A file where the store keeps its tree: every write fails.
        let tree = dir.join("contents");
        std::fs::remove_dir_all(&tree).unwrap();
        std::fs::write(&tree, b"").unwrap();
        let status = put_from(&app, &key, &upload_path, "203.0.113.45", PNG).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_hash_of(&pool, &report_id).await, None);
        // The slot is free again: with the store back it takes a body, any one.
        std::fs::remove_file(&tree).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        let other = other_png();
        let status = put_from(&app, &key, &upload_path, "203.0.113.45", &other).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            content_hash_of(&pool, &report_id).await,
            Some(crate::auth::hash_bytes_v1(&other))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_claim_without_its_bytes_is_not_uploaded() {
        let Some((app, dir, pool)) = app_and_pool().await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let key = random_key();
        let (report_id, slot, upload_path) = presign_one(&app, &key).await;
        // The claim of a `PUT` whose write to the store has not ended.
        sqlx::query(
            "UPDATE social.player_report_evidence \
             SET content_hash = $2, uploaded_at = timezone('UTC', now()) WHERE report_id = $1",
        )
        .bind(Uuid::parse_str(&report_id).unwrap())
        .bind(crate::auth::hash_bytes_v1(PNG))
        .execute(&pool)
        .await
        .unwrap();
        let status = put_from(&app, &key, &upload_path, "203.0.113.48", PNG).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let headers = signed_at_mount(&key, "post", CREATE).await;
        let (status, value, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&report_id, &fresh_wallet(), &[&slot]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_address_key_step_keeps_the_slots_of_before() {
        const STEP: &str = include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/schemas/migrations/0046_player_report_evidence_source_key.sql"
        ));
        let Some((app, dir, pool)) = app_alone(2 * NODE_SLOT_FLOOR_BYTES).await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        // The table as it was before the step, with one slot.
        sqlx::raw_sql(
            "ALTER TABLE social.player_report_evidence DROP COLUMN source_key; \
             INSERT INTO social.player_report_evidence \
               (report_id, evidence_key, reporter_address, filename, content_type, declared_size) \
             VALUES ('00000000-0000-4000-8000-000000000046', '0-shot.png', '0xbefore', \
                     'shot.png', 'image/png', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for _ in 0..2 {
            sqlx::raw_sql(STEP).execute(&pool).await.unwrap();
        }
        let keys = "SELECT source_key FROM social.player_report_evidence ORDER BY created_at";
        let before: Vec<Option<String>> = sqlx::query_scalar(keys).fetch_all(&pool).await.unwrap();
        assert_eq!(before, [None]);
        // That slot counts for the node, and a new slot keeps its address.
        let (status, value) = presign_sized(&app, "203.0.113.46", 1).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let (status, value) = presign_sized(&app, "203.0.113.47", 1).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
        let after: Vec<Option<String>> = sqlx::query_scalar(keys).fetch_all(&pool).await.unwrap();
        assert_eq!(after, [None, Some("203.0.113.46".to_string())]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_upload_spends_its_size_from_the_address_budget() {
        let budget = RateLimiter::new(kib(2 * MAX_FILE_BYTES), SOURCE_WINDOW);
        assert_eq!(kib(1), 1);
        assert_eq!(kib(1024), 1);
        assert_eq!(kib(1025), 2);
        let spent = |source, cost| spend(&budget, source, cost, "evidence bytes").is_ok();
        assert!(spent("203.0.113.7", kib(MAX_FILE_BYTES)));
        assert!(spent("203.0.113.7", kib(MAX_FILE_BYTES)));
        assert!(!spent("203.0.113.7", 1));
        assert!(spent("203.0.113.8", 1));
        // A failed store gives the bytes back.
        budget.refund("203.0.113.7", 1);
        assert!(spent("203.0.113.7", 1));
    }

    #[tokio::test]
    async fn a_presign_sweeps_expired_unfiled_slots_and_keeps_filed_ones() {
        // A database of its own: the presign of another test would hold the rows this one reads.
        let daily_bytes = Settings::for_tests().report_evidence_daily_bytes;
        let Some((app, dir, pool)) = app_alone(daily_bytes).await else {
            eprintln!("SKIP: player reports need CATALYST_TEST_DATABASE_URL");
            return;
        };
        let reporter_key = random_key();
        let (abandoned, _, _) = presign_one(&app, &reporter_key).await;
        let (pending, _, _) = presign_one(&app, &reporter_key).await;
        let (status, value) = file_one(&app, &reporter_key, &fresh_wallet()).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let filed = value["data"]["id"].as_str().unwrap().to_string();
        // The abandoned and the filed slots are past the window; the pending one is not.
        for id in [&abandoned, &filed] {
            sqlx::query(
                "UPDATE social.player_report_evidence \
                 SET created_at = created_at - ($2::int + 1) * interval '1 hour' \
                 WHERE report_id = $1",
            )
            .bind(Uuid::parse_str(id).unwrap())
            .bind(PENDING_SLOT_WINDOW_HOURS)
            .execute(&pool)
            .await
            .unwrap();
        }
        presign_one(&app, &random_key()).await;
        let left: Vec<Uuid> = sqlx::query_scalar(
            "SELECT report_id FROM social.player_report_evidence WHERE report_id = ANY($1)",
        )
        .bind(
            [&abandoned, &pending, &filed]
                .map(|id| Uuid::parse_str(id).unwrap())
                .to_vec(),
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!left.contains(&Uuid::parse_str(&abandoned).unwrap()));
        assert!(left.contains(&Uuid::parse_str(&pending).unwrap()));
        assert!(left.contains(&Uuid::parse_str(&filed).unwrap()));
        // The filed slot past the window lost its address key; the pending one has it.
        let keyed: Vec<Uuid> = sqlx::query_scalar(
            "SELECT report_id FROM social.player_report_evidence \
             WHERE report_id = ANY($1) AND source_key IS NOT NULL",
        )
        .bind(
            [&pending, &filed]
                .map(|id| Uuid::parse_str(id).unwrap())
                .to_vec(),
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(keyed, [Uuid::parse_str(&pending).unwrap()]);
        // Nothing is left to sweep, and the expired report cannot be filed.
        assert_eq!(sweep_expired_slots(&pool).await.unwrap(), 0);
        let headers = signed_at_mount(&reporter_key, "post", CREATE).await;
        let (status, _, _) = call(
            &app,
            "post",
            CREATE,
            headers,
            "application/json",
            create_body(&abandoned, &fresh_wallet(), &["0-shot_one.png"]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn evidence_bytes_must_begin_as_the_declared_type() {
        assert!(starts_as("image/png", PNG));
        assert!(!starts_as("image/jpeg", PNG));
        assert!(starts_as("image/jpeg", b"\xff\xd8\xff\xe0"));
        assert!(starts_as("image/gif", b"GIF89a.."));
        assert!(starts_as("image/webp", b"RIFF\x10\0\0\0WEBPVP8 "));
        assert!(!starts_as("image/webp", b"RIFF\x10\0\0\0WAVEfmt "));
        assert!(starts_as("video/mp4", b"\0\0\0\x18ftypmp42"));
        assert!(starts_as("video/webm", b"\x1a\x45\xdf\xa3\x9f"));
        assert!(starts_as("application/pdf", b"%PDF-1.7"));
        assert!(!starts_as("application/pdf", b"<html>"));
        assert!(!starts_as("text/html", b"<html>"));
        assert!(!starts_as("image/png", b""));
    }

    #[test]
    fn filenames_are_sanitized() {
        assert_eq!(sanitize_filename("  ../evil name.PNG "), "_evil_name.PNG");
        assert_eq!(sanitize_filename("..."), "file");
        assert_eq!(sanitize_filename("ünïcode.jpg"), "_n_code.jpg");
    }
}
