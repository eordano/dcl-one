//! The outbound cluster feed: the publisher seam the tracker hands assignments to, and the
//! coalescing outbox a broker-backed implementation drains.
//!
//! Publisher methods are fire-and-forget hand-offs: they enqueue and return without waiting
//! on a broker, so a stalled or absent one never slows a tracker pass, and none returns an
//! error the caller has to handle.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use crate::protocol::bus::cluster::ClusterChange;
use crate::protocol::bus::topology::ISLANDS_SUBJECT;
use crate::protocol::pulse::decentraland::common::Position;
use crate::protocol::pulse::decentraland::kernel::comms::v3::{IslandData, IslandStatusMessage};
use crate::protocol::pulse::decentraland::pulse::PeerClusterChange;
use crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot;
use crate::pulse::cluster::control::ControlSnapshot;
use crate::pulse::cluster::{ClusterPass, ClusterSession};

pub use super::assignments::{CurrentAssignment, CurrentAssignments};

/// `IslandData.max_peers` belongs to archipelago's shape and Pulse caps cluster size nowhere, so
/// every island goes out carrying zero rather than a bound this server does not enforce.
const NO_PEER_CAP: u32 = 0;

pub trait ClusterFeedPublisher: Send + Sync {
    fn current_assignments(&self) -> Option<std::sync::Arc<CurrentAssignments>> {
        None
    }

    fn control_snapshot(&self) -> ControlSnapshot {
        ControlSnapshot::default()
    }

    /// A peer's published (post-debounce) cluster assignment changed. `session` names the session
    /// that owns it and, on a takeover, the one it displaced.
    fn publish_cluster_change(
        &self,
        wallet: &str,
        cluster_id: &str,
        realm: &str,
        session: &ClusterSession,
    );

    /// The full cluster topology for a completed pass. `active_peers` is the snapshot board's
    /// active-peer count, not the clustered count: it is what the `engine.discovery` heartbeat
    /// advertises as service load, and a peer with no realm or no live wallet binding is carried
    /// by this server while belonging to no cluster.
    fn publish_topology(&self, pass: &ClusterPass, active_peers: u32);

    /// Commits a completed pass before deriving broker hints from its complete recovery plans.
    fn replace_assignments(
        &self,
        pass: u64,
        valid_for: Duration,
        assignments: Vec<CurrentAssignment>,
    );
}

/// Stats-only mode: the tracker runs and reports metrics, nothing reaches a broker.
pub struct NoopClusterFeedPublisher;

impl ClusterFeedPublisher for NoopClusterFeedPublisher {
    fn publish_cluster_change(&self, _: &str, _: &str, _: &str, _: &ClusterSession) {}

    fn publish_topology(&self, _: &ClusterPass, _: u32) {}

    fn replace_assignments(&self, _: u64, _: Duration, _: Vec<CurrentAssignment>) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedChange {
    pub wallet: String,
    pub cluster_id: String,
    pub realm: String,
    pub session: ClusterSession,
}

/// In-process fake: records every hand-off with zero network I/O, so the tracker's publish
/// decisions can be asserted without a broker.
#[derive(Default)]
pub struct RecordingClusterFeed {
    changes: Mutex<Vec<RecordedChange>>,
    topologies: Mutex<Vec<Vec<(String, usize)>>>,
    active_peers: Mutex<Vec<u32>>,
    current: std::sync::Arc<CurrentAssignments>,
    control: Mutex<ControlSnapshot>,
}

impl RecordingClusterFeed {
    pub fn changes(&self) -> Vec<RecordedChange> {
        self.changes.lock().unwrap().clone()
    }

    pub fn topologies(&self) -> Vec<Vec<(String, usize)>> {
        self.topologies.lock().unwrap().clone()
    }

    pub fn active_peers(&self) -> Vec<u32> {
        self.active_peers.lock().unwrap().clone()
    }

    pub fn current(&self, wallet: &str, session: &str) -> Option<PeerClusterSnapshot> {
        self.current.lookup(wallet, session)
    }

    pub fn clear(&self) {
        self.changes.lock().unwrap().clear();
        self.topologies.lock().unwrap().clear();
        self.active_peers.lock().unwrap().clear();
    }

    pub fn set_control_snapshot(&self, snapshot: ControlSnapshot) {
        *self.control.lock().unwrap() = snapshot;
    }
}

impl ClusterFeedPublisher for RecordingClusterFeed {
    fn current_assignments(&self) -> Option<std::sync::Arc<CurrentAssignments>> {
        Some(self.current.clone())
    }
    fn control_snapshot(&self) -> ControlSnapshot {
        self.control.lock().unwrap().clone()
    }
    fn replace_assignments(
        &self,
        pass: u64,
        valid_for: Duration,
        assignments: Vec<CurrentAssignment>,
    ) {
        let _ = self.current.replace(pass, valid_for, assignments);
    }

    fn publish_cluster_change(
        &self,
        wallet: &str,
        cluster_id: &str,
        realm: &str,
        session: &ClusterSession,
    ) {
        self.changes.lock().unwrap().push(RecordedChange {
            wallet: wallet.to_string(),
            cluster_id: cluster_id.to_string(),
            realm: realm.to_string(),
            session: session.clone(),
        });
    }

    fn publish_topology(&self, pass: &ClusterPass, active_peers: u32) {
        self.active_peers.lock().unwrap().push(active_peers);
        self.topologies.lock().unwrap().push(
            pass.clusters
                .iter()
                .map(|c| (c.id.clone(), c.count))
                .collect(),
        );
    }
}

/// What a `publish_cluster_change` hand-off puts on the bus. An absent displaced session or
/// cluster is the proto3 empty string.
pub fn cluster_change(
    wallet: &str,
    cluster_id: &str,
    realm: &str,
    session: &ClusterSession,
) -> ClusterChange {
    ClusterChange {
        wallet: wallet.to_string(),
        change: PeerClusterChange {
            cluster_id: cluster_id.to_string(),
            realm: realm.to_string(),
            session: session.session.clone(),
            displaced_session: session.displaced_session.clone().unwrap_or_default(),
            displaced_cluster_id: session.displaced_cluster_id.clone().unwrap_or_default(),
            // The committed ledger fills recovery and assignment identity below;
            // this projection supplies only the completed pass's topology hint.
            pending_takeovers: Vec::new(),
            token_not_before: None,
            assignment_version: 0,
            source_incarnation: String::new(),
        },
    }
}

/// Projects a pass onto archipelago's `IslandStatusMessage`, the shape archipelago-core published.
pub fn encode_topology(pass: &ClusterPass) -> Result<Vec<u8>, crate::protocol::bus::BusError> {
    crate::protocol::bus::topology::encode_islands(&topology_message(pass))
}

/// Complete topology for both the single-frame social feed and chunked statistics.
pub(super) fn topology_message(pass: &ClusterPass) -> IslandStatusMessage {
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    let mut data: Vec<IslandData> = Vec::with_capacity(pass.clusters.len());
    for cluster in &pass.clusters {
        by_id.insert(cluster.id.as_str(), data.len());
        data.push(IslandData {
            id: cluster.id.clone(),
            peers: Vec::new(),
            max_peers: NO_PEER_CAP,
            center: Some(Position {
                x: cluster.centroid.x,
                y: cluster.centroid.y,
                z: cluster.centroid.z,
            }),
            radius: cluster.radius as f64,
        });
    }
    for peer in &pass.peers {
        if let Some(&index) = by_id.get(peer.cluster_id.as_str()) {
            data[index].peers.push(peer.wallet.clone());
        }
    }
    IslandStatusMessage { data }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOutcome {
    /// Admitted as a new subject; nothing was lost.
    Admitted,
    /// Replaced an undelivered message for the same subject. Harmless -- the replacement carries
    /// strictly fresher state.
    Superseded,
    /// Admitted, and the longest-admitted pending subject was evicted to make room. The only path
    /// to genuine loss the outbox itself has.
    Dropped,
}

/// The outbox keeps the two feeds apart because they supersede differently. `engine.islands` is a
/// whole-world snapshot, so it lives in a single latest-wins slot and never competes with an
/// assignment. `peer.{addr}.cluster_change` supersedes per peer only, so changes are held one per
/// peer: a peer's newer assignment replaces its own older one, and one peer's event never
/// displaces another's -- which a shared FIFO with oldest-first eviction could not guarantee.
pub struct ClusterOutbox {
    capacity: usize,
    pending_change_by_subject: HashMap<String, Vec<u8>>,
    /// Admission order, not waiting order: a supersede leaves it untouched, so the evicted subject
    /// may be holding a message written a moment ago.
    change_order: VecDeque<String>,
    pending_topology: Option<(tokio::time::Instant, Vec<u8>)>,
}

impl ClusterOutbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            pending_change_by_subject: HashMap::new(),
            change_order: VecDeque::new(),
            pending_topology: None,
        }
    }

    pub fn queue_change(&mut self, subject: String, payload: Vec<u8>) -> QueueOutcome {
        if let Some(pending) = self.pending_change_by_subject.get_mut(&subject) {
            *pending = payload;
            return QueueOutcome::Superseded;
        }

        let mut dropped = false;
        while self.pending_change_by_subject.len() >= self.capacity {
            let Some(evicted) = self.change_order.pop_front() else {
                break;
            };
            if self.pending_change_by_subject.remove(&evicted).is_some() {
                dropped = true;
            }
        }

        self.pending_change_by_subject
            .insert(subject.clone(), payload);
        self.change_order.push_back(subject);

        if dropped {
            QueueOutcome::Dropped
        } else {
            QueueOutcome::Admitted
        }
    }

    /// Retained retry traffic cannot evict a fresh subject. Check and enqueue
    /// while the caller holds the same outbox lock used by fresh producers.
    /// Same-subject replacement still works even with no free slots.
    pub(crate) fn try_queue_retry(
        &mut self,
        subject: String,
        payload: Vec<u8>,
    ) -> Option<QueueOutcome> {
        if !self.pending_change_by_subject.contains_key(&subject)
            && self.pending_change_by_subject.len() >= self.capacity
        {
            return None;
        }
        Some(self.queue_change(subject, payload))
    }

    /// Latest wins: an undelivered snapshot is worthless once a newer one exists. Held apart from
    /// the changes, so it never counts against capacity.
    pub fn queue_topology(&mut self, payload: Vec<u8>) -> QueueOutcome {
        let superseded = self.pending_topology.is_some();
        self.pending_topology = Some((tokio::time::Instant::now(), payload));
        if superseded {
            QueueOutcome::Superseded
        } else {
            QueueOutcome::Admitted
        }
    }

    /// A pending topology snapshot ahead of any pending change, then changes in admission order.
    /// That priority is the whole of it and not an ordering between the two feeds: a snapshot
    /// admitted after a change preempts it, and one superseded before the drain gets here is never
    /// handed out at all.
    pub fn try_dequeue_next(&mut self) -> Option<(String, Vec<u8>)> {
        self.try_dequeue_next_timed()
            .map(|(subject, payload, _)| (subject, payload))
    }

    pub fn try_dequeue_next_timed(&mut self) -> Option<(String, Vec<u8>, tokio::time::Instant)> {
        if let Some((enqueued_at, topology)) = self.pending_topology.take()
            && enqueued_at.elapsed() < crate::topology::TOPOLOGY_RECEIPT_TTL
        {
            return Some((
                ISLANDS_SUBJECT.to_string(),
                topology,
                enqueued_at + crate::topology::TOPOLOGY_RECEIPT_TTL,
            ));
        }
        while let Some(subject) = self.change_order.pop_front() {
            if let Some(payload) = self.pending_change_by_subject.remove(&subject) {
                return Some((
                    subject,
                    payload,
                    tokio::time::Instant::now() + crate::topology::TOPOLOGY_RECEIPT_TTL,
                ));
            }
        }
        None
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn pending_changes(&self) -> usize {
        self.pending_change_by_subject.len()
    }

    pub fn has_topology(&self) -> bool {
        self.pending_topology.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    #[tokio::test(start_paused = true)]
    async fn pending_topology_expires_and_supersession_gets_its_own_deadline() {
        let mut queue = ClusterOutbox::new(4);
        queue.queue_topology(vec![1]);
        tokio::time::advance(crate::topology::TOPOLOGY_RECEIPT_TTL).await;
        assert!(queue.try_dequeue_next().is_none());
        queue.queue_topology(vec![2]);
        tokio::time::advance(Duration::from_secs(4)).await;
        queue.queue_topology(vec![3]);
        let (_, payload, deadline) = queue.try_dequeue_next_timed().unwrap();
        assert_eq!(payload, vec![3]);
        assert_eq!(
            deadline - tokio::time::Instant::now(),
            crate::topology::TOPOLOGY_RECEIPT_TTL
        );
    }

    fn payload(n: u8) -> Vec<u8> {
        vec![n]
    }

    #[test]
    fn a_fresher_change_for_one_peer_supersedes_its_own_older_one() {
        let mut outbox = ClusterOutbox::new(8);
        assert_eq!(
            outbox.queue_change("peer.a.cluster_change".into(), payload(1)),
            QueueOutcome::Admitted
        );
        assert_eq!(
            outbox.queue_change("peer.a.cluster_change".into(), payload(2)),
            QueueOutcome::Superseded
        );
        assert_eq!(outbox.pending_changes(), 1);
        assert_eq!(
            outbox.try_dequeue_next(),
            Some(("peer.a.cluster_change".to_string(), payload(2)))
        );
        assert_eq!(outbox.try_dequeue_next(), None);
    }

    #[test]
    fn retries_preserve_fresh_subjects_but_can_supersede_and_fill_free_slots() {
        let mut outbox = ClusterOutbox::new(1024);
        for n in 0..1024 {
            outbox.queue_change(format!("fresh-{n}"), vec![1]);
        }
        for n in 0..1024 {
            assert_eq!(outbox.try_queue_retry(format!("retry-{n}"), vec![2]), None);
        }
        assert_eq!(
            outbox.try_queue_retry("fresh-1023".into(), vec![3]),
            Some(QueueOutcome::Superseded)
        );
        for n in 0..1024 {
            assert_eq!(
                outbox.try_dequeue_next(),
                Some((
                    format!("fresh-{n}"),
                    if n == 1023 { vec![3] } else { vec![1] }
                ))
            );
        }
        assert_eq!(
            outbox.try_queue_retry("retry".into(), vec![2]),
            Some(QueueOutcome::Admitted)
        );
        assert_eq!(outbox.try_dequeue_next(), Some(("retry".into(), vec![2])));
        assert_eq!(
            ClusterOutbox::new(0).try_queue_retry("none".into(), vec![]),
            None
        );
    }

    #[test]
    fn retry_admission_serializes_with_a_fresh_producer() {
        let outbox = std::sync::Arc::new(Mutex::new(ClusterOutbox::new(2)));
        outbox.lock().unwrap().queue_change("first".into(), vec![1]);
        let producer = outbox.clone();
        std::thread::spawn(move || {
            producer
                .lock()
                .unwrap()
                .queue_change("fresh".into(), vec![2]);
        })
        .join()
        .unwrap();
        let mut queue = outbox.lock().unwrap();
        assert_eq!(queue.try_queue_retry("retained".into(), vec![3]), None);
        assert_eq!(queue.try_dequeue_next(), Some(("first".into(), vec![1])));
        assert_eq!(
            queue.try_queue_retry("retained".into(), vec![3]),
            Some(QueueOutcome::Admitted)
        );
        assert_eq!(queue.try_dequeue_next(), Some(("fresh".into(), vec![2])));
        assert_eq!(queue.try_dequeue_next(), Some(("retained".into(), vec![3])));
    }

    #[test]
    fn past_capacity_the_longest_admitted_subject_is_evicted() {
        let mut outbox = ClusterOutbox::new(2);
        outbox.queue_change("peer.a.cluster_change".into(), payload(1));
        outbox.queue_change("peer.b.cluster_change".into(), payload(2));
        outbox.queue_change("peer.a.cluster_change".into(), payload(3));
        assert_eq!(
            outbox.queue_change("peer.c.cluster_change".into(), payload(4)),
            QueueOutcome::Dropped
        );
        let drained: Vec<String> = std::iter::from_fn(|| outbox.try_dequeue_next())
            .map(|(s, _)| s)
            .collect();
        assert_eq!(
            drained,
            vec![
                "peer.b.cluster_change".to_string(),
                "peer.c.cluster_change".to_string()
            ]
        );
    }

    #[test]
    fn the_topology_slot_is_latest_wins_and_never_counts_against_capacity() {
        let mut outbox = ClusterOutbox::new(1);
        assert_eq!(outbox.queue_topology(payload(1)), QueueOutcome::Admitted);
        assert_eq!(outbox.queue_topology(payload(2)), QueueOutcome::Superseded);
        assert_eq!(
            outbox.queue_change("peer.a.cluster_change".into(), payload(9)),
            QueueOutcome::Admitted
        );
        assert!(outbox.has_topology());
        assert_eq!(outbox.pending_changes(), 1);
    }

    #[test]
    fn a_pending_topology_dequeues_ahead_of_a_change_admitted_before_it() {
        let mut outbox = ClusterOutbox::new(8);
        outbox.queue_change("peer.a.cluster_change".into(), payload(1));
        outbox.queue_topology(payload(2));
        assert_eq!(
            outbox.try_dequeue_next(),
            Some((ISLANDS_SUBJECT.to_string(), payload(2)))
        );
        assert_eq!(
            outbox.try_dequeue_next(),
            Some(("peer.a.cluster_change".to_string(), payload(1)))
        );
    }

    #[test]
    fn the_subject_lower_cases_the_wallet_once() {
        let session = ClusterSession::new("0xsession".to_string());
        assert_eq!(
            cluster_change("0xAbCd", "C1", "realm", &session).subject(),
            "peer.0xabcd.cluster_change"
        );
    }

    #[test]
    fn an_absent_displaced_session_encodes_as_the_proto3_empty_string() {
        let bytes = cluster_change(
            "0xwallet",
            "C1",
            "realm",
            &ClusterSession::new("0xsession".to_string()),
        )
        .encode();
        let decoded = PeerClusterChange::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.cluster_id, "C1");
        assert_eq!(decoded.realm, "realm");
        assert_eq!(decoded.session, "0xsession");
        assert_eq!(decoded.displaced_session, "");
        assert_eq!(decoded.displaced_cluster_id, "");
    }

    #[test]
    fn a_takeover_names_the_displaced_session_and_cluster() {
        let session = ClusterSession {
            session: "0xnew".to_string(),
            displaced_session: Some("0xold".to_string()),
            displaced_cluster_id: Some("C7".to_string()),
        };
        let bytes = cluster_change("0xwallet", "C9", "realm", &session).encode();
        let decoded = PeerClusterChange::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.session, "0xnew");
        assert_eq!(decoded.displaced_session, "0xold");
        assert_eq!(decoded.displaced_cluster_id, "C7");
    }

    /// The projection onto the islands snapshot, recorded before the subjects and decoders moved
    /// to `protocol::bus::topology`: one island with its centre, radius and two members.
    #[test]
    fn the_topology_projection_bytes_are_pinned() {
        use crate::protocol::pulse::decentraland::common::Vector3;
        use crate::pulse::cluster::{ClusterInfo, ClusterPeerInfo};

        let at = Vector3 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };
        let mut pass = ClusterPass::default();
        pass.clusters.push(ClusterInfo {
            id: "island-C1".into(),
            realm: "r".into(),
            count: 2,
            centroid: at,
            radius: 8.0,
        });
        for wallet in ["0xa", "0xb"] {
            pass.peers.push(ClusterPeerInfo {
                peer: 0,
                wallet: wallet.into(),
                cluster_id: "island-C1".into(),
                realm: "r".into(),
                position: at,
                parcel: 0,
            });
        }
        assert_eq!(
            hex::encode(encode_topology(&pass).unwrap()),
            concat!(
                "0a2f0a0969736c616e642d433112033078611203307862220f0d0000803f15000000",
                "401d00004040290000000000002040"
            )
        );
    }
}
