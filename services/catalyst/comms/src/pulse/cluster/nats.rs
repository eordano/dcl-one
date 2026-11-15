//! The broker-backed cluster feed: core NATS, no JetStream.
//!
//! Cluster changes are lossy edges, not a periodic assignment replay. A separate read-only lookup
//! serves a fresh, live, post-debounce wallet/session view; consumers must request it to recover
//! missing state. Topology alone does not reconstruct that ownership.
//!
//! The tracker never touches the socket: `publish_*` enqueues into a coalescing outbox and
//! returns, and a task of its own drains it. A stalled broker therefore costs pending messages,
//! never a slowed pass.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_nats::PublishErrorKind;
use futures::StreamExt;
use prost::Message;
use tokio::sync::Notify;

use crate::protocol::bus::REQUEST_DEADLINE;
use crate::protocol::bus::cluster::{ClusterLookup, MAX_REPLY_BYTES};
use crate::protocol::bus::topology::{DISCOVERY_SUBJECT, encode_discovery};
use crate::pulse::cluster::control::{
    ControlPositions, ControlSnapshot, DEFAULT_CONTROL_POSITION_CAPACITY,
};
use crate::pulse::cluster::feed::{
    ClusterFeedPublisher, ClusterOutbox, CurrentAssignment, CurrentAssignments, QueueOutcome,
    topology_message,
};
use crate::pulse::cluster::{ClusterPass, ClusterSession};

/// Distinct peers that may hold a pending change at once. Capacity eviction is counted, but core
/// NATS delivery can also be lost after a successful local enqueue.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

pub const DEFAULT_SERVER_NAME: &str = "pulse";

/// What the discovery heartbeat announces when the deployment did not inject its commit. Upstream
/// has exactly one fallback and it is this string, so a consumer never has to handle a blank
/// `commit_hash` as a third state.
pub const DEFAULT_COMMIT_HASH: &str = "unknown";

/// Matches archipelago-core's heartbeat, which is what the subject's consumers expect.
pub const DEFAULT_DISCOVERY_INTERVAL_MS: u64 = 10_000;

/// Long enough that a broker restart is not hammered, short enough that a recovered one is picked
/// up inside two clustering passes. Only ever waited on before a connection has been built at all:
/// once one exists, reconnection is the client's own job.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
/// The backoff's cap, and how long a link holds before the backoff resets.
const RECONNECT_CAP: Duration = Duration::from_secs(5);
const LOOKUP_CAPACITY: usize = 128;

#[derive(Debug, Clone)]
pub struct NatsFeedOptions {
    pub url: String,
    pub server_name: String,
    pub commit_hash: String,
    pub discovery_interval_ms: u64,
    pub capacity: usize,
}

impl Default for NatsFeedOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            server_name: DEFAULT_SERVER_NAME.to_string(),
            commit_hash: DEFAULT_COMMIT_HASH.to_string(),
            discovery_interval_ms: DEFAULT_DISCOVERY_INTERVAL_MS,
            capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }
}

struct Shared {
    outbox: Mutex<ClusterOutbox>,
    pending: Notify,
    user_count: AtomicU32,
    current: Arc<CurrentAssignments>,
    control: Mutex<ControlPositions>,
    control_enabled: Notify,
    statistics: Mutex<crate::pulse::statistics::Statistics>,
    statistics_epoch: uuid::Uuid,
    statistics_sequence: AtomicU64,
    statistics_pending: Notify,
}

/// Holds the outbox and the handle to the task draining it. Dropping it aborts that task, so the
/// feed lives exactly as long as the server that owns it.
pub struct NatsClusterFeed {
    shared: Arc<Shared>,
    drain: tokio::task::JoinHandle<()>,
}

impl Drop for NatsClusterFeed {
    fn drop(&mut self) {
        self.drain.abort();
    }
}

impl NatsClusterFeed {
    /// Returns immediately: the first connection is established by the drain task, so a broker
    /// that is down at boot delays no startup and needs no ordering against it in the unit file.
    pub fn spawn(options: NatsFeedOptions) -> Self {
        let statistics_epoch = uuid::Uuid::new_v4();
        let shared = Arc::new(Shared {
            outbox: Mutex::new(ClusterOutbox::new(options.capacity)),
            pending: Notify::new(),
            user_count: AtomicU32::new(0),
            current: Arc::new(CurrentAssignments::default()),
            control: Mutex::new(ControlPositions::default()),
            control_enabled: Notify::new(),
            statistics: Mutex::new(crate::pulse::statistics::Statistics::with_incarnation(
                statistics_epoch,
            )),
            statistics_epoch,
            statistics_sequence: AtomicU64::new(0),
            statistics_pending: Notify::new(),
        });
        let drain = tokio::spawn(supervise(shared.clone(), options));
        Self { shared, drain }
    }

    pub fn pending_changes(&self) -> usize {
        self.shared.outbox.lock().unwrap().pending_changes()
    }

    pub fn enable_control_positions(&self, audience: &str) -> Result<(), &'static str> {
        self.shared.control.lock().unwrap().enable(audience)?;
        self.shared.control_enabled.notify_waiters();
        Ok(())
    }
}

fn record(outcome: QueueOutcome) {
    match outcome {
        QueueOutcome::Admitted => {}
        QueueOutcome::Superseded => crate::pulse::metrics::nats_superseded(),
        QueueOutcome::Dropped => crate::pulse::metrics::nats_dropped(),
    }
}

impl ClusterFeedPublisher for NatsClusterFeed {
    fn current_assignments(&self) -> Option<Arc<CurrentAssignments>> {
        Some(self.shared.current.clone())
    }
    fn control_snapshot(&self) -> ControlSnapshot {
        self.shared.control.lock().unwrap().snapshot()
    }

    fn replace_assignments(
        &self,
        pass: u64,
        valid_for: Duration,
        assignments: Vec<CurrentAssignment>,
    ) {
        for snapshot in self.shared.current.replace_checked(
            pass,
            valid_for,
            assignments,
            Some(&self.shared.control),
        ) {
            queue_snapshot(&self.shared, snapshot);
        }
    }

    fn publish_cluster_change(
        &self,
        wallet: &str,
        cluster_id: &str,
        realm: &str,
        session: &ClusterSession,
    ) {
        // The complete pass commits authority and derives hints in replace_assignments.
        let _ = (wallet, cluster_id, realm, session);
    }

    fn publish_topology(&self, pass: &ClusterPass, active_peers: u32) {
        self.shared
            .user_count
            .store(active_peers, Ordering::Relaxed);
        let payload = topology_message(pass).encode_to_vec();
        self.shared
            .statistics
            .lock()
            .unwrap()
            .topology(payload.clone());
        self.shared.statistics_pending.notify_one();
        if payload.len() > crate::protocol::bus::topology::MAX_ISLANDS_BYTES {
            crate::pulse::metrics::nats_publish_failed();
            tracing::warn!(
                "complete topology exceeds single-frame cap; statistics retains the full report"
            );
            return;
        }
        record(self.shared.outbox.lock().unwrap().queue_topology(payload));
        self.shared.pending.notify_one();
    }
}

/// What a client connection event means for `pulse_nats_connected` and
/// `pulse_nats_reconnects_total`. Separated from the recording so the edges can be asserted
/// without a broker: the client reconnects on its own, so these events are the only place the
/// connection state is observable at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEdge {
    Opened,
    Reopened,
    Lost,
    None,
}

/// Every open past the first is by definition a reconnect. Tested and set in one step, so two
/// opens racing cannot both read themselves as the first.
pub fn connection_edge(ever_connected: &AtomicBool, event: &async_nats::Event) -> ConnectionEdge {
    match event {
        async_nats::Event::Connected => {
            if ever_connected.swap(true, Ordering::SeqCst) {
                ConnectionEdge::Reopened
            } else {
                ConnectionEdge::Opened
            }
        }
        async_nats::Event::Disconnected | async_nats::Event::Closed => ConnectionEdge::Lost,
        _ => ConnectionEdge::None,
    }
}

/// True for the two kinds that mean this publisher is being refused rather than merely disrupted:
/// the credential rejected and the publish rejected. async-nats folds the second into
/// `ServerError::Other`, so it is matched on the broker's own wording.
pub fn server_error_is_refusal(error: &async_nats::ServerError) -> bool {
    match error {
        async_nats::ServerError::AuthorizationViolation => true,
        async_nats::ServerError::Other(text) => {
            text.to_lowercase().contains("permissions violation")
        }
        _ => false,
    }
}

pub fn apply_connection_event(ever_connected: &AtomicBool, event: async_nats::Event) {
    match connection_edge(ever_connected, &event) {
        ConnectionEdge::Opened => {
            crate::pulse::metrics::nats_connected(true);
            tracing::info!("cluster feed connection opened");
        }
        ConnectionEdge::Reopened => {
            crate::pulse::metrics::nats_connected(true);
            crate::pulse::metrics::nats_reconnected();
            tracing::info!("cluster feed connection reopened");
        }
        ConnectionEdge::Lost => {
            crate::pulse::metrics::nats_connected(false);
            tracing::warn!(
                "cluster feed connection lost; pending changes are bounded and current assignments require lookup"
            );
        }
        ConnectionEdge::None => {}
    }
    match &event {
        async_nats::Event::ServerError(error) if server_error_is_refusal(error) => {
            tracing::error!("cluster feed refused by the broker");
        }
        async_nats::Event::ServerError(_) => {
            tracing::warn!("cluster feed broker error");
        }
        async_nats::Event::ClientError(_) => {
            tracing::warn!("cluster feed client error");
        }
        _ => {}
    }
}

fn connect_options(
    options: &NatsFeedOptions,
    ever_connected: Arc<AtomicBool>,
) -> async_nats::ConnectOptions {
    async_nats::ConnectOptions::new()
        .name(options.server_name.clone())
        .client_capacity(options.capacity.max(1))
        .subscription_capacity(LOOKUP_CAPACITY.max(DEFAULT_CONTROL_POSITION_CAPACITY))
        .event_callback(move |event| {
            let ever_connected = ever_connected.clone();
            async move { apply_connection_event(&ever_connected, event) }
        })
}

/// Rebuilds the pipeline only for a connection that was never established: once the client exists
/// it reconnects on its own, and `pulse_nats_connected` follows its events rather than this loop.
/// An authentication failure is retried on the same schedule as a refused socket: credentials are
/// rotated under a running server often enough that treating the rejection as permanent would turn
/// a rotation into an operator-visible outage.
async fn supervise(shared: Arc<Shared>, options: NatsFeedOptions) {
    let ever_connected = Arc::new(AtomicBool::new(false));
    let mut delay = RECONNECT_BACKOFF;
    loop {
        match connect_options(&options, ever_connected.clone())
            .connect(&options.url)
            .await
        {
            Ok(client) => {
                shared.pending.notify_one();
                tracing::info!("cluster feed connected");
                let connected = Instant::now();
                tokio::select! {
                    _ = drain(&shared, &options, &client) => {}
                    _ = serve_lookups(&shared, &client) => {}
                    _ = serve_completions(&shared, &client) => {}
                    _ = serve_control_positions(&shared, &client) => {}
                    _ = serve_statistics(&shared, &client) => {}
                }
                // A socket the server accepts and drops at once is a failure
                // for the backoff: the delay resets after a link that held.
                if connected.elapsed() >= RECONNECT_CAP {
                    delay = RECONNECT_BACKOFF;
                }
                crate::pulse::metrics::nats_connected(false);
            }
            Err(_) => {
                crate::pulse::metrics::nats_connected(false);
                tracing::warn!("cluster feed connect failed");
            }
        }
        tokio::time::sleep(delay.mul_f64(0.5 + rand::random::<f64>() * 0.5)).await;
        delay = delay.saturating_mul(2).min(RECONNECT_CAP);
    }
}

/// Reuses Pulse's connection. Subscription is installed before the first full
/// snapshot so readers can use that first snapshot as the producer-ready fence.
async fn serve_statistics(shared: &Shared, client: &async_nats::Client) {
    use crate::protocol::bus::statistics::{
        OBSERVATIONS_SUBJECT, Observation, PRODUCER_SUBJECT, ProducerRequest,
    };
    let Ok(Ok(observations)) =
        tokio::time::timeout(REQUEST_DEADLINE, client.subscribe(OBSERVATIONS_SUBJECT)).await
    else {
        return;
    };
    let Ok(Ok(control)) =
        tokio::time::timeout(REQUEST_DEADLINE, client.subscribe(PRODUCER_SUBJECT)).await
    else {
        return;
    };
    let mut observations = futures::stream::select(observations, control);
    let receive = async {
        while let Some(message) = observations.next().await {
            if message.subject.as_str() == PRODUCER_SUBJECT {
                let Some(reply) = message.reply.filter(|reply| private_reply(reply.as_str()))
                else {
                    continue;
                };
                let Some(request) = ProducerRequest::decode(&message.payload) else {
                    continue;
                };
                let response = shared.statistics.lock().unwrap().control(request).encode();
                if let Some(response) = response {
                    let _ = tokio::time::timeout(
                        REQUEST_DEADLINE,
                        client.publish(reply, response.into()),
                    )
                    .await;
                }
                continue;
            }
            let Some(observation) = Observation::decode(&message.payload) else {
                crate::pulse::metrics::statistics_observation_rejected();
                continue;
            };
            shared
                .statistics
                .lock()
                .unwrap()
                .observe(observation, chrono::Utc::now().timestamp_millis());
            shared.statistics_pending.notify_one();
        }
    };
    tokio::select! {
        _ = receive => {}
        _ = publish_statistics(shared, client) => {}
    }
}

async fn publish_statistics(shared: &Shared, client: &async_nats::Client) {
    use crate::protocol::bus::statistics::SNAPSHOTS_SUBJECT;
    publish_statistics_with(
        shared,
        || {
            (client.connection_state() == async_nats::connection::State::Connected)
                .then(|| client.max_payload())
        },
        |parts| async {
            for part in parts {
                if client.connection_state() != async_nats::connection::State::Connected {
                    return false;
                }
                if !matches!(
                    tokio::time::timeout(
                        REQUEST_DEADLINE,
                        client.publish(SNAPSHOTS_SUBJECT, part.into())
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    return false;
                }
            }
            true
        },
    )
    .await;
}

/// The transport adapter lets tests control arrivals at actual transfer boundaries.
/// Scheduling, checkpoint selection, encoding and the send deadline stay here.
async fn publish_statistics_with<F: Future<Output = bool>>(
    shared: &Shared,
    ready: impl Fn() -> Option<usize>,
    mut send: impl FnMut(crate::protocol::bus::statistics::Frames) -> F,
) {
    use crate::protocol::bus::statistics::{Frames, TRANSFER_TIMEOUT};
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut recovery = true;
    let mut pending = false;
    loop {
        let full = tokio::select! {
            biased;
            _ = interval.tick() => true,
            _ = shared.statistics_pending.notified() => false,
            _ = std::future::ready(()), if pending => false,
        };
        let Some(limit) = ready() else {
            recovery = true;
            pending = false;
            continue;
        };
        let update = {
            let mut statistics = shared.statistics.lock().unwrap();
            if recovery {
                statistics.require_recovery();
            }
            statistics.next_update(full)
        };
        pending = update.is_some();
        let Some(update) = update else {
            continue;
        };
        let Ok(previous) =
            shared
                .statistics_sequence
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
        else {
            return;
        };
        let producer = shared.statistics_epoch;
        // Await one encoding at a time; a cancelled blocking task may still finish.
        let Ok(Some(parts)) = tokio::task::spawn_blocking(move || {
            Frames::new(&update, producer, previous + 1, limit)
        })
        .await
        else {
            recovery = true;
            pending = false;
            tracing::warn!("statistics transfer exceeds supported capacity or frame budget");
            continue;
        };
        recovery = !matches!(
            tokio::time::timeout(TRANSFER_TIMEOUT, send(parts)).await,
            Ok(true)
        );
        if recovery {
            pending = false;
            tracing::warn!("statistics transfer interrupted; awaiting complete recovery");
            continue;
        }
        tokio::task::yield_now().await;
    }
}

async fn serve_control_positions(shared: &Shared, client: &async_nats::Client) {
    loop {
        let enabled = shared.control_enabled.notified();
        tokio::pin!(enabled);
        enabled.as_mut().enable();
        if shared.control.lock().unwrap().is_enabled() {
            break;
        }
        enabled.await;
    }
    let Ok(updates) = client
        .subscribe(crate::protocol::bus::control_position::SUBJECT)
        .await
    else {
        return;
    };
    let Ok(intents) = client
        .subscribe(crate::protocol::bus::control_position::LegacyRoomIntent::SUBJECT)
        .await
    else {
        return;
    };
    let mut updates = futures::stream::select(updates, intents);
    while let Some(message) = updates.next().await {
        use crate::protocol::bus::control_position::{ControlPosition, LegacyRoomIntent};
        match message.subject.as_str() {
            crate::protocol::bus::control_position::SUBJECT => {
                if let Some(update) = ControlPosition::decode(&message.payload) {
                    shared.control.lock().unwrap().ingest(update);
                }
            }
            LegacyRoomIntent::SUBJECT => {
                if let Some(update) = LegacyRoomIntent::decode(&message.payload) {
                    shared.control.lock().unwrap().ingest_intent(update);
                }
            }
            _ => {}
        }
    }
}

fn snapshot_change(
    snapshot: crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot,
) -> crate::protocol::bus::cluster::ClusterChange {
    let predecessor = snapshot.pending_takeovers.first();
    crate::protocol::bus::cluster::ClusterChange {
        wallet: snapshot.wallet,
        change: crate::protocol::pulse::decentraland::pulse::PeerClusterChange {
            cluster_id: snapshot.cluster_id,
            realm: snapshot.realm,
            session: snapshot.session,
            displaced_session: predecessor
                .map(|p| p.displaced_session.clone())
                .unwrap_or_default(),
            displaced_cluster_id: predecessor
                .map(|p| p.displaced_cluster_id.clone())
                .unwrap_or_default(),
            pending_takeovers: snapshot.pending_takeovers,
            token_not_before: snapshot.token_not_before,
            assignment_version: snapshot.assignment_version,
            source_incarnation: snapshot.source_incarnation,
        },
    }
}

fn queue_snapshot(
    shared: &Shared,
    snapshot: crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot,
) {
    let change = snapshot_change(snapshot);
    record(
        shared
            .outbox
            .lock()
            .unwrap()
            .queue_change(change.subject(), change.encode()),
    );
    shared.pending.notify_one();
}

async fn serve_completions(shared: &Shared, client: &async_nats::Client) {
    use crate::protocol::bus::cluster::Completion;
    let Ok(mut completions) = client.subscribe(Completion::SUBJECTS).await else {
        return;
    };
    let mut retry = tokio::time::interval(Duration::from_millis(1000));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            message = completions.next() => {
                let Some(message) = message else { return };
                let Some(completion) = Completion::decode(message.subject.as_str(), &message.payload) else { continue };
                if let Some(snapshot) = shared.current.complete_checked(&completion.wallet, &completion.completion, Some(&shared.control)) {
                    queue_snapshot(shared, snapshot);
                }
            }
            _ = retry.tick() => {
                let budget = shared.outbox.lock().unwrap().capacity();
                for snapshot in shared.current.retained_hints_checked(budget, Some(&shared.control)) {
                    let change = snapshot_change(snapshot);
                    let admitted = shared.outbox.lock().unwrap()
                        .try_queue_retry(change.subject(), change.encode());
                    if let Some(outcome) = admitted {
                        record(outcome);
                        shared.pending.notify_one();
                    }
                }
            }
        }
    }
}

fn private_reply(subject: &str) -> bool {
    subject.len() <= 256
        && subject.strip_prefix("_INBOX.").is_some_and(|tail| {
            tail.split('.').all(|token| {
                !token.is_empty()
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        })
}

async fn serve_lookups(shared: &Shared, client: &async_nats::Client) {
    let Ok(Ok(mut requests)) =
        tokio::time::timeout(REQUEST_DEADLINE, client.subscribe(ClusterLookup::SUBJECTS)).await
    else {
        return;
    };
    while let Some(request) = requests.next().await {
        let Some(reply) = request
            .reply
            .filter(|subject| private_reply(subject.as_str()))
        else {
            continue;
        };
        let Ok(lookup) = ClusterLookup::decode(request.subject.as_str(), &request.payload) else {
            continue;
        };
        crate::pulse::metrics::cluster_lookup_requested();
        let Some(snapshot) =
            shared
                .current
                .lookup_checked(&lookup.wallet, &lookup.session, Some(&shared.control))
        else {
            continue;
        };
        let payload = snapshot.encode_to_vec();
        if payload.len() > MAX_REPLY_BYTES {
            continue;
        }
        if !matches!(
            tokio::time::timeout(REQUEST_DEADLINE, client.publish(reply, payload.into())).await,
            Ok(Ok(()))
        ) {
            tracing::warn!("cluster lookup reply could not be submitted; rebuilding link");
            return;
        }
        crate::pulse::metrics::cluster_lookup_submitted();
    }
}

/// The publish seam, so the drain loop's failure handling is assertable without a broker.
pub trait FeedSink {
    fn send(
        &self,
        subject: String,
        payload: Vec<u8>,
    ) -> impl Future<Output = Option<PublishErrorKind>> + Send;
}

impl FeedSink for async_nats::Client {
    async fn send(&self, subject: String, payload: Vec<u8>) -> Option<PublishErrorKind> {
        self.publish(subject, payload.into())
            .await
            .err()
            .map(|e| e.kind())
    }
}

/// `Send` is the client's command channel being closed, which is the one publish failure that says
/// this client is finished. A payload the broker will not take and a subject it will not parse are
/// properties of the one message, so the feed drops it and keeps draining -- upstream's
/// PUBLISH_FAILED-and-continue.
pub fn connection_is_gone(kind: PublishErrorKind) -> bool {
    matches!(kind, PublishErrorKind::Send)
}

/// Records a publish outcome and says whether the drain loop may keep going.
pub fn record_publish(subject: &str, error: Option<PublishErrorKind>) -> bool {
    match error {
        None => {
            crate::pulse::metrics::nats_published();
            true
        }
        Some(kind) => {
            crate::pulse::metrics::nats_publish_failed();
            let gone = connection_is_gone(kind);
            tracing::warn!(
                subject = %subject,
                error = %kind,
                connection_lost = gone,
                "cluster feed publish failed; dropping"
            );
            !gone
        }
    }
}

/// Runs until the client itself is gone; every other publish failure costs its one message. The
/// failed message is dropped rather than held, so a broker outage cannot replay a backlog of stale
/// assignments once it ends.
async fn drain<S: FeedSink + Sync>(shared: &Arc<Shared>, options: &NatsFeedOptions, sink: &S) {
    let mut discovery = (options.discovery_interval_ms > 0).then(|| {
        let mut timer = tokio::time::interval(Duration::from_millis(options.discovery_interval_ms));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer
    });
    loop {
        tokio::select! {
            _ = shared.pending.notified() => {
                while let Some((subject, payload, deadline)) = next(shared) {
                    let error = match tokio::time::timeout_at(
                        deadline, sink.send(subject.clone(), payload)
                    ).await {
                        Ok(error) => error,
                        Err(_) => {
                            crate::pulse::metrics::nats_publish_failed();
                            tracing::warn!(%subject, "cluster feed publish deadline; dropping connection");
                            return;
                        }
                    };
                    if !record_publish(&subject, error) {
                        return;
                    }
                }
            }
            _ = next_heartbeat(discovery.as_mut()) => {
                let payload = encode_discovery(
                    &options.server_name,
                    &options.commit_hash,
                    chrono::Utc::now().timestamp_millis().max(0) as u64,
                    shared.user_count.load(Ordering::Relaxed),
                );
                shared.statistics.lock().unwrap().discovery(payload.clone());
                shared.statistics_pending.notify_one();
                let error = match tokio::time::timeout(
                    crate::topology::TOPOLOGY_RECEIPT_TTL, sink.send(DISCOVERY_SUBJECT.to_string(), payload)
                ).await {
                    Ok(error) => error,
                    Err(_) => { crate::pulse::metrics::nats_publish_failed(); return; }
                };
                if !record_publish(DISCOVERY_SUBJECT, error) {
                    return;
                }
            }
        }
    }
}

/// Never resolves when the heartbeat is off, which keeps a disabled interval out of the select
/// without a second copy of the loop.
async fn next_heartbeat(timer: Option<&mut tokio::time::Interval>) {
    match timer {
        Some(timer) => {
            timer.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// The lock is taken and released inside this call so no guard is ever held across an await.
fn next(shared: &Arc<Shared>) -> Option<(String, Vec<u8>, tokio::time::Instant)> {
    shared.outbox.lock().unwrap().try_dequeue_next_timed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_stay_in_private_inboxes() {
        assert!(private_reply("_INBOX.aB12.003"));
        for invalid in [
            "peer.0xa.cluster_change",
            "_INBOX.",
            "_INBOX.*",
            "_INBOX.a.>",
            "_INBOX.a..b",
        ] {
            assert!(!private_reply(invalid));
        }
    }

    fn metric_value(rendered: &str, name: &str) -> Option<f64> {
        rendered
            .lines()
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| line.strip_prefix(name)?.trim().parse().ok())
    }

    #[test]
    fn a_reconnect_drives_the_gauge_down_and_back_up_and_counts_once() {
        let ever = AtomicBool::new(false);
        let rendered = crate::pulse::metrics::render_recorded(|| {
            apply_connection_event(&ever, async_nats::Event::Connected);
            apply_connection_event(&ever, async_nats::Event::Disconnected);
            apply_connection_event(&ever, async_nats::Event::Connected);
        });

        assert_eq!(metric_value(&rendered, "pulse_nats_connected"), Some(1.0));
        assert_eq!(
            metric_value(&rendered, "pulse_nats_reconnects_total"),
            Some(1.0)
        );
    }

    #[test]
    fn the_first_open_is_not_a_reconnect() {
        let ever = AtomicBool::new(false);
        assert_eq!(
            connection_edge(&ever, &async_nats::Event::Connected),
            ConnectionEdge::Opened
        );
        assert_eq!(
            connection_edge(&ever, &async_nats::Event::Connected),
            ConnectionEdge::Reopened
        );
        assert_eq!(
            connection_edge(&ever, &async_nats::Event::Disconnected),
            ConnectionEdge::Lost
        );
        assert_eq!(
            connection_edge(&ever, &async_nats::Event::LameDuckMode),
            ConnectionEdge::None
        );
    }

    #[test]
    fn a_rejected_credential_and_a_rejected_publish_are_refusals() {
        assert!(server_error_is_refusal(
            &async_nats::ServerError::AuthorizationViolation
        ));
        assert!(server_error_is_refusal(&async_nats::ServerError::Other(
            "Permissions Violation for Publish to \"engine.islands\"".to_string()
        )));
        assert!(!server_error_is_refusal(&async_nats::ServerError::Other(
            "unknown protocol operation".to_string()
        )));
    }

    #[test]
    fn a_per_message_failure_keeps_the_feed_and_only_counts_publish_failed() {
        let rendered = crate::pulse::metrics::render_recorded(|| {
            assert!(record_publish(
                "engine.islands",
                Some(PublishErrorKind::MaxPayloadExceeded)
            ));
            assert!(record_publish(
                "peer.0xa.cluster_change",
                Some(PublishErrorKind::InvalidSubject)
            ));
        });

        assert_eq!(
            metric_value(&rendered, "pulse_nats_publish_failed_total"),
            Some(2.0)
        );
        assert_eq!(metric_value(&rendered, "pulse_nats_published_total"), None);
        assert_eq!(metric_value(&rendered, "pulse_nats_dropped_total"), None);
    }

    #[test]
    fn a_dead_client_ends_the_drain() {
        assert!(!record_publish(
            "engine.islands",
            Some(PublishErrorKind::Send)
        ));
        assert!(connection_is_gone(PublishErrorKind::Send));
        assert!(!connection_is_gone(PublishErrorKind::MaxPayloadExceeded));
        assert!(!connection_is_gone(PublishErrorKind::InvalidSubject));
    }

    struct FlakySink {
        sent: Mutex<Vec<String>>,
        reject: Option<PublishErrorKind>,
    }

    impl FeedSink for FlakySink {
        async fn send(&self, subject: String, _: Vec<u8>) -> Option<PublishErrorKind> {
            self.sent.lock().unwrap().push(subject);
            self.reject
        }
    }

    pub(super) fn feed_shared(capacity: usize) -> Arc<Shared> {
        Arc::new(Shared {
            outbox: Mutex::new(ClusterOutbox::new(capacity)),
            pending: Notify::new(),
            user_count: AtomicU32::new(0),
            current: Arc::new(CurrentAssignments::default()),
            control: Mutex::new(ControlPositions::default()),
            control_enabled: Notify::new(),
            statistics: Mutex::new(crate::pulse::statistics::Statistics::default()),
            statistics_epoch: uuid::Uuid::nil(),
            statistics_sequence: AtomicU64::new(0),
            statistics_pending: Notify::new(),
        })
    }

    // On a paused clock the 50 ms window only closes once the drain loop is idle, after it
    // has attempted every queued message.
    #[tokio::test(start_paused = true)]
    async fn the_drain_loop_survives_a_per_message_failure_and_stops_on_a_dead_client() {
        let options = NatsFeedOptions {
            discovery_interval_ms: 0,
            ..Default::default()
        };
        let shared = feed_shared(8);
        {
            let mut outbox = shared.outbox.lock().unwrap();
            outbox.queue_topology(vec![9]);
            for peer in 0..3u32 {
                outbox.queue_change(format!("peer.0x{peer}.cluster_change"), vec![1, 2, 3]);
            }
        }
        shared.pending.notify_one();
        let sink = FlakySink {
            sent: Mutex::new(Vec::new()),
            reject: Some(PublishErrorKind::MaxPayloadExceeded),
        };
        let survived =
            tokio::time::timeout(Duration::from_millis(50), drain(&shared, &options, &sink))
                .await
                .is_err();
        assert!(survived, "a rejected payload must not end the drain loop");
        assert_eq!(
            sink.sent.lock().unwrap().len(),
            4,
            "every queued message is attempted despite the first one failing"
        );

        let shared = feed_shared(8);
        shared
            .outbox
            .lock()
            .unwrap()
            .queue_change("peer.0xa.cluster_change".to_string(), vec![1]);
        shared.pending.notify_one();
        let sink = FlakySink {
            sent: Mutex::new(Vec::new()),
            reject: Some(PublishErrorKind::Send),
        };
        tokio::time::timeout(Duration::from_millis(500), drain(&shared, &options, &sink))
            .await
            .expect("a dead client must end the drain loop");
    }

    #[test]
    fn the_server_name_reaches_the_connection_options() {
        let options = NatsFeedOptions {
            server_name: "pulse-a".to_string(),
            ..Default::default()
        };
        let built = connect_options(&options, Arc::new(AtomicBool::new(false)));
        assert!(
            format!("{built:?}").contains("pulse-a"),
            "the configured server name must reach the client connection name"
        );
    }
}

#[cfg(test)]
#[path = "statistics_tests.rs"]
mod statistics_tests;
