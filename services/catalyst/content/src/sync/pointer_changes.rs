//! One peer's `/pointer-changes` stream: paginated fetch with prefetch,
//! per-delta timestamp plausibility, the overlap each poll re-reads and its
//! suppression, and the confirmed-progress rule for the resume cursor (see
//! [`deploy_entities_from_pointer_changes`]).

use reqwest::Client;
use std::collections::{BTreeSet, HashSet};
use tracing::{debug, info, warn};

use super::repository::LiveDeploymentRepository;

/// A pointer-changes page is at most 500 deltas of entity metadata (a few
/// MB); the cap stops a misbehaving peer from growing the buffer without
/// limit, as every other reader's does.
const MAX_PAGE_BYTES: usize = 64 * 1024 * 1024;
use super::{SyncDeployment, SyncError, Timestamp};
use crate::batch_deployer::{BatchDeployer, DeploymentReport};

#[derive(Debug, Clone)]
pub struct PointerChangesOptions<'a> {
    pub from_timestamp: Timestamp,
    pub wait_time_ms: u64,
    pub entity_type_filter: Option<&'a HashSet<String>>,
}

/// The peers a scheduled entity's files may be downloaded from; read per entity, so a running
/// stream follows a changed peer list.
pub type PeerUrls = std::sync::RwLock<std::sync::Arc<[String]>>;

/// Where a stream hands its deployments and how their acknowledgement is tracked.
pub struct ScheduleTarget<'a> {
    pub deployer: &'a BatchDeployer,
    pub content_servers: &'a PeerUrls,
    pub report: &'a std::sync::Arc<DeploymentReport>,
}

/// Upstream MAX_TIMESTAMP_CLOCK_SKEW_IN_MS: how far ahead of our own clock a remote timestamp
/// may sit and still be adopted as sync state.
pub(crate) const MAX_TIMESTAMP_CLOCK_SKEW_MS: Timestamp = 24 * 60 * 60 * 1000;

/// Bound on rejected-delta log lines per stream, as upstream's MAX_REJECTED_DELTA_LOGS: a server
/// can put a rejectable delta on every page, so per-item logging is attacker-sized.
const MAX_REJECTED_DELTA_LOGS: u64 = 100;

/// Upstream MAX_BOUNDARY_ROWS_TRACKED. Ours only stops *tracking* past the cap (the oldest rows
/// fall back to the DB-side dedup, wasted work but never wrong) where upstream fails the poll,
/// because for us the suppression is purely an optimization: identity below is entity-level, and
/// the deployer already dedups entities durably.
const MAX_DELIVERED_ROWS_TRACKED: usize = 10_000;

/// How long after stamping a deployment's `localTimestamp` a peer may take to make it visible.
/// Upstream stamps `Date.now()` before it stores the files and commits; a v2 flush stamps its
/// transaction's start (`now()`), and several flushes commit in any order. A poll that read past
/// such a row before it committed would pass it for good, so every poll re-reads this window
/// behind the newest row the stream has seen ([`settled_cursor`]).
pub const POINTER_CHANGES_OVERLAP_MS: Timestamp = 5 * 60_000;

/// The confirmed cursor after a poll from `from` whose pages arrived within `span_ms` of its
/// first request, `greatest` being the newest `localTimestamp` the stream has seen. The peer read
/// every page after stamping `greatest - span_ms`, so a row stamped more than the overlap before
/// that was visible when its page was read, and the poll delivered it: every row stamped before
/// the result has been delivered.
fn settled_cursor(from: Timestamp, greatest: Timestamp, span_ms: Timestamp) -> Timestamp {
    from.max(greatest.saturating_sub(POINTER_CHANGES_OVERLAP_MS.saturating_add(span_ms)))
}

/// Upstream `isUsableTimestamp`. These values become the stream's high-water mark, and the persisted
/// resume state only ever moves forward (`advance_sync_frontier` / `advance_server_sync_cursor` are
/// GREATEST-monotonic), so a single bad one is permanent: the node then polls /pointer-changes from a
/// point no real deployment can exceed and silently stops syncing. Hence the check before the
/// tentative mark ever sees the value. The JS safe-integer clause is a non-concern here: i64 covers
/// every value serde hands us, and anything that saturated on the way in (1e999 -> i64::MAX) fails
/// the upper bound.
pub(crate) fn is_usable_timestamp(ts: Timestamp, now_ms: Timestamp) -> bool {
    ts >= 0 && ts <= now_ms.saturating_add(MAX_TIMESTAMP_CLOCK_SKEW_MS)
}

/// Folds a delta's `localTimestamp` into the tentative high-water mark ONLY when the delta is
/// acceptable -- the upstream-53e9c07 guarantee this function keeps in one place. The mark becomes
/// the confirmed timestamp at the next poll boundary and from there the durable, GREATEST-monotonic
/// frontier, so one hostile delta reaching the max() below would poison the node permanently.
///
/// Rejection skips the single delta rather than failing the stream, matching upstream's deliberate
/// trade: failing would let one permanently-broken record stall every later deployment from that
/// server.
fn accept_delta_and_advance_mark(
    item: serde_json::Value,
    from_timestamp: Timestamp,
    now_ms: Timestamp,
    greatest_timestamp: &mut Timestamp,
    rejected_logged: &mut u64,
) -> Option<SyncDeployment> {
    let mut report_rejected = |reason: &str, detail: String| {
        if *rejected_logged < MAX_REJECTED_DELTA_LOGS {
            *rejected_logged += 1;
            warn!(reason, detail, "Rejecting delta from /pointer-changes");
            if *rejected_logged == MAX_REJECTED_DELTA_LOGS {
                warn!(
                    suppressed_after = MAX_REJECTED_DELTA_LOGS,
                    "Too many rejected deltas from /pointer-changes, suppressing further logs"
                );
            }
        }
    };

    let deployment: SyncDeployment = match serde_json::from_value(item) {
        Ok(d) => d,
        Err(e) => {
            report_rejected("unparseable", e.to_string());
            return None;
        }
    };

    let entity_ts_ok = is_usable_timestamp(deployment.entity_timestamp, now_ms);
    let local_ts_ok = deployment
        .local_timestamp
        .is_none_or(|ts| is_usable_timestamp(ts, now_ms));
    if !entity_ts_ok || !local_ts_ok {
        report_rejected(
            "implausible timestamp",
            format!(
                "entity_id={} entityTimestamp={} localTimestamp={:?}",
                deployment.entity_id, deployment.entity_timestamp, deployment.local_timestamp
            ),
        );
        return None;
    }

    if let Some(local_ts) = deployment.local_timestamp
        && local_ts >= from_timestamp
    {
        *greatest_timestamp = (*greatest_timestamp).max(local_ts);
    }

    Some(deployment)
}

/// Rows already delivered at or after the poll's `from`, so the overlap every poll re-reads costs
/// no `is_entity_deployed` round trip per row; upstream's `boundaryRowFingerprint` map covers the
/// inclusive `from` alone, which is all its polls re-read. Entity-level identity suffices -- the
/// deployer dedups by entity_id anyway -- so this never affects correctness, only wasted work.
#[derive(Default)]
struct DeliveredWindow(BTreeSet<(Timestamp, String, Timestamp)>);

impl DeliveredWindow {
    fn fingerprint(deployment: &SyncDeployment) -> Option<(Timestamp, String, Timestamp)> {
        deployment.local_timestamp.map(|local| {
            (
                local,
                deployment.entity_id.clone(),
                deployment.entity_timestamp,
            )
        })
    }

    /// Forgets the rows before the poll's `from`: the peer does not return them again.
    fn begin_poll(&mut self, from: Timestamp) {
        self.0 = self.0.split_off(&(from, String::new(), Timestamp::MIN));
    }

    fn already_delivered(&self, deployment: &SyncDeployment) -> bool {
        Self::fingerprint(deployment).is_some_and(|fp| self.0.contains(&fp))
    }

    fn record_delivered(&mut self, deployment: &SyncDeployment) {
        if let Some(fp) = Self::fingerprint(deployment) {
            self.0.insert(fp);
            if self.0.len() > MAX_DELIVERED_ROWS_TRACKED {
                self.0.pop_first();
            }
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct PointerChangesPage {
    deltas: Vec<serde_json::Value>,
    #[serde(default)]
    pagination: Option<PaginationInfo>,
}

#[derive(Debug, serde::Deserialize)]
struct PaginationInfo {
    #[serde(default, rename = "moreData")]
    more_data: bool,
    next: Option<String>,
}

/// One page, and whether the peer holds more rows after it.
async fn fetch_page(
    client: &Client,
    url: &str,
) -> Result<(Vec<serde_json::Value>, bool), SyncError> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let body = crate::http::read_body_capped(resp, MAX_PAGE_BYTES)
        .await
        .map_err(|e| SyncError::Other(format!("pointer-changes page {url}: {e}")))?;
    let page: PointerChangesPage = serde_json::from_slice(&body)?;
    let more = page
        .pagination
        .is_some_and(|p| p.more_data || p.next.is_some());
    Ok((page.deltas, more))
}

/// The ascending page after `page`: the keyset of its last row, `localTimestamp` passed through
/// as the peer wrote it. The peer's own `next` link carries no `sortingOrder` (upstream's does
/// not either), so it reads the same rows again downwards: following it, a poll never got past
/// its first page, and a page that one flush filled with a single timestamp stalled the stream
/// for good.
fn keyset_next_url(server: &str, page: &[serde_json::Value]) -> Option<String> {
    let (from, last_id) = page.iter().rev().find_map(|delta| {
        let from = match delta.get("localTimestamp")? {
            serde_json::Value::Number(n) => n.to_string(),
            _ => return None,
        };
        Some((from, delta.get("entityId")?.as_str()?))
    })?;
    let mut url = url::Url::parse(&format!("{server}/pointer-changes")).ok()?;
    url.query_pairs_mut()
        .append_pair("sortingOrder", "ASC")
        .append_pair("sortingField", "local_timestamp")
        .append_pair("from", &from)
        .append_pair("lastId", last_id);
    Some(url.to_string())
}

fn page_entity_ids(page: &[serde_json::Value]) -> HashSet<String> {
    page.iter()
        .filter_map(|delta| delta.get("entityId")?.as_str().map(str::to_owned))
        .collect()
}

/// The resume cursor (`progress`, the return value, and the persisted frontier) only ever reflects
/// CONFIRMED progress: the high-water timestamp stays tentative while entities are merely scheduled
/// and commits at each poll boundary -- the end of a pagination chain -- after the deployer has
/// drained and every deployment this stream scheduled has been acknowledged (deployed, or durably
/// recorded in failed_deployments). A boundary where something remains unacknowledged fails the
/// stream instead, so the caller reconnects from the last confirmed timestamp. Committing per entity,
/// as this used to, let a later confirmed entity carry the cursor past an earlier one still in flight
/// -- a crash in that window skipped it forever. The cursor a boundary confirms is the
/// [`settled_cursor`], the overlap behind the high-water mark, and the next poll starts there.
pub async fn deploy_entities_from_pointer_changes<S>(
    client: &Client,
    server: &str,
    options: &PointerChangesOptions<'_>,
    target: ScheduleTarget<'_>,
    heartbeat_repo: Option<std::sync::Arc<LiveDeploymentRepository>>,
    progress: &std::sync::atomic::AtomicI64,
    should_stop: S,
) -> Result<Timestamp, SyncError>
where
    S: Fn() -> bool,
{
    let ScheduleTarget {
        deployer,
        content_servers,
        report,
    } = target;
    let entity_type_filter = options.entity_type_filter;
    let mut last_beat: Option<std::time::Instant> = None;
    let mut last_persisted = options.from_timestamp;
    info!(
        server,
        from_timestamp = options.from_timestamp,
        has_type_filter = entity_type_filter.is_some(),
        "Starting pointer-changes stream"
    );

    let mut greatest_timestamp = options.from_timestamp;
    let mut confirmed_timestamp = options.from_timestamp;
    let mut rejected_logged: u64 = 0;
    let mut delivered = DeliveredWindow::default();
    let mut url = format!(
        "{}/pointer-changes?sortingOrder=ASC&sortingField=local_timestamp&from={}",
        server, options.from_timestamp
    );

    let spawn_fetch = |u: String| {
        let c = client.clone();
        tokio::spawn(async move { fetch_page(&c, &u).await })
    };

    let mut poll_started = std::time::Instant::now();
    let mut in_flight = Some(spawn_fetch(url.clone()));
    let mut previous_page: HashSet<String> = HashSet::new();

    loop {
        if should_stop() {
            if let Some(h) = in_flight.take() {
                h.abort();
            }
            return Ok(confirmed_timestamp);
        }

        let (items, more) = match in_flight.take() {
            Some(h) => h
                .await
                .map_err(|e| SyncError::Other(format!("pointer-changes prefetch join: {e}")))??,
            None => fetch_page(client, &url).await?,
        };
        let span = poll_started.elapsed();

        if let Some(repo) = &heartbeat_repo
            && last_beat.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10))
        {
            let _ = repo
                .set_sync_heartbeat(chrono::Utc::now().timestamp_millis())
                .await;
            last_beat = Some(std::time::Instant::now());
        }

        if items.is_empty() {
            debug!(server, from = confirmed_timestamp, "No new pointer-changes");
        }

        // A peer that ignores `lastId` on ascending pages (a v2 node from before the keyset
        // bound; upstream honours it) answers the continuation with rows it already sent;
        // ending the chain there hands the rest to the next poll instead of looping on one page.
        let page_ids = page_entity_ids(&items);
        let repeated = !page_ids.is_empty() && page_ids.is_subset(&previous_page);
        if repeated {
            warn!(
                server,
                from = confirmed_timestamp,
                "Peer answered a keyset continuation with the page before it; ending this poll"
            );
        }
        previous_page = page_ids;
        let next = if more && !repeated {
            keyset_next_url(server, &items)
        } else {
            None
        };

        if let Some(next) = &next {
            in_flight = Some(spawn_fetch(next.clone()));
            url = next.clone();
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        for item in items {
            if should_stop() {
                if let Some(h) = in_flight.take() {
                    h.abort();
                }
                return Ok(confirmed_timestamp);
            }

            let Some(deployment) = accept_delta_and_advance_mark(
                item,
                options.from_timestamp,
                now_ms,
                &mut greatest_timestamp,
                &mut rejected_logged,
            ) else {
                continue;
            };

            if delivered.already_delivered(&deployment) {
                continue;
            }

            if let Some(filter) = entity_type_filter
                && !filter.contains(&deployment.entity_type)
            {
                continue;
            }

            delivered.record_delivered(&deployment);
            let servers = content_servers.read().unwrap().clone();
            deployer
                .schedule_entity_deployment(deployment, &servers, Some(report))
                .await?;
        }

        if next.is_none() {
            deployer.on_idle().await?;
            if !report.is_complete() {
                return Err(SyncError::Other(format!(
                    "pointer-changes deployments for {} not fully acknowledged ({} of {}); \
                     reconnecting from the last confirmed timestamp",
                    server,
                    report.acknowledged(),
                    report.scheduled()
                )));
            }
            if report.lost() > 0 {
                return Err(SyncError::Other(format!(
                    "deployer reported silently lost entities while syncing {}; \
                     reconnecting from the last confirmed timestamp",
                    server
                )));
            }
            let span_ms = Timestamp::try_from(span.as_millis()).unwrap_or(Timestamp::MAX);
            confirmed_timestamp = settled_cursor(confirmed_timestamp, greatest_timestamp, span_ms);
            progress.fetch_max(confirmed_timestamp, std::sync::atomic::Ordering::Relaxed);
            if let Some(repo) = &heartbeat_repo
                && confirmed_timestamp > last_persisted
            {
                let _ = repo.advance_sync_frontier(confirmed_timestamp).await;
                let _ = repo
                    .advance_server_sync_cursor(server, confirmed_timestamp)
                    .await;
                last_persisted = confirmed_timestamp;
            }

            if options.wait_time_ms == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(options.wait_time_ms)).await;
            delivered.begin_poll(confirmed_timestamp);
            previous_page.clear();
            url = format!(
                "{}/pointer-changes?sortingOrder=ASC&sortingField=local_timestamp&from={}",
                server, confirmed_timestamp
            );
            poll_started = std::time::Instant::now();
            in_flight = Some(spawn_fetch(url.clone()));
        }
    }

    info!(server, confirmed_timestamp, "Pointer-changes stream ended");
    Ok(confirmed_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(local_timestamp: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "entityId": "QmPoisonPoisonPoisonPoisonPoisonPoisonPoison1",
            "entityType": "scene",
            "pointers": ["0,0"],
            "authChain": [],
            "entityTimestamp": 1_700_000_000_000i64,
            "localTimestamp": local_timestamp,
        })
    }

    const NOW_MS: i64 = 1_750_000_000_000;
    const FROM_TS: i64 = 1_600_000_000_000;

    #[test]
    fn far_future_local_timestamp_is_rejected_and_never_moves_the_mark() {
        let mut mark = FROM_TS;
        let mut logged = 0;
        let rejected = accept_delta_and_advance_mark(
            delta(serde_json::json!(253_402_300_799_000i64)),
            FROM_TS,
            NOW_MS,
            &mut mark,
            &mut logged,
        );
        assert!(rejected.is_none(), "far-future delta must be rejected");
        assert_eq!(mark, FROM_TS, "a rejected delta must not move the mark");

        let negative = accept_delta_and_advance_mark(
            delta(serde_json::json!(-1)),
            FROM_TS,
            NOW_MS,
            &mut mark,
            &mut logged,
        );
        assert!(negative.is_none(), "negative delta must be rejected");
        assert_eq!(mark, FROM_TS);
    }

    #[test]
    fn json_1e999_local_timestamp_cannot_poison_the_mark() {
        let raw = r#"{
            "entityId": "QmPoisonPoisonPoisonPoisonPoisonPoisonPoison1",
            "entityType": "scene",
            "pointers": ["0,0"],
            "authChain": [],
            "entityTimestamp": 1700000000000,
            "localTimestamp": 1e999
        }"#;
        let Ok(item) = serde_json::from_str::<serde_json::Value>(raw) else {
            return;
        };
        let mut mark = FROM_TS;
        let mut logged = 0;
        let rejected = accept_delta_and_advance_mark(item, FROM_TS, NOW_MS, &mut mark, &mut logged);
        assert!(rejected.is_none(), "overflowed timestamp must be rejected");
        assert_eq!(mark, FROM_TS, "a rejected delta must not move the mark");
    }

    #[test]
    fn implausible_entity_timestamp_rejects_the_delta_too() {
        let mut item = delta(serde_json::json!(FROM_TS + 5));
        item["entityTimestamp"] = serde_json::json!(253_402_300_799_000i64);
        let mut mark = FROM_TS;
        let mut logged = 0;
        let rejected = accept_delta_and_advance_mark(item, FROM_TS, NOW_MS, &mut mark, &mut logged);
        assert!(rejected.is_none());
        assert_eq!(mark, FROM_TS);
    }

    #[test]
    fn plausible_delta_is_accepted_and_moves_the_mark() {
        let mut mark = FROM_TS;
        let mut logged = 0;
        let accepted = accept_delta_and_advance_mark(
            delta(serde_json::json!(FROM_TS + 1000)),
            FROM_TS,
            NOW_MS,
            &mut mark,
            &mut logged,
        );
        assert!(accepted.is_some());
        assert_eq!(mark, FROM_TS + 1000);
        assert_eq!(logged, 0);

        let skewed = accept_delta_and_advance_mark(
            delta(serde_json::json!(NOW_MS + MAX_TIMESTAMP_CLOCK_SKEW_MS - 1)),
            FROM_TS,
            NOW_MS,
            &mut mark,
            &mut logged,
        );
        assert!(skewed.is_some());
        assert_eq!(mark, NOW_MS + MAX_TIMESTAMP_CLOCK_SKEW_MS - 1);
    }

    #[test]
    fn delta_without_local_timestamp_is_scheduled_but_never_moves_the_mark() {
        let mut item = delta(serde_json::json!(0));
        item.as_object_mut().unwrap().remove("localTimestamp");
        let mut mark = FROM_TS;
        let mut logged = 0;
        let accepted = accept_delta_and_advance_mark(item, FROM_TS, NOW_MS, &mut mark, &mut logged);
        assert!(accepted.is_some());
        assert_eq!(mark, FROM_TS);
    }

    /// The stream never sends `includeAuthChain`, and `SyncDeployment` parses
    /// `authChain` as required: a peer that dropped it when the flag is off
    /// would land every delta here, and the mark would never advance.
    /// That is why `queries::get_pointer_changes` emits `auth_chain` for
    /// both flag values, as peer.decentraland.org does.
    #[test]
    fn delta_without_auth_chain_is_rejected_and_never_moves_the_mark() {
        let mut item = delta(serde_json::json!(FROM_TS + 1000));
        item.as_object_mut().unwrap().remove("authChain");
        let mut mark = FROM_TS;
        let mut logged = 0;
        let accepted = accept_delta_and_advance_mark(item, FROM_TS, NOW_MS, &mut mark, &mut logged);
        assert!(accepted.is_none());
        assert_eq!(mark, FROM_TS);
        assert_eq!(logged, 1);
    }

    fn deployment_at(entity_id: &str, local_ts: Timestamp) -> SyncDeployment {
        serde_json::from_value(serde_json::json!({
            "entityId": entity_id,
            "entityType": "scene",
            "pointers": ["0,0"],
            "authChain": [],
            "entityTimestamp": 1_700_000_000_000i64,
            "localTimestamp": local_ts,
        }))
        .expect("test deployment must deserialize")
    }

    #[test]
    fn the_window_suppresses_rows_delivered_at_or_after_the_poll_from() {
        let mut window = DeliveredWindow::default();
        let (a, b) = (deployment_at("QmA", 200), deployment_at("QmB", 300));
        assert!(!window.already_delivered(&a));
        window.record_delivered(&a);
        window.record_delivered(&b);

        window.begin_poll(200);
        assert!(window.already_delivered(&a), "a replayed row is skipped");
        assert!(window.already_delivered(&b), "a replayed row is skipped");
        assert!(
            !window.already_delivered(&deployment_at("QmC", 250)),
            "a row that committed after the last poll read past it passes"
        );

        window.begin_poll(250);
        assert!(
            !window.already_delivered(&a),
            "rows before the poll's from are forgotten"
        );
        assert!(window.already_delivered(&b));
    }

    /// An upstream-style peer: `(localTimestamp, lower(entityId))` order, two rows a page, the
    /// keyset honoured on the `from` bound of ascending pages and on the `to` bound of
    /// descending ones, and a `next` link without `sortingOrder`, as upstream writes it. The
    /// `late` rows commit once it has answered one `/pointer-changes` request, whatever their
    /// `localTimestamp`; the counter is the requests it answered.
    async fn spawn_peer(
        rows: Vec<serde_json::Value>,
        late: Vec<serde_json::Value>,
        bodies: std::collections::HashMap<String, Vec<u8>>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use axum::extract::{Path, Query};
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};
        const PAGE: usize = 2;
        let key = |d: &serde_json::Value| {
            (
                d["localTimestamp"].as_i64().unwrap(),
                d["entityId"].as_str().unwrap().to_lowercase(),
            )
        };
        let late_ids: HashSet<String> = late
            .iter()
            .map(|d| d["entityId"].as_str().unwrap().to_owned())
            .collect();
        let mut rows = rows;
        rows.extend(late);
        rows.sort_by_key(key);
        let rows = std::sync::Arc::new(rows);
        let served = std::sync::Arc::new(AtomicUsize::new(0));
        let answered = served.clone();
        let changes = move |Query(q): Query<HashMap<String, String>>| {
            let rows = rows.clone();
            let committed = served.fetch_add(1, Ordering::SeqCst) > 0;
            let late_ids = late_ids.clone();
            async move {
                let num = |k: &str| q.get(k).map(|v| v.parse::<i64>().unwrap());
                let last = q.get("lastId").map(|s| s.to_lowercase());
                let asc = q.get("sortingOrder").map(String::as_str) == Some("ASC");
                let mut hits: Vec<&serde_json::Value> = rows
                    .iter()
                    .filter(|d| committed || !late_ids.contains(d["entityId"].as_str().unwrap()))
                    .filter(|d| {
                        let k = key(d);
                        let from_ok = match (num("from"), &last) {
                            (Some(f), Some(l)) if asc => k > (f, l.clone()),
                            (Some(f), _) => k.0 >= f,
                            (None, _) => true,
                        };
                        let to_ok = match (num("to"), &last) {
                            (Some(t), Some(l)) if !asc => k < (t, l.clone()),
                            (Some(t), _) => k.0 <= t,
                            (None, _) => true,
                        };
                        from_ok && to_ok
                    })
                    .collect();
                if !asc {
                    hits.reverse();
                }
                let more = hits.len() > PAGE;
                hits.truncate(PAGE);
                let next = (more && !hits.is_empty()).then(|| {
                    let last = hits.last().unwrap();
                    let from = num("from")
                        .map(|f| format!("from={f}&"))
                        .unwrap_or_default();
                    format!(
                        "?{from}to={}&limit={PAGE}&lastId={}",
                        last["localTimestamp"],
                        last["entityId"].as_str().unwrap()
                    )
                });
                axum::Json(serde_json::json!({
                    "deltas": hits,
                    "pagination": {"moreData": more, "next": next},
                }))
            }
        };
        let contents = move |Path(hash): Path<String>| {
            let body = bodies.get(&hash).cloned();
            async move { body.ok_or(axum::http::StatusCode::NOT_FOUND) }
        };
        let app = axum::Router::new()
            .route("/pointer-changes", axum::routing::get(changes))
            .route("/contents/{hash}", axum::routing::get(contents));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle, answered)
    }

    /// One signed scene per `localTimestamp`, each on its own pointer `{prefix}:{first + n}`:
    /// the peer's row, the entity id, and the entity file the peer serves for it.
    async fn scenes(
        prefix: &str,
        first: usize,
        stamps: &[Timestamp],
    ) -> Vec<(serde_json::Value, String, Vec<u8>)> {
        let mut out = Vec::new();
        for (n, local_ts) in stamps.iter().enumerate() {
            let pointer = format!("{prefix}:{}", first + n);
            let body = serde_json::to_vec(&serde_json::json!({
                "version": "v3", "type": "scene", "pointers": [pointer],
                "timestamp": 1_000, "content": [], "metadata": {}
            }))
            .unwrap();
            let id = crate::auth::hash_bytes_v1(&body);
            let row = serde_json::json!({
                "entityId": id, "entityType": "scene", "pointers": [pointer],
                "authChain": crate::sync_test_support::signed_chain(&id).await,
                "entityTimestamp": 1_000, "localTimestamp": local_ts,
            });
            out.push((row, id, body));
        }
        out
    }

    /// Streams from `base` into the scratch database from cursor 0 and returns the confirmed
    /// cursor and the report.
    async fn stream_into(
        pool: &sqlx::PgPool,
        base: &str,
        wait_time_ms: u64,
        should_stop: impl Fn() -> bool,
    ) -> (Timestamp, std::sync::Arc<DeploymentReport>) {
        use crate::batch_deployer::BatchDeployerConfig;
        use crate::bloom_filter::BloomFilter;
        use crate::failed_deployments_store::LiveFailedDeploymentsStore;
        use crate::sync_deployer::LiveSyncDeployer;
        use std::sync::Arc;

        let (storage, tmp) = crate::sync_test_support::temp_storage("pointer-changes").await;
        let batch = BatchDeployer::new(
            BatchDeployerConfig::default(),
            reqwest::Client::new(),
            storage,
            pool.clone(),
            Arc::new(LiveSyncDeployer::new(
                pool.clone(),
                2,
                crate::sync_deployer::SyncValidation::AuthChainOnly,
            )),
            Arc::new(LiveFailedDeploymentsStore::new(pool.clone())),
            BloomFilter::with_capacity(100),
        );
        let report = Arc::new(DeploymentReport::default());
        let progress = std::sync::atomic::AtomicI64::new(0);
        let confirmed = deploy_entities_from_pointer_changes(
            &reqwest::Client::new(),
            base,
            &PointerChangesOptions {
                from_timestamp: 0,
                wait_time_ms,
                entity_type_filter: None,
            },
            ScheduleTarget {
                deployer: &batch,
                content_servers: &PeerUrls::new(std::sync::Arc::from(vec![base.to_owned()])),
                report: &report,
            },
            None,
            &progress,
            should_stop,
        )
        .await
        .unwrap();
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        (confirmed, report)
    }

    async fn missing(pool: &sqlx::PgPool, ids: &[String]) -> Vec<String> {
        let mut missing = Vec::new();
        for id in ids {
            if !crate::sync_deployer::is_entity_deployed(pool, id, 0)
                .await
                .unwrap()
            {
                missing.push(id.clone());
            }
        }
        missing
    }

    async fn forget(pool: sqlx::PgPool, prefix: &str) {
        for sql in [
            "DELETE FROM content.active_pointers WHERE pointer LIKE $1",
            "DELETE FROM content.deployments WHERE entity_pointers[1] LIKE $1",
        ] {
            sqlx::query(sql)
                .bind(format!("{prefix}%"))
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
    }

    /// One v2 flush stamps up to 500 rows with one `now()` and a page holds 500: a poll that
    /// followed the peer's `next` link re-read its first page downwards and stopped, so rows
    /// past that page waited for later polls, and a full page at one timestamp stalled the
    /// stream for good. The keyset continuation walks
    /// every row in one poll.
    #[tokio::test]
    async fn one_poll_walks_every_page_past_a_group_sharing_one_timestamp() {
        let Some(pool) = crate::sync_test_support::test_pool(3).await else {
            return;
        };
        let prefix = crate::sync_test_support::unique();
        let rows = scenes(
            &prefix,
            0,
            &[1_000, 1_000, 1_000, 1_000, 1_000, 2_000, 2_000],
        )
        .await;
        let ids: Vec<String> = rows.iter().map(|(_, id, _)| id.clone()).collect();
        let bodies = rows
            .iter()
            .map(|(_, id, b)| (id.clone(), b.clone()))
            .collect();
        let rows = rows.into_iter().map(|(row, _, _)| row).collect();
        let (base, server, _) = spawn_peer(rows, Vec::new(), bodies).await;

        let (confirmed, report) = stream_into(&pool, &base, 0, || false).await;

        server.abort();
        let missing = missing(&pool, &ids).await;
        forget(pool, &prefix).await;
        assert!(missing.is_empty(), "never delivered: {missing:?}");
        assert_eq!(
            confirmed, 0,
            "the overlap keeps the cursor behind rows still committing"
        );
        assert_eq!((report.scheduled(), report.lost()), (7, 0));
    }

    /// A peer row that commits after a poll has read past its `localTimestamp` -- an upstream
    /// upload stamps before it stores the files, and v2 flushes commit out of order -- reaches
    /// the next poll, which re-reads the overlap behind the newest row seen. Polls from the
    /// newest row seen passed it for good.
    #[tokio::test]
    async fn a_row_committed_after_a_poll_read_past_it_reaches_the_next_poll() {
        use std::sync::atomic::Ordering;
        let Some(pool) = crate::sync_test_support::test_pool(3).await else {
            return;
        };
        let prefix = crate::sync_test_support::unique();
        let early = scenes(&prefix, 0, &[1_000_000]).await;
        let late = scenes(&prefix, 1, &[900_000]).await;
        let ids: Vec<String> = early
            .iter()
            .chain(&late)
            .map(|(_, id, _)| id.clone())
            .collect();
        let bodies = early
            .iter()
            .chain(&late)
            .map(|(_, id, b)| (id.clone(), b.clone()))
            .collect();
        let (base, server, served) = spawn_peer(
            early.into_iter().map(|(row, _, _)| row).collect(),
            late.into_iter().map(|(row, _, _)| row).collect(),
            bodies,
        )
        .await;

        let (confirmed, report) =
            stream_into(&pool, &base, 10, || served.load(Ordering::SeqCst) >= 3).await;

        server.abort();
        let missing = missing(&pool, &ids).await;
        forget(pool, &prefix).await;
        assert!(missing.is_empty(), "never delivered: {missing:?}");
        assert!(
            (600_000..=1_000_000 - POINTER_CHANGES_OVERLAP_MS).contains(&confirmed),
            "the cursor settles the overlap behind the newest row: {confirmed}"
        );
        assert_eq!((report.scheduled(), report.lost()), (2, 0));
    }
}
