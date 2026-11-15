//! Upstream forwarding: a deployment this node accepted from a client
//! (`POST /entities`, content or worlds) is sent on, unchanged, to the
//! catalyst or worlds content server the operator names
//! (`CONTENT_FORWARD_UPSTREAM_URL`, `WORLDS_FORWARD_UPSTREAM_URL`). Local
//! authority is untouched: the deploy commits here as before, and the same
//! statement (content) or transaction (worlds) queues one outbox row
//! (`content.forward_outbox`, `worlds.forward_outbox`). The leased worker
//! below sends it after the commit, so a crash between the two leaves a
//! queued row the next worker sends; the deploy's answer is the local one.
//!
//! A send is what catalyst-client's `deploy` does: ask the upstream which
//! files it already holds (`GET /available-content?cid=..`, split under the
//! client's 2,048-byte URL bound; a question the upstream refuses sends the
//! files), then POST one multipart form: `entityId`,
//! `authChain[i][type|payload|signature]`, the entity file and every content
//! file the upstream lacks, each file a part named by its hash. The entity
//! part is the stored bytes (the id is their hash) and the chain the stored
//! links, so the upstream verifies the signature the client made.
//!
//! Per row: 2xx `accepted`; 404, 405 (the route is missing: a read-only
//! catalyst), 408, 425, 429, 5xx, a status past 599 or a transport error
//! retry with a doubling delay until `FORWARD_MAX_ATTEMPTS` (then `failed`);
//! any other status is the upstream's verdict, `rejected`, its body kept. A
//! row whose deployment is no longer this node's before it was sent (content:
//! overwritten or unpublished; worlds: replaced, undeployed or mirrored over)
//! is `superseded` and never sent: a worlds upstream applies the last publish
//! it receives, so an old scene sent late would roll the world back, and a
//! scene its LAND holder took down stays down. Unpublishes are not forwarded.
//!
//! Both upstreams refuse an entity signed too long ago ([`Lane::max_age_ms`]:
//! catalyst 20 minutes, a worlds server 5), so a forward has that long from
//! the signature: an entity past it is `failed` unsent (after a long outage,
//! or a claim a dead worker held for [`SEND_LEASE`]) and only its signer's
//! new deploy reaches the upstream. A shutdown gives its send in flight back
//! as due, unsent; rows queued for another target are closed once past every
//! window ([`Forwarder::close_foreign`]). The operator requeues a `rejected`,
//! `failed` or stranded record for the current target while its entity is
//! inside the window ([`Forwarder::requeue`], the lanes' `.../retry` routes).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::NaiveDateTime;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::SharedState;
use super::errors::json_error;
use crate::http::{HttpClientCfg, Outbound, read_body_capped};
use crate::jobs::Jobs;
use crate::storage::Storage;

/// The longest one send may take, upload and answer included.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(600);
/// A claimed row is due again after this, so a send whose worker died is
/// retried and a live one (bounded by [`SEND_TIMEOUT`]) is never claimed twice.
const SEND_LEASE: Duration = Duration::from_secs(900);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
/// catalyst-client's `MAX_URL_LENGTH`: every `/available-content` query stays under it.
const MAX_URL_LENGTH: usize = 2048;
/// The upstream's answer kept in `last_error`, at most.
const ERROR_TEXT_CAP: usize = 8 * 1024;
const RESPONSE_BODY_CAP: usize = 1024 * 1024;
const TICK: Duration = Duration::from_secs(5);
/// Rows one pass claims at most before it yields to the next tick.
const PASS_LIMIT: usize = 256;
const RETENTION: chrono::Duration = chrono::Duration::days(30);
const SWEEP_EVERY: Duration = Duration::from_secs(3600);
const SWEEP_BATCH: i64 = 1000;
/// Statements one hourly sweep runs at most: 100,000 rows an hour, past any
/// local deploy rate; a larger backlog resumes the next hour.
const SWEEP_MAX_BATCHES: usize = 100;
const LIST_DEFAULT: i64 = 100;
const LIST_MAX: i64 = 1000;

pub const STATUSES: [&str; 6] = [
    "queued",
    "sending",
    "accepted",
    "rejected",
    "failed",
    "superseded",
];

/// What differs between the content and the worlds lane; every string is a
/// constant of the owning domain.
#[derive(Clone, Copy, Debug)]
pub struct Lane {
    /// The leased job's name.
    pub job: &'static str,
    /// The outbox table, schema-qualified.
    pub table: &'static str,
    /// Appended to the target for the deploy POST.
    pub deploy_path: &'static str,
    /// Appended to the target for the file probe.
    pub available_path: &'static str,
    /// `$1` the outbox row id: the stored auth chain while the row's entity is
    /// still this node's local deployment; no row once it is not.
    pub source_sql: &'static str,
    /// An outbox column the status rows carry as `worldName`.
    pub world_column: Option<&'static str>,
    /// How far behind and ahead of the upstream's clock an entity's signed
    /// `timestamp` may be (catalyst's `REQUEST_TTL_BACKWARDS` / `_FORWARDS`,
    /// worlds-content-server's `DEPLOYMENT_TTL` and future skew): the window
    /// this node's own deploy enforces, in milliseconds.
    pub max_age_ms: i64,
    pub max_ahead_ms: i64,
}

impl Lane {
    /// Why the upstream refuses `entity` for its age, if it does: its signed
    /// `timestamp` is more than [`Lane::max_age_ms`] behind `now_ms`, the
    /// comparison both upstreams make.
    pub fn too_old(&self, entity: &Value, now_ms: i64) -> Option<String> {
        let timestamp = entity.get("timestamp").and_then(Value::as_i64)?;
        (now_ms.saturating_sub(timestamp) > self.max_age_ms).then(|| {
            format!(
                "the entity is older than the upstream accepts ({} min from its signature); \
                 its signer must deploy it again",
                self.max_age_ms / 60_000
            )
        })
    }
}

/// `content.forward_outbox` to `<upstream>/content/entities`. The source is
/// the deployment while nothing overwrote it and its scene was not
/// unpublished (`DELETE /scenes/{coord}` tombstones the provenance row and
/// leaves the scene's own `deleter_deployment` alone).
pub const CONTENT_LANE: Lane = Lane {
    job: "content-forward",
    table: "content.forward_outbox",
    deploy_path: "/content/entities",
    available_path: "/content/available-content",
    source_sql: "SELECT d.auth_chain FROM content.forward_outbox o \
                 JOIN content.deployments d ON d.entity_id = o.entity_id \
                 WHERE o.id = $1 AND d.deleter_deployment IS NULL \
                 AND NOT EXISTS (SELECT 1 FROM content.local_entities le \
                                 WHERE le.entity_id = d.entity_id AND le.tombstoned_at IS NOT NULL)",
    world_column: None,
    max_age_ms: super::write_deployer::REQUEST_TTL_BACKWARDS_MS,
    max_ahead_ms: super::write_deployer::REQUEST_TTL_FORWARDS_MS,
};

/// The retry knobs (`FORWARD_MAX_ATTEMPTS`, `FORWARD_RETRY_BASE_SECS`).
#[derive(Clone, Copy, Debug)]
pub struct Retry {
    pub max_attempts: i32,
    pub base: Duration,
}

impl Retry {
    pub fn new(max_attempts: u32, base_secs: u64) -> Self {
        Self {
            max_attempts: max_attempts.clamp(1, i32::MAX as u32) as i32,
            base: Duration::from_secs(base_secs),
        }
    }

    /// The wait after the `attempts`-th send failed: the base, doubling,
    /// capped at five minutes (or the base, when that is longer).
    fn delay(&self, attempts: i32) -> Duration {
        let doublings = attempts.saturating_sub(1).clamp(0, 16) as u32;
        self.base
            .saturating_mul(1u32 << doublings)
            .min(MAX_RETRY_DELAY.max(self.base))
    }
}

/// One lane's sender: the deploy path queues rows for [`Forwarder::target`]
/// and calls [`Forwarder::kick`]; [`Forwarder::spawn`] runs the worker.
pub struct Forwarder {
    pool: PgPool,
    storage: Arc<Storage>,
    client: Outbound,
    lane: Lane,
    target: String,
    retry: Retry,
    kick: Notify,
}

impl std::fmt::Debug for Forwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forwarder")
            .field("lane", &self.lane.job)
            .field("target", &origin(&self.target))
            .field("retry", &self.retry)
            .finish()
    }
}

/// The client every lane sends with: no whole-request budget below
/// [`SEND_TIMEOUT`] and no read timeout (an upstream validating a large
/// scene answers after the last byte, not between them), no redirects (a
/// POST body is not replayed), the SSRF guard of [`Outbound`].
pub fn client() -> reqwest::Result<Outbound> {
    Outbound::new(HttpClientCfg {
        total_timeout: Some(SEND_TIMEOUT),
        read_timeout: None,
        max_redirects: None,
        ..HttpClientCfg::default()
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    pub accepted: usize,
    pub rejected: usize,
    pub retried: usize,
    pub failed: usize,
    pub superseded: usize,
}

impl PassReport {
    pub fn total(&self) -> usize {
        self.accepted + self.rejected + self.retried + self.failed + self.superseded
    }
}

struct Claimed {
    id: i64,
    entity_id: String,
    attempts: i32,
}

#[derive(Debug)]
enum Outcome {
    Accepted {
        status: u16,
    },
    Rejected {
        status: u16,
        body: String,
    },
    Retry {
        status: Option<u16>,
        error: String,
    },
    /// The stored deployment cannot be sent (its entity file or chain is
    /// unusable): terminal without a send.
    Failed(String),
    Superseded,
}

impl Forwarder {
    /// `target` is the upstream base (a trailing `/` is dropped).
    pub fn new(
        pool: PgPool,
        storage: Arc<Storage>,
        client: Outbound,
        lane: Lane,
        target: &str,
        retry: Retry,
    ) -> Self {
        Self {
            pool,
            storage,
            client,
            lane,
            target: target.trim().trim_end_matches('/').to_string(),
            retry,
            kick: Notify::new(),
        }
    }

    /// The value the deploy path writes into the row's `target`.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Wakes this process's worker after a commit queued a row; a worker in
    /// another process finds it on its next tick.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// One fleet-wide job per lane (the outbox is shared rows): a pass every
    /// five seconds and on every kick, the retention sweep once an hour; a
    /// failing or panicking pass is reported through the job trace, never fatal.
    pub fn spawn(self: &Arc<Self>, jobs: &Jobs) {
        let this = self.clone();
        jobs.spawn_leased(self.lane.job, move |ctx| {
            let this = this.clone();
            async move {
                let mut ticker = tokio::time::interval(TICK);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut swept: Option<Instant> = None;
                loop {
                    tokio::select! {
                        _ = ctx.cancel.cancelled() => return Ok(()),
                        _ = ticker.tick() => {}
                        _ = this.kick.notified() => {}
                    }
                    if swept.is_none_or(|at| at.elapsed() >= SWEEP_EVERY) {
                        swept = Some(Instant::now());
                        if let Err(e) = this.close_foreign(&ctx.cancel).await {
                            tracing::warn!(job = this.lane.job, error = %e, "closing other targets' rows failed");
                        }
                        if let Err(e) = this.sweep(now() - RETENTION, &ctx.cancel).await {
                            tracing::warn!(job = this.lane.job, error = %e, "forwarding retention sweep failed");
                        }
                    }
                    use futures::FutureExt;
                    match std::panic::AssertUnwindSafe(this.run_pass(&ctx.cancel))
                        .catch_unwind()
                        .await
                    {
                        Ok(Ok(report)) if report.total() > 0 => ctx.progress(format!("{report:?}")),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::error!(job = this.lane.job, error = %e, "forwarding pass failed");
                            ctx.error(format!("pass: {e}"));
                        }
                        Err(_) => {
                            tracing::error!(job = this.lane.job, "forwarding pass panicked");
                            ctx.error("pass: panic");
                        }
                    }
                }
            }
        });
    }

    /// Claims and sends due rows one at a time, at most [`PASS_LIMIT`], until
    /// none is due or `cancel` fires. A send in flight then is cut short and
    /// its claim given back ([`Forwarder::release`]): waiting out the lease
    /// would leave the next worker an entity past the upstream's window.
    pub async fn run_pass(&self, cancel: &CancellationToken) -> sqlx::Result<PassReport> {
        let mut report = PassReport::default();
        for _ in 0..PASS_LIMIT {
            if cancel.is_cancelled() {
                break;
            }
            let Some(row) = self.claim().await? else {
                break;
            };
            let outcome = if row.attempts > self.retry.max_attempts {
                // The last allowed send never recorded its outcome (its worker
                // died or its finish failed), or the bound was lowered: the
                // row ends here rather than being sent past the bound.
                Outcome::Failed(
                    "FORWARD_MAX_ATTEMPTS spent; the last send's outcome was never recorded".into(),
                )
            } else {
                let sent = tokio::select! {
                    biased;
                    outcome = self.send(&row) => Some(outcome),
                    _ = cancel.cancelled() => None,
                };
                let Some(outcome) = sent else {
                    self.release(&row).await?;
                    tracing::info!(job = self.lane.job, entity_id = %row.entity_id, "forwarding cut short by shutdown; queued again");
                    break;
                };
                outcome
            };
            let state = self.finish(&row, &outcome).await?;
            match state {
                "accepted" => report.accepted += 1,
                "rejected" => report.rejected += 1,
                "queued" => report.retried += 1,
                "superseded" => report.superseded += 1,
                _ => report.failed += 1,
            }
            tracing::info!(job = self.lane.job, entity_id = %row.entity_id, attempts = row.attempts, state, ?outcome, "forwarded deployment");
        }
        Ok(report)
    }

    async fn claim(&self) -> sqlx::Result<Option<Claimed>> {
        let now = now();
        let sql = format!(
            "UPDATE {t} SET status = 'sending', attempts = attempts + 1, next_attempt_at = $2, updated_at = $1 \
             WHERE id = (SELECT id FROM {t} \
                         WHERE status IN ('queued', 'sending') AND next_attempt_at <= $1 AND target = $3 \
                         ORDER BY next_attempt_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) \
             RETURNING id, entity_id, attempts",
            t = self.lane.table
        );
        let row: Option<(i64, String, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(now)
            .bind(now + chrono::Duration::from_std(SEND_LEASE).unwrap_or_default())
            .bind(&self.target)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(id, entity_id, attempts)| Claimed {
            id,
            entity_id,
            attempts,
        }))
    }

    async fn send(&self, row: &Claimed) -> Outcome {
        let chain: Option<Value> =
            match sqlx::query_scalar(sqlx::AssertSqlSafe(self.lane.source_sql))
                .bind(row.id)
                .fetch_optional(&self.pool)
                .await
            {
                Ok(chain) => chain,
                Err(e) => {
                    return Outcome::Retry {
                        status: None,
                        error: format!("reading the deployment: {e}"),
                    };
                }
            };
        let Some(chain) = chain else {
            return Outcome::Superseded;
        };
        let links = match chain_links(&chain) {
            Ok(links) => links,
            Err(e) => return Outcome::Failed(e),
        };
        let entity = match self.storage.retrieve(&row.entity_id).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return Outcome::Failed(
                    "the entity file is not in the blob store (missing or denylisted)".into(),
                );
            }
            Err(e) => {
                return Outcome::Retry {
                    status: None,
                    error: format!("reading the entity file: {e}"),
                };
            }
        };
        let entity_json: Value = match serde_json::from_slice(&entity) {
            Ok(json) => json,
            Err(e) => return Outcome::Failed(format!("the stored entity file is not JSON: {e}")),
        };
        if let Some(reason) = self.lane.too_old(&entity_json, now_ms()) {
            return Outcome::Failed(reason);
        }
        let hashes = content_hashes(&entity_json, &row.entity_id);
        // A file missing here is left to the upstream: it may hold it, and
        // its verdict on the deployment is the one that counts.
        let mut stored = Vec::with_capacity(hashes.len());
        for hash in hashes {
            match self.storage.file_info(&hash).await {
                Ok(Some(info)) => stored.push((hash, info.size)),
                Ok(None) => {}
                Err(e) => {
                    return Outcome::Retry {
                        status: None,
                        error: format!("reading {hash}: {e}"),
                    };
                }
            }
        }
        let held = match self.upstream_holds(&stored).await {
            Ok(held) => held,
            Err(outcome) => return outcome,
        };
        stored.retain(|(hash, _)| !held.contains(hash));

        let form = Form::new(boundary(), &row.entity_id, &links, entity, &stored);
        let length = form.content_length();
        let sent = self
            .client
            .post(format!("{}{}", self.target, self.lane.deploy_path))
            .header(header::CONTENT_TYPE, form.content_type())
            .header(header::CONTENT_LENGTH, length)
            .timeout(SEND_TIMEOUT)
            .body(reqwest::Body::wrap_stream(
                form.into_stream(self.storage.clone()),
            ))
            .send()
            .await;
        match sent {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = match read_body_capped(resp, RESPONSE_BODY_CAP).await {
                    Ok(body) => String::from_utf8_lossy(&body).into_owned(),
                    Err(e) => format!("(the answer's body was not read: {e})"),
                };
                verdict(status, capped(body))
            }
            Err(e) => Outcome::Retry {
                status: None,
                error: format!("POST {}: {e}", self.lane.deploy_path),
            },
        }
    }

    /// The hashes among `stored` the upstream says it has. The probe only
    /// saves bytes: an upstream that is down (a transport error or a
    /// retryable status) retries the row, one that refuses or garbles the
    /// question (a worlds server refuses a batch holding a v0 CID) gets that
    /// batch's files sent.
    async fn upstream_holds(&self, stored: &[(String, u64)]) -> Result<HashSet<String>, Outcome> {
        #[derive(Deserialize)]
        struct Availability {
            cid: String,
            available: bool,
        }
        let hashes: Vec<&str> = stored.iter().map(|(h, _)| h.as_str()).collect();
        let base = format!("{}{}", self.target, self.lane.available_path);
        let path = self.lane.available_path;
        let mut held = HashSet::new();
        for url in cid_queries(&base, &hashes) {
            let resp = self
                .client
                .get(url)
                .timeout(PROBE_TIMEOUT)
                .send()
                .await
                .map_err(|e| Outcome::Retry {
                    status: None,
                    error: format!("GET {path}: {e}"),
                })?;
            let status = resp.status().as_u16();
            let body = read_body_capped(resp, RESPONSE_BODY_CAP).await;
            let answer = match body {
                Ok(body) if (200..=299).contains(&status) => {
                    serde_json::from_slice::<Vec<Availability>>(&body)
                        .map_err(|e| format!("unreadable answer: {e}"))
                }
                Ok(body) if transient(status) => {
                    return Err(Outcome::Retry {
                        status: Some(status),
                        error: format!(
                            "GET {path}: {}",
                            capped(String::from_utf8_lossy(&body).into_owned())
                        ),
                    });
                }
                Ok(_) => Err(format!("status {status}")),
                Err(e) => Err(e.to_string()),
            };
            match answer {
                Ok(rows) => held.extend(rows.into_iter().filter(|r| r.available).map(|r| r.cid)),
                Err(e) => {
                    tracing::warn!(job = self.lane.job, error = %e, "file probe unanswered; sending the batch's files")
                }
            }
        }
        Ok(held)
    }

    /// Records the outcome; `attempts` fences a worker whose lease ran out
    /// and whose row another claim took. Answers the state written.
    async fn finish(&self, row: &Claimed, outcome: &Outcome) -> sqlx::Result<&'static str> {
        let now = now();
        let (state, next, error, upstream): (
            &'static str,
            NaiveDateTime,
            Option<String>,
            Option<u16>,
        ) = match outcome {
            Outcome::Accepted { status } => ("accepted", now, None, Some(*status)),
            Outcome::Rejected { status, body } => {
                ("rejected", now, Some(body.clone()), Some(*status))
            }
            Outcome::Retry { status, error } if row.attempts >= self.retry.max_attempts => {
                ("failed", now, Some(error.clone()), *status)
            }
            Outcome::Retry { status, error } => (
                "queued",
                now + chrono::Duration::from_std(self.retry.delay(row.attempts))
                    .unwrap_or_default(),
                Some(error.clone()),
                *status,
            ),
            Outcome::Failed(error) => ("failed", now, Some(error.clone()), None),
            Outcome::Superseded => (
                "superseded",
                now,
                Some("no longer this node's local deployment; not sent".into()),
                None,
            ),
        };
        let sql = format!(
            "UPDATE {} SET status = $3, next_attempt_at = $4, last_error = $5, upstream_status = $6, updated_at = $7 \
             WHERE id = $1 AND status = 'sending' AND attempts = $2",
            self.lane.table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(row.id)
            .bind(row.attempts)
            .bind(state)
            .bind(next)
            .bind(error.map(capped))
            .bind(upstream.map(|s| s as i16))
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(state)
    }

    /// Gives a claim back unsent: `queued`, due now, the attempt uncounted
    /// (the upstream's answer, if any, never arrived). Fenced as
    /// [`Forwarder::finish`] is.
    async fn release(&self, row: &Claimed) -> sqlx::Result<()> {
        let sql = format!(
            "UPDATE {} SET status = 'queued', attempts = attempts - 1, next_attempt_at = $3, updated_at = $3 \
             WHERE id = $1 AND status = 'sending' AND attempts = $2",
            self.lane.table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(row.id)
            .bind(row.attempts)
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Ends as `failed` the open rows queued for another target (the URL
    /// was changed, or a process configured with another one queued them)
    /// once no upstream takes their entity: created longer ago than the
    /// lane's whole window, since a deploy refuses a timestamp more than
    /// `max_ahead_ms` past its clock. No worker of this target sends them;
    /// closed, the retention sweep reclaims them. A newer row, or one
    /// another worker holds, is left to its own target. Answers the rows.
    pub async fn close_foreign(&self, cancel: &CancellationToken) -> sqlx::Result<u64> {
        let now = now();
        let created_before =
            now - chrono::Duration::milliseconds(self.lane.max_age_ms + self.lane.max_ahead_ms);
        let sql = format!(
            "UPDATE {t} SET status = 'failed', last_error = $4, updated_at = $1 \
             WHERE id IN (SELECT id FROM {t} \
                          WHERE status IN ('queued', 'sending') AND next_attempt_at <= $1 \
                          AND target <> $2 AND created_at < $3 \
                          LIMIT $5 FOR UPDATE SKIP LOCKED)",
            t = self.lane.table
        );
        let (sql, pool, target) = (sql.as_str(), &self.pool, self.target.as_str());
        in_batches(cancel, move || {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(now)
                .bind(target)
                .bind(created_before)
                .bind(FOREIGN_ERROR)
                .bind(SWEEP_BATCH)
                .execute(pool)
        })
        .await
    }

    /// Deletes terminal rows last updated before `cutoff`, [`in_batches`].
    /// Answers the rows deleted.
    pub async fn sweep(
        &self,
        cutoff: NaiveDateTime,
        cancel: &CancellationToken,
    ) -> sqlx::Result<u64> {
        let sql = format!(
            "DELETE FROM {t} WHERE id IN (SELECT id FROM {t} \
             WHERE status NOT IN ('queued', 'sending') AND updated_at < $1 LIMIT $2)",
            t = self.lane.table
        );
        let (sql, pool) = (sql.as_str(), &self.pool);
        in_batches(cancel, move || {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(cutoff)
                .bind(SWEEP_BATCH)
                .execute(pool)
        })
        .await
    }

    /// Queues one record again for this lane's current target, due now, its
    /// attempts reset: the operator's recovery after a refusal that was the
    /// upstream's configuration rather than a verdict on the deployment, after
    /// the attempts ran out, or for a row queued for a target since replaced.
    /// Only `queued`, `rejected` and `failed` records move, and only while
    /// the entity is inside the upstream's window ([`Lane::too_old`]); the
    /// claim still sends only a deployment that is this node's own.
    pub async fn requeue(&self, entity_id: &str) -> sqlx::Result<Requeued> {
        let Some(status) = self.status(entity_id).await? else {
            return Ok(Requeued::Unknown);
        };
        if !matches!(status.as_str(), "queued" | "rejected" | "failed") {
            return Ok(Requeued::Kept(status));
        }
        // An entity the store lacks or cannot parse is queued, and its claim
        // ends it `failed` with that reason.
        if let Ok(Some(entity)) = self.storage.retrieve(entity_id).await
            && let Ok(entity) = serde_json::from_slice::<Value>(&entity)
            && let Some(reason) = self.lane.too_old(&entity, now_ms())
        {
            return Ok(Requeued::TooOld(reason));
        }
        let now = now();
        let sql = format!(
            "UPDATE {t} SET status = 'queued', attempts = 0, next_attempt_at = $2, target = $3, updated_at = $2 \
             WHERE entity_id = $1 AND status IN ('queued', 'rejected', 'failed') RETURNING {cols}",
            t = self.lane.table,
            cols = status::status_columns(&self.lane)
        );
        let row: Option<status::StatusRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(entity_id)
            .bind(now)
            .bind(&self.target)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => {
                self.kick();
                Ok(Requeued::Queued(row.to_json()))
            }
            // A worker claimed it in between.
            None => Ok(self
                .status(entity_id)
                .await?
                .map_or(Requeued::Unknown, Requeued::Kept)),
        }
    }

    async fn status(&self, entity_id: &str) -> sqlx::Result<Option<String>> {
        let sql = format!(
            "SELECT status FROM {} WHERE entity_id = $1",
            self.lane.table
        );
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(entity_id)
            .fetch_optional(&self.pool)
            .await
    }
}

/// Why [`Forwarder::close_foreign`] closed a row.
const FOREIGN_ERROR: &str =
    "queued for another upstream and now older than any upstream accepts; not sent";

/// Runs `statement`, one batch of at most [`SWEEP_BATCH`] rows, until a
/// batch comes back short, `cancel` fires or [`SWEEP_MAX_BATCHES`] ran.
/// Answers the rows affected.
async fn in_batches<F, Fut>(cancel: &CancellationToken, mut statement: F) -> sqlx::Result<u64>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = sqlx::Result<sqlx::postgres::PgQueryResult>>,
{
    let mut total = 0;
    for _ in 0..SWEEP_MAX_BATCHES {
        let batch = statement().await?.rows_affected();
        total += batch;
        if batch < SWEEP_BATCH as u64 || cancel.is_cancelled() {
            break;
        }
    }
    Ok(total)
}

/// What [`Forwarder::requeue`] did.
#[derive(Debug)]
pub enum Requeued {
    /// The record as it is now: queued for the current target, due.
    Queued(Value),
    /// No record for this entity.
    Unknown,
    /// The record is in this state, which a requeue leaves alone.
    Kept(String),
    /// The entity is past the upstream's window: no send can succeed.
    TooOld(String),
}

impl Requeued {
    /// The refusal of a kept record, for both lanes' routes.
    pub fn kept_message(status: &str) -> String {
        format!("The record is {status}; only queued, rejected or failed records are requeued")
    }
}

/// The base URL a lane forwards to: an origin with no path, query or
/// fragment, since the lane appends its own (`/content/entities`,
/// `/entities`); a catalyst's `/content` URL would post to
/// `/content/content/entities` and every deploy would be refused. The error
/// names the variable, never the value (it may carry credentials).
pub fn check_target(var: &str, url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url.trim()).map_err(|_| format!("{var} is not a URL"))?;
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(format!(
            "{var} must be the upstream's base URL with no path, query or fragment \
             (e.g. https://peer.example): forwarding appends the deploy route itself"
        ));
    }
    Ok(())
}

fn now() -> NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A status that says the upstream is busy or down, not what it thinks of
/// the request: the row (or its file probe) is tried again.
fn transient(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500..=599)
}

/// The deploy POST's answer. 404 and 405 retry too: neither upstream answers
/// them about an entity, only when the route is missing (a catalyst in
/// read-only mode does not expose `POST /entities`). A status past 599 is
/// not stored (`upstream_status` is an HTTP status) and retries.
fn verdict(status: u16, body: String) -> Outcome {
    match status {
        200..=299 => Outcome::Accepted { status },
        404 | 405 => Outcome::Retry {
            status: Some(status),
            error: body,
        },
        _ if transient(status) => Outcome::Retry {
            status: Some(status),
            error: body,
        },
        100..=599 => Outcome::Rejected { status, body },
        _ => Outcome::Retry {
            status: None,
            error: format!("status {status}: {body}"),
        },
    }
}

fn capped(mut text: String) -> String {
    text.truncate(text.floor_char_boundary(ERROR_TEXT_CAP));
    text
}

/// `scheme://host[:port]` of a target: what the status routes show (a
/// configured URL may carry credentials).
fn origin(target: &str) -> String {
    url::Url::parse(target)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| "(unparseable)".into())
}

/// One auth-chain link as the form spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub kind: String,
    pub payload: String,
    pub signature: String,
}

/// The stored chain, link by link: `type` and `payload` as stored, a missing
/// or null `signature` as "" (both upstreams require the field as a string;
/// a SIGNER link carries none).
pub(crate) fn chain_links(chain: &Value) -> Result<Vec<Link>, String> {
    let links = chain
        .as_array()
        .filter(|links| !links.is_empty())
        .ok_or("the stored auth chain is not a non-empty array")?;
    links
        .iter()
        .enumerate()
        .map(|(i, link)| {
            let text = |field: &str| match link.get(field) {
                Some(Value::String(s)) => Ok(Some(s.clone())),
                None | Some(Value::Null) => Ok(None),
                Some(_) => Err(format!("auth chain link {i}: {field} is not a string")),
            };
            Ok(Link {
                kind: text("type")?.ok_or_else(|| format!("auth chain link {i} has no type"))?,
                payload: text("payload")?
                    .ok_or_else(|| format!("auth chain link {i} has no payload"))?,
                signature: text("signature")?.unwrap_or_default(),
            })
        })
        .collect()
}

/// The files the entity names, in its order, each once, itself left out.
fn content_hashes(entity: &Value, entity_id: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in entity
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(hash) = item.get("hash").and_then(Value::as_str)
            && hash != entity_id
            && seen.insert(hash)
        {
            out.push(hash.to_string());
        }
    }
    out
}

/// `base?cid=a&cid=b..`, split so no URL reaches catalyst-client's bound.
pub(crate) fn cid_queries(base: &str, hashes: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut url = String::new();
    for hash in hashes {
        let param = format!("cid={}", urlencoding::encode(hash));
        if !url.is_empty() && url.len() + 1 + param.len() >= MAX_URL_LENGTH {
            out.push(std::mem::take(&mut url));
        }
        if url.is_empty() {
            url = format!("{base}?{param}");
        } else {
            url.push('&');
            url.push_str(&param);
        }
    }
    if !url.is_empty() {
        out.push(url);
    }
    out
}

fn boundary() -> String {
    format!("----catalyst-forward-{}", uuid::Uuid::new_v4().simple())
}

enum Chunk {
    Ready(Bytes),
    /// A blob read from the store while the body streams.
    Stored {
        hash: String,
        size: u64,
    },
}

/// The multipart body of a forwarded deploy, with its exact length.
pub(crate) struct Form {
    boundary: String,
    chunks: Vec<Chunk>,
}

impl Form {
    /// The form catalyst-client's `buildEntityFormDataForDeployment` builds:
    /// `entityId`, each link's `type`, `payload` and `signature`, then the
    /// files, the entity first, each a part named by its hash with the hash
    /// as its file name.
    pub(crate) fn new(
        boundary: String,
        entity_id: &str,
        chain: &[Link],
        entity: Bytes,
        stored: &[(String, u64)],
    ) -> Self {
        let mut form = Self {
            boundary,
            chunks: Vec::new(),
        };
        form.text("entityId", entity_id);
        for (i, link) in chain.iter().enumerate() {
            form.text(&format!("authChain[{i}][type]"), &link.kind);
            form.text(&format!("authChain[{i}][payload]"), &link.payload);
            form.text(&format!("authChain[{i}][signature]"), &link.signature);
        }
        form.file_head(entity_id);
        form.chunks.push(Chunk::Ready(entity));
        form.ready("\r\n".to_string());
        for (hash, size) in stored {
            form.file_head(hash);
            form.chunks.push(Chunk::Stored {
                hash: hash.clone(),
                size: *size,
            });
            form.ready("\r\n".to_string());
        }
        let end = format!("--{}--\r\n", form.boundary);
        form.ready(end);
        form
    }

    fn ready(&mut self, text: String) {
        self.chunks.push(Chunk::Ready(Bytes::from(text)));
    }

    fn text(&mut self, name: &str, value: &str) {
        let part = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n",
            self.boundary
        );
        self.ready(part);
    }

    fn file_head(&mut self, hash: &str) {
        let head = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"{hash}\"; filename=\"{hash}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            self.boundary
        );
        self.ready(head);
    }

    pub(crate) fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    pub(crate) fn content_length(&self) -> u64 {
        self.chunks
            .iter()
            .map(|chunk| match chunk {
                Chunk::Ready(bytes) => bytes.len() as u64,
                Chunk::Stored { size, .. } => *size,
            })
            .sum()
    }

    /// The body, each stored blob opened when the stream reaches it; a blob
    /// whose size moved fails the stream (and the send) rather than the length.
    pub(crate) fn into_stream(
        self,
        storage: Arc<Storage>,
    ) -> BoxStream<'static, std::io::Result<Bytes>> {
        let pieces = self.chunks.into_iter().map(move |chunk| match chunk {
            Chunk::Ready(bytes) => futures::stream::once(async move { Ok(bytes) }).boxed(),
            Chunk::Stored { hash, size } => stored_blob(storage.clone(), hash, size),
        });
        futures::stream::iter(pieces).flatten().boxed()
    }
}

fn stored_blob(
    storage: Arc<Storage>,
    hash: String,
    size: u64,
) -> BoxStream<'static, std::io::Result<Bytes>> {
    futures::stream::once(async move {
        match storage.open(&hash).await {
            Ok(Some(file)) if file.size == size => Ok(file
                .into_body(Bytes::new())
                .into_data_stream()
                .map_err(std::io::Error::other)),
            Ok(Some(file)) => Err(std::io::Error::other(format!(
                "{hash} is {} bytes, {size} were announced",
                file.size
            ))),
            Ok(None) => Err(std::io::Error::other(format!(
                "{hash} left the blob store mid-send"
            ))),
            Err(e) => Err(std::io::Error::other(format!("opening {hash}: {e}"))),
        }
    })
    .try_flatten()
    .boxed()
}

mod status;
pub use status::*;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod tests;
