//! Durable, bounded streaming revocation and notification work. SQL transactions
//! only claim/commit facts; LiveKit and the offered notification interface run outside them.
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDateTime;
use sqlx::PgPool;
use uuid::Uuid;

use super::super::{CommsState, errors::ApiError};
use crate::Address;
use crate::explore::places::removed::{PlaceContext, RemovedPlaces};
use crate::interfaces::{StreamingNotification, StreamingNotificationKind};
use crate::jobs::{Jobs, PeriodicCfg};

const BATCH: i64 = 20;
const RECONCILE_BATCH: i64 = 20;

fn reconcile_bounds(now: NaiveDateTime) -> (NaiveDateTime, NaiveDateTime) {
    (
        now - chrono::Duration::minutes(2),
        now - chrono::Duration::seconds(60),
    )
}

/// Each workflow has its own fleet lease and full execution budget. Slow owner
/// preparation in enforcement cannot consume ingress cleanup's scheduling slot.
pub fn spawn_jobs(state: &Arc<CommsState>, jobs: &Jobs) {
    for (name, interval, workflow) in [
        ("comms-streaming-ttl", 60, Workflow::Streaming),
        ("comms-streaming-key-expiry", 600, Workflow::Keys),
        ("comms-streaming-cleanup", 60, Workflow::Cleanup),
    ] {
        let state = state.clone();
        jobs.spawn_periodic_leased(
            name,
            PeriodicCfg::every(Duration::from_secs(interval))
                .with_error_backoff(Duration::from_secs(600)),
            move |ctx| {
                let state = state.clone();
                async move {
                    if !state.livekit.configured {
                        return Ok(());
                    }
                    run_budgeted(&ctx.cancel, async {
                        match workflow {
                            Workflow::Streaming => {
                                run_streaming_workflows(expire_streams(&state), reconcile(&state))
                                    .await
                            }
                            Workflow::Keys => expire_keys(&state).await,
                            Workflow::Cleanup => drain_cleanup(&state).await,
                        }
                    })
                    .await?;
                    ctx.progress("bounded streaming lifecycle pass complete");
                    Ok(())
                }
            },
        );
    }
}

#[derive(Clone, Copy)]
enum Workflow {
    Streaming,
    Keys,
    Cleanup,
}

/// Poll both TTL responsibilities together; owner preparation must not postpone
/// stale-state reads. Join also runs reconciliation when enforcement returns an error.
async fn run_streaming_workflows<E, R>(enforcement: E, reconciliation: R) -> Result<(), ApiError>
where
    E: std::future::Future<Output = Result<(), ApiError>>,
    R: std::future::Future<Output = Result<(), ApiError>>,
{
    let (enforcement, reconciliation) = tokio::join!(enforcement, reconciliation);
    enforcement?;
    reconciliation?;
    Ok(())
}

async fn run_budgeted<F>(
    cancel: &tokio_util::sync::CancellationToken,
    work: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = Result<(), ApiError>>,
{
    tokio::select! {
        _=cancel.cancelled()=>Ok(()),
        result=tokio::time::timeout(Duration::from_secs(60),work)=>{
            result.map_err(|_|anyhow::anyhow!("streaming lifecycle pass exceeded 60 seconds"))??;
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CleanupCommand {
    IdleReplacement,
    Reset,
    Revoke,
    KeyExpiry,
    DurationExpiry,
}

impl CleanupCommand {
    fn notification(self) -> Option<StreamingNotificationKind> {
        match self {
            Self::IdleReplacement => None,
            Self::Reset => Some(StreamingNotificationKind::KeyReset),
            Self::Revoke => Some(StreamingNotificationKind::KeyRevoke),
            Self::KeyExpiry => Some(StreamingNotificationKind::KeyExpired),
            Self::DurationExpiry => Some(StreamingNotificationKind::TimeExceeded),
        }
    }

    fn permits_running(self) -> bool {
        self != Self::IdleReplacement
    }
}

#[derive(Clone, Copy)]
enum Expiry {
    Key,
    Duration,
}

impl Expiry {
    fn command(self) -> CleanupCommand {
        match self {
            Self::Key => CleanupCommand::KeyExpiry,
            Self::Duration => CleanupCommand::DurationExpiry,
        }
    }
}

/// Capture the recipient and metadata before a destructive transition, outside SQL locks.
pub(super) async fn enqueue(
    state: &CommsState,
    id: Uuid,
    command: CleanupCommand,
) -> Result<bool, ApiError> {
    let snapshot: Option<(String,i64)> = sqlx::query_as("SELECT place_id,streaming_state_version FROM social.scene_stream_access WHERE id=$1 AND active")
        .bind(id).fetch_optional(&state.pool).await?;
    let Some((place_id, version)) = snapshot else {
        return Ok(false);
    };
    let context = RemovedPlaces::new(state.pool.clone())
        .place(&place_id)
        .await?;
    let event = if let (Some(kind), Some(context)) = (command.notification(), context.as_ref()) {
        let key = format!("{}-{id}", kind.as_str());
        match tokio::time::timeout(
            Duration::from_secs(5),
            super::notifications::prepare_streaming_for_context(
                state,
                context,
                &key,
                kind,
                chrono::Utc::now().naive_utc(),
            ),
        )
        .await
        {
            Ok(Ok(event)) => Some(event),
            failure => {
                tracing::warn!(access_id=%id,error=?failure,"stream notification owner pending; cleanup will proceed");
                None
            }
        }
    } else {
        None
    };
    let context = context
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| ApiError::internal("stream context serialization failed"))?;
    enqueue_prepared(
        &state.pool,
        id,
        command,
        version,
        event.as_ref(),
        context.as_ref(),
    )
    .await
    .map_err(Into::into)
}

fn notification_kind(kind: &str) -> Result<StreamingNotificationKind, ApiError> {
    match kind {
        "streaming_key_reset" => Ok(StreamingNotificationKind::KeyReset),
        "streaming_key_revoke" => Ok(StreamingNotificationKind::KeyRevoke),
        "streaming_key_expired" => Ok(StreamingNotificationKind::KeyExpired),
        "streaming_time_exceeded" => Ok(StreamingNotificationKind::TimeExceeded),
        _ => Err(ApiError::internal("unknown streaming notification kind")),
    }
}

async fn enqueue_prepared(
    pool: &PgPool,
    id: Uuid,
    command: CleanupCommand,
    version: i64,
    event: Option<&StreamingNotification>,
    context: Option<&serde_json::Value>,
) -> Result<bool, sqlx::Error> {
    let kind = command.notification().map(|kind| kind.as_str());
    let force = command.permits_running();
    // The predicate rechecks expiry and the webhook version after the snapshot lookup.
    let changed:Option<Uuid> = sqlx::query_scalar(
        "WITH removed AS (UPDATE social.scene_stream_access SET active=false,streaming=false \
         WHERE id=$1 AND active AND ($3 OR NOT streaming) AND streaming_state_version=$4 \
         AND ($2::text IS DISTINCT FROM 'streaming_key_expired' OR (COALESCE(expiration_time,created_at+interval '4 days')<=timezone('UTC',now()))) \
         AND ($2::text IS DISTINCT FROM 'streaming_time_exceeded' OR (streaming AND streaming_start_time<=timezone('UTC',now())-interval '4 hours')) \
         RETURNING id,place_id,ingress_id) \
         INSERT INTO social.scene_stream_cleanup(access_id,place_id,ingress_id,notification_kind,occurred_at,due_at,recipient,metadata,source_context) \
         SELECT id,place_id,ingress_id,$2,COALESCE($5,timezone('UTC',now())),timezone('UTC',now()),$6,$7,$8 FROM removed \
         ON CONFLICT(access_id) DO NOTHING RETURNING access_id")
        .bind(id).bind(kind).bind(force).bind(version).bind(event.map(|e| e.occurred_at))
        .bind(event.map(|e| &e.recipient)).bind(event.map(|e| &e.metadata)).bind(context).fetch_optional(pool).await?;
    Ok(changed.is_some())
}

async fn expire_streams(state: &CommsState) -> Result<(), ApiError> {
    expire(state, Expiry::Duration).await
}
async fn expire_keys(state: &CommsState) -> Result<(), ApiError> {
    expire(state, Expiry::Key).await
}

async fn expire(state: &CommsState, expiry: Expiry) -> Result<(), ApiError> {
    let query = if matches!(expiry, Expiry::Duration) {
        "SELECT id FROM social.scene_stream_access WHERE active AND streaming AND streaming_start_time<=timezone('UTC',now())-interval '4 hours' AND (cleanup_retry_at IS NULL OR cleanup_retry_at<=timezone('UTC',now())) ORDER BY streaming_start_time,id LIMIT $1"
    } else {
        "SELECT id FROM social.scene_stream_access WHERE active AND COALESCE(expiration_time,created_at+interval '4 days')<=timezone('UTC',now()) AND (cleanup_retry_at IS NULL OR cleanup_retry_at<=timezone('UTC',now())) ORDER BY COALESCE(expiration_time,created_at+interval '4 days'),id LIMIT $1"
    };
    let ids: Vec<Uuid> = sqlx::query_scalar(query)
        .bind(BATCH)
        .fetch_all(&state.pool)
        .await?;
    for id in ids {
        if let Err(error) = enqueue(state, id, expiry.command()).await {
            // Historic rows with no resolvable owner remain explicit retry work; don't
            // deactivate them and lose the only remaining source of notification context.
            sqlx::query("UPDATE social.scene_stream_access SET cleanup_retry_at=timezone('UTC',now())+interval '10 minutes' WHERE id=$1 AND active")
                .bind(id).execute(&state.pool).await?;
            tracing::warn!(access_id=%id,error=%error,"stream lifecycle snapshot failed; retained for retry");
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Cleanup {
    access_id: Uuid,
    place_id: String,
    ingress_id: Option<String>,
    notification_kind: Option<String>,
    occurred_at: NaiveDateTime,
    ingress_deleted: bool,
    recipient: Option<Address>,
    metadata: Option<serde_json::Value>,
    source_context: Option<serde_json::Value>,
}

pub(super) async fn drain_cleanup(state: &CommsState) -> Result<(), ApiError> {
    cleanup(state, None).await
}

pub(super) async fn cleanup_access(state: &CommsState, id: Uuid) -> Result<(), ApiError> {
    cleanup(state, Some(id)).await
}

async fn cleanup(state: &CommsState, target: Option<Uuid>) -> Result<(), ApiError> {
    for _ in 0..BATCH {
        let token = Uuid::new_v4();
        // Five minutes covers one 5s admin call and the local notification lookup/write.
        let row: Option<Cleanup> = sqlx::query_as(
            "UPDATE social.scene_stream_cleanup SET claim_token=$1,claim_until=timezone('UTC',now())+interval '5 minutes' \
             WHERE access_id=(SELECT access_id FROM social.scene_stream_cleanup WHERE due_at<=timezone('UTC',now()) \
             AND (claim_until IS NULL OR claim_until<=timezone('UTC',now())) AND ($2::uuid IS NULL OR access_id=$2) ORDER BY due_at,access_id LIMIT 1 FOR UPDATE SKIP LOCKED) \
             RETURNING access_id,place_id,ingress_id,notification_kind,occurred_at,ingress_deleted,recipient,metadata,source_context")
            .bind(token).bind(target).fetch_optional(&state.pool).await?;
        let Some(row) = row else {
            if let Some(id) = target {
                let pending: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM social.scene_stream_cleanup WHERE access_id=$1)",
                )
                .bind(id)
                .fetch_one(&state.pool)
                .await?;
                if pending {
                    return Err(ApiError::internal("stream cleanup is pending retry"));
                }
            }
            break;
        };
        match deliver(state, &row, token).await {
            Ok(()) => {
                sqlx::query(
                    "DELETE FROM social.scene_stream_cleanup WHERE access_id=$1 AND claim_token=$2",
                )
                .bind(row.access_id)
                .bind(token)
                .execute(&state.pool)
                .await?;
            }
            Err(error) => {
                // No exhaustion discard: retries stay durable, delay is bounded at ten minutes.
                sqlx::query("UPDATE social.scene_stream_cleanup SET attempts=LEAST(attempts+1,30), \
                    due_at=timezone('UTC',now())+make_interval(secs=>LEAST(600,5*power(2,LEAST(attempts,7)))::double precision), \
                    claim_token=NULL,claim_until=NULL WHERE access_id=$1 AND claim_token=$2")
                    .bind(row.access_id).bind(token).execute(&state.pool).await?;
                tracing::warn!(access_id=%row.access_id,error=%error,"stream cleanup retained for retry");
                if target.is_some() {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

async fn deliver(state: &CommsState, row: &Cleanup, token: Uuid) -> Result<(), ApiError> {
    if !row.ingress_deleted {
        if let Some(id) = row.ingress_id.as_deref().filter(|id| !id.is_empty()) {
            tokio::time::timeout(Duration::from_secs(5), state.ingress().delete_ingress(id))
                .await
                .map_err(|_| ApiError::internal("stream cleanup ingress timed out"))?
                .map_err(|e| ApiError::internal(format!("stream cleanup ingress: {e}")))?;
        }
        let changed = sqlx::query("UPDATE social.scene_stream_cleanup SET ingress_deleted=true WHERE access_id=$1 AND claim_token=$2")
            .bind(row.access_id).bind(token).execute(&state.pool).await?;
        if changed.rows_affected() == 0 {
            return Err(ApiError::internal("stream cleanup lease lost"));
        }
    }
    let Some(kind_str) = row.notification_kind.as_deref() else {
        return Ok(());
    };
    let kind = notification_kind(kind_str)?;
    if kind == StreamingNotificationKind::KeyExpired || kind == StreamingNotificationKind::KeyReset
    {
        let replacement: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM social.scene_stream_access WHERE place_id=$1 AND active)",
        )
        .bind(&row.place_id)
        .fetch_one(&state.pool)
        .await?;
        if kind == StreamingNotificationKind::KeyExpired && replacement {
            return Ok(());
        }
        if kind == StreamingNotificationKind::KeyReset && !replacement {
            return Err(ApiError::internal(
                "stream reset awaiting replacement credential",
            ));
        }
    }
    let key = format!("{kind_str}-{}", row.access_id);
    let event = if let (Some(recipient), Some(metadata)) = (&row.recipient, &row.metadata) {
        StreamingNotification {
            event_key: key,
            recipient: recipient.clone(),
            kind,
            metadata: metadata.clone(),
            occurred_at: row.occurred_at,
        }
    } else {
        let resolved = if let Some(context) = row.source_context.as_ref() {
            let context: PlaceContext = serde_json::from_value(context.clone())
                .map_err(|_| ApiError::internal("stream context snapshot invalid"))?;
            tokio::time::timeout(
                Duration::from_secs(5),
                super::notifications::prepare_streaming_for_context(
                    state,
                    &context,
                    &key,
                    kind,
                    row.occurred_at,
                ),
            )
            .await
            .map_err(|_| ApiError::internal("stream notification owner resolution timed out"))??
        } else {
            // Historic row with no catalogue context: retain explicit durable failure
            // after ingress cleanup. Never fabricate a notification recipient.
            tokio::time::timeout(
                Duration::from_secs(5),
                super::notifications::prepare_streaming_for_place(
                    state,
                    &row.place_id,
                    &key,
                    kind,
                    row.occurred_at,
                ),
            )
            .await
            .map_err(|_| ApiError::internal("historic stream owner resolution timed out"))??
        };
        let updated=sqlx::query("UPDATE social.scene_stream_cleanup SET recipient=$3,metadata=$4 WHERE access_id=$1 AND claim_token=$2")
            .bind(row.access_id).bind(token).bind(&resolved.recipient).bind(&resolved.metadata).execute(&state.pool).await?;
        if updated.rows_affected() == 0 {
            return Err(ApiError::internal("stream cleanup lease lost"));
        }
        resolved
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        state.social.notify_streaming_notification(&event),
    )
    .await
    .map_err(|_| ApiError::internal("stream notification delivery timed out"))??;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Snapshot {
    id: Uuid,
    ingress_id: String,
    streaming_start_time: Option<NaiveDateTime>,
    streaming_state_version: i64,
    streaming_checked_at: Option<NaiveDateTime>,
}

impl Snapshot {
    fn eligible(&self, now: NaiveDateTime) -> bool {
        let (start, checked) = reconcile_bounds(now);
        self.streaming_start_time.is_some_and(|at| at <= start)
            && self.streaming_checked_at.is_none_or(|at| at <= checked)
    }
}

async fn reconcile(state: &CommsState) -> Result<(), ApiError> {
    let now = chrono::Utc::now().naive_utc();
    let (started_before, checked_before) = reconcile_bounds(now);
    let rows: Vec<Snapshot> = sqlx::query_as("SELECT id,ingress_id,streaming_start_time,streaming_state_version,streaming_checked_at FROM social.scene_stream_access \
        WHERE active AND streaming AND ingress_id IS NOT NULL AND ingress_id<>'' \
        AND streaming_start_time<=$2 \
        AND (streaming_checked_at IS NULL OR streaming_checked_at<=$3) \
        ORDER BY streaming_checked_at NULLS FIRST,id LIMIT $1")
        .bind(RECONCILE_BATCH).bind(started_before).bind(checked_before).fetch_all(&state.pool).await?;
    // At most 20 reads in groups of five; each group finishes before the next.
    for group in rows.chunks(5) {
        let results = futures::future::join_all(group.iter().map(|row| async {
            if !row.eligible(now) { return Ok(()); }
            let status = state.ingress().is_ingress_streaming(&row.ingress_id).await;
            if matches!(status, Ok(Some(false))) {
                clear_stale(&state.pool,row,now).await?;
            } else if let Err(error) = status {
                tracing::warn!(access_id=%row.id,error=%error,"stream state reconciliation failed; flag preserved");
            }
            // Rotate the bounded read set even on transport errors to avoid starving later rows.
            sqlx::query("UPDATE social.scene_stream_access SET streaming_checked_at=timezone('UTC',now()) WHERE id=$1 AND streaming_state_version=$2")
                .bind(row.id).bind(row.streaming_state_version).execute(&state.pool).await?;
            Ok::<(),sqlx::Error>(())
        })).await;
        for result in results {
            result?;
        }
    }
    Ok(())
}

async fn clear_stale(
    pool: &PgPool,
    row: &Snapshot,
    now: NaiveDateTime,
) -> Result<u64, sqlx::Error> {
    let (started_before, checked_before) = reconcile_bounds(now);
    let result=sqlx::query("UPDATE social.scene_stream_access SET streaming=false,streaming_start_time=NULL \
        WHERE id=$1 AND ingress_id=$2 AND active AND streaming AND streaming_start_time IS NOT DISTINCT FROM $3 AND streaming_state_version=$4 \
        AND streaming_start_time<=$5 AND (streaming_checked_at IS NULL OR streaming_checked_at<=$6)")
        .bind(row.id).bind(&row.ingress_id).bind(row.streaming_start_time).bind(row.streaming_state_version)
        .bind(started_before).bind(checked_before).execute(pool).await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_commands_keep_persisted_labels_and_reject_non_cleanup_kinds() {
        for (command, label) in [
            (CleanupCommand::Reset, "streaming_key_reset"),
            (CleanupCommand::Revoke, "streaming_key_revoke"),
            (CleanupCommand::KeyExpiry, "streaming_key_expired"),
            (CleanupCommand::DurationExpiry, "streaming_time_exceeded"),
        ] {
            assert_eq!(command.notification().unwrap().as_str(), label);
            assert_eq!(
                notification_kind(label).unwrap(),
                command.notification().unwrap()
            );
            assert!(command.permits_running());
        }
        assert!(CleanupCommand::IdleReplacement.notification().is_none());
        assert!(!CleanupCommand::IdleReplacement.permits_running());
        for invalid in [
            "streaming_place_updated",
            "",
            "STREAMING_KEY_RESET",
            "unknown",
        ] {
            assert_eq!(
                notification_kind(invalid).unwrap_err().message,
                "unknown streaming notification kind"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn production_ttl_orchestration_reconciles_during_owner_backlog_and_failure() {
        for fail_immediately in [false, true] {
            let token = tokio_util::sync::CancellationToken::new();
            let reconciled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observed = reconciled.clone();
            let pass = tokio::spawn(async move {
                run_budgeted(
                    &token,
                    run_streaming_workflows(
                        async move {
                            if fail_immediately {
                                return Err(ApiError::internal("owner source failed"));
                            }
                            for _ in 0..20 {
                                tokio::time::sleep(Duration::from_secs(5)).await;
                            }
                            Ok(())
                        },
                        async move {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            observed.store(true, std::sync::atomic::Ordering::SeqCst);
                            Ok(())
                        },
                    ),
                )
                .await
            });
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(2)).await;
            tokio::task::yield_now().await;
            assert!(
                reconciled.load(std::sync::atomic::Ordering::SeqCst),
                "production TTL orchestration must poll reconciliation before owner backlog completes"
            );
            if !fail_immediately {
                assert!(!pass.is_finished());
                tokio::time::advance(Duration::from_secs(59)).await;
            }
            assert!(pass.await.unwrap().is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_budget_progresses_independently_of_repeated_owner_timeouts() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let deletions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..2 {
            let owner_cancel = cancel.clone();
            let enforcement = tokio::spawn(async move {
                run_budgeted(&owner_cancel, async {
                    for _ in 0..20 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    Ok(())
                })
                .await
            });
            let cleanup_cancel = cancel.clone();
            let attempted = deletions.clone();
            let cleanup = tokio::spawn(async move {
                run_budgeted(&cleanup_cancel, async {
                    // A failed due delivery does not consume a different workflow's budget.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    attempted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
            });
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(6)).await;
            assert!(cleanup.await.unwrap().is_ok());
            assert!(
                !enforcement.is_finished(),
                "cleanup ran while owner work remained blocked"
            );
            tokio::time::advance(Duration::from_secs(55)).await;
            assert!(enforcement.await.unwrap().is_err());
        }
        assert_eq!(deletions.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_drops_preparation_and_delivery_and_allows_next_pass() {
        for _stage in ["preparation", "delivery"] {
            let token = tokio_util::sync::CancellationToken::new();
            let child = token.clone();
            let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            struct OnDrop(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for OnDrop {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let guard = OnDrop(stopped.clone());
            let pass = tokio::spawn(async move {
                run_budgeted(&child, async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                    Ok(())
                })
                .await
            });
            tokio::task::yield_now().await;
            token.cancel();
            assert!(pass.await.unwrap().is_ok());
            assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
            let fresh = tokio_util::sync::CancellationToken::new();
            assert!(run_budgeted(&fresh, async { Ok(()) }).await.is_ok());
        }
    }

    #[test]
    fn settling_window_and_check_cadence_protect_new_streams_from_inactive_reads() {
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .naive_utc();
        let mut row = Snapshot {
            id: Uuid::nil(),
            ingress_id: "IN_test".into(),
            streaming_start_time: Some(now),
            streaming_state_version: 0,
            streaming_checked_at: None,
        };
        assert!(!row.eligible(now));
        assert!(!row.eligible(now + chrono::Duration::seconds(119)));
        assert!(row.eligible(now + chrono::Duration::seconds(120)));
        row.streaming_checked_at = Some(now + chrono::Duration::seconds(120));
        assert!(!row.eligible(now + chrono::Duration::seconds(179)));
        assert!(row.eligible(now + chrono::Duration::seconds(180)));
        row.streaming_start_time = None;
        assert!(!row.eligible(now + chrono::Duration::hours(1)));
    }

    #[tokio::test]
    async fn cleanup_intent_and_deactivation_commit_together_and_stream_start_blocks_idle_renewal()
    {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            eprintln!(
                "SKIP: streaming lifecycle PostgreSQL test requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        };
        let id = Uuid::new_v4();
        let place_id = format!("stream-cleanup-test-{id}");
        sqlx::query("INSERT INTO social.scene_stream_access(id,place_id,streaming_url,streaming_key,generated_by,streaming) VALUES($1,$2,'rtmp://example.org','test-key','test',true)")
            .bind(id).bind(&place_id).execute(&pool).await.unwrap();
        let event = StreamingNotification {
            event_key: format!("streaming_key_revoke-{id}"),
            recipient: "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            kind: StreamingNotificationKind::KeyRevoke,
            metadata: serde_json::json!({"title":"snapshot"}),
            occurred_at: chrono::Utc::now().naive_utc(),
        };
        assert!(
            !enqueue_prepared(&pool, id, CleanupCommand::IdleReplacement, 0, None, None)
                .await
                .unwrap()
        );
        let unchanged: (bool, bool, bool) = sqlx::query_as(
            "SELECT active,streaming,EXISTS(SELECT 1 FROM social.scene_stream_cleanup WHERE access_id=$1) FROM social.scene_stream_access WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unchanged, (true, true, false));
        // Revoke intentionally permits a running stream; IdleReplacement is the
        // only non-forced command exposed by the lifecycle API.
        // A start/end webhook after metadata capture invalidates that snapshot.
        assert!(
            !enqueue_prepared(&pool, id, CleanupCommand::Revoke, 1, Some(&event), None)
                .await
                .unwrap()
        );
        assert!(
            enqueue_prepared(&pool, id, CleanupCommand::Revoke, 0, Some(&event), None)
                .await
                .unwrap()
        );
        assert!(
            !enqueue_prepared(&pool, id, CleanupCommand::Reset, 0, Some(&event), None)
                .await
                .unwrap()
        );
        let facts: (bool,String,bool) = sqlx::query_as("SELECT a.active,c.notification_kind,c.ingress_deleted FROM social.scene_stream_access a JOIN social.scene_stream_cleanup c ON a.id=c.access_id WHERE a.id=$1")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(facts, (false, "streaming_key_revoke".into(), false));
        let stored: (Address, serde_json::Value) = sqlx::query_as(
            "SELECT recipient,metadata FROM social.scene_stream_cleanup WHERE access_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, (event.recipient, event.metadata));
        sqlx::query("DELETE FROM social.scene_stream_cleanup WHERE access_id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM social.scene_stream_access WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        // The same idle command succeeds once the stream has stopped and commits
        // its cleanup intent together with deactivation, without a notification.
        let idle = Uuid::new_v4();
        sqlx::query("INSERT INTO social.scene_stream_access(id,place_id,streaming_url,streaming_key,generated_by,streaming) VALUES($1,$2,'rtmp://example.org','idle-key','test',false)")
            .bind(idle).bind(format!("idle-test-{idle}")).execute(&pool).await.unwrap();
        assert!(
            enqueue_prepared(&pool, idle, CleanupCommand::IdleReplacement, 0, None, None)
                .await
                .unwrap()
        );
        let idle_facts: (bool, bool, Option<String>) = sqlx::query_as(
            "SELECT a.active,a.streaming,c.notification_kind FROM social.scene_stream_access a JOIN social.scene_stream_cleanup c ON a.id=c.access_id WHERE a.id=$1",
        ).bind(idle).fetch_one(&pool).await.unwrap();
        assert_eq!(idle_facts, (false, false, None));
        sqlx::query("DELETE FROM social.scene_stream_cleanup WHERE access_id=$1")
            .bind(idle)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM social.scene_stream_access WHERE id=$1")
            .bind(idle)
            .execute(&pool)
            .await
            .unwrap();

        // The accepted four-day credential deadline also revokes a running stream.
        let running = Uuid::new_v4();
        sqlx::query("INSERT INTO social.scene_stream_access(id,place_id,streaming_url,streaming_key,generated_by,streaming,expiration_time) VALUES($1,$2,'rtmp://example.org','test-key','test',true,timezone('UTC',now())+interval '4 days')")
            .bind(running).bind(format!("deadline-test-{running}")).execute(&pool).await.unwrap();
        assert!(
            !enqueue_prepared(&pool, running, CleanupCommand::KeyExpiry, 0, None, None)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE social.scene_stream_access SET expiration_time=timezone('UTC',now())-interval '1 second' WHERE id=$1")
            .bind(running).execute(&pool).await.unwrap();
        // Unresolved historic owner does not prevent durable revocation intent.
        assert!(
            enqueue_prepared(&pool, running, CleanupCommand::KeyExpiry, 0, None, None)
                .await
                .unwrap()
        );
        let stopped: bool = sqlx::query_scalar(
            "SELECT NOT active AND NOT streaming FROM social.scene_stream_access WHERE id=$1",
        )
        .bind(running)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(stopped);
        sqlx::query("DELETE FROM social.scene_stream_cleanup WHERE access_id=$1")
            .bind(running)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM social.scene_stream_access WHERE id=$1")
            .bind(running)
            .execute(&pool)
            .await
            .unwrap();
        let settling = Uuid::new_v4();
        let now = chrono::Utc::now().naive_utc();
        sqlx::query("INSERT INTO social.scene_stream_access(id,place_id,ingress_id,streaming_url,streaming_key,generated_by,streaming,streaming_start_time) VALUES($1,$2,'IN_settling','rtmp://example.org','test','test',true,$3)")
            .bind(settling).bind(format!("settling-test-{settling}")).bind(now).execute(&pool).await.unwrap();
        let mut snapshot = Snapshot {
            id: settling,
            ingress_id: "IN_settling".into(),
            streaming_start_time: Some(now),
            streaming_state_version: 0,
            streaming_checked_at: None,
        };
        assert_eq!(
            clear_stale(&pool, &snapshot, now).await.unwrap(),
            0,
            "recent start cannot be cleared"
        );
        let started = now - chrono::Duration::minutes(3);
        snapshot.streaming_start_time = Some(started);
        sqlx::query("UPDATE social.scene_stream_access SET streaming_start_time=$2,streaming_checked_at=$3 WHERE id=$1")
            .bind(settling).bind(started).bind(now).execute(&pool).await.unwrap();
        assert_eq!(
            clear_stale(&pool, &snapshot, now).await.unwrap(),
            0,
            "recently checked cannot be cleared"
        );
        sqlx::query("UPDATE social.scene_stream_access SET streaming_checked_at=NULL,streaming_state_version=1 WHERE id=$1")
            .bind(settling).execute(&pool).await.unwrap();
        assert_eq!(
            clear_stale(&pool, &snapshot, now).await.unwrap(),
            0,
            "duplicate start after snapshot fences stale read"
        );
        snapshot.streaming_state_version = 1;
        assert_eq!(
            clear_stale(&pool, &snapshot, now).await.unwrap(),
            1,
            "unchanged settled snapshot clears"
        );
        sqlx::query("DELETE FROM social.scene_stream_access WHERE id=$1")
            .bind(settling)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }
}
