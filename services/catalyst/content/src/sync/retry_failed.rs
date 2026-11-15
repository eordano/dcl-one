//! The retry worker over `content.failed_deployments`: each cycle fixes `now`,
//! sheds the records that are due and at the retry cap, then pages through the
//! due set in `(next_retry_at, entity_id)` order in bounded batches, re-attempting
//! each through the same hash-and-chain-verified path as sync and re-reporting a
//! failure with the attempt it spent and the backoff it earned. Waiting records
//! (deadline ahead) and records without an auth chain never leave the database:
//! the due query excludes them, so a cycle moves only the rows it acts on. Plus
//! the daily prune of records older than a TTL.

use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::{DeploymentContext, FailedDeployment, FailureReason, SyncError};
use crate::deploy_remote_entity::{ContentSource, RemoteDeployment, deploy_entity_streaming};
use crate::failed_deployments_store::{
    DueCursor, LiveFailedDeploymentsStore, prune_failed_deployments,
};
use crate::storage::Storage;
use crate::sync_deployer::LiveSyncDeployer;

const BASE_RETRY_INTERVAL_MS: u64 = 900_000;
const MAX_RETRY_INTERVAL_MS: u64 = 86_400_000;
pub const DEFAULT_MAX_RETRIES: u32 = 10;
/// The old `SYNC_RETRY_CONCURRENCY` default.
pub const DEFAULT_RETRY_CONCURRENCY: usize = 10;
/// Due records fetched per page: the most failure payloads a cycle holds at once.
pub const DEFAULT_RETRY_BATCH_SIZE: usize = 500;
/// At-cap records dropped per give-up statement.
const EVICT_BATCH_SIZE: usize = 1000;

#[derive(Debug, Clone)]
pub struct RetryFailedConfig {
    /// Cadence of the polling loop. Distinct from the per-entity backoff below, which decides
    /// whether an entity the loop walks past is due at all.
    pub retry_delay_ms: u64,
    /// Attempts an entity gets before the worker gives up on it and drops the row
    /// (`MAX_FAILED_DEPLOYMENT_RETRIES`).
    pub max_retries: u32,
    /// Due entities re-attempted at once (`SYNC_RETRY_CONCURRENCY`).
    pub retry_concurrency: usize,
    /// Due records per page of a cycle. A cycle keeps paging until the due set it started with
    /// is drained, so this bounds memory and statement size, not the work a cycle does.
    pub retry_batch_size: usize,
}

impl Default for RetryFailedConfig {
    fn default() -> Self {
        Self {
            retry_delay_ms: 900_000,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_concurrency: DEFAULT_RETRY_CONCURRENCY,
            retry_batch_size: DEFAULT_RETRY_BATCH_SIZE,
        }
    }
}

/// Wait an entity earns after spending `retry_count` attempts: the base interval doubled per
/// attempt, clamped at a day so a long-lived failure still gets a daily look.
fn backoff_ms(retry_count: u32) -> u64 {
    BASE_RETRY_INTERVAL_MS
        .saturating_mul(1u64.checked_shl(retry_count).unwrap_or(u64::MAX))
        .min(MAX_RETRY_INTERVAL_MS)
}

/// Attempt state an entity earns for the attempt about to be dispatched: one more attempt spent
/// against the cap, and the wait that attempt buys. The wait is returned relative because the
/// deadline belongs to the instant the failure is recorded, not to the instant the attempt was
/// dispatched -- the work in between can take a while, and stamping early would hand back a
/// deadline that is already in the past by the time the re-report lands.
fn next_attempt(failure: &FailedDeployment) -> (u32, u64) {
    (failure.retry_count + 1, backoff_ms(failure.retry_count))
}

/// How long a record may stay undecided before the worker gives up on it: past any RPC outage
/// (hours, rarely a day or two) and an operator's fix of a broken RPC, with a daily attempt
/// for most of it.
pub const UNDECIDED_GIVE_UP_MS: i64 = 7 * 86_400_000;

/// `content.failed_deployments` rows waiting for a verdict, set each retry cycle.
pub const UNDECIDED_RECORDS: &str = "content_failed_deployments_undecided";

/// An undecided attempt spends nothing against the cap. It waits as long as the record has been
/// undecided (its first failure, which no re-report moves), between the base interval and a
/// day: the wait doubles attempt by attempt.
fn undecided_attempt(failure: &FailedDeployment, now_ms: i64) -> (u32, u64) {
    let undecided_for = (now_ms - failure.failure_timestamp).max(0) as u64;
    (
        failure.retry_count,
        undecided_for.clamp(BASE_RETRY_INTERVAL_MS, MAX_RETRY_INTERVAL_MS),
    )
}

/// The record a failed retry leaves behind, re-reported at `now_ms`. An undecided record past
/// [`UNDECIDED_GIVE_UP_MS`] becomes a refusal, which is never retried and the prune takes.
fn retried_record(
    failure: &FailedDeployment,
    error: &SyncError,
    attempt: (u32, u64),
    now_ms: i64,
) -> FailedDeployment {
    let reason = error.failure_reason();
    if reason == FailureReason::ValidationUndecided
        && now_ms - failure.failure_timestamp >= UNDECIDED_GIVE_UP_MS
    {
        return FailedDeployment {
            reason: FailureReason::Rejected,
            error_description: format!(
                "Gave up after {} days undecided: {error}",
                UNDECIDED_GIVE_UP_MS / 86_400_000
            ),
            failure_timestamp: now_ms,
            next_retry_at: now_ms,
            ..failure.clone()
        };
    }
    let (retry_count, wait_ms) = match reason {
        FailureReason::ValidationUndecided => undecided_attempt(failure, now_ms),
        _ => attempt,
    };
    FailedDeployment {
        reason,
        error_description: error.to_string(),
        retry_count,
        next_retry_at: now_ms + wait_ms as i64,
        ..failure.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDisposition {
    Due,
    Waiting { remaining_ms: i64 },
    Exhausted,
    NoAuthChain,
}

fn classify(failure: &FailedDeployment, max_retries: u32, now_ms: i64) -> RetryDisposition {
    if failure.auth_chain.is_empty() {
        return RetryDisposition::NoAuthChain;
    }
    if failure.retry_count >= max_retries && failure.reason != FailureReason::ValidationUndecided {
        return RetryDisposition::Exhausted;
    }
    let remaining_ms = failure.next_retry_at - now_ms;
    if remaining_ms > 0 {
        return RetryDisposition::Waiting { remaining_ms };
    }
    RetryDisposition::Due
}

pub struct RetryFailedDeployments {
    config: RetryFailedConfig,
    http_client: reqwest::Client,
    storage: Arc<Storage>,
    deployer: Arc<LiveSyncDeployer>,
    failed_store: Arc<LiveFailedDeploymentsStore>,
    peer_servers: Arc<tokio::sync::RwLock<Vec<String>>>,
}

impl RetryFailedDeployments {
    pub fn new(
        config: RetryFailedConfig,
        http_client: reqwest::Client,
        storage: Arc<Storage>,
        deployer: Arc<LiveSyncDeployer>,
        failed_store: Arc<LiveFailedDeploymentsStore>,
        peer_servers: Arc<tokio::sync::RwLock<Vec<String>>>,
    ) -> Self {
        RetryFailedDeployments {
            config,
            http_client,
            storage,
            deployer,
            failed_store,
            peer_servers,
        }
    }

    /// Drops the records that are due and at or over the cap, in bounded statements until the
    /// due prefix holds no more, answering how many went. Runs before the peer-server guard: a
    /// node that loses its peer list must still shed entries it already gave up on rather than
    /// hold them at the cap forever. A statement that fails leaves the rest for the next cycle.
    async fn evict_exhausted(&self, now_ms: i64) -> u64 {
        let mut evicted = 0u64;
        loop {
            let removed = match self
                .failed_store
                .remove_exhausted_due(now_ms, self.config.max_retries, EVICT_BATCH_SIZE)
                .await
            {
                Ok(removed) => removed,
                Err(e) => {
                    error!(error = %e, "Failed to evict exhausted failed deployments");
                    return evicted;
                }
            };
            for record in &removed {
                warn!(
                    entity_id = %record.entity_id,
                    entity_type = %record.entity_type,
                    retry_count = record.retry_count,
                    "Permanently giving up on failed deployment"
                );
            }
            evicted += removed.len() as u64;
            if removed.len() < EVICT_BATCH_SIZE {
                return evicted;
            }
        }
    }

    /// One cycle: `now` is fixed once, so a record re-reported during the cycle (deadline moved
    /// past `now`) or recorded fresh (deadline at the epoch, before the cursor) waits for the next
    /// cycle, and the keyset cursor moves forward even when a re-report fails to land. Each page
    /// is fully attempted before the next is fetched, so the payloads in memory never exceed one
    /// page. Every due record is reached within the cycle: the due order is by deadline then id,
    /// and the pages run until the set the cycle started with is drained.
    pub async fn execute_retry_cycle(&self) -> Result<RetryStats, SyncError> {
        use std::sync::atomic::{AtomicU64, Ordering};

        let now_ms = chrono::Utc::now().timestamp_millis();
        let evicted = self.evict_exhausted(now_ms).await;
        match self.failed_store.count_undecided().await {
            Ok(undecided) => metrics::gauge!(UNDECIDED_RECORDS).set(undecided as f64),
            Err(e) => warn!(error = %e, "Failed to count undecided failed deployments"),
        }
        let waiting = self.failed_store.count_waiting(now_ms).await?;

        let retry_concurrency = self.config.retry_concurrency.max(1);
        let batch_size = self.config.retry_batch_size.max(1);
        let content_semaphore = Arc::new(Semaphore::new(50));
        let succeeded = AtomicU64::new(0);
        let still_failing = AtomicU64::new(0);
        let mut skipped: u64 = 0;
        let mut pages: u64 = 0;

        let mut cursor: Option<DueCursor> = None;
        let mut servers: Option<Vec<String>> = None;
        loop {
            let batch = self
                .failed_store
                .select_due(now_ms, self.config.max_retries, cursor.as_ref(), batch_size)
                .await?;
            let fetched = batch.failures.len();
            if fetched == 0 {
                break;
            }
            pages += 1;
            if servers.is_none() {
                let peers = self.peer_servers.read().await.clone();
                if peers.is_empty() {
                    warn!(due = fetched, "No peer servers available for retry");
                    return Ok(RetryStats {
                        waiting,
                        evicted,
                        pages,
                        ..Default::default()
                    });
                }
                servers = Some(peers);
            }
            if pages == 1 {
                info!(count = fetched, waiting, "Retrying failed deployments");
            }

            let mut due: Vec<&FailedDeployment> = Vec::with_capacity(fetched);
            for failure in &batch.failures {
                match classify(failure, self.config.max_retries, now_ms) {
                    RetryDisposition::Due => due.push(failure),
                    RetryDisposition::NoAuthChain => {
                        info!(
                            entity_id = %failure.entity_id,
                            entity_type = %failure.entity_type,
                            "Can't retry failed deployment. Because it lacks of authChain"
                        );
                        skipped += 1;
                    }
                    RetryDisposition::Exhausted | RetryDisposition::Waiting { .. } => {
                        debug!(
                            entity_id = %failure.entity_id,
                            retry_count = failure.retry_count,
                            next_retry_at = failure.next_retry_at,
                            "Due query returned a record the cycle no longer acts on"
                        );
                        skipped += 1;
                    }
                }
            }

            let servers_ref: &[String] = servers.as_deref().unwrap_or_default();
            let succeeded_ref = &succeeded;
            let still_failing_ref = &still_failing;
            let content_semaphore = &content_semaphore;

            futures::stream::iter(due)
                .for_each_concurrent(retry_concurrency, |failure| {
                    let content_semaphore = content_semaphore.clone();
                    async move {
                        let attempt = next_attempt(failure);
                        match deploy_entity_streaming(
                            ContentSource {
                                client: &self.http_client,
                                storage: self.storage.clone(),
                                servers: servers_ref,
                                content_semaphore,
                            },
                            self.deployer.as_ref(),
                            RemoteDeployment {
                                entity_id: &failure.entity_id,
                                auth_chain: &failure.auth_chain,
                                context: DeploymentContext::SyncedFix,
                                report: None,
                                retry_state: Some(attempt),
                            },
                        )
                        .await
                        {
                            // Buffered, not yet written: the flush transaction that writes
                            // it deletes the failure record, so a crash or a failed flush
                            // before then leaves the record for the next cycle.
                            Ok(()) => {
                                info!(entity_id = %failure.entity_id, "Retried failed deployment");
                                succeeded_ref.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                debug!(entity_id = %failure.entity_id, error = %e, "Retry still failing");
                                let deferred = retried_record(
                                    failure,
                                    &e,
                                    attempt,
                                    chrono::Utc::now().timestamp_millis(),
                                );
                                if deferred.reason == FailureReason::Rejected
                                    && failure.reason == FailureReason::ValidationUndecided
                                {
                                    warn!(
                                        entity_id = %failure.entity_id,
                                        entity_type = %failure.entity_type,
                                        "Giving up on an entity whose validation stayed undecided"
                                    );
                                }
                                if let Err(report_err) = self.failed_store.report_failure(deferred).await {
                                    error!(entity_id = %failure.entity_id, error = %report_err, "Failed to record retry backoff");
                                }
                                still_failing_ref.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                })
                .await;

            cursor = batch.cursor;
            if fetched < batch_size {
                break;
            }
        }

        let stats = RetryStats {
            succeeded: succeeded.load(Ordering::Relaxed),
            still_failing: still_failing.load(Ordering::Relaxed),
            waiting,
            evicted,
            skipped,
            pages,
        };

        if pages == 0 && evicted == 0 {
            debug!(waiting, "No failed deployments to retry");
        } else {
            info!(
                succeeded = stats.succeeded,
                still_failing = stats.still_failing,
                waiting = stats.waiting,
                evicted = stats.evicted,
                skipped = stats.skipped,
                pages = stats.pages,
                "Retry cycle complete"
            );
        }
        Ok(stats)
    }

    /// The polling loop, until `cancel` fires. `on_cycle` hears each cycle's outcome (the job
    /// registry's progress line).
    pub async fn run(
        &self,
        cancel: CancellationToken,
        on_cycle: impl Fn(Result<&RetryStats, &SyncError>),
    ) {
        info!(
            delay_ms = self.config.retry_delay_ms,
            max_retries = self.config.max_retries,
            "Starting retry-failed-deployments loop"
        );

        let mut delay = std::time::Duration::ZERO;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("Retry-failed-deployments loop stopped");
                    return;
                }
                _ = tokio::time::sleep(delay) => {
                    match self.execute_retry_cycle().await {
                        Ok(stats) => on_cycle(Ok(&stats)),
                        Err(e) => {
                            error!(error = %e, "Error during retry cycle");
                            on_cycle(Err(&e));
                        }
                    }
                    delay = std::time::Duration::from_millis(self.config.retry_delay_ms);
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetryStats {
    pub succeeded: u64,
    pub still_failing: u64,
    /// Records whose deadline was still ahead when the cycle started.
    pub waiting: u64,
    pub evicted: u64,
    /// Due records the cycle fetched but did not attempt (an auth chain that does not parse).
    pub skipped: u64,
    /// Due pages the cycle fetched, the page the peer guard turned back included.
    pub pages: u64,
}

pub async fn run_prune_loop(
    pool: sqlx::PgPool,
    ttl_days: i64,
    cancel: CancellationToken,
    on_pass: impl Fn(Result<u64, &SyncError>),
) {
    let mut delay = std::time::Duration::ZERO;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {
                match prune_failed_deployments(&pool, ttl_days).await {
                    Ok(pruned) => {
                        if pruned > 0 {
                            info!(pruned, ttl_days, "Pruned old failed_deployments");
                        }
                        on_pass(Ok(pruned));
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to prune old failed_deployments");
                        on_pass(Err(&e));
                    }
                }
                delay = std::time::Duration::from_secs(86_400);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{AuthLink, AuthLinkType};

    fn failure(retry_count: u32, next_retry_at: i64) -> FailedDeployment {
        FailedDeployment {
            entity_type: "scene".to_string(),
            entity_id: "bafytest".to_string(),
            reason: FailureReason::DeploymentError,
            auth_chain: vec![AuthLink {
                link_type: AuthLinkType::SIGNER,
                payload: "0x0000000000000000000000000000000000000001".to_string(),
                signature: None,
            }],
            error_description: "boom".to_string(),
            failure_timestamp: 0,
            snapshot_hash: None,
            retry_count,
            next_retry_at,
        }
    }

    fn without_auth_chain(mut failure: FailedDeployment) -> FailedDeployment {
        failure.auth_chain = Vec::new();
        failure
    }

    #[test]
    fn backoff_doubles_per_spent_attempt() {
        assert_eq!(backoff_ms(0), BASE_RETRY_INTERVAL_MS);
        assert_eq!(backoff_ms(1), BASE_RETRY_INTERVAL_MS * 2);
        assert_eq!(backoff_ms(4), BASE_RETRY_INTERVAL_MS * 16);
    }

    #[test]
    fn backoff_clamps_at_a_day_without_overflowing() {
        assert_eq!(backoff_ms(7), MAX_RETRY_INTERVAL_MS);
        assert_eq!(backoff_ms(64), MAX_RETRY_INTERVAL_MS);
        assert_eq!(backoff_ms(u32::MAX), MAX_RETRY_INTERVAL_MS);
    }

    #[test]
    fn an_attempt_spends_one_retry_and_buys_the_backoff_of_the_attempt_before_it() {
        assert_eq!(next_attempt(&failure(0, 0)), (1, BASE_RETRY_INTERVAL_MS));
        assert_eq!(next_attempt(&failure(3, 0)), (4, backoff_ms(3)));
        assert_eq!(
            next_attempt(&failure(9, 1_700_000_000_000)),
            (10, MAX_RETRY_INTERVAL_MS)
        );
    }

    #[test]
    fn an_entity_at_the_cap_is_exhausted_not_retried() {
        assert_eq!(
            classify(&failure(10, 0), 10, 1_000),
            RetryDisposition::Exhausted
        );
        assert_eq!(
            classify(&failure(11, 0), 10, 1_000),
            RetryDisposition::Exhausted
        );
        assert_eq!(classify(&failure(9, 0), 10, 1_000), RetryDisposition::Due);
    }

    #[test]
    fn a_cap_of_zero_gives_up_on_the_first_look() {
        assert_eq!(
            classify(&failure(0, 0), 0, 1_000),
            RetryDisposition::Exhausted
        );
    }

    #[test]
    fn an_entity_still_in_backoff_waits() {
        assert_eq!(
            classify(&failure(2, 5_000), 10, 1_000),
            RetryDisposition::Waiting {
                remaining_ms: 4_000
            }
        );
        assert_eq!(
            classify(&failure(2, 1_000), 10, 1_000),
            RetryDisposition::Due
        );
        assert_eq!(classify(&failure(2, 0), 10, 1_000), RetryDisposition::Due);
    }

    #[test]
    fn the_cap_is_checked_before_the_deadline() {
        assert_eq!(
            classify(&failure(10, i64::MAX), 10, 1_000),
            RetryDisposition::Exhausted
        );
    }

    fn undecided(first_failure: i64, retry_count: u32) -> FailedDeployment {
        FailedDeployment {
            reason: FailureReason::ValidationUndecided,
            failure_timestamp: first_failure,
            ..failure(retry_count, 0)
        }
    }

    #[test]
    fn an_undecided_wait_doubles_from_its_first_failure_up_to_a_day() {
        let mut record = undecided(0, 3);
        let mut now = 0;
        let mut waits = Vec::new();
        for _ in 0..10 {
            let next = retried_record(
                &record,
                &SyncError::ValidationUndecided("rpc down".into()),
                next_attempt(&record),
                now,
            );
            assert_eq!(next.retry_count, 3, "nothing spent against the cap");
            assert_eq!(next.failure_timestamp, 0, "the first failure stays");
            waits.push((next.next_retry_at - now) / 60_000);
            now = next.next_retry_at;
            record = next;
        }
        assert_eq!(waits, [15, 15, 30, 60, 120, 240, 480, 960, 1440, 1440]);
    }

    #[test]
    fn an_undecided_record_is_given_up_after_a_week_as_a_refusal() {
        let undecided_error = SyncError::ValidationUndecided("rpc down".into());
        let record = undecided(1_000, 0);
        let attempt = next_attempt(&record);
        let now = 1_000 + UNDECIDED_GIVE_UP_MS - 1;
        let kept = retried_record(&record, &undecided_error, attempt, now);
        assert_eq!(kept.reason, FailureReason::ValidationUndecided);
        assert_eq!(kept.next_retry_at, now + MAX_RETRY_INTERVAL_MS as i64);

        let now = 1_000 + UNDECIDED_GIVE_UP_MS;
        let given_up = retried_record(&record, &undecided_error, attempt, now);
        assert_eq!(given_up.reason, FailureReason::Rejected);
        assert!(
            given_up
                .error_description
                .starts_with("Gave up after 7 days undecided:"),
            "{}",
            given_up.error_description
        );
        assert_eq!(
            given_up.failure_timestamp, now,
            "the prune counts from the give-up"
        );

        let other = retried_record(&record, &SyncError::Other("peer down".into()), attempt, now);
        assert_eq!(other.reason, FailureReason::DeploymentError);
        assert_eq!((other.retry_count, other.failure_timestamp), (1, 1_000));
    }

    #[test]
    fn an_entity_without_an_auth_chain_is_never_retried_or_evicted() {
        assert_eq!(
            classify(&without_auth_chain(failure(0, 0)), 10, 1_000),
            RetryDisposition::NoAuthChain
        );
        assert_eq!(
            classify(&without_auth_chain(failure(10, 0)), 10, 1_000),
            RetryDisposition::NoAuthChain
        );
        assert_eq!(
            classify(&without_auth_chain(failure(99, i64::MAX)), 0, 1_000),
            RetryDisposition::NoAuthChain
        );
    }
}
