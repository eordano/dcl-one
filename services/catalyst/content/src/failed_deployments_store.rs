//! Persistence and due-order reads for failed content deployments.
//! Publication removes records in its existing transaction; retry scheduling stays in sync.

use crate::sync_types::{FailedDeployment, FailureReason, SyncError};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

fn reason_label(reason: &FailureReason) -> &'static str {
    match reason {
        FailureReason::DeploymentError => "Deployment error",
        FailureReason::NoEntity => "No entity",
        FailureReason::ValidationUndecided => VALIDATION_UNDECIDED,
        FailureReason::Rejected => DEPLOYMENT_REJECTED,
    }
}

fn reason_from_label(label: &str) -> FailureReason {
    match label {
        "No entity" => FailureReason::NoEntity,
        VALIDATION_UNDECIDED => FailureReason::ValidationUndecided,
        DEPLOYMENT_REJECTED => FailureReason::Rejected,
        _ => FailureReason::DeploymentError,
    }
}

/// The `reason` labels the retry worker branches on (the column is free text).
pub const VALIDATION_UNDECIDED: &str = "Validation undecided";
pub const DEPLOYMENT_REJECTED: &str = "Deployment rejected";

/// The columns every read of `content.failed_deployments` maps into a [`FailedDeployment`],
/// plus the raw deadline (`due_at`) that keys the due order: the epoch-millisecond projection
/// truncates, so a keyset cursor built from it could sit one millisecond before its row and
/// re-read it forever. The projection aliases `next_retry_at`, so an ORDER BY on that name binds
/// to the alias; the due queries qualify the column.
macro_rules! failed_columns {
    () => {
        "entity_id, entity_type, reason, auth_chain, error_description, \
         COALESCE(snapshot_hash, '') AS snapshot_hash, retry_count, \
         date_part('epoch', next_retry_at) * 1000 AS next_retry_at, \
         date_part('epoch', failure_time) * 1000 AS failure_timestamp, \
         next_retry_at AS due_at"
    };
}

/// A record the worker retries: never a refusal, and under the cap unless it is undecided.
macro_rules! retryable {
    () => {
        "(retry_count < $2 OR reason = 'Validation undecided') AND reason <> 'Deployment rejected'"
    };
}

/// A record the cap may evict: neither undecided nor a recorded refusal.
macro_rules! evictable {
    () => {
        "reason NOT IN ('Validation undecided', 'Deployment rejected')"
    };
}

/// A record the worker may act on: its auth chain is a non-empty JSON array. The CASE keeps
/// `json_array_length` off any non-array value, whatever order the planner evaluates the quals in.
macro_rules! has_auth_chain {
    () => {
        "CASE WHEN json_typeof(auth_chain) = 'array' THEN json_array_length(auth_chain) > 0 ELSE false END"
    };
}

#[derive(sqlx::FromRow)]
struct FailedRow {
    entity_id: String,
    entity_type: String,
    reason: String,
    auth_chain: Value,
    error_description: String,
    snapshot_hash: String,
    retry_count: i32,
    next_retry_at: f64,
    failure_timestamp: f64,
    due_at: chrono::NaiveDateTime,
}

impl From<FailedRow> for FailedDeployment {
    fn from(r: FailedRow) -> Self {
        FailedDeployment {
            entity_id: r.entity_id,
            entity_type: r.entity_type,
            reason: reason_from_label(&r.reason),
            auth_chain: serde_json::from_value(r.auth_chain).unwrap_or_default(),
            error_description: r.error_description,
            failure_timestamp: r.failure_timestamp as i64,
            snapshot_hash: if r.snapshot_hash.is_empty() {
                None
            } else {
                Some(r.snapshot_hash)
            },
            retry_count: r.retry_count.max(0) as u32,
            next_retry_at: r.next_retry_at as i64,
        }
    }
}

fn naive_utc(epoch_ms: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .naive_utc()
}

/// Position in the due order `(next_retry_at, entity_id)`: the key of the last record a
/// [`LiveFailedDeploymentsStore::select_due`] page returned, handed back to fetch the page after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueCursor {
    pub next_retry_at: chrono::NaiveDateTime,
    pub entity_id: String,
}

/// One page of due records and the cursor that continues after it (`None` for an empty page).
#[derive(Debug, Default)]
pub struct DueBatch {
    pub failures: Vec<FailedDeployment>,
    pub cursor: Option<DueCursor>,
}

/// A record the give-up pass dropped, as the worker logs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictedRecord {
    pub entity_id: String,
    pub entity_type: String,
    pub retry_count: u32,
}

pub struct LiveFailedDeploymentsStore {
    pool: PgPool,
}

impl LiveFailedDeploymentsStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// GREATEST-monotonic on the retry columns: the sync path reports a failure with a fresh
    /// entity's zeroed retry state every time an entity reappears in a snapshot, and that must
    /// never rewind a backoff deadline or attempt count the retry worker already advanced. An
    /// undecided record re-reported undecided keeps its first failure time, which its backoff
    /// and give-up are measured from.
    pub async fn report_failure(&self, failure: FailedDeployment) -> Result<(), SyncError> {
        let reason = reason_label(&failure.reason);
        sqlx::query(
            r#"INSERT INTO content.failed_deployments (entity_id, entity_type, failure_time, reason, auth_chain, error_description, snapshot_hash, retry_count, next_retry_at)
               VALUES ($1, $2, to_timestamp($9::double precision / 1000.0), $3, $4::json, $5, $6, $7, to_timestamp($8::double precision / 1000.0))
               ON CONFLICT (entity_id) DO UPDATE
               SET failure_time = CASE
                       WHEN failed_deployments.reason = 'Validation undecided' AND $3 = 'Validation undecided'
                       THEN LEAST(failed_deployments.failure_time, EXCLUDED.failure_time)
                       ELSE EXCLUDED.failure_time END,
                   reason = $3, error_description = $5,
                   retry_count = GREATEST(failed_deployments.retry_count, EXCLUDED.retry_count),
                   next_retry_at = GREATEST(failed_deployments.next_retry_at, EXCLUDED.next_retry_at)"#,
        )
        .bind(&failure.entity_id)
        .bind(&failure.entity_type)
        .bind(reason)
        .bind(serde_json::to_value(&failure.auth_chain).unwrap_or_else(|_| Value::Array(Vec::new())))
        .bind(&failure.error_description)
        .bind(failure.snapshot_hash.as_deref().unwrap_or(""))
        .bind(failure.retry_count as i32)
        .bind(failure.next_retry_at as f64)
        .bind(failure.failure_timestamp as f64)
        .execute(&self.pool)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Every record with its payload, in no particular order. Diagnostics and tests only: the
    /// retry worker reads its due batches through [`Self::select_due`] instead of loading the
    /// whole table.
    pub async fn get_all_failed(&self) -> Result<Vec<FailedDeployment>, SyncError> {
        let rows: Vec<FailedRow> = sqlx::query_as(concat!(
            "SELECT ",
            failed_columns!(),
            " FROM content.failed_deployments"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(FailedDeployment::from).collect())
    }

    /// The next `limit` records that are due at `now_ms` and still under the retry cap (an
    /// undecided record always is; a refusal never is due), in the
    /// due order `(next_retry_at, entity_id)`, continuing after `after` when the caller is paging
    /// through one cycle's due set. Records whose auth chain is an empty array are left out in
    /// SQL: they can never be retried, so they must not take a slot of the batch cycle after
    /// cycle. `failed_deployments_due_idx` serves the range and the order.
    pub async fn select_due(
        &self,
        now_ms: i64,
        max_retries: u32,
        after: Option<&DueCursor>,
        limit: usize,
    ) -> Result<DueBatch, SyncError> {
        const FIRST: &str = concat!(
            "SELECT ",
            failed_columns!(),
            " FROM content.failed_deployments
             WHERE next_retry_at <= $1 AND ",
            retryable!(),
            " AND ",
            has_auth_chain!(),
            " ORDER BY failed_deployments.next_retry_at, failed_deployments.entity_id
             LIMIT $3"
        );
        const NEXT: &str = concat!(
            "SELECT ",
            failed_columns!(),
            " FROM content.failed_deployments
             WHERE next_retry_at <= $1 AND ",
            retryable!(),
            " AND ",
            has_auth_chain!(),
            " AND (next_retry_at, entity_id) > ($4, $5)
             ORDER BY failed_deployments.next_retry_at, failed_deployments.entity_id
             LIMIT $3"
        );
        let query = match after {
            None => sqlx::query_as::<_, FailedRow>(FIRST),
            Some(_) => sqlx::query_as::<_, FailedRow>(NEXT),
        }
        .bind(naive_utc(now_ms))
        .bind(max_retries.min(i32::MAX as u32) as i32)
        .bind(limit.min(i64::MAX as usize) as i64);
        let query = match after {
            None => query,
            Some(cursor) => query.bind(cursor.next_retry_at).bind(&cursor.entity_id),
        };
        let rows: Vec<FailedRow> = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        let cursor = rows.last().map(|r| DueCursor {
            next_retry_at: r.due_at,
            entity_id: r.entity_id.clone(),
        });
        Ok(DueBatch {
            failures: rows.into_iter().map(FailedDeployment::from).collect(),
            cursor,
        })
    }

    /// Drops up to `limit` records that are due at `now_ms` and at or over the retry cap, oldest
    /// deadline first, answering which ones went; undecided records and refusals stay. A record at the cap is shed once its deadline
    /// passes, so the give-up walks the same due prefix of `failed_deployments_due_idx` as
    /// [`Self::select_due`] instead of scanning the whole table for the cap. Records without an
    /// auth chain are never evicted here (nor retried); the TTL prune takes them. The outer
    /// `retry_count >= $2` re-check keeps the [`Self::remove_exhausted`] guard against a row a
    /// successful deployment cleared and a fresh failure re-created under the cap.
    pub async fn remove_exhausted_due(
        &self,
        now_ms: i64,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<EvictedRecord>, SyncError> {
        let rows: Vec<(String, String, i32)> = sqlx::query_as(concat!(
            "DELETE FROM content.failed_deployments
             WHERE entity_id IN (
                 SELECT entity_id FROM content.failed_deployments
                 WHERE next_retry_at <= $1 AND retry_count >= $2 AND ",
            evictable!(),
            " AND ",
            has_auth_chain!(),
            "    ORDER BY next_retry_at, entity_id
                 LIMIT $3)
               AND retry_count >= $2 AND ",
            evictable!(),
            "
             RETURNING entity_id, entity_type, retry_count"
        ))
        .bind(naive_utc(now_ms))
        .bind(max_retries.min(i32::MAX as u32) as i32)
        .bind(limit.min(i64::MAX as usize) as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(entity_id, entity_type, retry_count)| EvictedRecord {
                entity_id,
                entity_type,
                retry_count: retry_count.max(0) as u32,
            })
            .collect())
    }

    /// Records waiting for a verdict.
    pub async fn count_undecided(&self) -> Result<u64, SyncError> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM content.failed_deployments WHERE reason = 'Validation undecided'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
        Ok(count.max(0) as u64)
    }

    /// Records whose deadline is still ahead of `now_ms`: the backlog gauge of the cycle report.
    /// An index-only range over `failed_deployments_due_idx`; it counts at-cap rows still inside
    /// their last backoff too, since the cap lives in the heap.
    pub async fn count_waiting(&self, now_ms: i64) -> Result<u64, SyncError> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM content.failed_deployments WHERE next_retry_at > $1",
        )
        .bind(naive_utc(now_ms))
        .fetch_one(&self.pool)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
        Ok(count.max(0) as u64)
    }

    /// Drops the entries that have burned through the retry cap, answering which ones actually
    /// went. The `retry_count >= $2` guard is the point of the call: a give-up decision taken
    /// against an earlier read must not delete a row that a successful deployment cleared and a
    /// fresh failure has since re-created with a lower count. Chunked so one give-up pass over a
    /// large backlog never grows into a single unbounded statement; a chunk that fails leaves its
    /// entries in place for the next cycle.
    pub async fn remove_exhausted(
        &self,
        entity_ids: &[String],
        min_retry_count: u32,
    ) -> Result<Vec<String>, SyncError> {
        let mut removed = Vec::new();
        for chunk in entity_ids.chunks(EXHAUSTED_DELETE_BATCH_SIZE) {
            let rows: Vec<(String,)> = sqlx::query_as(
                concat!(
                    "DELETE FROM content.failed_deployments WHERE entity_id = ANY($1::text[]) AND retry_count >= $2 AND ",
                    evictable!(),
                    " RETURNING entity_id"
                ),
            )
            .bind(chunk)
            .bind(min_retry_count as i32)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SyncError::Storage(e.to_string()))?;
            removed.extend(rows.into_iter().map(|r| r.0));
        }
        Ok(removed)
    }
}

/// Entity ids per give-up DELETE. `ANY($1)` binds the whole chunk as one parameter, so this is
/// not about the bind-parameter ceiling: it bounds each statement's payload, lock footprint and
/// duration.
const EXHAUSTED_DELETE_BATCH_SIZE: usize = 1000;

/// Remove published entities' failures in the publication transaction.
pub(crate) async fn remove_published(
    tx: &mut PgConnection,
    entity_ids: &[String],
) -> Result<(), SyncError> {
    sqlx::query("DELETE FROM content.failed_deployments WHERE entity_id = ANY($1)")
        .bind(entity_ids)
        .execute(tx)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
    Ok(())
}

/// One pass of the old daily prune: records whose first failure is older than `ttl_days`.
/// Undecided records stay: the retry worker gives up on them after [`crate::sync::retry_failed::UNDECIDED_GIVE_UP_MS`].
pub async fn prune_failed_deployments(
    pool: &sqlx::PgPool,
    ttl_days: i64,
) -> Result<u64, SyncError> {
    let result = sqlx::query(
        "DELETE FROM content.failed_deployments
         WHERE failure_time < timezone('UTC', now()) - make_interval(days => $1)
           AND reason <> 'Validation undecided'",
    )
    .bind(ttl_days as i32)
    .execute(pool)
    .await
    .map_err(|e| SyncError::Storage(e.to_string()))?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failure_reason_survives_the_column_round_trip() {
        for reason in [
            FailureReason::DeploymentError,
            FailureReason::NoEntity,
            FailureReason::ValidationUndecided,
            FailureReason::Rejected,
        ] {
            let label = reason_label(&reason);
            assert_eq!(reason_from_label(label), reason);
        }
        assert_eq!(reason_label(&FailureReason::NoEntity), "No entity");
        assert_eq!(
            reason_label(&FailureReason::DeploymentError),
            "Deployment error"
        );
    }
}
