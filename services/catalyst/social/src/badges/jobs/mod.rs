//! The badge processor: upstream's event consumer as leased periodic jobs
//! over the tables that already hold its inputs, each feeding
//! [`super::evaluators`] through [`super::apply_progress`].
//!
//! - `badges-client-events`: `social.client_events` by `seq` (citizen,
//!   traveler, walkabout, moves master, social butterfly, vertical voyager);
//! - `badges-deployments`: `content.deployments` by `id` (the rarity badges,
//!   profile pro, land architect, the store half of open for business);
//! - `badges-sales`: the Polygon shop logs by chain position (emotionista,
//!   fashionista);
//! - `badges-recount`: the creators' items, the creators' collections (the
//!   other half of open for business) and the accepted referrals, recounted
//!   against the stored progress.
//!
//! Each source keeps one row of `social.badge_cursors`, locked and advanced
//! in the transaction that writes the progress of the rows it passed, so a
//! database error rolls both back and the next tick retries the same rows; a
//! row an evaluator cannot read (a malformed event) is logged and passed. A
//! cursor's first pass starts at the source's head (no history replay), and a
//! recount's first pass writes no notification.
//!
//! The id cursors ([`read_ids`]) never skip a committed row. A sequence value
//! is drawn before its transaction commits, so ids become visible out of
//! order: a younger transaction can hold a lower id than rows already
//! committed. A cursor therefore passes every visible row past it and
//! remembers the ids it passed without seeing (`social.badge_cursor_gaps`);
//! each batch re-reads them, processes the rows that appeared and drops a
//! gap only once no transaction can still commit it (see [`Gap`]).

mod client_events;
mod deployments;
mod recount;
mod sales;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDateTime;
use sqlx::{PgConnection, PgPool};

use super::SharedState;
use super::progress::{Applied, ApplyOptions, Progress, apply_progress_granting};
use crate::identity::Address;

pub use client_events::SOURCE as CLIENT_EVENTS_SOURCE;
pub use deployments::SOURCE as DEPLOYMENTS_SOURCE;

/// The processor's knobs (`BADGES_PROCESSOR_*`, `BADGES_EVENT_MAX_AGE_SECS`,
/// `BADGES_DEPLOYMENT_MAX_LAG_SECS`).
#[derive(Clone, Debug)]
pub struct ProcessorSettings {
    /// Run the jobs (on the primary; they are leased).
    pub enabled: bool,
    /// Rows (or recounted wallets) per transaction.
    pub batch_size: i64,
    /// Upstream's `EVENT_PROCESSING_MAX_AGE_IN_SECONDS`: an event older than
    /// this (its own time) is passed without effect. `None` keeps every one.
    pub max_event_age: Option<Duration>,
    /// A deployment the node stored this long after its entity timestamp is
    /// history a syncing node caught up on, not a new deployment: passed
    /// without effect. `None` keeps every one.
    pub deployment_max_lag: Option<Duration>,
    /// How long an id cursor keeps a gap at least (see [`Gap`]).
    pub gap_grace: Duration,
    /// The marketplace items mirror runs (`MARKETPLACE_MIRRORS_ENABLED`).
    /// Without it the sales job would hold at the first sale, so it is not
    /// started.
    pub items_mirror_enabled: bool,
}

impl Default for ProcessorSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            batch_size: 200,
            max_event_age: None,
            deployment_max_lag: None,
            gap_grace: Duration::from_secs(600),
            items_mirror_enabled: false,
        }
    }
}

/// What one pass needs: the pool, the notification asset base, the knobs.
#[derive(Clone)]
pub(crate) struct Pass {
    pub pool: PgPool,
    pub asset_base: String,
    pub settings: ProcessorSettings,
    /// The snapshot `xmin` gap expiry compares with; `None` reads the
    /// database's (tests pin it: the cluster's is shared).
    pub pinned_xmin: Option<i64>,
}

impl Pass {
    pub fn new(state: &SharedState) -> Self {
        Self {
            pool: state.badges.pool().clone(),
            asset_base: state.badges.asset_base_url().to_string(),
            settings: state.settings.processor.clone(),
            pinned_xmin: None,
        }
    }

    fn batch(&self) -> i64 {
        self.settings.batch_size.clamp(1, 10_000)
    }

    /// `true` when an event at `at` is past the age window.
    fn too_old(&self, at: NaiveDateTime, now: NaiveDateTime) -> bool {
        self.settings
            .max_event_age
            .is_some_and(|max| chrono::Duration::from_std(max).is_ok_and(|max| at < now - max))
    }
}

/// Batches one tick drains at most per source before yielding.
const MAX_BATCHES_PER_TICK: usize = 50;

/// Runs `batch` (one transaction: rows passed, a full batch was passed) until
/// it reports no more rows or the tick's budget is spent; returns the rows
/// passed. An error stops the drain; the batches before it stay committed.
async fn drain<F, Fut>(mut batch: F) -> Result<u64, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(u64, bool), sqlx::Error>>,
{
    let mut passed = 0;
    for _ in 0..MAX_BATCHES_PER_TICK {
        let (n, more) = batch().await?;
        passed += n;
        if !more {
            break;
        }
    }
    Ok(passed)
}

/// Gap ranges an id cursor keeps at most; at the cap it passes no row that
/// would open another until gaps resolve (it holds, never skips).
const MAX_GAPS: usize = 5_000;

/// A run of ids `lo..=hi` an id cursor passed without seeing their rows.
///
/// Invariant: every id at or below the cursor's position is either processed
/// or inside a gap. A gap is dropped only when both hold:
///
/// - `horizon <= xmin` of a snapshot taken before the re-read that still
///   misses the ids. `horizon` is the id of the transaction that found the
///   gap, assigned after its scan, so every transaction that had an id at
///   that scan, and so every one that could hold an id of the gap, has a
///   lower one; once a snapshot's `xmin` reaches it, all of them have ended,
///   and a committed row would be visible to the re-read.
/// - `seen_at` is older than [`ProcessorSettings::gap_grace`], and the gap
///   was then `restamped`: its `horizon` replaced by a later batch's id and
///   reached by `xmin` again. A writer draws the value in its INSERT before
///   it takes its transaction id (the row's default runs before the heap
///   insert assigns it), so it had no id at the scan and the first horizon
///   does not cover it. Usually that window is one row's projection, but an
///   `ON CONFLICT` pre-check can wait in it on another open transaction; the
///   second horizon covers that transaction, so the gap outlives it.
///
/// Values that never commit (a rollback, an `ON CONFLICT DO NOTHING` that
/// burned its value) are the gaps this drops.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct Gap {
    pub lo: i64,
    pub hi: i64,
    pub horizon: i64,
    pub seen_at: NaiveDateTime,
    pub restamped: bool,
}

fn grace(pass: &Pass) -> chrono::Duration {
    chrono::Duration::from_std(pass.settings.gap_grace).unwrap_or(chrono::Duration::MAX)
}

/// `gaps` without the ids `found` (ascending), each split around them.
fn subtract(gaps: &[Gap], found: &[i64]) -> Vec<Gap> {
    let mut out = Vec::with_capacity(gaps.len());
    for gap in gaps {
        let mut lo = gap.lo;
        for &id in found.iter().filter(|&&id| id >= gap.lo && id <= gap.hi) {
            if id > lo {
                out.push(Gap {
                    lo,
                    hi: id - 1,
                    ..gap.clone()
                });
            }
            lo = id + 1;
        }
        if lo <= gap.hi {
            out.push(Gap { lo, ..gap.clone() });
        }
    }
    out
}

/// The ids missing between `position` and each of `keys` (ascending, all
/// past `position`), as gap ranges, and how many of `keys` can be passed
/// without holding more than `room` new ranges.
fn missing(position: i64, keys: &[i64], room: usize) -> (Vec<(i64, i64)>, usize) {
    let mut ranges = Vec::new();
    let mut previous = position;
    for (i, &key) in keys.iter().enumerate() {
        if key > previous + 1 {
            if ranges.len() == room {
                return (ranges, i);
            }
            ranges.push((previous + 1, key - 1));
        }
        previous = key;
    }
    (ranges, keys.len())
}

/// A source read by an id cursor.
pub(crate) struct IdSource {
    pub name: &'static str,
    /// `SELECT <columns> FROM <table> <alias>`, without a WHERE.
    pub select: &'static str,
    /// The qualified id column.
    pub key: &'static str,
    /// The source's head, a bigint.
    pub head: &'static str,
}

/// A row read by an id cursor.
pub(crate) trait Keyed {
    fn key(&self) -> i64;
}

/// One batch of an id cursor: the rows to process (re-read gap rows, then
/// the rows past the cursor, ascending) and the state [`IdBatch::finish`]
/// writes.
pub(crate) struct IdBatch<R> {
    pub rows: Vec<R>,
    source: &'static str,
    start: i64,
    position: i64,
    gaps: Vec<Gap>,
    loaded: Vec<Gap>,
    /// Another batch has rows waiting.
    pub more: bool,
}

impl<R> IdBatch<R> {
    /// `true` when nothing changed: the caller rolls back.
    pub fn is_idle(&self) -> bool {
        self.rows.is_empty() && self.gaps == self.loaded && self.position == self.start
    }

    /// Writes the position and the gaps that changed.
    pub async fn finish(&self, conn: &mut PgConnection) -> Result<(), sqlx::Error> {
        advance(conn, self.source, self.position, 0).await?;
        let gone: Vec<i64> = self
            .loaded
            .iter()
            .filter(|g| !self.gaps.contains(g))
            .map(|g| g.lo)
            .collect();
        let added: Vec<&Gap> = self
            .gaps
            .iter()
            .filter(|g| !self.loaded.contains(g))
            .collect();
        if !gone.is_empty() {
            sqlx::query("DELETE FROM social.badge_cursor_gaps WHERE source = $1 AND lo = ANY($2)")
                .bind(self.source)
                .bind(&gone)
                .execute(&mut *conn)
                .await?;
        }
        if !added.is_empty() {
            sqlx::query(
                "INSERT INTO social.badge_cursor_gaps (source, lo, hi, horizon, seen_at, restamped) \
                 SELECT $1, * FROM unnest($2::bigint[], $3::bigint[], $4::bigint[], \
                                          $5::timestamp[], $6::boolean[])",
            )
            .bind(self.source)
            .bind(added.iter().map(|g| g.lo).collect::<Vec<_>>())
            .bind(added.iter().map(|g| g.hi).collect::<Vec<_>>())
            .bind(added.iter().map(|g| g.horizon).collect::<Vec<_>>())
            .bind(added.iter().map(|g| g.seen_at).collect::<Vec<_>>())
            .bind(added.iter().map(|g| g.restamped).collect::<Vec<_>>())
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
}

/// Reads one batch of `source`: the rows that appeared in its gaps, then the
/// visible rows past it, recording the ids it passes without seeing as new
/// gaps, and locks the cursor. `None` when the cursor did not exist: it is
/// created at the source's head (no history replay) and the caller commits.
/// A batch that finds the cursor moved under it (a second runner after a
/// lease expiry) comes back idle.
///
/// The cursor is read first and locked after the scans: the lock gives the
/// transaction its id, so that id is assigned after the scan and is the
/// new gaps' `horizon` (every transaction id assigned before the scan is
/// lower). A snapshot's `xmax` would not do: it is one past the highest
/// *completed* id, and a transaction still running can be above it.
pub(crate) async fn read_ids<R>(
    conn: &mut PgConnection,
    pass: &Pass,
    source: &IdSource,
) -> Result<Option<IdBatch<R>>, sqlx::Error>
where
    R: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Keyed + Send + Unpin,
{
    // Before the re-read: what ended before it is visible to it.
    let xmin: i64 = match pass.pinned_xmin {
        Some(x) => x,
        None => {
            sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text::bigint")
                .fetch_one(&mut *conn)
                .await?
        }
    };
    let read: Option<(i64, NaiveDateTime)> =
        sqlx::query_as("SELECT position, updated_at FROM social.badge_cursors WHERE source = $1")
            .bind(source.name)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((position, stamp)) = read else {
        let head: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(source.head))
            .fetch_one(&mut *conn)
            .await?;
        init_cursor(conn, source.name, head, 0).await?;
        return Ok(None);
    };
    let limit = pass.batch();
    let loaded: Vec<Gap> = sqlx::query_as(
        "SELECT lo, hi, horizon, seen_at, restamped FROM social.badge_cursor_gaps \
         WHERE source = $1 ORDER BY lo",
    )
    .bind(source.name)
    .fetch_all(&mut *conn)
    .await?;
    let mut rows: Vec<R> = Vec::new();
    let mut gaps = loaded.clone();
    let now = crate::shared::now_utc();
    let mut restamp = false;
    if !loaded.is_empty() {
        rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{select} JOIN unnest($1::bigint[], $2::bigint[]) AS g(lo, hi) \
               ON {key} BETWEEN g.lo AND g.hi ORDER BY {key} LIMIT $3",
            select = source.select,
            key = source.key,
        )))
        .bind(loaded.iter().map(|g| g.lo).collect::<Vec<_>>())
        .bind(loaded.iter().map(|g| g.hi).collect::<Vec<_>>())
        .bind(limit)
        .fetch_all(&mut *conn)
        .await?;
        let found: Vec<i64> = rows.iter().map(Keyed::key).collect();
        gaps = subtract(&loaded, &found);
        // Expire only after a complete re-read: a truncated one did not look
        // at every gap.
        if (rows.len() as i64) < limit {
            let grace = grace(pass);
            gaps.retain(|g| !(g.restamped && g.horizon <= xmin && g.seen_at <= now - grace));
            restamp = gaps
                .iter()
                .any(|g| !g.restamped && g.horizon <= xmin && g.seen_at <= now - grace);
        }
    }
    let room = (limit - rows.len() as i64).max(0);
    let mut position_after = position;
    let mut more = rows.len() as i64 == limit;
    let mut new_gaps = Vec::new();
    if room > 0 {
        let mut ahead: Vec<R> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{select} WHERE {key} > $1 ORDER BY {key} LIMIT $2",
            select = source.select,
            key = source.key,
        )))
        .bind(position)
        .bind(room)
        .fetch_all(&mut *conn)
        .await?;
        let full = ahead.len() as i64 == room;
        let keys: Vec<i64> = ahead.iter().map(Keyed::key).collect();
        let (ranges, passable) = missing(position, &keys, MAX_GAPS.saturating_sub(gaps.len()));
        if passable < ahead.len() {
            tracing::warn!(
                source = source.name,
                gaps = gaps.len(),
                "badge cursor at its gap cap: holding until gaps resolve"
            );
            ahead.truncate(passable);
        } else {
            more = more || full;
        }
        new_gaps = ranges;
        if let Some(last) = ahead.last() {
            position_after = last.key();
        }
        rows.extend(ahead);
    }
    let locked: Option<(i64, NaiveDateTime)> = sqlx::query_as(
        "SELECT position, updated_at FROM social.badge_cursors WHERE source = $1 FOR UPDATE",
    )
    .bind(source.name)
    .fetch_optional(&mut *conn)
    .await?;
    if locked != Some((position, stamp)) {
        tracing::warn!(
            source = source.name,
            "badge cursor moved during the batch: retried"
        );
        return Ok(Some(IdBatch {
            rows: Vec::new(),
            source: source.name,
            start: position,
            position,
            gaps: loaded.clone(),
            loaded,
            more: false,
        }));
    }
    if !new_gaps.is_empty() || restamp {
        // Assigned by the lock, after the scan.
        let horizon: i64 = sqlx::query_scalar("SELECT pg_current_xact_id()::text::bigint")
            .fetch_one(&mut *conn)
            .await?;
        // Past the grace once: wait for every transaction open now as well.
        for g in gaps
            .iter_mut()
            .filter(|g| restamp && !g.restamped && g.horizon <= xmin)
            .filter(|g| g.seen_at <= now - grace(pass))
        {
            g.horizon = horizon;
            g.restamped = true;
        }
        gaps.extend(new_gaps.into_iter().map(|(lo, hi)| Gap {
            lo,
            hi,
            horizon,
            seen_at: now,
            restamped: false,
        }));
    }
    Ok(Some(IdBatch {
        rows,
        source: source.name,
        start: position,
        position: position_after,
        gaps,
        loaded,
        more,
    }))
}

/// Publishes how far `source`'s id cursor is behind its head
/// (`badges_processor_cursor_lag`, in ids), its open gaps
/// (`badges_processor_cursor_gaps`) and the oldest one's age
/// (`badges_processor_cursor_oldest_gap_seconds`): a gap that never expires
/// is a transaction held open on the cluster. A failed read only logs.
pub(crate) async fn report_id_lag(pool: &PgPool, source: &IdSource) {
    match id_lag(pool, source).await {
        Ok(Some((lag, gaps, oldest))) => {
            report_lag(source.name, lag);
            metrics::gauge!("badges_processor_cursor_gaps", "source" => source.name)
                .set(gaps as f64);
            metrics::gauge!("badges_processor_cursor_oldest_gap_seconds", "source" => source.name)
                .set(oldest.unwrap_or(0.0).max(0.0));
        }
        Ok(None) => {}
        Err(e) => tracing::debug!(source = source.name, error = %e, "badge cursor lag not read"),
    }
}

/// (ids behind the head, open gaps, the oldest gap's age in seconds) of
/// `source`'s cursor; `None` before its first pass.
pub(crate) async fn id_lag(
    pool: &PgPool,
    source: &IdSource,
) -> Result<Option<(i64, i64, Option<f64>)>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT ({head}) - c.position, count(g.lo), \
                extract(epoch FROM timezone('UTC', now()) - min(g.seen_at))::float8 \
         FROM social.badge_cursors c \
         LEFT JOIN social.badge_cursor_gaps g ON g.source = c.source \
         WHERE c.source = $1 GROUP BY c.position",
        head = source.head
    )))
    .bind(source.name)
    .fetch_optional(pool)
    .await
}

/// `badges_processor_cursor_lag` of `source`, in its own unit (ids, blocks).
pub(crate) fn report_lag(source: &'static str, lag: i64) {
    metrics::gauge!("badges_processor_cursor_lag", "source" => source).set(lag.max(0) as f64);
}

/// The cursor of `source`, locked for this transaction; `None` when the
/// source has none yet (the caller then records the source's head with
/// [`init_cursor`] and ends the pass, so the first pass replays no history).
pub(crate) async fn lock_cursor(
    conn: &mut PgConnection,
    source: &str,
) -> Result<Option<(i64, i32)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT position, log_index FROM social.badge_cursors WHERE source = $1 FOR UPDATE",
    )
    .bind(source)
    .fetch_optional(conn)
    .await
}

pub(crate) async fn init_cursor(
    conn: &mut PgConnection,
    source: &str,
    position: i64,
    log_index: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO social.badge_cursors (source, position, log_index) VALUES ($1, $2, $3) \
         ON CONFLICT (source) DO NOTHING",
    )
    .bind(source)
    .bind(position)
    .bind(log_index)
    .execute(conn)
    .await?;
    tracing::info!(
        source,
        position,
        "badge cursor initialized at the source's head (no history replay)"
    );
    Ok(())
}

pub(crate) async fn advance(
    conn: &mut PgConnection,
    source: &str,
    position: i64,
    log_index: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE social.badge_cursors SET position = $2, log_index = $3, \
         updated_at = timezone('UTC', now()) WHERE source = $1",
    )
    .bind(source)
    .bind(position)
    .bind(log_index)
    .execute(conn)
    .await?;
    Ok(())
}

/// The (address, badge) pairs of `addresses` already complete: an evaluator
/// leaves those untouched, so the batch skips their write path.
pub(crate) async fn completed(
    conn: &mut PgConnection,
    addresses: &[String],
) -> Result<HashSet<(String, String)>, sqlx::Error> {
    if addresses.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT address, badge_id FROM social.user_badge_progress \
         WHERE address = ANY($1) AND completed_at IS NOT NULL",
    )
    .bind(addresses)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().collect())
}

/// One evaluator write, with its notification (`notify`) unless suppressed,
/// achieving the tiers its observer would ([`super::evaluators::tier_grant`]).
pub(crate) async fn award<F>(
    conn: &mut PgConnection,
    address: &Address,
    badge_id: &str,
    notify: Option<&str>,
    update: F,
) -> Result<Applied, sqlx::Error>
where
    F: FnOnce(&mut Progress),
{
    let options = ApplyOptions {
        at: crate::shared::now_utc(),
        notify,
        granted_by: None,
    };
    let grant = super::evaluators::tier_grant(badge_id);
    let applied = apply_progress_granting(conn, address, badge_id, &options, grant, update).await?;
    if let Applied::Written(change) = &applied
        && (change.completed || !change.new_tiers.is_empty())
    {
        tracing::info!(
            address = address.as_str(),
            badge_id,
            tiers = ?change.new_tiers,
            completed = change.completed,
            "badge progress granted"
        );
    }
    Ok(applied)
}

/// Spawns the processor's jobs (leased: only the primary runs them).
pub fn spawn(state: &SharedState, jobs: &crate::jobs::Jobs) {
    if !state.settings.processor.enabled {
        tracing::info!("BADGES_PROCESSOR_ENABLED=false: no badge processor");
        return;
    }
    let pass = Arc::new(Pass::new(state));
    spawn_one(
        jobs,
        "badges-client-events",
        Duration::from_secs(15),
        pass.clone(),
        |p| Box::pin(async move { client_events::run(&p).await }),
    );
    spawn_one(
        jobs,
        "badges-deployments",
        Duration::from_secs(30),
        pass.clone(),
        |p| Box::pin(async move { deployments::run(&p).await }),
    );
    if pass.settings.items_mirror_enabled {
        spawn_one(
            jobs,
            "badges-sales",
            Duration::from_secs(60),
            pass.clone(),
            |p| Box::pin(async move { sales::run(&p).await }),
        );
    } else {
        tracing::warn!(
            "MARKETPLACE_MIRRORS_ENABLED=false: no badges-sales job (a sale waits for its item in the items mirror)"
        );
    }
    spawn_one(
        jobs,
        "badges-recount",
        Duration::from_secs(300),
        pass,
        |p| Box::pin(async move { recount::run(&p).await }),
    );
    tracing::info!("badge processor up");
}

type PassFuture = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<u64>> + Send>>;

fn spawn_one(
    jobs: &crate::jobs::Jobs,
    name: &'static str,
    every: Duration,
    pass: Arc<Pass>,
    run: fn(Arc<Pass>) -> PassFuture,
) {
    jobs.spawn_periodic_leased(
        name,
        crate::jobs::PeriodicCfg::every(every)
            .with_jitter(every / 4)
            .with_error_backoff(Duration::from_secs(600)),
        move |ctx| {
            let pass = pass.clone();
            async move {
                // A restore hold keeps every social write back, these too.
                if crate::shared::writes::refusal(true).is_some() {
                    return Ok(());
                }
                let written = run(pass).await?;
                if written > 0 {
                    ctx.progress(format!("{written} rows passed"));
                }
                Ok(())
            }
        },
    );
}
